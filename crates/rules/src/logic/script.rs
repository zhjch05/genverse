//! Package scripts: sandboxed Luau that answers players' intents.
//!
//! Every intent runs the package's scripts in a fresh VM, so nothing carries over between
//! actions except what the scripts keep in the world (entities and their components).
//! The VM has no files, OS, clock or network, a seeded random number generator, and
//! limits on work and memory. Everything a script changes goes through the [`Batch`],
//! which checks it as it happens, so a script sees its own changes and hears at once
//! when it isn't allowed something.
//!
//! A script defines handlers, each called with the event:
//!
//! ```lua
//! function on_place(ev) end     -- ev.item put down at ev.pos
//! function on_interact(ev) end  -- E on ev.entity
//! function on_use(ev) end       -- ev.item used on ev.entity
//! function on_key(ev) end       -- ev.key in ev.entity's zone: "north", "south",
//!                               -- "east", "west", "action", "reset", "enter", "leave"
//! function on_timer(ev) end     -- timer ev.name went off, with ev.data; ev.missed
//!                               -- more times went by unrun (nobody was here)
//! function on_choose(ev) end    -- ev.player picked ev.choice in the panel ev.panel
//!                               -- (its id) that the package showed them
//! function on_arrive(ev) end    -- ev.entity got where walk() sent it (or where its
//!                               -- routine said), at ev.pos; ev.stuck: no way there
//! ```
//!
//! `ev` also has `player` (nil for timers), `pos` (`{x, y, z}` in meters), `day` and
//! `time` (0..1 of the day), and `now` (seconds). The world, as the package sees it (names without a `:` are the package's own;
//! `look`, `label`, `zone` and `pawn` are the engine's, see [`super::entity`]):
//!
//! ```lua
//! get(id)                       -- {id, pos, owner, c = components} or nil
//! find(component)               -- every entity with it, in id order
//! spawn(pos, components) -> id
//! despawn(id)   move(id, pos)   set(id, name, value)   remove(id, name)
//! give(player, item, n)   take(player, item, n)   count(player, item)
//! notify(player, text)
//! random() -> 0..1   random_int(a, b)
//! after(seconds, name, data, near)  -- on_timer once, `seconds` from now (at least 1)
//! every(seconds, name, data, near)  -- on_timer every `seconds`, the first in `seconds`
//! cancel(name)                      -- a timer of the same name is replaced by after/every
//! ```
//!
//! `near` (an entity id, optional) puts the timer to sleep while no player is within
//! 64 m of that entity; it catches up (`ev.missed`) when someone comes back.
//!
//! Players, and the world's voxels:
//!
//! ```lua
//! stat(player, name) -> value or nil        stat_max(player, name) -> max or nil
//! set_stat(player, name, value, max)         -- kept between 0 and max; max optional
//! add_stat(player, name, delta) -> new value
//! hud(player, id, widget)   -- player "*" is everyone; widget nil takes it off:
//!   {kind = "bar", label = "HP", stat = "hp", color = "#d84a4a"}
//!   {kind = "counter", label = "Gold", stat = "gold"}   {kind = "text", text = "..."}
//!   {kind = "items", label = "Coins", item = "coin"}    -- how many they carry
//! show(player, {id, title, text, choices = {{id, label, detail}}, entity})   close(player)
//! build(model, pos, turn) -> build id     unbuild(id)
//! terrain({x, z, w, d}, {name, ops})      -- meters; ops as for land changes
//! ```
//!
//! Pieces that move on their own ([`super::walk`]): the engine finds the way over the
//! ground, and players see them walk.
//!
//! ```lua
//! walk(id, pos, speed)      -- there, at speed m/s (default 1.5); on_arrive when there
//! stop(id)                  -- stops where it is
//! routine(id, {{at = "8:00", to = pos}, ...})   -- where to be when, every day; nil stops
//! wander(id, {radius = 6, speed = 1, pause = 4, home = pos})   -- nil stops
//! spawner(id, {model = "slime", label = "Slime", tag = "slime", max = 4, every = 30,
//!              radius = 8, when = "night", wander = true})       -- nil stops
//! ```
//!
//! `get(id).pos` is where a walking piece is now; `get(id).walking` says whether it is.
//!
//! Timers are how a game moves on its own (crops, countdowns, NPC turns). Nothing runs
//! between them, so a quiet game costs nothing; each timer that does something becomes
//! one change in the log.

use super::{Batch, Event, Grants, Package};

/// Work one action's scripts may do, in Luau interrupts (calls and loop iterations).
pub const MAX_STEPS: u64 = 200_000;
/// Memory one action's scripts may use.
pub const MAX_MEMORY: usize = 16 << 20;

/// Syntax errors in the package's scripts, phrased for the agent that wrote them.
#[cfg(not(target_arch = "wasm32"))]
pub fn check(package: &Package) -> Vec<String> {
    package
        .scripts
        .iter()
        .filter_map(|(file, source)| {
            mlua::Compiler::new().compile(source).err().map(|e| format!("{file}: {}", first_lines(&e.to_string())))
        })
        .collect()
}

#[cfg(target_arch = "wasm32")]
pub fn check(_package: &Package) -> Vec<String> {
    Vec::new()
}

#[cfg(target_arch = "wasm32")]
pub fn run(_package: &Package, _event: &Event, _world: &mut Batch, _grants: &Grants) -> Result<(), String> {
    Err("game scripts don't run on the web yet".into())
}

/// An error's first lines: Luau adds a traceback the player doesn't need.
#[cfg(not(target_arch = "wasm32"))]
fn first_lines(text: &str) -> String {
    text.lines().take_while(|l| !l.starts_with("stack traceback")).take(2).collect::<Vec<_>>().join(" ")
}

/// Runs the handler for `event` in `package`'s scripts, if they have one.
#[cfg(not(target_arch = "wasm32"))]
pub fn run(package: &Package, event: &Event, world: &mut Batch, grants: &Grants) -> Result<(), String> {
    vm::run(package, event, world, grants).map_err(|e| format!("script error: {}", first_lines(&e.to_string())))
}

#[cfg(not(target_arch = "wasm32"))]
mod vm {
    use std::{cell::RefCell, collections::BTreeMap, rc::Rc};

    use mlua::{Error, Function, Lua, LuaOptions, LuaSerdeExt, StdLib, Table, Value, VmState};

    use super::{MAX_MEMORY, MAX_STEPS};
    use crate::logic::{
        Batch, Effect, Event, Grants, Intent, Package, Trigger,
        entity::{EntityId, MIN_WAIT, RESERVED},
    };

    fn fail(text: impl Into<String>) -> Error {
        Error::runtime(text.into())
    }

    fn id_text(id: EntityId) -> String {
        format!("{}.{}", id.change, id.n)
    }

    fn parse_id(text: &str) -> mlua::Result<EntityId> {
        let (change, n) = text.split_once('.').ok_or_else(|| fail(format!("\"{text}\" isn't an entity id")))?;
        match (change.parse(), n.parse()) {
            (Ok(change), Ok(n)) => Ok(EntityId { change, n }),
            _ => Err(fail(format!("\"{text}\" isn't an entity id"))),
        }
    }

    fn pos_table(lua: &Lua, p: [f32; 3]) -> mlua::Result<Table> {
        let t = lua.create_table()?;
        t.set("x", p[0])?;
        t.set("y", p[1])?;
        t.set("z", p[2])?;
        Ok(t)
    }

    fn pos_of(t: &Table) -> mlua::Result<[f32; 3]> {
        let p: [f32; 3] = [t.get("x")?, t.get("y")?, t.get("z")?];
        if p.iter().all(|v| v.is_finite()) { Ok(p) } else { Err(fail("a position needs finite x, y and z")) }
    }

    /// splitmix64: small, and the same everywhere.
    fn next_random(state: &mut u64) -> f64 {
        *state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = *state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        ((z ^ (z >> 31)) >> 11) as f64 / (1u64 << 53) as f64
    }

    pub fn run(package: &Package, event: &Event, world: &mut Batch, grants: &Grants) -> mlua::Result<()> {
        let lua = Lua::new_with(StdLib::MATH | StdLib::STRING | StdLib::TABLE | StdLib::UTF8 | StdLib::BIT, LuaOptions::default())?;
        lua.set_memory_limit(MAX_MEMORY)?;
        let steps = Rc::new(RefCell::new(0u64));
        let counter = steps.clone();
        lua.set_interrupt(move |_| {
            let mut n = counter.borrow_mut();
            *n += 1;
            if *n > MAX_STEPS { Err(fail("the script ran too long (an endless loop?)")) } else { Ok(VmState::Continue) }
        });
        let globals = lua.globals();
        // Nothing that reaches outside the action, or differs between machines.
        for name in ["print", "loadstring", "getfenv", "setfenv", "collectgarbage", "gcinfo", "newproxy"] {
            globals.set(name, Value::Nil)?;
        }
        let math: Table = globals.get("math")?;
        math.set("random", Value::Nil)?;
        math.set("randomseed", Value::Nil)?;

        let ns = package.manifest.name.clone();
        let full = {
            let ns = ns.clone();
            move |name: &str| -> String {
                if name.contains(':') || RESERVED.contains(&name) { name.to_string() } else { format!("{ns}:{name}") }
            }
        };
        let short = {
            let prefix = format!("{ns}:");
            move |name: &str| -> String { name.strip_prefix(&prefix).unwrap_or(name).to_string() }
        };
        let world = RefCell::new(world);
        let rng = RefCell::new(event.seed);

        let entity_table = |lua: &Lua, id: EntityId, world: &Batch| -> mlua::Result<Value> {
            let Some(e) = world.get(id) else { return Ok(Value::Nil) };
            let t = lua.create_table()?;
            t.set("id", id_text(id))?;
            t.set("pos", pos_table(lua, crate::logic::walk::position_at(e, event.at as f64))?)?;
            t.set("walking", e.components.contains_key("walk"))?;
            t.set("owner", e.package.clone())?;
            let c = lua.create_table()?;
            for (name, value) in &e.components {
                c.set(short(name), lua.to_value(value)?)?;
            }
            t.set("c", c)?;
            Ok(Value::Table(t))
        };
        let apply = |effect: Effect| -> mlua::Result<Option<EntityId>> { world.borrow_mut().apply(grants, &effect).map_err(fail) };

        lua.scope(|scope| {
            globals.set(
                "get",
                scope.create_function(|lua, id: String| entity_table(lua, parse_id(&id)?, &world.borrow()))?,
            )?;
            globals.set(
                "find",
                scope.create_function(|lua, component: String| {
                    let name = full(&component);
                    let world = world.borrow();
                    let list = lua.create_table()?;
                    for id in world.ids() {
                        if world.get(id).is_some_and(|e| e.components.contains_key(&name)) {
                            list.push(entity_table(lua, id, &world)?)?;
                        }
                    }
                    Ok(list)
                })?,
            )?;
            globals.set(
                "spawn",
                scope.create_function(|lua, (pos, components): (Table, Option<Table>)| {
                    let mut map = BTreeMap::new();
                    if let Some(components) = components {
                        for pair in components.pairs::<String, Value>() {
                            let (name, value) = pair?;
                            let value: serde_json::Value = lua.from_value(value)?;
                            map.insert(full(&name), value);
                        }
                    }
                    let id = apply(Effect::Spawn { position: pos_of(&pos)?, components: map })?;
                    Ok(id.map(id_text))
                })?,
            )?;
            globals.set(
                "despawn",
                scope.create_function(|_, id: String| apply(Effect::Despawn { entity: parse_id(&id)? }).map(|_| ()))?,
            )?;
            globals.set(
                "move",
                scope.create_function(|_, (id, pos): (String, Table)| {
                    apply(Effect::Move { entity: parse_id(&id)?, position: pos_of(&pos)? }).map(|_| ())
                })?,
            )?;
            globals.set(
                "set",
                scope.create_function(|lua, (id, name, value): (String, String, Value)| {
                    let value: serde_json::Value = lua.from_value(value)?;
                    apply(Effect::Set { entity: parse_id(&id)?, component: full(&name), value }).map(|_| ())
                })?,
            )?;
            globals.set(
                "remove",
                scope.create_function(|_, (id, name): (String, String)| {
                    apply(Effect::Remove { entity: parse_id(&id)?, component: full(&name) }).map(|_| ())
                })?,
            )?;
            globals.set(
                "give",
                scope.create_function(|_, (player, item, n): (String, String, Option<u32>)| {
                    apply(Effect::Give { player, item: full(&item), count: n.unwrap_or(1) }).map(|_| ())
                })?,
            )?;
            globals.set(
                "take",
                // take(item, n), or take(player, item, n) like give: only from the player acting.
                scope.create_function(|_, args: mlua::Variadic<Value>| {
                    let text = |v: Option<&Value>| v.and_then(|v| v.as_string().map(|s| s.to_string_lossy()));
                    let (item, n) = match (text(args.first()), text(args.get(1))) {
                        (Some(player), Some(item)) => {
                            if player != event.player {
                                return Err(fail(format!("take: only from the player acting ({}), not {player}", event.player)));
                            }
                            (item, args.get(2))
                        }
                        (Some(item), _) => (item, args.get(1)),
                        _ => return Err(fail("take: take(item, n) or take(player, item, n)")),
                    };
                    let count = match n {
                        None | Some(Value::Nil) => 1,
                        Some(Value::Integer(n)) if *n >= 0 => *n as u32,
                        Some(Value::Number(n)) if *n >= 0.0 && n.fract() == 0.0 => *n as u32,
                        Some(_) => return Err(fail("take: n is a whole number")),
                    };
                    apply(Effect::Take { item: full(&item), count }).map(|_| ())
                })?,
            )?;
            globals.set(
                "count",
                scope.create_function(|_, (player, item): (String, String)| Ok(world.borrow().items(&player, &full(&item))))?,
            )?;
            globals.set(
                "notify",
                scope.create_function(|_, (player, text): (String, String)| apply(Effect::Notify { player, text }).map(|_| ()))?,
            )?;
            globals.set(
                "stat",
                scope.create_function(|_, (player, name): (String, String)| Ok(world.borrow().stat(&player, &full(&name)).map(|s| s.value)))?,
            )?;
            globals.set(
                "stat_max",
                scope.create_function(|_, (player, name): (String, String)| {
                    Ok(world.borrow().stat(&player, &full(&name)).and_then(|s| s.max))
                })?,
            )?;
            globals.set(
                "set_stat",
                scope.create_function(|_, (player, name, value, max): (String, String, f64, Option<f64>)| {
                    apply(Effect::SetStat { player, stat: full(&name), value, max }).map(|_| ())
                })?,
            )?;
            globals.set(
                "add_stat",
                scope.create_function(|_, (player, name, delta): (String, String, f64)| {
                    let stat = full(&name);
                    let now = world.borrow().stat(&player, &stat).map_or(0.0, |s| s.value);
                    apply(Effect::SetStat { player: player.clone(), stat: stat.clone(), value: now + delta, max: None })?;
                    Ok(world.borrow().stat(&player, &stat).map_or(0.0, |s| s.value))
                })?,
            )?;
            globals.set(
                "hud",
                scope.create_function(|lua, (player, id, widget): (String, String, Value)| {
                    let widget: Option<crate::logic::Widget> = match widget {
                        Value::Nil => None,
                        w => {
                            let mut w: serde_json::Value = lua.from_value(w)?;
                            for key in ["stat", "item"] {
                                if let Some(name) = w[key].as_str() {
                                    w[key] = full(name).into();
                                }
                            }
                            Some(serde_json::from_value(w).map_err(|e| fail(format!("hud: {e}")))?)
                        }
                    };
                    apply(Effect::Hud { player, id, widget }).map(|_| ())
                })?,
            )?;
            globals.set(
                "show",
                scope.create_function(|lua, (player, panel): (String, Value)| {
                    let mut panel: serde_json::Value = lua.from_value(panel)?;
                    // Entities are ids as text in scripts.
                    if let Some(id) = panel["entity"].as_str() {
                        let id = parse_id(id)?;
                        panel["entity"] = serde_json::to_value(id).unwrap_or_default();
                    }
                    if panel["choices"].as_object().is_some_and(|o| o.is_empty()) {
                        panel["choices"] = serde_json::json!([]);
                    }
                    let panel: crate::logic::PanelSpec = serde_json::from_value(panel).map_err(|e| fail(format!("show: {e}")))?;
                    apply(Effect::Show { player, panel: Some(panel) }).map(|_| ())
                })?,
            )?;
            globals.set(
                "close",
                scope.create_function(|_, player: String| apply(Effect::Show { player, panel: None }).map(|_| ()))?,
            )?;
            globals.set(
                "build",
                scope.create_function(|_, (model, pos, turn): (String, Table, Option<u8>)| {
                    apply(Effect::Build { model, position: pos_of(&pos)?, turn: turn.unwrap_or(0) })?;
                    Ok(world.borrow().last_build.map(|id| id.to_string()))
                })?,
            )?;
            globals.set(
                "unbuild",
                scope.create_function(|_, id: String| {
                    let build = id.parse().map_err(|_| fail(format!("\"{id}\" isn't a build id")))?;
                    apply(Effect::Unbuild { build }).map(|_| ())
                })?,
            )?;
            globals.set(
                "terrain",
                scope.create_function(|lua, (area, spec): (Table, Value)| {
                    let cell = |m: f64| (m / crate::terrain::VOXEL_SIZE as f64).floor() as i32;
                    let (x, z, w, d): (f64, f64, f64, f64) = (area.get("x")?, area.get("z")?, area.get("w")?, area.get("d")?);
                    let area = crate::sculpt::Area { min: [cell(x), cell(z)], size: [cell(w).max(1), cell(d).max(1)] };
                    let spec: crate::sculpt::TerrainSpec = lua.from_value(spec)?;
                    apply(Effect::Terrain { area, spec }).map(|_| ())
                })?,
            )?;
            let schedule = |lua: &Lua, (seconds, name, data, near, every): (f64, String, Value, Option<String>, bool)| -> mlua::Result<()> {
                if name.starts_with('~') {
                    return Err(fail("timers starting with ~ are the engine's"));
                }
                if !seconds.is_finite() || seconds < MIN_WAIT as f64 || seconds > 1e9 {
                    return Err(fail(format!("a timer waits at least {MIN_WAIT} second")));
                }
                let seconds = seconds.round() as u64;
                let data: serde_json::Value = lua.from_value(data)?;
                let near = near.as_deref().map(parse_id).transpose()?;
                let effect = Effect::Schedule { name, due: event.at + seconds, every: every.then_some(seconds), data, near };
                apply(effect).map(|_| ())
            };
            globals.set(
                "after",
                scope.create_function(move |lua, (s, name, data, near): (f64, String, Value, Option<String>)| {
                    schedule(lua, (s, name, data, near, false))
                })?,
            )?;
            globals.set(
                "every",
                scope.create_function(move |lua, (s, name, data, near): (f64, String, Value, Option<String>)| {
                    schedule(lua, (s, name, data, near, true))
                })?,
            )?;
            globals.set("cancel", scope.create_function(|_, name: String| apply(Effect::Cancel { name }).map(|_| ()))?)?;
            let all = |effects: Vec<Effect>| effects.into_iter().try_for_each(|e| apply(e).map(|_| ()));
            let staged = &world;
            globals.set(
                "walk",
                scope.create_function(move |_, (id, pos, speed): (String, Table, Option<f32>)| {
                    all(crate::logic::walk::walk_effects(parse_id(&id)?, pos_of(&pos)?, speed, event.at))
                })?,
            )?;
            globals.set(
                "stop",
                scope.create_function(move |_, id: String| {
                    let id = parse_id(&id)?;
                    let e = staged.borrow().get(id).cloned().ok_or_else(|| fail(format!("entity {} doesn't exist", id_text(id))))?;
                    all(crate::logic::walk::stop_effects(id, &e, event.at))
                })?,
            )?;
            globals.set(
                "routine",
                scope.create_function(move |lua, (id, stops): (String, Option<Table>)| {
                    let id = parse_id(&id)?;
                    let value = match stops {
                        None => None,
                        Some(stops) => {
                            let mut list = Vec::new();
                            for stop in stops.sequence_values::<Table>() {
                                let stop = stop?;
                                let at: String = stop.get("at")?;
                                let at = crate::logic::walk::time_of_day(&at).ok_or_else(|| fail(format!("routine: \"{at}\" isn't a time like \"8:30\"")))?;
                                let to = pos_of(&stop.get::<Table>("to")?)?;
                                list.push(serde_json::json!({ "at": at, "to": to }));
                            }
                            let _ = lua;
                            Some(serde_json::Value::Array(list))
                        }
                    };
                    all(crate::logic::walk::start_effects(id, "routine", value, event.at))
                })?,
            )?;
            globals.set(
                "wander",
                scope.create_function(move |lua, (id, spec): (String, Option<Table>)| {
                    let id = parse_id(&id)?;
                    let value = match spec {
                        None => None,
                        Some(spec) => {
                            let home = match spec.get::<Option<Table>>("home")? {
                                Some(t) => pos_of(&t)?,
                                None => staged.borrow().get(id).map(|e| e.position).ok_or_else(|| fail("wander: no such entity"))?,
                            };
                            let mut value: serde_json::Value = lua.from_value(Value::Table(spec))?;
                            value["home"] = serde_json::json!(home);
                            for (key, default) in [("radius", 6.0), ("speed", 1.0), ("pause", 4.0)] {
                                if value[key].is_null() {
                                    value[key] = default.into();
                                }
                            }
                            Some(value)
                        }
                    };
                    all(crate::logic::walk::start_effects(id, "wander", value, event.at))
                })?,
            )?;
            globals.set(
                "spawner",
                scope.create_function(move |lua, (id, spec): (String, Option<Table>)| {
                    let id = parse_id(&id)?;
                    let value = match spec {
                        None => None,
                        Some(spec) => {
                            let mut value: serde_json::Value = lua.from_value(Value::Table(spec))?;
                            if value["when"].is_null() {
                                value["when"] = "any".into();
                            }
                            Some(value)
                        }
                    };
                    all(crate::logic::walk::start_effects(id, "spawner", value, event.at))
                })?,
            )?;
            globals.set("random", scope.create_function(|_, ()| Ok(next_random(&mut rng.borrow_mut())))?)?;
            globals.set(
                "random_int",
                scope.create_function(|_, (a, b): (i64, i64)| {
                    let (lo, hi) = (a.min(b), a.max(b));
                    Ok(lo + (next_random(&mut rng.borrow_mut()) * (hi - lo + 1) as f64) as i64)
                })?,
            )?;

            for (file, source) in &package.scripts {
                lua.load(source.as_str()).set_name(file.as_str()).exec()?;
            }
            let ev = lua.create_table()?;
            if !event.player.is_empty() {
                ev.set("player", event.player)?;
            }
            ev.set("now", event.at)?;
            ev.set("day", event.time.day)?;
            ev.set("time", event.time.time_of_day)?;
            let at = |id: &EntityId| staged.borrow().get(*id).map(|e| e.position);
            let handler = match event.trigger {
                Trigger::Arrive { entity, stuck } => {
                    ev.set("entity", id_text(entity))?;
                    ev.set("stuck", stuck)?;
                    if let Some(p) = at(&entity) {
                        ev.set("pos", pos_table(&lua, p)?)?;
                    }
                    "on_arrive"
                }
                Trigger::Timer { name, data, missed } => {
                    ev.set("name", name)?;
                    ev.set("data", lua.to_value(data)?)?;
                    ev.set("missed", missed)?;
                    "on_timer"
                }
                Trigger::Intent(Intent::Place { item, position }) => {
                    ev.set("item", short(item))?;
                    ev.set("pos", pos_table(&lua, *position)?)?;
                    "on_place"
                }
                Trigger::Intent(Intent::Interact { entity }) => {
                    ev.set("entity", id_text(*entity))?;
                    "on_interact"
                }
                Trigger::Intent(Intent::UseItem { item, target }) => {
                    ev.set("item", short(item))?;
                    ev.set("entity", id_text(*target))?;
                    "on_use"
                }
                Trigger::Intent(Intent::Key { entity, key }) => {
                    ev.set("entity", id_text(*entity))?;
                    ev.set("key", key.as_str())?;
                    "on_key"
                }
                Trigger::Intent(Intent::Choose { choice }) => {
                    let panel = world.borrow().panel(event.player).map(|p| (p.spec.id.clone(), p.spec.entity));
                    if let Some((id, entity)) = panel {
                        ev.set("panel", id)?;
                        if let Some(entity) = entity {
                            ev.set("entity", id_text(entity))?;
                        }
                    }
                    ev.set("choice", choice.clone())?;
                    "on_choose"
                }
            };
            if let Trigger::Intent(Intent::Interact { entity } | Intent::UseItem { target: entity, .. } | Intent::Key { entity, .. }) = event.trigger
                && let Some(p) = at(entity)
            {
                ev.set("pos", pos_table(&lua, p)?)?;
            }
            ev.set("kind", handler.trim_start_matches("on_"))?;
            if let Some(f) = globals.get::<Option<Function>>(handler)? {
                f.call::<()>(ev)?;
            }
            Ok(())
        })
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::logic::{EntityId, GameState, Intent, Manifest, Permission, Scope, Trigger, WorldTime};

    /// A package with one script and a crate model.
    fn package(source: &str) -> Package {
        let manifest = Manifest { name: "game".into(), version: 1, title: "Game".into(), permissions: vec![Permission::Spawn], depends: Vec::new() };
        let crate_model = serde_json::from_str(
            r##"{"name":"Crate","size":[8,8,8],"palette":[{"name":"wood","hex":"#b07a45","material":"matte"}],
                "shapes":[{"kind":"box","color":"wood","from":[0,0,0],"to":[8,8,8],"radius":null,"wobble":null}]}"##,
        )
        .unwrap();
        Package {
            manifest,
            defs: BTreeMap::new(),
            scripts: BTreeMap::from([("main.luau".into(), source.into())]),
            models: BTreeMap::from([("crate".into(), crate_model)]),
        }
    }

    fn act(state: &mut GameState, package: &Package, intent: Intent) -> Result<Vec<(String, String)>, String> {
        let time = WorldTime { day: 0, year: 0, season: 0, day_of_season: 0, time_of_day: 0.0 };
        let event = Event { change: 9, player: "p", trigger: Trigger::Intent(&intent), time, at: 100, seed: 1 };
        let models = package.models.keys().cloned().collect();
        let grants = Grants { package: "game".into(), write: Vec::new(), spawn: true, scope: Scope::World, max_entities: 100, models, drawn: true, build: true };
        let mut batch = state.batch(9, "p", 256);
        run(package, &event, &mut batch, &grants)?;
        let done = batch.finish();
        let notes = done.notes.clone();
        state.commit(done);
        Ok(notes)
    }

    #[test]
    fn scripts_change_the_world_through_checks() {
        let p = package(
            r##"
            function on_place(ev)
              local id = spawn(ev.pos, { box = { n = 1 }, look = { model = "crate" } })
              set(id, "box", { n = get(id).c.box.n + 1 })   -- sees its own change
              notify(ev.player, "placed " .. ev.item .. " #" .. #find("box"))
            end
            "##,
        );
        let mut state = GameState::default();
        let notes = act(&mut state, &p, Intent::Place { item: "game:crate".into(), position: [1.0, 2.04, 3.0] }).unwrap();
        assert_eq!(notes, vec![("p".to_string(), "placed crate #1".to_string())]);
        let e = state.get(EntityId { change: 9, n: 0 }).unwrap();
        assert_eq!(e.components["game:box"]["n"], 2.0);
        // Snapped to the build voxel grid.
        assert_eq!(e.position, [1.0, 2.0, 3.0]);
        let unknown = package("function on_place(ev) spawn(ev.pos, { look = { model = 'barrel' } }) end");
        let place = Intent::Place { item: "game:x".into(), position: [0.0; 3] };
        assert!(act(&mut state, &unknown, place).unwrap_err().contains("models (crate)"));
    }

    #[test]
    fn the_sandbox_holds() {
        let mut state = GameState::default();
        let place = || Intent::Place { item: "game:x".into(), position: [0.0; 3] };
        let error = |source: &str, state: &mut GameState| act(state, &package(source), place()).unwrap_err();
        assert!(error("function on_place() while true do end end", &mut state).contains("ran too long"));
        assert!(error("function on_place() set('1.0', 'other:x', 1) end", &mut state).contains("doesn't exist"));
        assert!(error("function on_place(ev) spawn(ev.pos, { look = { model = 'crate' }, ['other:x'] = 1 }) end", &mut state).contains("belongs to other"));
        assert!(error("function on_place(ev) spawn(ev.pos, { box = true }) end", &mut state).contains("every piece needs a look"));
        // No OS, files or printing: those names don't exist.
        for source in ["function on_place() os.exit() end", "function on_place() io.open('x') end", "function on_place() print('hi') end"] {
            assert!(error(source, &mut state).contains("nil"), "{source}");
        }
        // A failed action changes nothing.
        assert!(error("function on_place(ev) spawn(ev.pos, { pawn = 'p' }); error('nope') end", &mut state).contains("nope"));
        assert!(state.is_empty());
        // Random numbers come from the action's seed: the same every time.
        let roll = package("function on_place(ev) notify('p', tostring(random_int(1, 1000000))) end");
        let a = act(&mut state, &roll, place()).unwrap();
        assert_eq!(a, act(&mut state, &roll, place()).unwrap());
        assert_eq!(check(&package("function on_place( end")).len(), 1);
    }
}
