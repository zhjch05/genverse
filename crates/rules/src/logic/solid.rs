//! What stands in walking pieces' way besides the land: builds, and pieces whose look
//! is `solid`. Each is read as the terrain columns it fills and the heights it fills
//! them to, so a walk goes around a wall, up onto a low floor or deck like a step, and
//! under a roof or through a doorway with room overhead.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    sync::{Arc, Mutex},
};

use bevy_math::{IVec3, Quat, Vec3};

use super::Logic;
use crate::{
    build::{self, BuildLimits, BuildSpec},
    log::{Builds, fnv1a},
    terrain::VOXEL_SIZE,
    voxel::AIR,
};

/// How much room a piece needs over where it stands, in meters (a hero is 1.35 m).
pub const HEADROOM: f32 = 1.4;

/// The side of an [`Areas`] square, in meters.
pub const AREA: f32 = 16.0;

/// How far the largest model reaches from where it stands, across the ground.
pub const MAX_REACH: f32 = BuildLimits::ACCEPTED.max_size as f32 * build::VOXEL_SIZE * std::f32::consts::FRAC_1_SQRT_2 + VOXEL_SIZE;

/// Things by the [`AREA`]-meter squares of ground they reach into, so what's near a
/// place is found without looking at everything in the world.
#[derive(Clone, Debug, PartialEq)]
pub struct Areas<K: Ord> {
    squares: BTreeMap<(i32, i32), BTreeSet<K>>,
    /// The squares each thing is in, as their lowest and highest corners.
    of: BTreeMap<K, ((i32, i32), (i32, i32))>,
}

impl<K: Ord> Default for Areas<K> {
    fn default() -> Self {
        Self { squares: BTreeMap::new(), of: BTreeMap::new() }
    }
}

fn square(x: f32, z: f32) -> (i32, i32) {
    ((x / AREA).floor() as i32, (z / AREA).floor() as i32)
}

impl<K: Ord + Copy> Areas<K> {
    /// `key` stands at `at` and reaches `reach` meters around it.
    pub fn put(&mut self, key: K, at: [f32; 3], reach: f32) {
        let squares = (square(at[0] - reach, at[2] - reach), square(at[0] + reach, at[2] + reach));
        if self.of.get(&key) == Some(&squares) {
            return;
        }
        self.take(key);
        let ((x0, z0), (x1, z1)) = squares;
        for x in x0..=x1 {
            for z in z0..=z1 {
                self.squares.entry((x, z)).or_default().insert(key);
            }
        }
        self.of.insert(key, squares);
    }

    pub fn take(&mut self, key: K) {
        let Some(((x0, z0), (x1, z1))) = self.of.remove(&key) else { return };
        for x in x0..=x1 {
            for z in z0..=z1 {
                if let Some(keys) = self.squares.get_mut(&(x, z)) {
                    keys.remove(&key);
                    if keys.is_empty() {
                        self.squares.remove(&(x, z));
                    }
                }
            }
        }
    }

    /// What may reach into the ground from `lo` to `hi` (x, z).
    pub fn within(&self, lo: [f32; 2], hi: [f32; 2]) -> BTreeSet<K> {
        let ((x0, z0), (x1, z1)) = (square(lo[0], lo[1]), square(hi[0], hi[1]));
        let mut found = BTreeSet::new();
        for x in x0..=x1 {
            for (_, keys) in self.squares.range((x, z0)..=(x, z1)) {
                found.extend(keys.iter().copied());
            }
        }
        found
    }
}

/// One solid thing in the world: a model standing at `position`, turned `yaw` radians.
pub struct Solid<'a> {
    pub spec: &'a BuildSpec,
    /// Which model this is, cheaply: the same model always has the same key.
    pub model: u64,
    pub position: [f32; 3],
    pub yaw: f32,
}

/// What a solid fills, by terrain column: the heights (meters, low to high) of each run of
/// voxels in it.
pub type Spans = HashMap<(i32, i32), Vec<(f32, f32)>>;

impl Solid<'_> {
    /// How far it reaches from where it stands, across the ground.
    pub fn reach(&self) -> f32 {
        let side = |i: usize| self.spec.size.get(i).copied().unwrap_or(1) as f32 * build::VOXEL_SIZE;
        (side(0).hypot(side(2))) / 2.0 + VOXEL_SIZE
    }

    /// The columns it fills (worked out once for each model, place and turn, and kept
    /// while it's among the most recently used).
    pub fn spans(&self) -> Arc<Spans> {
        const KEEP: usize = 4096;
        static CACHE: Mutex<Option<(u64, HashMap<u64, (Arc<Spans>, u64)>)>> = Mutex::new(None);
        let key = fnv1a(&[self.model.to_le_bytes().to_vec(), bytes(self.position, self.yaw)].concat());
        if let Ok(mut cache) = CACHE.lock() {
            let (tick, cache) = cache.get_or_insert_with(Default::default);
            *tick += 1;
            if let Some((found, used)) = cache.get_mut(&key) {
                *used = *tick;
                return found.clone();
            }
        }
        let spans = Arc::new(self.fill());
        if let Ok(mut cache) = CACHE.lock() {
            let (tick, cache) = cache.get_or_insert_with(Default::default);
            if cache.len() >= KEEP {
                // Forget the half used longest ago.
                let mut used: Vec<u64> = cache.values().map(|(_, u)| *u).collect();
                let (_, middle, _) = used.select_nth_unstable(KEEP / 2);
                let middle = *middle;
                cache.retain(|_, (_, u)| *u > middle);
            }
            cache.insert(key, (spans.clone(), *tick));
        }
        spans
    }

    fn fill(&self) -> Spans {
        let mut spans = Spans::new();
        let Ok(compiled) = build::compile(self.spec, &BuildLimits::ACCEPTED) else { return spans };
        let model = compiled.model;
        let size = model.grid.size().as_ivec3();
        let turn = Quat::from_rotation_y(self.yaw);
        let at = Vec3::from_array(self.position);
        for z in 0..size.z {
            for x in 0..size.x {
                // The middle of this column of the model, in the world.
                let local = (Vec3::new(x as f32 + 0.5, 0.0, z as f32 + 0.5) - model.pivot) * model.voxel_size;
                let middle = at + turn * local;
                let column = ((middle.x / VOXEL_SIZE).floor() as i32, (middle.z / VOXEL_SIZE).floor() as i32);
                let mut y = 0;
                while y < size.y {
                    if model.grid.get(IVec3::new(x, y, z)) == AIR {
                        y += 1;
                        continue;
                    }
                    let low = y;
                    while y < size.y && model.grid.get(IVec3::new(x, y, z)) != AIR {
                        y += 1;
                    }
                    let height = |y: i32| at.y + (y as f32 - model.pivot.y) * model.voxel_size;
                    spans.entry(column).or_default().push((height(low), height(y)));
                }
            }
        }
        for runs in spans.values_mut() {
            runs.sort_by(|a, b| a.0.total_cmp(&b.0));
            let mut merged: Vec<(f32, f32)> = Vec::with_capacity(runs.len());
            for &(low, high) in runs.iter() {
                match merged.last_mut() {
                    Some(last) if low <= last.1 + 1e-4 => last.1 = last.1.max(high),
                    _ => merged.push((low, high)),
                }
            }
            *runs = merged;
        }
        spans
    }
}

fn bytes(position: [f32; 3], yaw: f32) -> Vec<u8> {
    position.iter().chain([yaw].iter()).flat_map(|f| f.to_bits().to_le_bytes()).collect()
}

/// Where a piece ends up standing in a column whose ground is at `ground` meters, given
/// what fills it: on top of anything that leaves it too little room.
pub fn stand(ground: f32, runs: &[(f32, f32)]) -> f32 {
    let mut at = ground;
    // Runs are low to high, so one pass climbs a stack.
    for &(low, high) in runs {
        if high > at + 1e-3 && low < at + HEADROOM {
            at = high;
        }
    }
    at
}

/// A world's solids that may reach into the ground from `lo` to `hi` (x, z): its builds,
/// and its pieces whose look is solid and which aren't on the move.
pub fn of_world<'a>(builds: &'a Builds, logic: &'a Logic, lo: [f32; 2], hi: [f32; 2]) -> Vec<Solid<'a>> {
    of_builds(builds, lo, hi).chain(of_pieces(logic, lo, hi)).collect()
}

pub fn of_builds(builds: &Builds, lo: [f32; 2], hi: [f32; 2]) -> impl Iterator<Item = Solid<'_>> {
    builds.within(lo, hi).filter_map(|(id, b)| Some(Solid { spec: &b.spec, model: builds.model(id)?, position: b.position, yaw: b.yaw }))
}

pub fn of_pieces(logic: &Logic, lo: [f32; 2], hi: [f32; 2]) -> impl Iterator<Item = Solid<'_>> {
    // Pieces are found by where they stand, so look as far around as a model can reach.
    let (lo, hi) = ([lo[0] - MAX_REACH, lo[1] - MAX_REACH], [hi[0] + MAX_REACH, hi[1] + MAX_REACH]);
    logic.state.within(lo, hi).filter_map(|(_, e)| {
        let look = e.components.get("look")?;
        if look["solid"] != true || e.components.contains_key("walk") || e.components.contains_key("chase") {
            return None;
        }
        let hash = &logic.installed.get(&e.package)?.package.hash;
        let name = look["model"].as_str()?;
        let spec = logic.packages.get(hash)?.models.get(name)?;
        let turn = look["turn"].as_u64().unwrap_or(0).min(3) as f32;
        let model = fnv1a(format!("{hash}/{name}").as_bytes());
        Some(Solid { spec, model, position: e.position, yaw: turn * std::f32::consts::FRAC_PI_2 })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wall() -> BuildSpec {
        serde_json::from_str(
            r##"{"name":"Wall","size":[2,16,16],"palette":[{"name":"s","hex":"#888888","material":"matte"}],
            "shapes":[{"kind":"box","color":"s","from":[0,0,0],"to":[2,16,16],"radius":null,"wobble":null}]}"##,
        )
        .unwrap()
    }

    #[test]
    fn walls_fill_their_columns() {
        let spec = wall();
        let solid = Solid { spec: &spec, model: 1, position: [1.125, 2.0, 1.0], yaw: 0.0 };
        let spans = solid.spans();
        // 2 voxels thick (25 cm: one column), 2 m long (8 columns), 2 m tall.
        assert_eq!(spans.len(), 8);
        assert_eq!(spans[&(4, 4)], vec![(2.0, 4.0)]);
        assert_eq!(stand(2.0, &spans[&(4, 4)]), 4.0, "a piece can't stand inside it");
        // A roof 2 m up leaves room under it; a deck 20 cm up is a step onto it.
        assert_eq!(stand(2.0, &[(4.0, 4.25)]), 2.0);
        assert_eq!(stand(2.0, &[(2.0, 2.2), (4.0, 4.25)]), 2.2);
    }

    #[test]
    fn areas_find_whats_near() {
        let mut areas = Areas::default();
        areas.put(1, [5.0, 0.0, 5.0], 0.0);
        areas.put(2, [100.0, 0.0, 100.0], 30.0);
        areas.put(3, [-40.0, 0.0, 3.0], 1.0);
        let near = |a: &Areas<i32>, lo: [f32; 2], hi: [f32; 2]| a.within(lo, hi).into_iter().collect::<Vec<_>>();
        assert_eq!(near(&areas, [0.0, 0.0], [10.0, 10.0]), vec![1]);
        // A big thing is found from the edge of its reach.
        assert_eq!(near(&areas, [65.0, 65.0], [70.0, 70.0]), vec![2]);
        assert_eq!(near(&areas, [-50.0, -50.0], [10.0, 10.0]), vec![1, 3]);
        // Moving and taking away.
        areas.put(1, [-38.0, 0.0, 4.0], 0.0);
        assert_eq!(near(&areas, [0.0, 0.0], [10.0, 10.0]), Vec::<i32>::new());
        areas.take(3);
        assert_eq!(near(&areas, [-50.0, -50.0], [10.0, 10.0]), vec![1]);
        assert_eq!(areas, { let mut a = Areas::default(); a.put(2, [100.0, 0.0, 100.0], 30.0); a.put(1, [-38.0, 0.0, 4.0], 0.0); a });
    }
}
