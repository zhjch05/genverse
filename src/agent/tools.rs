//! Read-only world tools for agents, served over [`mcp`](super::mcp).
//!
//! Everything is expressed in the *build frame* the agent designs in: build voxels
//! (12.5 cm), origin at the build site's ground, +Y up, z = 0 the front row facing the
//! player, x to the right as the player sees it. Nothing here changes the world.

use std::fmt::Write;

use bevy::prelude::*;
use serde_json::{Value, json};

use super::{
    creations::{Creations, SavedCreation, anchor_in_front},
    mcp::{McpServer, ToolCall},
    spec::{BuildSpec, VOXEL_SIZE, compile, limits},
};
use crate::{
    player::Player,
    voxel::{AIR, VoxelId},
    world::{
        VoxelWorld,
        terrain::{self, block},
    },
};

pub const INSTRUCTIONS: &str = "Read-only tools for the Cubelark voxel world. Coordinates are build voxels \
(12.5 cm) in the frame of the build site: y=0 is the ground there, z=0 is the front row facing the player.";

/// Where the next build goes, frozen while a request runs so tool answers stay consistent.
#[derive(Resource, Default, Clone, Copy)]
pub struct BuildSite {
    /// Bottom-center of the footprint.
    pub origin: Vec3,
    pub yaw: f32,
}

impl BuildSite {
    fn rotation(self) -> Quat {
        Quat::from_rotation_y(self.yaw)
    }

    /// Build-frame voxel coordinates (relative to the footprint center) to world space.
    fn to_world(self, local: Vec3) -> Vec3 {
        self.origin + self.rotation() * (local * VOXEL_SIZE)
    }

    fn to_local(self, world: Vec3) -> Vec3 {
        self.rotation().inverse() * (world - self.origin) / VOXEL_SIZE
    }
}

pub fn definitions() -> Value {
    let size = json!({ "type": "integer", "minimum": 2, "maximum": limits::MAX_SIZE });
    json!([
        {
            "name": "get_build_site",
            "description": "Ground heights and surface types under a footprint centered on the build site, \
                plus where the player stands. Call this before designing anything that should fit the terrain.",
            "inputSchema": {
                "type": "object",
                "properties": { "width": size, "depth": size },
                "required": ["width", "depth"],
            },
        },
        {
            "name": "list_nearby_builds",
            "description": "Builds already placed near the site: id, name, size, position and turn in the build \
                frame, and the request that made them.",
            "inputSchema": {
                "type": "object",
                "properties": { "radius_m": { "type": "number", "minimum": 1, "maximum": 60 } },
            },
        },
        {
            "name": "get_build_spec",
            "description": "The full build JSON of a placed build (by id from list_nearby_builds), e.g. to match \
                its style or palette. Use id \"draft\" for the draft being revised.",
            "inputSchema": {
                "type": "object",
                "properties": { "id": { "type": "string" } },
                "required": ["id"],
            },
        },
        {
            "name": "check_build",
            "description": "Validate a build against the limits and report how it sits on the ground at the site: \
                floating columns, voxels buried in terrain or under water, and overlaps with nearby builds. \
                Does not place anything.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "build": { "type": "object", "description": "Same shape as the \"build\" in your answer." },
                },
                "required": ["build"],
            },
        },
    ])
}

/// Keeps the build site current while no request is running: the draft or selected
/// build if there is one, otherwise a spot in front of the player.
pub fn update_build_site(
    mut site: ResMut<BuildSite>,
    chat: Res<super::Chat>,
    creations: Res<Creations>,
    world: Res<VoxelWorld>,
    players: Query<&Transform, With<Player>>,
    transforms: Query<&Transform>,
) {
    if chat.pending.is_some() {
        return;
    }
    let target = creations.draft.as_ref().map(|d| d.entity).or(creations.selected);
    if let Some(t) = target.and_then(|e| transforms.get(e).ok()) {
        *site = BuildSite { origin: t.translation, yaw: t.rotation.to_euler(EulerRot::YXZ).0 };
    } else if let Ok(player) = players.single() {
        let (origin, yaw) = anchor_in_front(player, &world, UVec3::splat(limits::MAX_SIZE / 2));
        *site = BuildSite { origin, yaw };
    }
}

pub fn answer_tool_calls(
    server: Option<Res<McpServer>>,
    site: Res<BuildSite>,
    world: Res<VoxelWorld>,
    creations: Res<Creations>,
    players: Query<&Transform, With<Player>>,
    transforms: Query<&Transform>,
) {
    let Some(server) = server else {
        return;
    };
    let calls = server.drain();
    if calls.is_empty() {
        return;
    }
    let placed: Vec<_> = creations
        .placed
        .iter()
        .filter_map(|(e, c)| transforms.get(*e).ok().map(|t| (*t, c)))
        .collect();
    for ToolCall { name, args, reply } in calls {
        let ctx = Ctx { site: *site, world: &world, creations: &creations, placed: &placed };
        let result = match name.as_str() {
            "get_build_site" => Ok(ctx.build_site(&args, players.single().ok())),
            "list_nearby_builds" => Ok(ctx.nearby(&args)),
            "get_build_spec" => ctx.spec(&args),
            "check_build" => ctx.check(&args),
            other => Err(format!("unknown tool {other}")),
        };
        info!("agent tool {name}: {}", if result.is_ok() { "ok" } else { "error" });
        let _ = reply.send(result);
    }
}

struct Ctx<'a> {
    site: BuildSite,
    world: &'a VoxelWorld,
    creations: &'a Creations,
    /// Placed builds with their current transforms.
    placed: &'a [(Transform, &'a SavedCreation)],
}

/// What the ground is made of at the top of a column.
fn surface_code(id: VoxelId) -> char {
    match id {
        id if block::GRASS.contains(&id) => 'g',
        id if block::DIRT.contains(&id) => 'd',
        id if block::STONE.contains(&id) => 'r',
        id if block::SAND.contains(&id) => 's',
        id if block::BARK.contains(&id) => 't',
        id if block::LEAVES.contains(&id) => 'l',
        block::WATER => 'w',
        block::SNOW => 'n',
        _ => '?',
    }
}

const SURFACE_LEGEND: &str = "g grass, d dirt, r rock, s sand, w water (height is the water surface), \
t tree trunk, l leaves, n snow, ? not loaded";

impl Ctx<'_> {
    /// Top of the first ground voxel (terrain, water, trees) under a world point, as a
    /// build-frame height, with its surface code. Grass tufts and flowers don't count.
    fn ground(&self, world_xz: Vec3) -> (Option<f32>, char) {
        let start = VoxelWorld::voxel_at(Vec3::new(world_xz.x, self.site.origin.y + 6.0, world_xz.z));
        for dy in 0..120 {
            let p = start - IVec3::Y * dy;
            match self.world.get(p) {
                None => return (None, '?'),
                Some(AIR | block::TUFT) => {}
                Some(id) if block::FLOWERS.contains(&id) => {}
                Some(id) => {
                    let top = (p.y + 1) as f32 * terrain::VOXEL_SIZE;
                    return (Some((top - self.site.origin.y) / VOXEL_SIZE), surface_code(id));
                }
            }
        }
        (None, '?')
    }

    /// Ground under build-frame column `(x, z)` of a `width`×`depth` footprint.
    fn column_ground(&self, x: f32, z: f32, width: f32, depth: f32) -> (Option<f32>, char) {
        let local = Vec3::new(x + 0.5 - width / 2.0, 0.0, z + 0.5 - depth / 2.0);
        self.ground(self.site.to_world(local))
    }

    fn build_site(&self, args: &Value, player: Option<&Transform>) -> String {
        let dim = |key: &str| args[key].as_u64().unwrap_or(32).clamp(2, limits::MAX_SIZE as u64) as u32;
        let (width, depth) = (dim("width"), dim("depth"));
        let mut out = String::new();
        let _ = writeln!(
            out,
            "Footprint {width}×{depth} build voxels centered on the site. x runs left→right as the player sees it, \
             z runs front→back (z=0 faces the player). Each entry is the ground height at that column relative to \
             y=0, in build voxels (8 per meter), followed by the surface: {SURFACE_LEGEND}. Terrain is 2 build \
             voxels per step, so every second column is shown."
        );
        if let Some(player) = player {
            let p = self.site.to_local(player.translation);
            let _ = writeln!(
                out,
                "The player stands at x={:.0}, z={:.0} (relative to the footprint center), feet at y={:.0}.",
                p.x + width as f32 / 2.0,
                p.z + depth as f32 / 2.0,
                p.y
            );
        }
        let _ = write!(out, "\n      ");
        for x in (0..width).step_by(2) {
            let _ = write!(out, "x{x:<3} ");
        }
        out.push('\n');
        for z in (0..depth).step_by(2) {
            let _ = write!(out, "z{z:<3}  ");
            for x in (0..width).step_by(2) {
                let (h, code) = self.column_ground(x as f32, z as f32, width as f32, depth as f32);
                match h {
                    Some(h) => {
                        let _ = write!(out, "{:<+4}{code} ", h.round() as i32);
                    }
                    None => out.push_str(" ?   "),
                }
            }
            out.push('\n');
        }
        if let Some(draft) = &self.creations.draft
            && draft.replaces.is_some()
        {
            let _ = writeln!(
                out,
                "\nYou are revising \"{}\" ({}×{}×{}); it stands on this site. get_build_spec id \"draft\" returns it.",
                draft.spec.name, draft.stats.size.x, draft.stats.size.y, draft.stats.size.z
            );
        }
        out
    }

    fn nearby(&self, args: &Value) -> String {
        let radius = args["radius_m"].as_f64().unwrap_or(24.0) as f32;
        let mut rows: Vec<_> = self
            .placed
            .iter()
            .filter_map(|(t, saved)| {
                let distance = t.translation.distance(self.site.origin);
                (distance <= radius).then_some((distance, t, saved))
            })
            .collect();
        rows.sort_by(|a, b| a.0.total_cmp(&b.0));
        if rows.is_empty() {
            return format!("No builds within {radius:.0} m of the site.");
        }
        let mut out = format!(
            "Builds within {radius:.0} m. Positions are their bottom-center relative to the site's footprint center, \
             in build voxels; turn is relative to the site's front.\n"
        );
        for (distance, t, c) in rows.into_iter().take(12) {
            let p = self.site.to_local(t.translation);
            let turn = (t.rotation.to_euler(EulerRot::YXZ).0 - self.site.yaw).to_degrees().rem_euclid(360.0);
            let _ = writeln!(
                out,
                "- id {} \"{}\" size {}×{}×{} at x={:+.0} y={:+.0} z={:+.0} ({distance:.1} m away), turned {:.0}°, \
                 made for \"{}\"",
                c.id, c.name, c.spec.size[0], c.spec.size[1], c.spec.size[2], p.x, p.y, p.z, turn, c.prompt
            );
        }
        out
    }

    fn spec(&self, args: &Value) -> Result<String, String> {
        let id = args["id"].as_str().map(str::to_string).or_else(|| args["id"].as_u64().map(|n| n.to_string()));
        let id = id.ok_or("pass an id from list_nearby_builds, or \"draft\"")?;
        let spec = if id == "draft" {
            self.creations.draft.as_ref().map(|d| &d.spec).ok_or("there is no draft")?
        } else {
            let n: u64 = id.parse().map_err(|_| format!("unknown id {id:?}"))?;
            self.creations.placed.iter().find(|(_, c)| c.id == n).map(|(_, c)| &c.spec).ok_or(format!("no build {n}"))?
        };
        serde_json::to_string(spec).map_err(|e| e.to_string())
    }

    fn check(&self, args: &Value) -> Result<String, String> {
        let spec: BuildSpec =
            serde_json::from_value(args["build"].clone()).map_err(|e| format!("build doesn't match the schema: {e}"))?;
        let build = compile(&spec).map_err(|errors| format!("Rejected:\n- {}", errors.join("\n- ")))?;
        let s = build.stats;
        let grid = &build.model.grid;
        let (w, d) = (s.size.x as f32, s.size.z as f32);

        let (mut floating, mut max_gap, mut buried, mut wet, mut columns) = (0, 0.0_f32, 0, 0, 0);
        for z in 0..s.size.z as i32 {
            for x in 0..s.size.x as i32 {
                let ys: Vec<i32> = (0..s.size.y as i32).filter(|&y| grid.get(IVec3::new(x, y, z)) != AIR).collect();
                let Some(&lowest) = ys.first() else {
                    continue;
                };
                columns += 1;
                let (Some(ground), code) = self.column_ground(x as f32, z as f32, w, d) else {
                    continue;
                };
                let gap = lowest as f32 - ground;
                if gap >= 1.5 {
                    floating += 1;
                    max_gap = max_gap.max(gap);
                }
                let under = ys.iter().filter(|&&y| (y as f32) < ground - 0.5).count();
                if code == 'w' {
                    wet += under;
                } else {
                    buried += under;
                }
            }
        }

        let mut out = format!(
            "Valid: {}×{}×{} voxels, {} solid, {} visible faces, {} glowing (limits {} / {} / {}).\n",
            s.size.x, s.size.y, s.size.z, s.voxels, s.faces, s.glow,
            limits::MAX_VOXELS, limits::MAX_FACES, limits::MAX_GLOW_VOXELS
        );
        let _ = writeln!(
            out,
            "On this site: {floating} of {columns} footprint columns float above the ground (largest gap {:.0} \
             voxels); {buried} voxels are buried in terrain; {wet} are under water.",
            max_gap
        );
        // Rough overlap check: square bounds that cover any rotation.
        let reach = |w: f32, d: f32| w.max(d) * VOXEL_SIZE / 2.0;
        let r = reach(w, d);
        let lo = self.site.origin - Vec3::new(r, 0.0, r);
        let hi = self.site.origin + Vec3::new(r, s.size.y as f32 * VOXEL_SIZE, r);
        let replaced = self.creations.draft.as_ref().and_then(|d| d.replaces.as_ref()).map(|r| r.id);
        let overlaps: Vec<_> = self
            .placed
            .iter()
            .filter(|(_, c)| Some(c.id) != replaced)
            .filter_map(|(t, c)| {
                let r = reach(c.spec.size[0] as f32, c.spec.size[2] as f32);
                let height = c.spec.size[1] as f32 * VOXEL_SIZE;
                let (blo, bhi) = (t.translation - Vec3::new(r, 0.0, r), t.translation + Vec3::new(r, height, r));
                (lo.cmplt(bhi).all() && blo.cmplt(hi).all()).then(|| format!("\"{}\" (id {})", c.name, c.id))
            })
            .collect();
        if overlaps.is_empty() {
            out.push_str("No nearby builds in the way.");
        } else {
            let _ = write!(out, "Its bounds overlap: {}.", overlaps.join(", "));
        }
        Ok(out)
    }
}
