//! Trying a package out before it's installed: it plays in a world of its own (flat,
//! empty, with one admin) through the same rules a real world runs, and the report says
//! what each step did and what the world looks like after, as a map and a list. Agents
//! use it (`test_game`) to play what they wrote and fix it before anyone else sees it.

use std::{collections::BTreeMap, fmt::Write};

use super::{
    EntityId, GameState, Intent, KEYS, Logic, LogicRules, Package, Scope,
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

/// Plays `package` through `steps` and says what happened. Steps are `place <item>`
/// (each 32 m from the last), a key (`enter`, `north`, ..., sent to the zone the
/// tester's pawn or last placement is in), or `interact` (the labeled entity nearest
/// them).
pub fn play(package: &Package, steps: &[String]) -> String {
    let mut out = String::new();
    if let Err(errors) = package.check() {
        let _ = write!(out, "The package doesn't pass the checks, so it wouldn't install:\n- {}", errors.join("\n- "));
        return out;
    }
    let rules = LogicRules::default();
    let mut logic = Logic::default();
    let reference = package.reference();
    logic.packages.insert(reference.hash.clone(), std::sync::Arc::new(package.clone()));
    if let Err(why) = logic.install(TESTER, true, &rules, &reference, &Scope::World) {
        let _ = write!(out, "It wouldn't install: {why}");
        return out;
    }
    let ground = (FLAT_GROUND + 1) as f32 * VOXEL_SIZE;
    let mut spot = None::<[f32; 3]>;
    let mut placed = 0;
    out.push_str("Installed in an empty flat world (ground at y = 0 below; positions are meters from the last place).\n");
    for (i, step) in steps.iter().enumerate() {
        let step = step.trim();
        let change = i as u64 + 2;
        let here = pawn_of(&logic.state, TESTER).or(spot);
        let intent = if let Some(item) = step.strip_prefix("place ") {
            let item = item.trim();
            let item = if split_name(item).is_some() { item.to_string() } else { format!("{}:{item}", package.manifest.name) };
            let at = [placed as f32 * SPACING, ground, 0.0];
            placed += 1;
            spot = Some(at);
            Ok(Intent::Place { item, position: at })
        } else if KEYS.contains(&step) {
            match here.and_then(|at| zone_at(&logic.state, at)) {
                Some(entity) => Ok(Intent::Key { entity, key: step.into() }),
                None => Err("no zone there to send keys to".to_string()),
            }
        } else if step == "interact" {
            let near = here.and_then(|at| {
                logic
                    .state
                    .entities()
                    .filter(|(_, e)| e.components.contains_key("label"))
                    .min_by(|a, b| distance(a.1.position, at).total_cmp(&distance(b.1.position, at)))
                    .map(|(id, _)| id)
            });
            near.map(|entity| Intent::Interact { entity }).ok_or("nothing labeled to interact with".into())
        } else {
            Err(format!("unknown step \"{step}\" (use \"place <item>\", a key: {}, or \"interact\")", KEYS.join(", ")))
        };
        let result = intent.and_then(|intent| logic.act(change, TESTER, 1_000 + change, 7, &rules, &intent));
        let _ = match result {
            Ok(o) if o.effects == 0 => writeln!(out, "{}. {step}: nothing changed", i + 1),
            Ok(o) => {
                let said: Vec<_> = o.notes.iter().map(|(_, text)| format!("\"{text}\"")).collect();
                let said = if said.is_empty() { String::new() } else { format!("; said {}", said.join(", ")) };
                writeln!(out, "{}. {step}: ok, {} change(s){said}", i + 1, o.effects)
            }
            Err(why) => writeln!(out, "{}. {step}: REFUSED: {why}", i + 1),
        };
    }
    let origin = pawn_of(&logic.state, TESTER).and(spot).or(spot).unwrap_or([0.0, ground, 0.0]);
    describe(&mut out, &logic.state, origin, ground);
    out
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
        let others: BTreeMap<_, _> = e.components.iter().filter(|(k, _)| *k != "look").collect();
        let text = serde_json::to_string(&others).unwrap_or_default();
        let text: String = text.chars().take(160).collect();
        let _ = writeln!(out, "  {}.{} {} at ({:.2}, {:.2}, {:.2}) {text}", id.change, id.n, model(e), rel[0], rel[1], rel[2]);
    }
    if near.len() > 60 {
        let _ = writeln!(out, "  … and {} more", near.len() - 60);
    }
}
