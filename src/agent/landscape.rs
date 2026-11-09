//! Reshaping the land by describing it: select an area of ground (G, then drag), tell
//! the agent what it should become, and preview the result before keeping it.
//!
//! The agent answers with a [`TerrainSpec`] (a few operations, never voxels) and
//! [`sculpt::compile`] turns it into voxel edits against the current ground. A preview
//! is applied to the world for real and put back voxel by voxel if it's discarded.
//! Kept changes go into the world's change log as their spec, never voxels.

use std::fmt::Write;

use bevy::{
    asset::RenderAssetUsages,
    gizmos::config::GizmoLineJoint,
    light::NotShadowCaster,
    mesh::{Indices, PrimitiveTopology},
    prelude::*,
    window::PrimaryWindow,
};
use genverse_rules::{
    land::{Access, PARCEL, area_parcels},
    log::{Action, terrain_footprint},
    undo::Cell,
    settings::LandMode,
};
use genverse_rules::sculpt::{self, Area, Columns, SculptStats, TerrainEdit, TerrainSpec, level_of, surface};

use super::{Chat, cursor_ray};
use crate::{
    player::{OrbitLock, PointerOverUi},
    voxel::{AIR, VoxelId},
    world::{
        VoxelWorld,
        ledger::Ledger,
        terrain::{VOXEL_SIZE, WORLD_HEIGHT, block},
    },
};

/// A click without a drag selects a square this wide, in meters.
const CLICK_AREA_M: f32 = 6.0;

/// How a voxel was before a change touched it.
struct Before {
    p: IVec3,
    edit: Option<VoxelId>,
    visible: VoxelId,
}

pub struct TerrainDraft {
    pub spec: TerrainSpec,
    pub prompt: String,
    pub provider: String,
    pub area: Area,
    pub stats: SculptStats,
    before: Vec<Before>,
}

/// A land change of ours the server hasn't answered yet: how the ground was, to put
/// it back if it refuses.
struct Kept {
    id: u64,
    before: Vec<Before>,
}

#[derive(Resource, Default)]
pub struct Landscape {
    /// G: left-drag on the ground selects an area instead of turning the camera.
    pub selecting: bool,
    pub area: Option<Area>,
    drag_from: Option<IVec2>,
    pub draft: Option<TerrainDraft>,
    /// Kept changes the server may still refuse or redo (online).
    kept: Vec<Kept>,
}

impl Landscape {
    /// The area a chat message reshapes: the preview's, or the selection.
    pub fn target(&self) -> Option<Area> {
        self.draft.as_ref().map(|d| d.area).or(self.area)
    }

    /// Replaces the preview with `spec`, compiled against the ground without the old
    /// preview. If it's rejected, the old preview stays.
    pub fn preview(
        &mut self,
        world: &mut VoxelWorld,
        ledger: &Ledger,
        spec: TerrainSpec,
        prompt: String,
        provider: String,
    ) -> Result<SculptStats, PreviewError> {
        let area = self.target().ok_or_else(|| PreviewError::Invalid(vec!["no area is selected".to_string()]))?;
        let old = self.draft.take();
        if let Some(old) = &old {
            revert(world, &old.before);
        }
        match prepare(world, ledger, &spec, area) {
            Ok(edit) => {
                let before = apply(world, &edit.edits);
                world.hold_saves = true;
                self.draft = Some(TerrainDraft { spec, prompt, provider, area, stats: edit.stats, before });
                Ok(edit.stats)
            }
            Err(errors) => {
                if let Some(mut old) = old
                    && let Ok(edit) = prepare(world, ledger, &old.spec, old.area)
                {
                    old.before = apply(world, &edit.edits);
                    self.draft = Some(old);
                } else {
                    world.hold_saves = false;
                }
                Err(errors)
            }
        }
    }

    /// Keeps the preview, recording it in the change log. Returns its change id (for
    /// the shared undo history) and name.
    pub fn keep(&mut self, world: &mut VoxelWorld, ledger: &mut Ledger) -> Option<(u64, String)> {
        let draft = self.draft.take()?;
        world.hold_saves = false;
        let name = draft.spec.name.clone();
        let action = Action::Terrain { name: name.clone(), area: draft.area, spec: draft.spec, changed: draft.stats.changed };
        let id = ledger.record(Some(&draft.provider), Some(&draft.prompt), action);
        // Offline, the ledger keeps what the voxels were, to undo it.
        if !ledger.is_online() {
            let cells = draft
                .before
                .iter()
                .map(|b| Cell { p: b.p.to_array(), from: (b.edit, b.visible), to: (world.edit_at(b.p), world.get(b.p).unwrap_or(b.visible)) })
                .collect();
            ledger.note_cells(id, cells);
        }
        self.kept.push(Kept { id, before: draft.before });
        Some((id, name))
    }

    pub fn discard(&mut self, world: &mut VoxelWorld) {
        if let Some(draft) = self.draft.take() {
            revert(world, &draft.before);
            world.hold_saves = false;
        }
    }

    /// Online, the server committed our land change `reference` as `edits`, compiled
    /// against the ground as the server had it. Swaps the preview voxels shown for the
    /// server's; returns whether they differed (someone changed the ground meanwhile).
    pub fn settle(&mut self, world: &mut VoxelWorld, reference: u64, edits: &[(IVec3, VoxelId)]) -> bool {
        let Some(kept) = self.kept.iter_mut().find(|k| k.id == reference) else {
            apply(world, edits);
            return false;
        };
        let same = kept.before.len() == edits.len() && edits.iter().all(|(p, id)| world.get(*p) == Some(*id));
        if !same {
            revert(world, &kept.before);
            kept.before = apply(world, edits);
        }
        !same
    }

    /// Online, the server refused our land change `reference`: the ground goes back.
    pub fn refused(&mut self, world: &mut VoxelWorld, reference: u64) {
        if let Some(index) = self.kept.iter().position(|k| k.id == reference) {
            let kept = self.kept.remove(index);
            revert(world, &kept.before);
        }
    }

    /// Forgets the voxel snapshots kept for undo (after the world was rebuilt from the
    /// log, they no longer describe it).
    pub fn forget_history(&mut self) {
        self.kept.clear();
    }

    /// What the agent is told about the area: size, frame, a coarse height map, where
    /// the player is, and the preview it may be revising.
    pub fn context(&self, world: &VoxelWorld, ledger: &Ledger, player: Option<Vec3>, view: Option<Vec3>) -> Option<String> {
        let area = self.target()?;
        let size = area.size_m();
        let mut out = format!(
            "Selected area: {:.1} × {:.1} m (x 0..{:.1}, z 0..{:.1}). x runs along world east, z along world south.\n",
            size.x, size.y, size.x, size.y
        );
        let _ = writeln!(
            out,
            "{}. Changes reaching past the area into someone else's parcel are cut off there.",
            land_summary(ledger, area)
        );
        out.push_str(&sample(world, area, Vec2::ZERO, size, 16));
        if let Some(p) = player {
            let local = (p.xz() / VOXEL_SIZE - area.min().as_vec2()) * VOXEL_SIZE;
            let inside = local.cmpge(Vec2::ZERO).all() && local.cmplt(size).all();
            let _ = write!(
                out,
                "The player stands at x={:.1}, z={:.1} ({}), feet at {:+.1} m.",
                local.x,
                local.y,
                if inside { "inside the area" } else { "outside the area" },
                p.y - (crate::world::terrain::SEA_LEVEL + 1) as f32 * VOXEL_SIZE,
            );
        }
        if let Some(view) = view.map(|v| v.xz()).filter(|v| v.length() > 0.01) {
            let right = Vec2::new(-view.y, view.x);
            let _ = write!(out, " They look toward {}; their right is {}.", axis(view), axis(right));
        }
        out.push('\n');
        if let Some(draft) = &self.draft {
            out.push_str("\nThe terrain preview the player is looking at (revise it if they ask for changes; your new ops replace it):\n");
            out.push_str(&serde_json::to_string(&draft.spec).unwrap_or_default());
            out.push('\n');
        }
        Some(out)
    }
}

fn axis(v: Vec2) -> &'static str {
    if v.x.abs() > v.y.abs() {
        if v.x > 0.0 { "+x" } else { "-x" }
    } else if v.y > 0.0 {
        "+z"
    } else {
        "-z"
    }
}

/// Why a terrain change can't be previewed.
pub enum PreviewError {
    /// The spec breaks the rules or limits; the agent may be able to fix it.
    Invalid(Vec<String>),
    /// The land rules don't allow changing this area; no spec would help.
    Refused(String),
}

/// Compiles `spec` against the loaded ground and keeps the edits the land rules allow,
/// exactly as replaying the change log will.
fn prepare(world: &VoxelWorld, ledger: &Ledger, spec: &TerrainSpec, area: Area) -> Result<TerrainEdit, PreviewError> {
    let refused = ledger.check(&area_parcels(area)).err();
    if let Some(why) = refused {
        return Err(PreviewError::Refused(why));
    }
    let columns = Columns::read(area, |x, z, out| world.column(x, z, out)).ok_or_else(|| {
        PreviewError::Invalid(vec!["part of the area isn't loaded; the player needs to be closer".to_string()])
    })?;
    let mut edit =
        sculpt::compile(spec, area, world.terrain, columns, &ledger.rules.terrain).map_err(PreviewError::Invalid)?;
    let (_, edits) =
        ledger.land.authorize_terrain(&ledger.profile.id, area, edit.edits).map_err(PreviewError::Refused)?;
    edit.stats.changed = edits.len();
    edit.edits = edits;
    ledger
        .budgets
        .check(&terrain_footprint(&ledger.profile.id, edit.stats.changed), None, &ledger.rules.budgets)
        .map_err(PreviewError::Refused)?;
    ledger.check_rate(edit.stats.changed).map_err(PreviewError::Refused)?;
    Ok(edit)
}

/// Who may change the parcels under `area`, in a few words for the panel and the agent.
pub fn land_summary(ledger: &Ledger, area: Area) -> String {
    if ledger.land.mode == LandMode::Open {
        let spawn = area_parcels(area).into_iter().any(|p| ledger.land.in_spawn(p));
        return match (spawn, ledger.land.is_admin(&ledger.profile.id)) {
            (false, _) => "Open world: anyone can change this land".into(),
            (true, true) => "Part of this is the protected spawn (you're an admin)".into(),
            (true, false) => "Part of this is the protected spawn, which only admins change".into(),
        };
    }
    let (mut yours, mut free, mut spawn) = (0, 0, 0);
    let mut others: Vec<String> = Vec::new();
    for parcel in area_parcels(area) {
        match ledger.land.access(&ledger.profile.id, parcel) {
            Access::Yours | Access::Shared => yours += 1,
            Access::Free => free += 1,
            Access::Spawn => spawn += 1,
            Access::Taken { owner_name } => {
                if !others.contains(&owner_name) {
                    others.push(owner_name);
                }
            }
        }
    }
    let mut parts = Vec::new();
    if yours > 0 {
        parts.push(format!("{yours} yours"));
    }
    if free > 0 {
        parts.push(format!("{free} unclaimed"));
    }
    if spawn > 0 {
        parts.push(format!("{spawn} in the protected spawn"));
    }
    if !others.is_empty() {
        parts.push(format!("owned by {}", others.join(", ")));
    }
    let admin = if ledger.land.is_admin(&ledger.profile.id) { " (you're an admin)" } else { "" };
    format!("Parcels: {}{admin}", parts.join(", "))
}

fn apply(world: &mut VoxelWorld, edits: &[(IVec3, VoxelId)]) -> Vec<Before> {
    edits
        .iter()
        .filter_map(|&(p, id)| {
            let before = Before { p, edit: world.edit_at(p), visible: world.get(p)? };
            world.set(p, id).then_some(before)
        })
        .collect()
}

fn revert(world: &mut VoxelWorld, before: &[Before]) {
    for b in before.iter().rev() {
        world.restore(b.p, b.edit, b.visible);
    }
}

/// A height map of part of `area` (`from`, `size` in area-frame meters), at most
/// `cells` samples per side. Each cell: mean ground level, the surface at its center,
/// and whether trees stand in it.
pub fn sample(world: &VoxelWorld, area: Area, from: Vec2, size: Vec2, cells: usize) -> String {
    let step = ((size.max_element() / cells as f32) / VOXEL_SIZE).ceil().max(1.0) * VOXEL_SIZE;
    let (nx, nz) = ((size.x / step).ceil() as usize, (size.y / step).ceil() as usize);
    let mut column = vec![AIR; WORLD_HEIGHT as usize];
    let (mut lowest, mut highest, mut wet, mut total) = (f32::MAX, f32::MIN, 0, 0);
    let mut rows = Vec::new();
    for j in 0..nz {
        let mut row = Vec::new();
        for i in 0..nx {
            let lo = from + Vec2::new(i as f32, j as f32) * step;
            let (c0, c1) = (area.to_world(lo), area.to_world((lo + step).min(from + size)) - 1);
            let center = area.to_world(lo + step / 2.0);
            let (mut sum, mut count, mut trees) = (0.0, 0, false);
            for z in c0.y..=c1.y.max(c0.y) {
                for x in c0.x..=c1.x.max(c0.x) {
                    if !world.column(x, z, &mut column) {
                        continue;
                    }
                    let (g, _, _) = surface(&column);
                    sum += level_of(g);
                    count += 1;
                    trees |= column[g as usize + 1..].iter().take(4).any(|id| block::BARK.contains(id));
                }
            }
            if count == 0 || !world.column(center.x, center.y, &mut column) {
                row.push("   ?  ".to_string());
                continue;
            }
            let (g, w, top) = surface(&column);
            let mean = sum / count as f32;
            lowest = lowest.min(mean);
            highest = highest.max(mean);
            total += 1;
            let cell = if w > g {
                wet += 1;
                format!("{:+.1}w", level_of(w))
            } else {
                format!("{:+.1}{}", mean, surface_code(top))
            };
            row.push(format!("{cell}{:<1}", if trees { "T" } else { "" }));
        }
        rows.push(row);
    }
    let mut out = format!(
        "Height map: meters relative to sea level, one cell per {step:.2} m (cell centers shown). Codes: g grass, \
         d dirt, r rock, s sand, n snow, w water (the number is the water surface), T trees stand there.\n"
    );
    if total > 0 {
        let _ = writeln!(
            out,
            "Ground spans {lowest:+.1} to {highest:+.1} m; {} of {total} cells are water.",
            wet
        );
    }
    out.push_str("        ");
    for i in 0..nx {
        let _ = write!(out, "x{:<6.1}", from.x + (i as f32 + 0.5) * step);
    }
    out.push('\n');
    for (j, row) in rows.iter().enumerate() {
        let _ = write!(out, "z{:<6.1} ", from.y + (j as f32 + 0.5) * step);
        for cell in row {
            let _ = write!(out, "{cell:<7}");
        }
        out.push('\n');
    }
    out
}

fn surface_code(id: VoxelId) -> char {
    match id {
        id if block::GRASS.contains(&id) => 'g',
        id if block::DIRT.contains(&id) => 'd',
        id if block::STONE.contains(&id) => 'r',
        id if block::SAND.contains(&id) => 's',
        block::SNOW => 'n',
        _ => '?',
    }
}

/// G mode: drag on the ground to select an area; a click selects a small square.
pub fn select_area(
    buttons: Res<ButtonInput<MouseButton>>,
    windows: Query<&Window, With<PrimaryWindow>>,
    cameras: Query<(&Camera, &GlobalTransform), With<Camera3d>>,
    over_ui: Res<PointerOverUi>,
    world: Res<VoxelWorld>,
    chat: Res<Chat>,
    ledger: Res<Ledger>,
    mut landscape: ResMut<Landscape>,
    mut lock: ResMut<OrbitLock>,
) {
    if landscape.draft.is_some() || chat.pending.is_some() {
        landscape.selecting = false;
    }
    lock.0 = landscape.selecting;
    if !landscape.selecting {
        landscape.drag_from = None;
        return;
    }
    let cell = cursor_ray(&windows, &cameras, &over_ui).and_then(|(_, ray)| {
        // Through trees and plants to the ground, so the corner doesn't jump onto them.
        let hit = world.raycast_ground(ray, 60.0)?;
        Some(VoxelWorld::voxel_at(ray.get_point(hit.distance) - hit.normal * 0.01).xz())
    });
    if buttons.just_pressed(MouseButton::Left) {
        landscape.drag_from = cell;
    }
    let Some(from) = landscape.drag_from else {
        return;
    };
    let max = (ledger.rules.terrain.max_area_m / VOXEL_SIZE) as i32 - 1;
    if let Some(to) = cell {
        let to = from + (to - from).clamp(IVec2::splat(-max), IVec2::splat(max));
        landscape.area = Some(Area::from_corners(from, to));
    }
    if buttons.just_released(MouseButton::Left) {
        landscape.drag_from = None;
        landscape.selecting = false;
        // A click, not a drag: a small square around it.
        if landscape.area.is_some_and(|a| a.size().max_element() <= 2) {
            let half = (CLICK_AREA_M / VOXEL_SIZE / 2.0) as i32;
            landscape.area = Some(Area::from_corners(from - half, from + half - 1));
        }
    }
}

/// Gizmos for the area outline: thick, and drawn slightly in front of the ground so
/// grass bumps don't hide it.
#[derive(Default, Reflect, GizmoConfigGroup)]
pub struct AreaGizmos;

pub fn configure_area_gizmos(mut store: ResMut<GizmoConfigStore>) {
    let (config, _) = store.config_mut::<AreaGizmos>();
    config.line.width = 14.0;
    config.line.joints = GizmoLineJoint::Round(4);
    config.depth_bias = -0.3;
}

/// The outline's ground-hugging lines, rebuilt when the area changes and a few
/// times a second (the ground under a preview changes as it remeshes).
#[derive(Default)]
pub struct OutlineCache {
    area: Option<Area>,
    age: u32,
    border: Vec<Vec3>,
    posts: Vec<Vec3>,
    grid: Vec<Vec<Vec3>>,
}

/// Outlines the selected area (or the preview's) along the ground, with corner posts
/// and a faint grid across it.
pub fn draw_area(
    time: Res<Time>,
    landscape: Res<Landscape>,
    world: Res<VoxelWorld>,
    mut cache: Local<OutlineCache>,
    mut bold: Gizmos<AreaGizmos>,
    mut thin: Gizmos,
) {
    let Some(area) = landscape.target() else {
        cache.area = None;
        return;
    };
    let (r, g, b) = area_color(&landscape);
    if cache.area != Some(area) || cache.age >= 20 {
        *cache = outline(&world, area);
    }
    cache.age += 1;

    // Brighter than white on purpose: tonemapping would dull plain colors, and bloom
    // makes these glow.
    let pulse = 3.0 + 0.4 * (time.elapsed_secs() * 2.0).sin();
    let color = Color::LinearRgba(LinearRgba::rgb(r * pulse, g * pulse, b * pulse));
    bold.linestrip(cache.border.iter().copied(), color);
    for base in &cache.posts {
        bold.line(*base, *base + Vec3::Y * 2.0, color);
    }
    let faint = Color::LinearRgba(LinearRgba::new(r * 1.5, g * 1.5, b * 1.5, 0.6));
    for line in &cache.grid {
        thin.linestrip(line.iter().copied(), faint);
    }
}

/// Selected: warm yellow; dragging: white; preview: cyan.
fn area_color(landscape: &Landscape) -> (f32, f32, f32) {
    if landscape.draft.is_some() {
        (0.2, 0.9, 1.0)
    } else if landscape.drag_from.is_some() {
        (1.0, 1.0, 1.0)
    } else {
        (1.0, 0.72, 0.1)
    }
}

#[derive(Component)]
pub struct AreaHighlight;

/// What the highlight was built for; it's rebuilt when this changes.
#[derive(Default, PartialEq)]
pub struct HighlightKey {
    area: Option<Area>,
    preview: Option<(String, usize)>,
}

/// Tints the ground inside the area with a see-through overlay that hugs the terrain,
/// steps included.
pub fn highlight_area(
    mut commands: Commands,
    time: Res<Time>,
    landscape: Res<Landscape>,
    world: Res<VoxelWorld>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut built: Local<(HighlightKey, Option<Entity>, Option<Handle<StandardMaterial>>)>,
) {
    let key = HighlightKey {
        area: landscape.target(),
        preview: landscape.draft.as_ref().map(|d| (d.spec.name.clone(), d.stats.changed)),
    };
    // While dragging, rebuild at most a few times a second.
    let dragging = landscape.drag_from.is_some() && (time.elapsed_secs() * 8.0) as u32 % 2 == 1;
    if built.0 != key && !dragging {
        if let Some(e) = built.1.take() {
            commands.entity(e).despawn();
        }
        if let Some(area) = key.area {
            let material = built
                .2
                .get_or_insert_with(|| {
                    materials.add(StandardMaterial {
                        unlit: true,
                        alpha_mode: AlphaMode::Blend,
                        depth_bias: 200.0,
                        ..default()
                    })
                })
                .clone();
            let mesh = highlight_mesh(&world, area);
            built.1 = Some(
                commands
                    .spawn((AreaHighlight, Mesh3d(meshes.add(mesh)), MeshMaterial3d(material), NotShadowCaster, Transform::default()))
                    .id(),
            );
        }
        built.0 = key;
    }
    if let Some(mut material) = built.2.as_ref().and_then(|h| materials.get_mut(h)) {
        let (r, g, b) = area_color(&landscape);
        let alpha = 0.34 + 0.06 * (time.elapsed_secs() * 3.0).sin();
        material.base_color = Color::LinearRgba(LinearRgba::new(r * 1.4, g * 1.4, b * 1.4, alpha));
    }
}

fn highlight_mesh(world: &VoxelWorld, area: Area) -> Mesh {
    let (min, size) = (area.min(), area.size());
    let mut column = vec![AIR; WORLD_HEIGHT as usize];
    // Top of each column (ground or water), in meters, slightly above the surface.
    let tops: Vec<Option<f32>> = (0..size.x * size.y)
        .map(|i| {
            let c = min + IVec2::new(i % size.x, i / size.x);
            world.column(c.x, c.y, &mut column).then(|| {
                let (g, w, _) = surface(&column);
                (g.max(w) + 1) as f32 * VOXEL_SIZE + 0.02
            })
        })
        .collect();
    let top = |x: i32, z: i32| tops[(x + z * size.x) as usize];
    let (mut positions, mut normals, mut indices) = (Vec::<[f32; 3]>::new(), Vec::<[f32; 3]>::new(), Vec::<u32>::new());
    let mut quad = |corners: [Vec3; 4], normal: Vec3| {
        let base = positions.len() as u32;
        positions.extend(corners.map(|c| c.to_array()));
        normals.extend([normal.to_array(); 4]);
        indices.extend([base, base + 1, base + 2, base, base + 2, base + 3]);
    };
    for z in 0..size.y {
        for x in 0..size.x {
            let Some(h) = top(x, z) else { continue };
            let (x0, z0) = ((min.x + x) as f32 * VOXEL_SIZE, (min.y + z) as f32 * VOXEL_SIZE);
            let (x1, z1) = (x0 + VOXEL_SIZE, z0 + VOXEL_SIZE);
            quad([Vec3::new(x0, h, z0), Vec3::new(x0, h, z1), Vec3::new(x1, h, z1), Vec3::new(x1, h, z0)], Vec3::Y);
            // Steps to the +x and +z neighbors, so the tint has no gaps on slopes.
            if x + 1 < size.x && let Some(n) = top(x + 1, z) && n != h {
                let (lo, hi) = (h.min(n), h.max(n));
                quad([Vec3::new(x1, lo, z0), Vec3::new(x1, hi, z0), Vec3::new(x1, hi, z1), Vec3::new(x1, lo, z1)], Vec3::X);
            }
            if z + 1 < size.y && let Some(n) = top(x, z + 1) && n != h {
                let (lo, hi) = (h.min(n), h.max(n));
                quad([Vec3::new(x0, lo, z1), Vec3::new(x1, lo, z1), Vec3::new(x1, hi, z1), Vec3::new(x0, hi, z1)], Vec3::Z);
            }
        }
    }
    Mesh::new(PrimitiveTopology::TriangleList, RenderAssetUsages::RENDER_WORLD)
        .with_inserted_attribute(Mesh::ATTRIBUTE_POSITION, positions)
        .with_inserted_attribute(Mesh::ATTRIBUTE_NORMAL, normals)
        .with_inserted_indices(Indices::U32(indices))
}

/// The corners of the cells around an area's edge, every `stride` cells, going round
/// once and back to the start. Each corner of the area is in it exactly once: a corner
/// repeated or skipped would leave a zero-length or cut-off segment, and the outline's
/// rounded joints flicker on those.
pub fn perimeter(area: Area, stride: i32) -> Vec<IVec2> {
    let (min, size) = (area.min(), area.size());
    let corners = [min, min + IVec2::new(size.x, 0), min + size, min + IVec2::new(0, size.y)];
    let mut points = Vec::new();
    for i in 0..4 {
        let (a, b) = (corners[i], corners[(i + 1) % 4]);
        let steps = (b - a).abs().max_element();
        // Up to, not including, the next corner; the next side starts there.
        points.extend((0..steps).step_by(stride.max(1) as usize).map(|s| a + (b - a).signum() * s));
    }
    points.push(min);
    points
}

fn outline(world: &VoxelWorld, area: Area) -> OutlineCache {
    let (min, size) = (area.min(), area.size());
    let mut column = vec![AIR; WORLD_HEIGHT as usize];
    // Height of the corner where four columns meet: the highest of them.
    let mut height = |c: IVec2| {
        let mut top = None::<i32>;
        for q in [c, c - IVec2::X, c - IVec2::Y, c - IVec2::ONE] {
            if world.column(q.x, q.y, &mut column) {
                let (g, w, _) = surface(&column);
                top = Some(top.unwrap_or(0).max(g.max(w)));
            }
        }
        top.map(|y| Vec3::new(c.x as f32 * VOXEL_SIZE, (y + 1) as f32 * VOXEL_SIZE + 0.08, c.y as f32 * VOXEL_SIZE))
    };
    let stride = (2 * (size.x + size.y) / 300).max(1);
    let border = perimeter(area, stride).into_iter().filter_map(&mut height).collect();
    let mut line = |a: IVec2, b: IVec2, stride: i32| -> Vec<Vec3> {
        let steps = (b - a).abs().max_element();
        (0..=steps).step_by(stride as usize).filter_map(|s| height(a + (b - a).signum() * s)).collect()
    };

    // A grid line every meter (every 2 m on big areas), sampled every half meter.
    let spacing = if size.max_element() > 64 { 8 } else { 4 };
    let mut grid = Vec::new();
    for x in (spacing..size.x).step_by(spacing as usize) {
        grid.push(line(min + IVec2::new(x, 0), min + IVec2::new(x, size.y), 2));
    }
    for z in (spacing..size.y).step_by(spacing as usize) {
        grid.push(line(min + IVec2::new(0, z), min + IVec2::new(size.x, z), 2));
    }
    let corners = [min, min + IVec2::new(size.x, 0), min + size, min + IVec2::new(0, size.y)];
    let posts = corners.iter().filter_map(|c| height(*c)).collect();
    OutlineCache { area: Some(area), age: 0, border, posts, grid }
}

pub fn describe_stats(area: Area, s: &SculptStats) -> String {
    let size = area.size_m();
    let mut parts = vec![format!("{:.0}×{:.0} m", size.x, size.y), format!("{} voxels changed", s.changed)];
    if s.raised > 0.0 {
        parts.push(format!("up to +{:.2} m", s.raised));
    }
    if s.lowered > 0.0 {
        parts.push(format!("down to −{:.2} m", s.lowered));
    }
    if s.trees > 0 {
        parts.push(format!("{} trees", s.trees));
    }
    if s.water_columns > 0 {
        parts.push(format!("{:.0} m² of new water", s.water_columns as f32 * VOXEL_SIZE * VOXEL_SIZE));
    }
    parts.join(" · ")
}

/// P shows who owns the land around the hero.
#[derive(Resource, Default)]
pub struct ParcelOverlay {
    pub visible: bool,
}

/// How far around the hero the overlay reaches, in parcels.
const OVERLAY_RADIUS: i32 = 5;

/// The overlay's outlines, rebuilt when the hero crosses into another parcel or the
/// land changes.
#[derive(Default)]
pub struct ParcelOutlines {
    key: Option<(IVec2, u64)>,
    lines: Vec<(Access, Vec<Vec3>)>,
}

pub fn draw_parcels(
    overlay: Res<ParcelOverlay>,
    ledger: Res<Ledger>,
    world: Res<VoxelWorld>,
    players: Query<&Transform, With<crate::player::Player>>,
    mut cache: Local<ParcelOutlines>,
    mut bold: Gizmos<AreaGizmos>,
) {
    let Ok(hero) = players.single() else {
        return;
    };
    if !overlay.visible {
        cache.key = None;
        return;
    }
    let here = genverse_rules::land::parcel_of(VoxelWorld::voxel_at(hero.translation).xz());
    if cache.key != Some((here, ledger.version)) {
        cache.key = Some((here, ledger.version));
        cache.lines.clear();
        let mut column = vec![AIR; WORLD_HEIGHT as usize];
        let mut height = |c: IVec2| {
            world.column(c.x, c.y, &mut column).then(|| {
                let (g, w, _) = surface(&column);
                (g.max(w) + 1) as f32 * VOXEL_SIZE + 0.1
            })
        };
        for dz in -OVERLAY_RADIUS..=OVERLAY_RADIUS {
            for dx in -OVERLAY_RADIUS..=OVERLAY_RADIUS {
                let parcel = here + IVec2::new(dx, dz);
                let access = ledger.land.access(&ledger.profile.id, parcel);
                if access == Access::Free {
                    continue;
                }
                // Inset by a voxel so neighbors' outlines don't overlap.
                let (lo, hi) = (parcel * PARCEL + 1, parcel * PARCEL + PARCEL - 1);
                let corners = [lo, IVec2::new(hi.x, lo.y), hi, IVec2::new(lo.x, hi.y), lo];
                let mut points = Vec::new();
                for edge in corners.windows(2) {
                    let (a, b) = (edge[0], edge[1]);
                    for s in (0..(b - a).abs().max_element()).step_by(2) {
                        let c = a + (b - a).signum() * s;
                        if let Some(y) = height(c) {
                            points.push(Vec3::new(c.x as f32 + 0.5, 0.0, c.y as f32 + 0.5) * VOXEL_SIZE + Vec3::Y * y);
                        }
                    }
                }
                if let Some(&first) = points.first() {
                    points.push(first);
                }
                cache.lines.push((access, points));
            }
        }
    }
    for (access, points) in &cache.lines {
        let (r, g, b) = match access {
            Access::Yours => (0.3, 1.0, 0.4),
            Access::Shared => (0.2, 0.9, 0.9),
            Access::Spawn => (0.45, 0.6, 1.0),
            Access::Taken { .. } => (1.0, 0.35, 0.3),
            Access::Free => continue,
        };
        bold.linestrip(points.iter().copied(), Color::LinearRgba(LinearRgba::rgb(r * 2.0, g * 2.0, b * 2.0)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn perimeter_has_each_corner_once() {
        // Sides of 10 and 7 cells, sampled every 4: neither divides evenly.
        let area = Area::from_corners(IVec2::new(0, 0), IVec2::new(9, 6));
        let points = perimeter(area, 4);
        assert_eq!(points.first(), points.last());
        assert!(points.windows(2).all(|w| w[0] != w[1]), "no zero-length segments");
        let (min, size) = (area.min(), area.size());
        for corner in [min, min + IVec2::new(size.x, 0), min + size, min + IVec2::new(0, size.y)] {
            assert_eq!(points[..points.len() - 1].iter().filter(|p| **p == corner).count(), 1, "{corner}");
        }
    }
}
