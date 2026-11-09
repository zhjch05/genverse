//! Player scripts: a package's files named `*.player.luau` run on each player's own
//! machine, every frame, for what has to feel immediate: how the hero moves (fly, float,
//! swim, dash), pieces that bob, spin or swing, and the camera. Nothing they do goes in
//! the log, and the world's own game logic never runs them; they read the world as the
//! player's game has it, and steer only what that player sees and does.
//!
//! Unlike the world's scripts, a player script's VM lives on between frames, so a file
//! may keep its own state in locals (a dash's cooldown). Each frame calls every file's
//! `on_frame(f)` with what the player is doing:
//!
//! ```lua
//! function on_frame(f)
//!   -- f.dt, f.now, f.me, f.keys.space (held), f.pressed.q (just pressed),
//!   -- f.hero = {pos, vel, grounded, in_water, facing}, f.walk = {x, z} (where the
//!   -- usual controls would go), f.camera = {yaw, pitch}, f.body (what the game set),
//!   -- f.gravity (m/s², with body.gravity)
//!   if f.keys.space then hero_velocity({x = f.walk.x, y = 4, z = f.walk.z}) end
//! end
//! ```

use std::{cell::RefCell, rc::Rc};

use mlua::{Function, Lua, LuaOptions, LuaSerdeExt, StdLib, Table, Value, VmState};

use super::{
    Body, EntityId, GameState, Package,
    entity::RESERVED,
    script::vm::{fail, id_text, parse_id, pos_of, pos_table},
    walk::position_at,
};

/// What a file's name ends in when it's a player script.
pub const SUFFIX: &str = ".player.luau";

/// Work a player script may do in one frame: a frame has to be quick.
const MAX_STEPS: u64 = 50_000;
const MAX_MEMORY: usize = 8 << 20;

pub fn is_player_script(file: &str) -> bool {
    file.ends_with(SUFFIX)
}

/// The hero, as the frame starts.
#[derive(Clone, Copy, Debug, Default)]
pub struct Hero {
    pub pos: [f32; 3],
    pub velocity: [f32; 3],
    pub grounded: bool,
    pub in_water: bool,
    /// Radians around Y, 0 facing north (-z).
    pub facing: f32,
}

/// What the player is doing this frame.
pub struct FrameIn<'a> {
    pub dt: f32,
    /// Seconds, Unix time.
    pub now: f64,
    pub me: &'a str,
    pub hero: Hero,
    /// Keys held, and just pressed: "w", "space", "shift", "q", "1", "mouse_left", ...
    pub keys: &'a [&'a str],
    pub pressed: &'a [&'a str],
    /// Where the usual controls would take the hero across the ground, m/s.
    pub walk: [f32; 2],
    /// The camera's yaw and pitch, radians.
    pub camera: [f32; 2],
    pub body: Body,
    /// How fast the hero falls, m/s² (with the body's gravity).
    pub gravity: f32,
}

/// How a piece is shown this frame, on top of where it stands.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pose {
    /// Meters, in the world's axes.
    pub offset: [f32; 3],
    /// Radians around Y, on top of its turn.
    pub turn: f32,
    pub scale: f32,
}

/// How the camera follows the hero this frame (unset: as the player has it).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CameraRig {
    /// Meters behind the hero; 0 looks out of their eyes.
    pub distance: Option<f32>,
    /// Radians: 0 level, up to 1.5 looking straight down.
    pub pitch: Option<f32>,
    /// Radians around Y.
    pub yaw: Option<f32>,
    /// Meters the camera's target rises above the hero's head.
    pub height: Option<f32>,
    /// Meters of shake.
    pub shake: f32,
}

/// What the player scripts decided this frame.
#[derive(Clone, Debug, Default)]
pub struct FrameOut {
    /// The hero's velocity for this frame, m/s: they move with it (the engine keeps
    /// them out of walls). `None`: the usual controls.
    pub velocity: Option<[f32; 3]>,
    pub poses: Vec<(EntityId, Pose)>,
    pub camera: Option<CameraRig>,
    /// What it sent its package's game logic (`send(name, data)`: `on_message`).
    pub sent: Vec<(String, serde_json::Value)>,
}

/// A package's player scripts, loaded once and run every frame.
pub struct PlayerVm {
    lua: Lua,
    package: String,
    steps: Rc<RefCell<u64>>,
    /// Each file's `on_frame`.
    frames: Vec<(String, Function)>,
}

impl PlayerVm {
    /// Loads `package`'s player scripts; `None` if it has none.
    pub fn new(package: &Package) -> Result<Option<Self>, String> {
        let files: Vec<(&String, &String)> = package.scripts.iter().filter(|(f, _)| is_player_script(f)).collect();
        if files.is_empty() {
            return Ok(None);
        }
        let lua = Lua::new_with(StdLib::MATH | StdLib::STRING | StdLib::TABLE | StdLib::UTF8 | StdLib::BIT, LuaOptions::default()).map_err(|e| e.to_string())?;
        lua.set_memory_limit(MAX_MEMORY).map_err(|e| e.to_string())?;
        let steps = Rc::new(RefCell::new(0u64));
        let counter = steps.clone();
        lua.set_interrupt(move |_| {
            let mut n = counter.borrow_mut();
            *n += 1;
            if *n > MAX_STEPS { Err(fail("on_frame ran too long for one frame")) } else { Ok(VmState::Continue) }
        });
        let globals = lua.globals();
        for name in ["print", "loadstring", "getfenv", "setfenv", "collectgarbage", "gcinfo", "newproxy"] {
            globals.set(name, Value::Nil).map_err(|e| e.to_string())?;
        }
        let mut frames = Vec::new();
        for (file, source) in files {
            *steps.borrow_mut() = 0;
            lua.load(source.as_str()).set_name(file.as_str()).exec().map_err(|e| format!("{file}: {e}"))?;
            if let Some(f) = globals.get::<Option<Function>>("on_frame").map_err(|e| e.to_string())? {
                frames.push((file.clone(), f));
                globals.set("on_frame", Value::Nil).map_err(|e| e.to_string())?;
            }
        }
        Ok(Some(Self { lua, package: package.manifest.name.clone(), steps, frames }))
    }

    /// Runs every file's `on_frame` for one frame, with `state` as the player's game has
    /// the world.
    pub fn frame(&mut self, input: &FrameIn, state: &GameState) -> Result<FrameOut, String> {
        let lua = &self.lua;
        let ns = self.package.clone();
        let full = move |name: &str| -> String { if name.contains(':') || RESERVED.contains(&name) { name.to_string() } else { format!("{ns}:{name}") } };
        let prefix = format!("{}:", self.package);
        let short = |name: &str| -> String { name.strip_prefix(&prefix).unwrap_or(name).to_string() };
        let out = RefCell::new(FrameOut::default());
        let now = input.now;
        let entity_table = |lua: &Lua, id: EntityId| -> mlua::Result<Value> {
            let Some(e) = state.get(id) else { return Ok(Value::Nil) };
            let t = lua.create_table()?;
            t.set("id", id_text(id))?;
            t.set("pos", pos_table(lua, position_at(e, now))?)?;
            t.set("walking", e.components.contains_key("walk"))?;
            t.set("owner", e.package.clone())?;
            let c = lua.create_table()?;
            for (name, value) in &e.components {
                c.set(short(name), lua.to_value(value)?)?;
            }
            t.set("c", c)?;
            Ok(Value::Table(t))
        };
        let f = self.frame_table(input).map_err(|e| e.to_string())?;
        let globals = lua.globals();
        let result = lua.scope(|scope| {
            globals.set("get", scope.create_function(|lua, id: String| entity_table(lua, parse_id(&id)?))?)?;
            globals.set(
                "find",
                scope.create_function(|lua, component: String| {
                    let name = full(&component);
                    let list = lua.create_table()?;
                    for (id, e) in state.entities() {
                        if e.components.contains_key(&name) {
                            list.push(entity_table(lua, id)?)?;
                        }
                    }
                    Ok(list)
                })?,
            )?;
            globals.set(
                "near",
                scope.create_function(|lua, (pos, radius, component): (Table, f32, Option<String>)| {
                    let at = pos_of(&pos)?;
                    let radius = radius.clamp(0.0, 128.0);
                    let name = component.as_deref().map(&full);
                    let mut found: Vec<(f32, EntityId)> = state
                        .within([at[0] - radius, at[2] - radius], [at[0] + radius, at[2] + radius])
                        .filter(|(_, e)| name.as_ref().is_none_or(|n| e.components.contains_key(n)))
                        .filter_map(|(id, e)| {
                            let p = position_at(e, now);
                            let d = ((p[0] - at[0]).powi(2) + (p[1] - at[1]).powi(2) + (p[2] - at[2]).powi(2)).sqrt();
                            (d <= radius).then_some((d, id))
                        })
                        .collect();
                    found.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
                    let list = lua.create_table()?;
                    for (d, id) in found {
                        if let Value::Table(t) = entity_table(lua, id)? {
                            t.set("distance", d)?;
                            list.push(t)?;
                        }
                    }
                    Ok(list)
                })?,
            )?;
            globals.set("stat", scope.create_function(|_, name: String| Ok(state.stat(input.me, &full(&name)).map(|s| s.value)))?)?;
            globals.set("count", scope.create_function(|_, item: String| Ok(state.items(input.me, &full(&item))))?)?;
            globals.set(
                "hero_velocity",
                scope.create_function(|_, v: Table| {
                    let v = pos_of(&v)?;
                    if v.iter().any(|c| c.abs() > 60.0) {
                        return Err(fail("hero_velocity: at most 60 m/s each way"));
                    }
                    out.borrow_mut().velocity = Some(v);
                    Ok(())
                })?,
            )?;
            globals.set(
                "pose",
                scope.create_function(|_, (id, spec): (String, Table)| {
                    let id = parse_id(&id)?;
                    if state.get(id).is_none_or(|e| e.package != self.package) {
                        return Err(fail(format!("pose: {} isn't one of {}'s pieces", id_text(id), self.package)));
                    }
                    let num = |key: &str, default: f32| -> mlua::Result<f32> { Ok(spec.get::<Option<f32>>(key)?.unwrap_or(default)) };
                    let pose = Pose { offset: [num("x", 0.0)?, num("y", 0.0)?, num("z", 0.0)?], turn: num("turn", 0.0)?, scale: num("scale", 1.0)?.clamp(0.01, 8.0) };
                    if pose.offset.iter().chain([&pose.turn]).any(|v| !v.is_finite() || v.abs() > 64.0) {
                        return Err(fail("pose: offsets are meters, at most 64 each way"));
                    }
                    out.borrow_mut().poses.push((id, pose));
                    Ok(())
                })?,
            )?;
            globals.set(
                "camera",
                scope.create_function(|_, spec: Table| {
                    let num = |key: &str| -> mlua::Result<Option<f32>> { Ok(spec.get::<Option<f32>>(key)?.filter(|v| v.is_finite())) };
                    let rig = CameraRig {
                        distance: num("distance")?.map(|d| d.clamp(0.0, 40.0)),
                        pitch: num("pitch")?.map(|p| p.clamp(-0.5, 1.5)),
                        yaw: num("yaw")?,
                        height: num("height")?.map(|h| h.clamp(-1.0, 20.0)),
                        shake: num("shake")?.unwrap_or(0.0).clamp(0.0, 2.0),
                    };
                    out.borrow_mut().camera = Some(rig);
                    Ok(())
                })?,
            )?;
            globals.set(
                "send",
                scope.create_function(|lua, (name, data): (String, Value)| {
                    let data: serde_json::Value = lua.from_value(data)?;
                    let mut out = out.borrow_mut();
                    if out.sent.len() >= 4 {
                        return Err(fail("send: at most 4 messages a frame (send when something happens, not every frame)"));
                    }
                    out.sent.push((name, data));
                    Ok(())
                })?,
            )?;
            for (file, on_frame) in &self.frames {
                *self.steps.borrow_mut() = 0;
                on_frame.call::<()>(&f).map_err(|e| mlua::Error::runtime(format!("{file}: {e}")))?;
            }
            Ok(())
        });
        result.map_err(|e| first_line(&e.to_string()))?;
        Ok(out.into_inner())
    }

    fn frame_table(&self, input: &FrameIn) -> mlua::Result<Table> {
        let lua = &self.lua;
        let f = lua.create_table()?;
        f.set("dt", input.dt)?;
        f.set("now", input.now)?;
        f.set("me", input.me)?;
        let hero = lua.create_table()?;
        hero.set("pos", pos_table(lua, input.hero.pos)?)?;
        hero.set("vel", pos_table(lua, input.hero.velocity)?)?;
        hero.set("grounded", input.hero.grounded)?;
        hero.set("in_water", input.hero.in_water)?;
        hero.set("facing", input.hero.facing)?;
        f.set("hero", hero)?;
        let set = |keys: &[&str]| -> mlua::Result<Table> {
            let t = lua.create_table()?;
            for k in keys {
                t.set(*k, true)?;
            }
            Ok(t)
        };
        f.set("keys", set(input.keys)?)?;
        f.set("pressed", set(input.pressed)?)?;
        let walk = lua.create_table()?;
        walk.set("x", input.walk[0])?;
        walk.set("z", input.walk[1])?;
        f.set("walk", walk)?;
        let camera = lua.create_table()?;
        camera.set("yaw", input.camera[0])?;
        camera.set("pitch", input.camera[1])?;
        f.set("camera", camera)?;
        f.set("body", lua.to_value(&input.body)?)?;
        f.set("gravity", input.gravity)?;
        Ok(f)
    }
}

fn first_line(text: &str) -> String {
    text.lines().take_while(|l| !l.starts_with("stack traceback")).take(2).collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::logic::{Manifest, Permission};

    fn package(files: &[(&str, &str)]) -> Package {
        let manifest = Manifest { name: "game".into(), version: 1, title: "Game".into(), permissions: vec![Permission::Spawn], depends: Vec::new(), renamed: Default::default() };
        Package { manifest, defs: BTreeMap::new(), scripts: files.iter().map(|(n, s)| (n.to_string(), s.to_string())).collect(), models: BTreeMap::new() }
    }

    fn input<'a>(keys: &'a [&'a str], pressed: &'a [&'a str]) -> FrameIn<'a> {
        FrameIn {
            dt: 0.016,
            now: 100.0,
            me: "ada",
            hero: Hero { pos: [0.0, 9.0, 0.0], grounded: false, ..Default::default() },
            keys,
            pressed,
            walk: [4.0, 0.0],
            camera: [0.0, 0.3],
            body: Body::default(),
            gravity: 24.0,
        }
    }

    #[test]
    fn player_scripts_steer_the_hero_and_keep_their_state() {
        // A jetpack: hold space to rise; Q dashes, then waits a second.
        let jetpack = r#"
            local ready = 0
            function on_frame(f)
              local vx = f.walk.x
              if f.pressed.q and f.now >= ready then vx = 30; ready = f.now + 1 end
              if f.keys.space then hero_velocity({x = vx, y = 5, z = f.walk.z}) end
              camera({distance = 0, shake = 0.1})
            end
        "#;
        let world_file = "function on_join(ev) end";
        let p = package(&[("jetpack.player.luau", jetpack), ("main.luau", world_file)]);
        let mut vm = PlayerVm::new(&p).unwrap().unwrap();
        let state = GameState::default();
        let out = vm.frame(&input(&["space"], &["q"]), &state).unwrap();
        assert_eq!(out.velocity, Some([30.0, 5.0, 0.0]));
        assert_eq!(out.camera.unwrap().distance, Some(0.0));
        // The cooldown kept from the frame before: no dash yet.
        let out = vm.frame(&input(&["space"], &["q"]), &state).unwrap();
        assert_eq!(out.velocity, Some([4.0, 5.0, 0.0]));
        // Not held: the usual controls.
        assert_eq!(vm.frame(&input(&[], &[]), &state).unwrap().velocity, None);
        // A package with no player scripts has no VM; a runaway frame is stopped.
        assert!(PlayerVm::new(&package(&[("main.luau", world_file)])).unwrap().is_none());
        let mut slow = PlayerVm::new(&package(&[("slow.player.luau", "function on_frame(f) while true do end end")])).unwrap().unwrap();
        assert!(slow.frame(&input(&[], &[]), &state).unwrap_err().contains("too long"));
    }
}
