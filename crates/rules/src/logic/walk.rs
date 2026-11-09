//! Pieces that move on their own: walking somewhere, a daily routine, wandering about,
//! and spawners that keep a few pieces around. The engine runs them with timers of its
//! own (named `~walk:<id>` and so on, on the piece's package), so they cost nothing
//! while nobody is near, and what they do is recorded like any timer's: replay never
//! looks for a path again.
//!
//! A walk is one change, not one per step: the `walk` component holds the path, when
//! it starts and how fast, and players' games move the piece along it as time passes
//! ([`position_at`]). The piece's position stays where it set off until it arrives;
//! then it moves there, and its package's `on_arrive` runs.
//!
//! Engine components (only the piece's own package sets them):
//! - `walk`: `{"to": [x, y, z], "speed": m/s}`, and once on its way `"path"` and `"start"`.
//! - `routine`: `[{"at": 0.0-1.0 of the day, "to": [x, y, z]}, ...]`: where to be when.
//! - `wander`: `{"home": [x, y, z], "radius": m, "speed": m/s, "pause": s}`.
//! - `spawner`: `{"model", "label", "tag", "max", "every", "radius", "when", "wander"}`.
//!
//! Paths follow the ground the host knows ([`Ground`]): around water and up steps no
//! taller than [`CLIMB`]. Builds don't block them yet.

use std::{
    cmp::Ordering,
    collections::{BinaryHeap, HashMap},
};

use serde_json::{Value, json};

use super::{
    Batch, Effect, EntityId, Grants, LogicRules,
    entity::{Entity, snap},
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
/// The most stops in a routine.
pub const MAX_STOPS: usize = 12;
/// The most pieces one spawner keeps around.
pub const MAX_SPAWNED: u64 = 32;
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
    let cell_of = |p: [f32; 3]| ((column(p[0]) - min[0]) / CELL, (column(p[2]) - min[1]) / CELL);
    let height = |c: (i32, i32)| -> Option<f32> {
        (c.0 >= 0 && c.1 >= 0 && c.0 < cells[0] && c.1 < cells[1]).then_some(())?;
        heights.get((c.1 * cells[0] + c.0) as usize).copied().flatten().map(|y| (y + 1) as f32 * VOXEL_SIZE)
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
        let here = height(cell).unwrap_or(0.0);
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
        Effect::Schedule { name: timer("walk", id), due: at + 1, every: None, data: Value::Null, near: Some(id) },
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

/// What gives `id` the engine component `name` (`routine`, `wander`, `spawner`) and
/// starts its timer at `at`; `None` takes it away.
pub fn start_effects(id: EntityId, name: &str, value: Option<Value>, at: u64) -> Vec<Effect> {
    let kind = match name {
        "routine" => "plan",
        "spawner" => "spawn",
        other => other,
    };
    match value {
        Some(value) => {
            let every = (name == "spawner").then(|| value["every"].as_u64().unwrap_or(60).max(1));
            vec![
                Effect::Set { entity: id, component: name.into(), value },
                Effect::Schedule { name: timer(kind, id), due: at + 1, every, data: Value::Null, near: Some(id) },
            ]
        }
        None => vec![Effect::Remove { entity: id, component: name.into() }, Effect::Cancel { name: timer(kind, id) }],
    }
}

/// The engine's timer `kind` for entity `id`: `~walk:2.0`.
pub fn timer(kind: &str, id: EntityId) -> String {
    format!("~{kind}:{}.{}", id.change, id.n)
}

/// "8:30" as a share of the day.
pub fn time_of_day(text: &str) -> Option<f64> {
    let (h, m) = text.trim().split_once(':')?;
    let (h, m): (u32, u32) = (h.parse().ok()?, m.parse().ok()?);
    (h < 24 && m < 60).then(|| (h * 60 + m) as f64 / 1440.0)
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
        "routine" => value.as_array().is_some_and(|stops| {
            !stops.is_empty()
                && stops.len() <= MAX_STOPS
                && stops.iter().all(|s| s["at"].as_f64().is_some_and(|t| (0.0..1.0).contains(&t)) && point(&s["to"]).is_some())
        }),
        "wander" => point(&value["home"]).is_some() && num("radius", 1.0..=32.0) && num("speed", 0.2..=12.0) && num("pause", 0.0..=3600.0),
        "spawner" => {
            value["model"].as_str().is_some()
                && num("max", 1.0..=MAX_SPAWNED as f64)
                && num("every", 5.0..=86_400.0)
                && num("radius", 1.0..=32.0)
                && matches!(value["when"].as_str(), Some("any" | "day" | "night"))
        }
        _ => true,
    };
    if ok {
        return Ok(());
    }
    Err(match name {
        "walk" => format!("\"walk\" is {{\"to\": [x, y, z], \"speed\": {}-{} m/s}}; use walk(id, pos, speed)", SPEEDS.start(), SPEEDS.end()),
        "routine" => format!("a routine is 1 to {MAX_STOPS} stops {{at = \"8:00\", to = pos}}"),
        "wander" => "\"wander\" is {home = pos, radius = 1-32 m, speed = 0.2-12 m/s, pause = 0-3600 s}".into(),
        _ => format!(
            "a spawner is {{model = one of the package's models, max = 1-{MAX_SPAWNED}, every = 5-86400 s, radius = 1-32 m, when = \"any\"|\"day\"|\"night\"}}"
        ),
    })
}

/// What an engine timer needs.
pub(super) struct Engine<'a, 'b> {
    pub batch: &'a mut Batch<'b>,
    pub grants: &'a Grants,
    pub ground: &'a dyn Ground,
    pub rules: &'a LogicRules,
    pub now: u64,
    pub seed: u64,
}

/// A piece reached the end of its walk (`stuck`: it couldn't find the way), for its
/// package's `on_arrive`.
pub(super) struct Arrived {
    pub entity: EntityId,
    pub stuck: bool,
}

impl Engine<'_, '_> {
    fn random(&mut self) -> f32 {
        self.seed = self.seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (self.seed >> 40) as f32 / (1u64 << 24) as f32
    }

    fn apply(&mut self, effect: Effect) -> Result<Option<EntityId>, String> {
        self.batch.apply(self.grants, &effect)
    }

    /// Runs the engine's timer `name` (`~walk:2.0`, without the `~`).
    pub fn fire(&mut self, name: &str) -> Result<Option<Arrived>, String> {
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
            "plan" => self.plan(id, &e).map(|_| None),
            "wander" => self.wander(id, &e).map(|_| None),
            "spawn" => self.spawn(id, &e).map(|_| None),
            _ => Ok(None),
        }
    }

    /// Finds the way for a walk asked for, and sets off.
    fn set_off(&mut self, id: EntityId, e: &Entity) -> Result<Option<Arrived>, String> {
        let Some(walk) = e.components.get("walk") else { return Ok(None) };
        let (Some(to), speed) = (point(&walk["to"]), walk["speed"].as_f64().unwrap_or(DEFAULT_SPEED as f64) as f32) else {
            return Ok(None);
        };
        match self.go(id, e, to, speed, 0) {
            Ok(()) => Ok(None),
            Err(_) => {
                self.apply(Effect::Remove { entity: id, component: "walk".into() })?;
                Ok(Some(Arrived { entity: id, stuck: true }))
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
        self.apply(Effect::Schedule { name: timer("arrive", id), due: start + takes, every: None, data: Value::Null, near: Some(id) })?;
        Ok(())
    }

    /// A walk is over: the piece stands at its end. A wanderer sets off again after its
    /// pause; anyone else's package hears about it.
    fn arrive(&mut self, id: EntityId, e: &Entity) -> Result<Option<Arrived>, String> {
        let Some(walk) = e.components.get("walk") else { return Ok(None) };
        let end = points(&walk["path"]).and_then(|p| p.last().copied()).unwrap_or(e.position);
        self.apply(Effect::Move { entity: id, position: end })?;
        let wander = e.components.get("wander").cloned();
        let Some(wander) = wander.filter(|_| !e.components.contains_key("routine") || walk["wandering"].as_bool() == Some(true)) else {
            self.apply(Effect::Remove { entity: id, component: "walk".into() })?;
            return Ok(Some(Arrived { entity: id, stuck: false }));
        };
        let mut e = e.clone();
        e.position = end;
        e.components.remove("walk");
        self.leg(id, &e, &wander)
            .or_else(|_| self.apply(Effect::Remove { entity: id, component: "walk".into() }).map(|_| ()))?;
        Ok(None)
    }

    /// The next leg of a wander: somewhere within its radius of home.
    fn leg(&mut self, id: EntityId, e: &Entity, wander: &Value) -> Result<(), String> {
        let home = point(&wander["home"]).unwrap_or(e.position);
        let radius = wander["radius"].as_f64().unwrap_or(6.0) as f32;
        let speed = wander["speed"].as_f64().unwrap_or(1.0) as f32;
        let pause = wander["pause"].as_f64().unwrap_or(4.0) as u64;
        // A few tries, in case the first spot is in water.
        for _ in 0..4 {
            let (angle, r) = (self.random() * std::f32::consts::TAU, radius * self.random().sqrt());
            let to = [home[0] + angle.cos() * r, home[1], home[2] + angle.sin() * r];
            if self.go(id, e, to, speed, pause).is_ok() {
                // Marked, so a routine's arrival isn't mistaken for a wander's.
                let mut walk = self.batch.get(id).and_then(|e| e.components.get("walk").cloned()).unwrap_or_default();
                walk["wandering"] = true.into();
                self.apply(Effect::Set { entity: id, component: "walk".into(), value: walk })?;
                return Ok(());
            }
        }
        Err("nowhere to wander".into())
    }

    /// Starts wandering, unless it's on its way somewhere.
    fn wander(&mut self, id: EntityId, e: &Entity) -> Result<(), String> {
        let Some(wander) = e.components.get("wander").cloned() else { return Ok(()) };
        if e.components.contains_key("walk") {
            return Ok(());
        }
        self.leg(id, e, &wander)
    }

    /// A routine: goes where it should be at this time of day, and wakes at the next stop.
    fn plan(&mut self, id: EntityId, e: &Entity) -> Result<(), String> {
        let Some(stops) = e.components.get("routine").and_then(|r| r.as_array()).cloned() else { return Ok(()) };
        let mut stops: Vec<(f64, [f32; 3])> = stops.iter().filter_map(|s| Some((s["at"].as_f64()?, point(&s["to"])?))).collect();
        stops.sort_by(|a, b| a.0.total_cmp(&b.0));
        let (Some(first), Some(last)) = (stops.first().copied(), stops.last().copied()) else { return Ok(()) };
        let day = self.rules.clock.day_seconds.max(1) as f64;
        let now = self.rules.clock.at(self.now).time_of_day as f64;
        let current = stops.iter().rev().find(|s| s.0 <= now).copied().unwrap_or(last);
        let next = stops.iter().find(|s| s.0 > now).map_or(first.0 + 1.0, |s| s.0);
        let wait = ((next - now) * day).ceil().max(1.0) as u64;
        self.apply(Effect::Schedule { name: timer("plan", id), due: self.now + wait, every: None, data: Value::Null, near: Some(id) })?;
        let here = position_at(e, self.now as f64);
        if flat_distance(here, current.1) > 0.5 {
            let speed = e.components.get("wander").and_then(|w| w["speed"].as_f64()).unwrap_or(DEFAULT_SPEED as f64) as f32;
            // A routine stop is where it wanders from, too.
            if let Some(mut wander) = e.components.get("wander").cloned() {
                wander["home"] = json!(current.1);
                self.apply(Effect::Set { entity: id, component: "wander".into(), value: wander })?;
            }
            self.apply(Effect::Set { entity: id, component: "walk".into(), value: json!({ "to": current.1, "speed": speed }) })?;
            let e = self.batch.get(id).cloned().unwrap_or_else(|| e.clone());
            self.go(id, &e, current.1, speed, 0)?;
        }
        Ok(())
    }

    /// A spawner's turn: one more piece, if it has fewer than its most and it's the
    /// right time of day.
    fn spawn(&mut self, id: EntityId, e: &Entity) -> Result<(), String> {
        let Some(spawner) = e.components.get("spawner").cloned() else { return Ok(()) };
        let when = spawner["when"].as_str().unwrap_or("any");
        let time = self.rules.clock.at(self.now).time_of_day;
        let day = (0.25..0.75).contains(&time);
        if (when == "day" && !day) || (when == "night" && day) {
            return Ok(());
        }
        let me = json!(format!("{}.{}", id.change, id.n));
        let alive = self.batch.ids().into_iter().filter(|i| self.batch.get(*i).is_some_and(|o| o.components.get("spawned") == Some(&me))).count() as u64;
        if alive >= spawner["max"].as_u64().unwrap_or(1).min(MAX_SPAWNED) {
            return Ok(());
        }
        let radius = spawner["radius"].as_f64().unwrap_or(4.0) as f32;
        let (angle, r) = (self.random() * std::f32::consts::TAU, radius * self.random().sqrt());
        let mut at = [e.position[0] + angle.cos() * r, e.position[1], e.position[2] + angle.sin() * r];
        if let Some(heights) = self.ground.heights([column(at[0]), column(at[2])], 1, [1, 1]) {
            match heights.first().copied().flatten() {
                Some(y) => at[1] = (y + 1) as f32 * VOXEL_SIZE,
                // In water: try again next time.
                None => return Ok(()),
            }
        }
        let mut components = serde_json::Map::new();
        components.insert("look".into(), json!({ "model": spawner["model"] }));
        components.insert("spawned".into(), me);
        if let Some(label) = spawner["label"].as_str().filter(|l| !l.is_empty()) {
            components.insert("label".into(), label.into());
        }
        if let Some(tag) = spawner["tag"].as_str().filter(|t| !t.is_empty()) {
            let tag = if tag.contains(':') { tag.to_string() } else { format!("{}:{tag}", self.grants.package) };
            components.insert(tag, json!({}));
        }
        let wanders = spawner["wander"].as_bool().unwrap_or(true);
        if wanders {
            components.insert("wander".into(), json!({ "home": snap(e.position), "radius": radius.max(2.0), "speed": 1.0, "pause": 4 }));
        }
        let components = components.into_iter().collect();
        let Some(new) = self.apply(Effect::Spawn { position: at, components })? else { return Ok(()) };
        if wanders {
            self.apply(Effect::Schedule { name: timer("wander", new), due: self.now + 1, every: None, data: Value::Null, near: Some(new) })?;
        }
        Ok(())
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

    #[test]
    fn pieces_move_along_their_walk() {
        let mut e = Entity { package: "town".into(), position: [0.0, 0.0, 0.0], components: Default::default() };
        e.components.insert("walk".into(), json!({ "to": [10, 0, 0], "speed": 2.0, "path": [[0, 0, 0], [4, 0, 0], [4, 0, 6]], "start": 100 }));
        assert_eq!(position_at(&e, 50.0), [0.0, 0.0, 0.0]);
        assert_eq!(position_at(&e, 101.0), [2.0, 0.0, 0.0]);
        assert_eq!(position_at(&e, 104.0), [4.0, 0.0, 4.0]);
        assert_eq!(position_at(&e, 1000.0), [4.0, 0.0, 6.0]);
        assert_eq!(time_of_day("6:00"), Some(0.25));
        assert_eq!(time_of_day("25:00"), None);
    }
}
