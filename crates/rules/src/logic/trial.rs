//! Trying a package out before it's installed: it plays in a world of its own (flat,
//! empty, with one admin) through the same rules a real world runs, and the report says
//! what each step did and what the world looks like after, as a map and a list. Agents
//! use it (`test_game`) to play what they wrote and fix it before anyone else sees it.

use std::{collections::BTreeMap, fmt::Write};

use super::{
    Clockwork, EntityId, GameState, Intent, KEYS, Logic, LogicRules, Package, Scope,
    package::split_name,
};
use crate::terrain::{FLAT_GROUND, VOXEL_SIZE};

const TESTER: &str = "tester";
/// Things put down are this far apart, so boards don't overlap.
const SPACING: f32 = 32.0;
/// How far around the last spot the map reaches, in meters.
const MAP_REACH: f32 = 14.0;

/// The zone (an entity's `zone` rectangle, centered on it) at `at`, if any.
pub fn zone_at(state: &GameState, at: [f32; 3]) -> Option<EntityId> {
    state.entities().find_map(|(id, e)| {
        let size = e.components.get("zone")?["size"].as_array()?;
        let (w, d) = (size.first()?.as_f64()? as f32, size.get(1)?.as_f64()? as f32);
        let inside = (at[0] - e.position[0]).abs() <= w / 2.0 && (at[2] - e.position[2]).abs() <= d / 2.0;
        inside.then_some(id)
    })
}

/// Where `player`'s pawn stands, if a game has given them one.
pub fn pawn_of(state: &GameState, player: &str) -> Option<[f32; 3]> {
    state.entities().find(|(_, e)| e.components.get("pawn").and_then(|p| p.as_str()) == Some(player)).map(|(_, e)| e.position)
}

/// The tester, as the world sees them: where they stand, and the last teleport they
/// followed.
struct Tester {
    at: [f32; 3],
    moved: u64,
}

impl Tester {
    /// Where they are: on their pawn if a game gave them one.
    fn here(&self, state: &GameState) -> [f32; 3] {
        pawn_of(state, TESTER).unwrap_or(self.at)
    }

    /// Follows a game's teleport (or, `arriving`, the world's spawn point).
    fn follow(&mut self, state: &GameState, arriving: bool) {
        let to = state.teleport(TESTER).filter(|t| t.change > self.moved);
        let to = to.or_else(|| state.teleport("*").filter(|_| arriving));
        if let Some(t) = to {
            self.at = t.position;
            self.moved = self.moved.max(t.change);
        }
    }
}

/// The labeled entity nearest `at` (with `label` as its label or model, if given).
fn nearest(state: &GameState, at: [f32; 3], label: Option<&str>) -> Option<EntityId> {
    state
        .entities()
        .filter(|(_, e)| match label {
            None => e.components.contains_key("label"),
            Some(label) => {
                e.components.get("label").and_then(|l| l.as_str()).is_some_and(|l| l.eq_ignore_ascii_case(label))
                    || e.components.get("look").and_then(|l| l["model"].as_str()).is_some_and(|m| m.eq_ignore_ascii_case(label))
            }
        })
        .min_by(|a, b| distance(a.1.position, at).total_cmp(&distance(b.1.position, at)))
        .map(|(id, _)| id)
}

/// Plays `package` through `steps` and says what happened. The game is installed
/// (`on_start` runs) and the tester comes into the world at the spawn point (or 0, 0).
/// Steps are `place <item>` (each 32 m from the last; the tester stands there after), a
/// key (`enter`, `north`, ..., sent to the zone the tester's pawn or last placement is
/// in), `interact` (the labeled entity nearest them; `interact <label or model>` for a
/// particular one), `hit` / `hit <label>` (attacks it), `go <x> <z>` (walks there, meters),
/// `choose <id>`, `close`, or `wait <seconds>` (timers go off as they would, and pieces
/// see where the tester is). Each step takes a second.
pub fn play(package: &Package, steps: &[String]) -> String {
    play_in(package, steps, None)
}

/// A real world to try a package in, copied: its games as they are now (pieces, timers,
/// players' things), its rules, its ground, and where the player stands.
pub struct Copy<'a> {
    pub logic: Logic,
    pub rules: LogicRules,
    /// Where the player stands: the tester starts there.
    pub at: [f32; 3],
    /// Unix seconds now, and the id the next change would get.
    pub now: u64,
    pub next_change: u64,
    pub ground: &'a dyn super::Ground,
}

/// [`play`], in a copy of a real world (`world`) instead of an empty one: the package goes
/// in as it would there, a new version upgrading the one it runs (migrations and all).
pub fn play_in(package: &Package, steps: &[String], world: Option<Copy>) -> String {
    let mut out = String::new();
    if let Err(errors) = package.check() {
        let _ = write!(out, "The package doesn't pass the checks, so it wouldn't install:\n- {}", errors.join("\n- "));
        return out;
    }
    let empty = (FLAT_GROUND + 1) as f32 * VOXEL_SIZE;
    let copied = world.is_some();
    let (mut logic, rules, start, mut now, mut change, ground): (Logic, LogicRules, [f32; 3], u64, u64, &dyn super::Ground) = match world {
        Some(w) => (Logic { runtime: None, ..w.logic }, w.rules, w.at, w.now, w.next_change, w.ground),
        None => (Logic::default(), LogicRules::default(), [0.0, empty, 0.0], 1_000, 1, &super::NoGround),
    };
    let name = package.manifest.name.clone();
    let old = logic.installed.get(&name).cloned();
    let reference = package.reference();
    logic.packages.insert(reference.hash.clone(), std::sync::Arc::new(package.clone()));
    let scope = old.as_ref().map_or(Scope::World, |o| o.scope.clone());
    if let Err(why) = logic.install(TESTER, true, &rules, &reference, &scope) {
        let _ = write!(out, "It wouldn't install: {why}");
        return out;
    }
    let upgrade = old.as_ref().and_then(|o| logic.packages.get(&o.package.hash)).map(|o| super::Upgrade::between(o, package));
    let ground_y = start[1];
    let mut spot = None::<[f32; 3]>;
    let mut placed = 0;
    let mut clock = Clockwork::new(now);
    let mut tester = Tester { at: start, moved: 0 };
    if copied {
        let _ = writeln!(
            out,
            "Tried on a copy of this world as it is now (its pieces, timers, and players' stats and items); the tester starts where the player stands, at ({:.1}, {:.1}), and walks on its ground.",
            start[0], start[2]
        );
    } else {
        out.push_str("Installed in an empty flat world, ground at y = 0 below (pieces stand at y = 8.75 m in game positions).\n");
    }
    if let Some(u) = &upgrade {
        let list = |l: &[String]| if l.is_empty() { "none".to_string() } else { l.join(", ") };
        let renamed: Vec<String> = u.renamed.iter().map(|(a, b)| format!("{a} → {b}")).collect();
        let _ = writeln!(
            out,
            "An upgrade from v{} to v{}: on_start runs for {}; taken out (pieces and timers go): {}{}.",
            u.from,
            package.manifest.version,
            list(&u.changed),
            list(&u.removed),
            if renamed.is_empty() { String::new() } else { format!("; renamed: {}", renamed.join(", ")) }
        );
    }
    // The game sets the world up, then the tester comes in.
    change += 1;
    let before = logic.state.len();
    match logic.prepare_start(change, TESTER, now, 7, &rules, &name, upgrade.as_ref()) {
        Ok(prepared) => {
            let o = logic.commit(prepared);
            if o.effects > 0 {
                let said: Vec<_> = o.notes.iter().map(|(_, t)| format!("\"{t}\"")).collect();
                let said = if said.is_empty() { String::new() } else { format!("; said {}", said.join(", ")) };
                let pieces = logic.state.len() as i64 - before as i64;
                let _ = writeln!(out, "on_start: {} change(s), {pieces:+} piece(s){said}{}", o.effects, world_line(&o.world));
            }
        }
        Err(why) => {
            let _ = writeln!(out, "on_start: REFUSED (it wouldn't install{}): {why}", if upgrade.is_some() { "; the world keeps the old version" } else { "" });
            return out;
        }
    }
    tester.follow(&logic.state, true);
    let target = now;
    let came = wait(&mut logic, &mut clock, &mut change, &mut now, target, &rules, &mut tester, ground);
    let _ = writeln!(out, "The tester came in at ({:.1}, {:.1}): {came}", tester.at[0], tester.at[2]);
    for (i, step) in steps.iter().enumerate() {
        let step = step.trim();
        if let Some(seconds) = step.strip_prefix("wait ") {
            let Ok(seconds) = seconds.trim().parse::<u64>() else {
                let _ = writeln!(out, "{}. {step}: REFUSED: wait takes whole seconds", i + 1);
                continue;
            };
            let target = now + seconds.min(30 * 86_400);
            let report = wait(&mut logic, &mut clock, &mut change, &mut now, target, &rules, &mut tester, ground);
            let _ = writeln!(out, "{}. {step}: {report}", i + 1);
            continue;
        }
        now += 1;
        change += 1;
        let target = now;
        let tick = wait(&mut logic, &mut clock, &mut change, &mut now, target, &rules, &mut tester, ground);
        if tick != "nothing happened" {
            let _ = writeln!(out, "   (before step {}: {tick})", i + 1);
        }
        if let Some(to) = step.strip_prefix("go ") {
            let xz: Vec<f32> = to.split_whitespace().filter_map(|v| v.parse().ok()).collect();
            let [x, z] = xz[..] else {
                let _ = writeln!(out, "{}. {step}: REFUSED: go takes x and z in meters", i + 1);
                continue;
            };
            tester.at = [x, ground_y, z];
            let report = wait(&mut logic, &mut clock, &mut change, &mut now, target, &rules, &mut tester, ground);
            let _ = writeln!(out, "{}. {step}: {report}", i + 1);
            continue;
        }
        let here = Some(tester.here(&logic.state));
        let intent = if let Some(item) = step.strip_prefix("place ") {
            let item = item.trim();
            let item = if split_name(item).is_some() { item.to_string() } else { format!("{}:{item}", package.manifest.name) };
            let at = [start[0] + placed as f32 * SPACING, ground_y, start[2]];
            placed += 1;
            spot = Some(at);
            tester.at = at;
            Ok(Intent::Place { item, position: at })
        } else if KEYS.contains(&step) {
            match pawn_of(&logic.state, TESTER).or(spot).and_then(|at| zone_at(&logic.state, at)) {
                Some(entity) => Ok(Intent::Key { entity, key: step.into() }),
                None => Err("no zone there to send keys to".to_string()),
            }
        } else if let Some(choice) = step.strip_prefix("choose ") {
            Ok(Intent::Choose { choice: Some(choice.trim().to_string()) })
        } else if step == "close" {
            Ok(Intent::Choose { choice: None })
        } else if step == "interact" || step.starts_with("interact ") || step == "hit" || step.starts_with("hit ") {
            // The entity with that label (or that model) nearest the tester.
            let (verb, label) = step.split_once(' ').map_or((step, None), |(v, l)| (v, Some(l.trim())));
            let near = here.and_then(|at| nearest(&logic.state, at, label));
            match (near, verb) {
                (Some(entity), "hit") => Ok(Intent::Hit { entity }),
                (Some(entity), _) => Ok(Intent::Interact { entity }),
                (None, _) => Err(label.map_or("nothing labeled near the tester".into(), |l| format!("nothing labeled or modeled \"{l}\""))),
            }
        } else {
            Err(format!(
                "unknown step \"{step}\" (use \"place <item>\", a key: {}, \"interact\", \"interact <label>\", \"hit <label>\", \"go <x> <z>\", \"choose <choice id>\", \"close\" or \"wait <seconds>\")",
                KEYS.join(", ")
            ))
        };
        // Which entity an interaction reached, so a report says what was pressed.
        let target = match &intent {
            Ok(Intent::Interact { entity } | Intent::Hit { entity }) => logic.state.get(*entity).map(|e| {
                let label = e.components.get("label").and_then(|l| l.as_str()).unwrap_or("unlabeled");
                format!(" ({label}, {}.{})", entity.change, entity.n)
            }),
            _ => None,
        }
        .unwrap_or_default();
        let result = intent.and_then(|intent| logic.act(change, TESTER, now, 7, &rules, &intent));
        tester.follow(&logic.state, false);
        let step = format!("{step}{target}");
        let step = step.as_str();
        let _ = match result {
            Ok(o) if o.effects == 0 => writeln!(out, "{}. {step}: nothing changed", i + 1),
            Ok(o) => {
                let said: Vec<_> = o.notes.iter().map(|(_, text)| format!("\"{text}\"")).collect();
                let said = if said.is_empty() { String::new() } else { format!("; said {}", said.join(", ")) };
                writeln!(out, "{}. {step}: ok, {} change(s){said}{}", i + 1, o.effects, world_line(&o.world))
            }
            Err(why) => writeln!(out, "{}. {step}: REFUSED: {why}", i + 1),
        };
    }
    // What the tester has: stats, items, what's on their screen, and an open panel.
    let stats: Vec<String> = logic
        .state
        .stats(TESTER)
        .map(|(n, s)| format!("{n} = {}{}", s.value, s.max.map_or(String::new(), |m| format!("/{m}"))))
        .collect();
    let items: Vec<String> = logic.state.inventory(TESTER).map(|(n, c)| format!("{n} ×{c}")).collect();
    let at = tester.here(&logic.state);
    let _ = writeln!(out, "\nThe tester, at ({:.1}, {:.1}): stats [{}], items [{}].", at[0], at[2], stats.join(", "), items.join(", "));
    // In a copy of a world, the players who were there before: what a migration did to them.
    let others: Vec<String> = logic.state.players().into_iter().filter(|p| p != TESTER).collect();
    for player in others.iter().take(5) {
        let stats: Vec<String> = logic.state.stats(player).map(|(n, s)| format!("{n} = {}", s.value)).collect();
        let items: Vec<String> = logic.state.inventory(player).map(|(n, c)| format!("{n} ×{c}")).collect();
        let short: String = player.chars().take(10).collect();
        let _ = writeln!(out, "  Player {short}…: stats [{}], items [{}]", stats.join(", "), items.join(", "));
    }
    if others.len() > 5 {
        let _ = writeln!(out, "  … and {} more players", others.len() - 5);
    }
    for (id, widget) in logic.state.hud(TESTER) {
        let _ = writeln!(out, "  HUD {id}: {}", serde_json::to_string(widget).unwrap_or_default());
    }
    if let Some(panel) = logic.state.panel(TESTER) {
        let choices: Vec<String> = panel.spec.choices.iter().map(|c| format!("{} ({})", c.id, c.label)).collect();
        let _ = writeln!(out, "  Panel open: \"{}\" ({}): {} Choices: {}", panel.spec.title, panel.spec.id, panel.spec.text, choices.join(", "));
    }
    let origin = pawn_of(&logic.state, TESTER).and(spot).or(spot).unwrap_or(tester.here(&logic.state));
    describe(&mut out, &logic.state, origin, ground_y);
    out
}

/// The trial's world, for timers: what they do lands at once (builds and land changes
/// are only noted; a trial has no voxels).
struct Trial<'a> {
    logic: &'a mut Logic,
    change: u64,
    effects: usize,
    ground: &'a dyn super::Ground,
}

/// The world's ground, if it's a copy of one; an empty trial's is unknown (walks go
/// straight).
impl super::Ground for Trial<'_> {
    fn heights(&self, min: [i32; 2], step: i32, count: [i32; 2]) -> Option<Vec<Option<i32>>> {
        self.ground.heights(min, step, count)
    }
}

impl super::TimerHost for Trial<'_> {
    fn logic(&self) -> &Logic {
        self.logic
    }

    fn next_change(&self) -> u64 {
        self.change + 1
    }

    fn record(&mut self, package: &str, now: u64, effects: Vec<super::Effect>) -> Result<(), String> {
        self.change += 1;
        self.effects += effects.len();
        let rules = LogicRules::default();
        self.logic.apply_timers(self.change, package, now, &effects, &rules).map(|_| ())
    }
}

/// A piece as reports name it: "the Wolf (2.1)".
fn name_of(state: &GameState, id: EntityId) -> String {
    let label = state.get(id).and_then(|e| e.components.get("label")?.as_str().map(str::to_string));
    match label {
        Some(label) => format!("the {label} ({}.{})", id.change, id.n),
        None => format!("{}.{}", id.change, id.n),
    }
}

/// What a game did to the world's voxels, for a report line.
fn world_line(world: &[super::WorldChange]) -> String {
    let world: Vec<String> = world
        .iter()
        .map(|w| match w {
            super::WorldChange::Build { model, .. } => format!("built {model}"),
            super::WorldChange::Unbuild { id } => format!("removed build {id}"),
            super::WorldChange::Terrain { area, .. } => format!("reshaped {}x{} columns", area.size[0], area.size[1]),
        })
        .collect();
    if world.is_empty() { String::new() } else { format!("; {}", world.join(", ")) }
}

/// Lets time run up to `target` with the tester where they stand: what they walked
/// into, and the timers that go off (pieces more than 64 m from the tester sleep), and
/// says what happened.
#[allow(clippy::too_many_arguments)]
fn wait(
    logic: &mut Logic,
    clock: &mut Clockwork,
    change: &mut u64,
    now: &mut u64,
    target: u64,
    rules: &LogicRules,
    tester: &mut Tester,
    ground: &dyn super::Ground,
) -> String {
    let (mut fired, mut effects, mut said, mut errors, mut rounds) = (0, 0, Vec::new(), Vec::new(), 0);
    let mut heard: Vec<String> = Vec::new();
    let mut at = *now;
    loop {
        rounds += 1;
        if rounds > 2000 {
            errors.push("stopped after 2000 rounds of timers".to_string());
            break;
        }
        let players = vec![(TESTER.to_string(), tester.here(&logic.state))];
        for package in logic.installed.keys() {
            for seen in clock.watch(logic, package, &players, at) {
                heard.push(match seen {
                    super::Seen::Join(_) => "joined".into(),
                    super::Seen::Leave(_) => "left".into(),
                    super::Seen::Enter(id, _) => format!("entered {}'s region", name_of(&logic.state, id)),
                    super::Seen::Exit(id, _) => format!("left {}'s region", name_of(&logic.state, id)),
                });
            }
        }
        let mut host = Trial { logic, change: *change, effects: 0, ground };
        let run = clock.run(&mut host, at, 7, rules, &players);
        *change = host.change;
        effects += host.effects;
        fired += run.changes;
        said.extend(run.notes.iter().map(|(_, t)| format!("\"{t}\"")));
        errors.extend(run.errors);
        tester.follow(&logic.state, false);
        let positions = [tester.here(&logic.state)];
        match clock.next_due(logic, &positions).filter(|next| *next <= target) {
            Some(next) => at = next.max(at),
            None => break,
        }
    }
    *now = target;
    if fired == 0 && errors.is_empty() {
        return if heard.is_empty() { "nothing happened".into() } else { format!("the tester {}; nothing changed", heard.join(", ")) };
    }
    said.truncate(8);
    heard.dedup();
    let heard = if heard.is_empty() { String::new() } else { format!("the tester {}; ", heard.join(", ")) };
    let mut text = format!("{heard}games acted {fired} time(s), {effects} change(s)");
    if !said.is_empty() {
        text += &format!("; said {}", said.join(", "));
    }
    if !errors.is_empty() {
        text += &format!("; ERRORS: {}", errors.join("; "));
    }
    text
}

fn distance(a: [f32; 3], b: [f32; 3]) -> f32 {
    ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)).sqrt()
}

/// A map (1 m cells, north up) and a list of the entities near `origin`.
fn describe(out: &mut String, state: &GameState, origin: [f32; 3], ground: f32) {
    let near: Vec<_> = state
        .entities()
        .filter(|(_, e)| (e.position[0] - origin[0]).abs() <= MAP_REACH && (e.position[2] - origin[2]).abs() <= MAP_REACH)
        .collect();
    let _ = writeln!(out, "\nAfter the steps: {} entities in the world, {} near the last place.", state.len(), near.len());
    if near.is_empty() {
        return;
    }
    let model = |e: &super::Entity| e.components.get("look").and_then(|l| l["model"].as_str()).unwrap_or("-").to_string();
    let cell = |p: [f32; 3]| (((p[0] - origin[0]).floor()) as i32, ((p[2] - origin[2]).floor()) as i32);
    // What shows in each cell: a pawn, else a piece. Zones (a board under everything)
    // are in the list, not on the map.
    let mut cells: BTreeMap<(i32, i32), (u8, char)> = BTreeMap::new();
    let mut legend: BTreeMap<char, String> = BTreeMap::new();
    for (_, e) in near.iter().filter(|(_, e)| !e.components.contains_key("zone")) {
        let (rank, ch) = if e.components.contains_key("pawn") {
            (2, '@')
        } else {
            let name = model(e);
            let ch = name.chars().next().unwrap_or('?');
            legend.entry(ch).or_insert(name);
            (1, ch)
        };
        let slot = cells.entry(cell(e.position)).or_insert((0, ' '));
        if rank > slot.0 {
            *slot = (rank, ch);
        }
    }
    if cells.is_empty() {
        return;
    }
    let xs = cells.keys().map(|(x, _)| *x);
    let zs = cells.keys().map(|(_, z)| *z);
    let (x0, x1, z0, z1) = (xs.clone().min().unwrap(), xs.max().unwrap(), zs.clone().min().unwrap(), zs.max().unwrap());
    let _ = writeln!(out, "Map, 1 m cells, north (-z) up, x {x0}..{x1}, z {z0}..{z1}. @ = the tester's pawn; letters are models:");
    for z in z0..=z1 {
        let row: String = (x0..=x1).map(|x| cells.get(&(x, z)).map_or('.', |c| c.1)).collect();
        let _ = writeln!(out, "  {row}");
    }
    let names: Vec<_> = legend.iter().map(|(c, n)| format!("{c} = {n}")).collect();
    let _ = writeln!(out, "  ({})", names.join(", "));
    let _ = writeln!(out, "Entities near the last place (position relative to it, components without look):");
    for (id, e) in near.iter().take(60) {
        let rel = [e.position[0] - origin[0], e.position[1] - ground, e.position[2] - origin[2]];
        // A walk shows where it's going, not every turn of the way.
        let walk = e.components.get("walk").map(|w| serde_json::json!({ "to": w["to"], "speed": w["speed"] }));
        let others: BTreeMap<_, _> =
            e.components.iter().filter(|(k, _)| *k != "look").map(|(k, v)| (k, if k == "walk" { walk.as_ref().unwrap_or(v) } else { v })).collect();
        let text = serde_json::to_string(&others).unwrap_or_default();
        let text: String = text.chars().take(160).collect();
        let _ = writeln!(out, "  {}.{} {} at ({:.2}, {:.2}, {:.2}) {text}", id.change, id.n, model(e), rel[0], rel[1], rel[2]);
    }
    if near.len() > 60 {
        let _ = writeln!(out, "  … and {} more", near.len() - 60);
    }
}
