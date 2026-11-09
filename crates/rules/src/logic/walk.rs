//! Pieces that move on their own: walking somewhere, and chasing a player. They're the
//! engine's because a script can't do them: a path needs the ground (which scripts
//! don't see), a chase needs where players are now, and players' games have to move the
//! piece smoothly between changes. Routines, wandering and spawners are scripts' to
//! build out of these and timers. The engine runs them with timers of its own (named
//! `~walk:<id>` and so on, on the piece's package), so they cost nothing while nobody
//! is near, and what they do is recorded like any timer's: replay never looks for a path
//! again.
//!
//! A walk is one change, not one per step: the `walk` component holds the path, when
//! it starts and how fast, and players' games move the piece along it as time passes
//! ([`position_at`]). The piece's position stays where it set off until it arrives;
//! then it moves there, and its package's `on_arrive` runs.
//!
//! Engine components (only the piece's own package sets them):
//! - `walk`: `{"to": [x, y, z], "speed": m/s}`, and once on its way `"path"` and `"start"`.
//! - `chase`: `{"player", "speed", "reach"}`: every couple of seconds it heads for where
//!   that player is now, and its package's `on_catch` runs when it's within reach.
//!
//! Paths follow the ground the host knows ([`Ground`]): around water and up steps no
//! taller than [`CLIMB`], and around builds and solid pieces ([`super::solid`]): onto
//! low ones, under roofs, never through walls.

use std::{
    cmp::Ordering,
    collections::{BinaryHeap, HashMap},
};

use serde_json::{Value, json};

use super::{
    Batch, Effect, EntityId, Grants,
    entity::{Entity, snap},
    solid::{Solid, stand},
};
use crate::terrain::VOXEL_SIZE;

/// The furthest one walk goes, in meters.
pub const MAX_WALK: f32 = 64.0;
/// How fast pieces may walk, in meters a second.
pub const SPEEDS: std::ops::RangeInclusive<f32> = 0.2..=12.0;
pub const DEFAULT_SPEED: f32 = 1.5;
/// The tallest step a walking piece takes between two path cells, in meters.
pub const CLIMB: f32 = 0.6;
/// The most points a path keeps.
pub const MAX_POINTS: usize = 48;
/// How often a chaser looks for its player again, in seconds.
pub const CHASE_EVERY: u64 = 2;
/// Terrain columns per path cell (half a meter).
const CELL: i32 = 2;
/// How far around the walk the path may stray, in meters.
const MARGIN: f32 = 8.0;

/// The ground pieces walk on, as the host of the world knows it.
pub trait Ground {
    /// Where pieces stand in the terrain columns `min + (i, j) * step`, for `i <
    /// count[0]` and `j < count[1]`, `i` fastest: the top ground voxel's y, or `None`
    /// under water. `None` if that ground isn't known (walks then go straight).
    fn heights(&self, min: [i32; 2], step: i32, count: [i32; 2]) -> Option<Vec<Option<i32>>>;

    /// What stands on the ground from `lo` to `hi` (x, z): builds and solid pieces (and
    /// maybe some just beyond it).
    fn solids(&self, _lo: [f32; 2], _hi: [f32; 2]) -> Vec<Solid<'_>> {
        Vec::new()
    }
}

/// No ground known: walks go in a straight line.
pub struct NoGround;

impl Ground for NoGround {
    fn heights(&self, _min: [i32; 2], _step: i32, _count: [i32; 2]) -> Option<Vec<Option<i32>>> {
        None
    }
}

/// Where a piece stands on a terrain column (`y = 0..`): its top ground voxel, unless
/// water covers it.
pub fn stand_in(column: &[crate::voxel::VoxelId]) -> Option<i32> {
    let (ground, water, top) = crate::sculpt::surface(column);
    (top != crate::voxel::AIR && water <= ground).then_some(ground)
}

/// The terrain column `m` meters falls in.
pub fn column(m: f32) -> i32 {
    (m / VOXEL_SIZE).floor() as i32
}

fn flat_distance(a: [f32; 3], b: [f32; 3]) -> f32 {
    ((a[0] - b[0]).powi(2) + (a[2] - b[2]).powi(2)).sqrt()
}

#[derive(PartialEq)]
struct Node {
    cost: f32,
    cell: (i32, i32),
}

impl Eq for Node {}

impl Ord for Node {
    fn cmp(&self, other: &Self) -> Ordering {
        other.cost.total_cmp(&self.cost).then_with(|| self.cell.cmp(&other.cell))
    }
}

impl PartialOrd for Node {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// A way from `from` to `to` over `ground`: points in meters, `from` first. Straight
/// when the ground isn't known.
pub fn path(ground: &dyn Ground, from: [f32; 3], to: [f32; 3]) -> Result<Vec<[f32; 3]>, String> {
    if flat_distance(from, to) > MAX_WALK {
        return Err(format!("a walk is at most {MAX_WALK} m; walk in steps"));
    }
    let lo = [from[0].min(to[0]) - MARGIN, from[2].min(to[2]) - MARGIN];
    let hi = [from[0].max(to[0]) + MARGIN, from[2].max(to[2]) + MARGIN];
    let min = [column(lo[0]), column(lo[1])];
    // Cells of CELL × CELL columns, read at their corner column.
    let cells = [(column(hi[0]) - min[0]) / CELL + 1, (column(hi[1]) - min[1]) / CELL + 1];
    let Some(heights) = ground.heights(min, CELL, cells) else {
        return Ok(vec![from, to]);
    };
    // Where a piece stands in each cell: on the ground, or on top of what's built there
    // (in any of the cell's columns, so a thin wall can't be slipped through).
    let mut stands: Vec<Option<f32>> = heights.iter().map(|y| y.map(|y| (y + 1) as f32 * VOXEL_SIZE)).collect();
    let near = |s: &Solid| {
        let r = s.reach();
        s.position[0] + r >= lo[0] && s.position[0] - r <= hi[0] && s.position[2] + r >= lo[1] && s.position[2] - r <= hi[1]
    };
    for solid in ground.solids(lo, hi).iter().filter(|s| near(s)) {
        for (&(x, z), runs) in solid.spans().iter() {
            let (i, j) = ((x - min[0]).div_euclid(CELL), (z - min[1]).div_euclid(CELL));
            if i < 0 || j < 0 || i >= cells[0] || j >= cells[1] {
                continue;
            }
            if let Some(Some(at)) = stands.get_mut((j * cells[0] + i) as usize) {
                let base = (heights[(j * cells[0] + i) as usize].unwrap_or(0) + 1) as f32 * VOXEL_SIZE;
                *at = at.max(stand(base, runs));
            }
        }
    }
    let cell_of = |p: [f32; 3]| ((column(p[0]) - min[0]) / CELL, (column(p[2]) - min[1]) / CELL);
    let height = |c: (i32, i32)| -> Option<f32> {
        (c.0 >= 0 && c.1 >= 0 && c.0 < cells[0] && c.1 < cells[1]).then_some(())?;
        stands.get((c.1 * cells[0] + c.0) as usize).copied().flatten()
    };
    let center = |c: (i32, i32), y: f32| {
        let m = |i: i32, at: i32| (at + i * CELL) as f32 * VOXEL_SIZE + VOXEL_SIZE / 2.0;
        [m(c.0, min[0]), y, m(c.1, min[1])]
    };
    let (start, goal) = (cell_of(from), cell_of(to));
    let goal_y = height(goal).ok_or("there's nowhere to stand there")?;
    if height(start).is_none() {
        // Standing in water: wade out the way it came.
        return Ok(vec![from, [to[0], goal_y, to[2]]]);
    }
    // A* over the cells, 8 ways.
    let guess = |c: (i32, i32)| (((c.0 - goal.0).pow(2) + (c.1 - goal.1).pow(2)) as f32).sqrt();
    let mut open = BinaryHeap::from([Node { cost: guess(start), cell: start }]);
    let mut best: HashMap<(i32, i32), (f32, (i32, i32))> = HashMap::from([(start, (0.0, start))]);
    let mut reached = false;
    while let Some(Node { cell, .. }) = open.pop() {
        if cell == goal {
            reached = true;
            break;
        }
        let (so_far, _) = best[&cell];
        // Setting off, it's where it stands (even right by a wall in its cell).
        let here = if cell == start { from[1] } else { height(cell).unwrap_or(0.0) };
        for (dx, dz) in [(1, 0), (-1, 0), (0, 1), (0, -1), (1, 1), (1, -1), (-1, 1), (-1, -1)] {
            let next = (cell.0 + dx, cell.1 + dz);
            let Some(y) = height(next) else { continue };
            if (y - here).abs() > CLIMB {
                continue;
            }
            // Diagonals don't cut corners.
            if dx != 0 && dz != 0 && (height((cell.0 + dx, cell.1)).is_none() || height((cell.0, cell.1 + dz)).is_none()) {
                continue;
            }
            let cost = so_far + if dx != 0 && dz != 0 { std::f32::consts::SQRT_2 } else { 1.0 };
            if best.get(&next).is_none_or(|(c, _)| cost < *c) {
                best.insert(next, (cost, cell));
                open.push(Node { cost: cost + guess(next), cell: next });
            }
        }
    }
    if !reached {
        return Err("there's no way there on foot".into());
    }
    let mut cells_back = vec![goal];
    while let Some(&last) = cells_back.last() {
        let (_, prev) = best[&last];
        if prev == last {
            break;
        }
        cells_back.push(prev);
    }
    cells_back.reverse();
    // Only the turns, the climbs and drops.
    let mut points = vec![from];
    for w in cells_back.windows(3) {
        let (a, b, c) = (w[0], w[1], w[2]);
        let turn = (b.0 - a.0, b.1 - a.1) != (c.0 - b.0, c.1 - b.1);
        let (ya, yb) = (height(a).unwrap_or(0.0), height(b).unwrap_or(0.0));
        if turn || (ya - yb).abs() > f32::EPSILON {
            points.push(center(b, yb));
        }
    }
    points.push([to[0], goal_y, to[2]]);
    while points.len() > MAX_POINTS {
        // Keep the ends; drop every other point between.
        let mut i = 0;
        points.retain(|_| {
            i += 1;
            i == 1 || i % 2 == 0
        });
        let last = [to[0], goal_y, to[2]];
        if points.last() != Some(&last) {
            points.push(last);
        }
    }
    Ok(points)
}

fn length(path: &[[f32; 3]]) -> f32 {
    path.windows(2).map(|w| ((w[0][0] - w[1][0]).powi(2) + (w[0][1] - w[1][1]).powi(2) + (w[0][2] - w[1][2]).powi(2)).sqrt()).sum()
}

fn point(v: &Value) -> Option<[f32; 3]> {
    let a = v.as_array().filter(|a| a.len() == 3)?;
    let p = [a[0].as_f64()? as f32, a[1].as_f64()? as f32, a[2].as_f64()? as f32];
    p.iter().all(|v| v.is_finite()).then_some(p)
}

fn points(v: &Value) -> Option<Vec<[f32; 3]>> {
    v.as_array()?.iter().map(point).collect()
}

/// Where a piece is at `now` (Unix seconds): along its walk if it's on one, else where
/// it stands.
pub fn position_at(e: &Entity, now: f64) -> [f32; 3] {
    walking_at(e, now).map_or(e.position, |(p, _)| p)
}

/// Where a walking piece is at `now`, and which way it faces (radians around Y, 0 is
/// north), if it's on its way.
pub fn walking_at(e: &Entity, now: f64) -> Option<([f32; 3], f32)> {
    let walk = e.components.get("walk")?;
    let path = points(&walk["path"])?;
    let (start, speed) = (walk["start"].as_f64()?, walk["speed"].as_f64()?.max(0.01));
    let mut left = ((now - start) * speed).max(0.0) as f32;
    let mut facing = 0.0;
    for w in path.windows(2) {
        let step = ((w[1][0] - w[0][0]).powi(2) + (w[1][1] - w[0][1]).powi(2) + (w[1][2] - w[0][2]).powi(2)).sqrt();
        facing = (-(w[1][0] - w[0][0])).atan2(-(w[1][2] - w[0][2]));
        if left <= step && step > 0.0 {
            let t = left / step;
            return Some((std::array::from_fn(|i| w[0][i] + (w[1][i] - w[0][i]) * t), facing));
        }
        left -= step;
    }
    path.last().map(|p| (*p, facing))
}

/// What a script or a package's data does to send `id` walking to `to` from `at`.
pub fn walk_effects(id: EntityId, to: [f32; 3], speed: Option<f32>, at: u64) -> Vec<Effect> {
    let speed = speed.unwrap_or(DEFAULT_SPEED);
    vec![
        Effect::Set { entity: id, component: "walk".into(), value: json!({ "to": to, "speed": speed }) },
        // A second on: the clock may have run up to `at` already.
        Effect::Schedule { name: timer("walk", id), due: at + 1, every: None, data: Value::Null, near: Some(id), feature: None },
    ]
}

/// What stops `id` where it is at `at`.
pub fn stop_effects(id: EntityId, e: &Entity, at: u64) -> Vec<Effect> {
    let mut out = Vec::new();
    if e.components.contains_key("walk") {
        let here = position_at(e, at as f64);
        if here != e.position {
            out.push(Effect::Move { entity: id, position: here });
        }
        out.push(Effect::Remove { entity: id, component: "walk".into() });
    }
    out.push(Effect::Cancel { name: timer("walk", id) });
    out.push(Effect::Cancel { name: timer("arrive", id) });
    out
}

/// What sets `id` chasing (`chase`: `{"player", "speed", "reach"}`) from `at`, or stops
/// it chasing (`None`).
pub fn chase_effects(id: EntityId, chase: Option<Value>, at: u64) -> Vec<Effect> {
    match chase {
        Some(value) => vec![
            Effect::Set { entity: id, component: "chase".into(), value },
            Effect::Schedule { name: timer("chase", id), due: at + 1, every: Some(CHASE_EVERY), data: Value::Null, near: Some(id), feature: None },
        ],
        None => vec![Effect::Remove { entity: id, component: "chase".into() }, Effect::Cancel { name: timer("chase", id) }],
    }
}

/// The engine's timer `kind` for entity `id`: `~walk:2.0`.
pub fn timer(kind: &str, id: EntityId) -> String {
    format!("~{kind}:{}.{}", id.change, id.n)
}

/// Checks an engine component this module owns, phrased for the agent that wrote it.
pub fn check(name: &str, value: &Value) -> Result<(), String> {
    let num = |key: &str, range: std::ops::RangeInclusive<f64>| value[key].as_f64().is_some_and(|v| range.contains(&v));
    let ok = match name {
        "walk" => {
            point(&value["to"]).is_some()
                && num("speed", *SPEEDS.start() as f64..=*SPEEDS.end() as f64)
                && (value["path"].is_null() || points(&value["path"]).is_some_and(|p| (2..=MAX_POINTS).contains(&p.len())))
        }
        "chase" => value["player"].as_str().is_some_and(|p| !p.is_empty()) && num("speed", 0.2..=12.0) && num("reach", 0.5..=16.0),
        _ => true,
    };
    if ok {
        return Ok(());
    }
    Err(match name {
        "walk" => format!("\"walk\" is {{\"to\": [x, y, z], \"speed\": {}-{} m/s}}; use walk(id, pos, speed)", SPEEDS.start(), SPEEDS.end()),
        _ => "chase(id, player, {speed = 0.2-12 m/s, reach = 0.5-16 m})".into(),
    })
}

/// What an engine timer needs.
pub(super) struct Engine<'a, 'b> {
    pub batch: &'a mut Batch<'b>,
    pub grants: &'a Grants,
    pub ground: &'a dyn Ground,
    pub now: u64,
    /// Who's in the world, and where.
    pub players: &'a [(String, [f32; 3])],
}

/// What an engine timer tells the piece's package.
pub(super) enum Heard {
    /// It reached the end of its walk (`stuck`: it couldn't find the way): `on_arrive`.
    Arrived { entity: EntityId, stuck: bool },
    /// It caught up with the player it chases: `on_catch`.
    Caught { entity: EntityId, player: String },
}

impl Engine<'_, '_> {
    fn apply(&mut self, effect: Effect) -> Result<Option<EntityId>, String> {
        self.batch.apply(self.grants, &effect)
    }

    /// Runs the engine's timer `name` (`~walk:2.0`, without the `~`).
    pub fn fire(&mut self, name: &str) -> Result<Option<Heard>, String> {
        let Some((kind, id)) = name.split_once(':') else { return Ok(None) };
        let id = id.split_once('.').and_then(|(c, n)| Some(EntityId { change: c.parse().ok()?, n: n.parse().ok()? }));
        let Some(id) = id else { return Ok(None) };
        let Some(e) = self.batch.get(id).cloned() else {
            // Its piece is gone: so is the timer.
            self.apply(Effect::Cancel { name: format!("~{name}") })?;
            return Ok(None);
        };
        match kind {
            "walk" => self.set_off(id, &e),
            "arrive" => self.arrive(id, &e),
            "chase" => self.chase(id, &e),
            _ => Ok(None),
        }
    }

    /// Finds the way for a walk asked for, and sets off.
    fn set_off(&mut self, id: EntityId, e: &Entity) -> Result<Option<Heard>, String> {
        let Some(walk) = e.components.get("walk") else { return Ok(None) };
        let (Some(to), speed) = (point(&walk["to"]), walk["speed"].as_f64().unwrap_or(DEFAULT_SPEED as f64) as f32) else {
            return Ok(None);
        };
        match self.go(id, e, to, speed, 0) {
            Ok(()) => Ok(None),
            Err(_) => {
                self.apply(Effect::Remove { entity: id, component: "walk".into() })?;
                Ok(Some(Heard::Arrived { entity: id, stuck: true }))
            }
        }
    }

    /// Sends `id` from where it is now to `to`, leaving after `pause` seconds.
    fn go(&mut self, id: EntityId, e: &Entity, to: [f32; 3], speed: f32, pause: u64) -> Result<(), String> {
        let here = position_at(e, self.now as f64);
        if here != e.position {
            self.apply(Effect::Move { entity: id, position: here })?;
        }
        let path = path(self.ground, here, to)?;
        let start = self.now + pause;
        let speed = speed.clamp(*SPEEDS.start(), *SPEEDS.end());
        let takes = (length(&path) / speed).ceil().max(1.0) as u64;
        let walk = json!({ "to": snap(to), "speed": speed, "path": path.iter().map(|p| snap(*p)).collect::<Vec<_>>(), "start": start });
        self.apply(Effect::Set { entity: id, component: "walk".into(), value: walk })?;
        self.apply(Effect::Schedule { name: timer("arrive", id), due: start + takes, every: None, data: Value::Null, near: Some(id), feature: None })?;
        Ok(())
    }

    /// A walk is over: the piece stands at its end, and its package hears about it.
    fn arrive(&mut self, id: EntityId, e: &Entity) -> Result<Option<Heard>, String> {
        let Some(walk) = e.components.get("walk") else { return Ok(None) };
        let end = points(&walk["path"]).and_then(|p| p.last().copied()).unwrap_or(e.position);
        self.apply(Effect::Move { entity: id, position: end })?;
        self.apply(Effect::Remove { entity: id, component: "walk".into() })?;
        // A chaser's walks are the engine's business, not its package's.
        if e.components.contains_key("chase") {
            return Ok(None);
        }
        Ok(Some(Heard::Arrived { entity: id, stuck: false }))
    }

    /// A chaser's turn: caught up, or off again toward where its player is now.
    fn chase(&mut self, id: EntityId, e: &Entity) -> Result<Option<Heard>, String> {
        let Some(chase) = e.components.get("chase") else { return Ok(None) };
        let Some(player) = chase["player"].as_str() else { return Ok(None) };
        // Gone from the world: it waits where it is.
        let Some((_, target)) = self.players.iter().find(|(p, _)| p == player) else { return Ok(None) };
        let here = position_at(e, self.now as f64);
        let reach = chase["reach"].as_f64().unwrap_or(1.5) as f32;
        let distance = ((here[0] - target[0]).powi(2) + (here[2] - target[2]).powi(2)).sqrt();
        if distance <= reach {
            for effect in stop_effects(id, e, self.now) {
                if !matches!(effect, Effect::Cancel { .. }) {
                    self.apply(effect)?;
                }
            }
            return Ok(Some(Heard::Caught { entity: id, player: player.to_string() }));
        }
        // Still heading close enough to where they are: keep going.
        let heading = e.components.get("walk").and_then(|w| point(&w["to"]));
        if heading.is_some_and(|to| flat_distance(to, *target) < reach) {
            return Ok(None);
        }
        let speed = chase["speed"].as_f64().unwrap_or(2.5) as f32;
        // Too far, or no way there: it gives up this time and looks again.
        let _ = self.go(id, e, *target, speed, 0);
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Flat ground, with a pond across x = 10..12 m from z = -5 to 5 m.
    struct Pond;

    impl Ground for Pond {
        fn heights(&self, min: [i32; 2], step: i32, count: [i32; 2]) -> Option<Vec<Option<i32>>> {
            let pond = |x: i32, z: i32| (column(10.0)..column(12.0)).contains(&x) && (column(-5.0)..column(5.0)).contains(&z);
            let mut out = Vec::new();
            for j in 0..count[1] {
                for i in 0..count[0] {
                    let (x, z) = (min[0] + i * step, min[1] + j * step);
                    out.push((!pond(x, z)).then_some(crate::terrain::FLAT_GROUND));
                }
            }
            Some(out)
        }
    }

    #[test]
    fn walks_go_around_water() {
        let y = (crate::terrain::FLAT_GROUND + 1) as f32 * VOXEL_SIZE;
        let (from, to) = ([5.0, y, 0.0], [17.0, y, 0.0]);
        // No ground known: straight there.
        assert_eq!(path(&NoGround, from, to).unwrap(), vec![from, to]);
        let way = path(&Pond, from, to).unwrap();
        assert!(way.len() > 2, "{way:?}");
        // It goes around the pond's end, never through the water.
        assert!(way.iter().any(|p| p[2].abs() >= 5.0), "{way:?}");
        assert!(way.iter().all(|p| p[1] == y));
        assert!(path(&Pond, from, [11.0, y, 0.0]).is_err(), "nowhere to stand in a pond");
        assert!(path(&NoGround, from, [200.0, y, 0.0]).is_err(), "too far");
    }

    /// Flat ground with things built on it.
    struct Built(Vec<(crate::build::BuildSpec, [f32; 3])>);

    impl Ground for Built {
        fn heights(&self, min: [i32; 2], step: i32, count: [i32; 2]) -> Option<Vec<Option<i32>>> {
            let _ = (min, step);
            Some(vec![Some(crate::terrain::FLAT_GROUND); (count[0] * count[1]) as usize])
        }

        fn solids(&self, _lo: [f32; 2], _hi: [f32; 2]) -> Vec<Solid<'_>> {
            self.0.iter().enumerate().map(|(i, (spec, at))| Solid { spec, model: i as u64, position: *at, yaw: 0.0 }).collect()
        }
    }

    fn block(size: [i64; 3]) -> crate::build::BuildSpec {
        let [x, y, z] = size;
        serde_json::from_value(json!({ "name": "Block", "size": size,
            "palette": [{ "name": "s", "hex": "#888888", "material": "matte" }],
            "shapes": [{ "kind": "box", "color": "s", "from": [0, 0, 0], "to": [x, y, z], "radius": null, "wobble": null }] }))
        .unwrap()
    }

    #[test]
    fn walks_go_around_builds_and_onto_low_ones() {
        let y = (crate::terrain::FLAT_GROUND + 1) as f32 * VOXEL_SIZE;
        // A wall 25 cm thick, 2 m tall and 8 m long across the way; a deck 25 cm high.
        let ground = Built(vec![(block([2, 16, 64]), [10.125, y, 0.0]), (block([16, 2, 16]), [15.0, y, 10.0])]);
        let (from, to) = ([5.0, y, 0.0], [17.0, y, 0.0]);
        let way = path(&ground, from, to).unwrap();
        assert!(way.iter().any(|p| p[2].abs() >= 4.0), "around the wall's end: {way:?}");
        // Onto the deck, standing on top of it.
        let deck = path(&ground, from, [15.0, y, 10.0]).unwrap();
        assert_eq!(deck.last().unwrap()[1], y + 0.25, "{deck:?}");
        // Nobody stands inside a wall: its top is too high a step.
        assert!(path(&ground, from, [10.1, y, 0.0]).is_err());
    }

    #[test]
    fn pieces_move_along_their_walk() {
        let mut e = Entity { package: "town".into(), position: [0.0, 0.0, 0.0], components: Default::default() };
        e.components.insert("walk".into(), json!({ "to": [10, 0, 0], "speed": 2.0, "path": [[0, 0, 0], [4, 0, 0], [4, 0, 6]], "start": 100 }));
        assert_eq!(position_at(&e, 50.0), [0.0, 0.0, 0.0]);
        assert_eq!(position_at(&e, 101.0), [2.0, 0.0, 0.0]);
        assert_eq!(position_at(&e, 104.0), [4.0, 0.0, 4.0]);
        assert_eq!(position_at(&e, 1000.0), [4.0, 0.0, 6.0]);
    }
}
