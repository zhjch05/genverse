//! Reshaping the land by describing it: select an area of ground (G, then drag), tell
//! the agent what it should become, and preview the result before keeping it.
//!
//! The agent answers with a [`TerrainSpec`] (a few operations, never voxels) and
//! [`sculpt::compile`] turns it into voxel edits against the current ground. A preview
//! is applied to the world for real and put back voxel by voxel if it's discarded.
//! Kept changes are logged as specs in `saves/terrain_changes.json`; the voxels
//! themselves are saved with the world's edits.

use std::fmt::Write;

use bevy::{
    asset::RenderAssetUsages,
    gizmos::config::GizmoLineJoint,
    light::NotShadowCaster,
    mesh::{Indices, PrimitiveTopology},
    prelude::*,
    window::PrimaryWindow,
};
use bevy_egui::input::EguiWantsInput;
use serde::{Deserialize, Serialize};

use cubelark_rules::sculpt::{self, Area, Columns, SculptStats, TerrainEdit, TerrainSpec, level_of, limits, surface};

use super::{Chat, cursor_ray};
use crate::{
    player::OrbitLock,
    voxel::{AIR, VoxelId},
    world::{
        VoxelWorld,
        terrain::{VOXEL_SIZE, WORLD_HEIGHT, block},
    },
};

const LOG_PATH: &str = "saves/terrain_changes.json";
/// A click without a drag selects a square this wide, in meters.
const CLICK_AREA_M: f32 = 6.0;

/// A kept terrain change, as saved: what was asked and the operations, not voxels.
#[derive(Clone, Serialize, Deserialize)]
pub struct SavedTerrainChange {
    pub id: u64,
    pub name: String,
    pub prompt: String,
    pub provider: String,
    pub area: Area,
    pub spec: TerrainSpec,
}

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

struct Kept {
    id: u64,
    name: String,
    before: Vec<Before>,
}

#[derive(Resource, Default)]
pub struct Landscape {
    /// G: left-drag on the ground selects an area instead of turning the camera.
    pub selecting: bool,
    pub area: Option<Area>,
    drag_from: Option<IVec2>,
    pub draft: Option<TerrainDraft>,
    /// Kept changes this session, for Undo.
    kept: Vec<Kept>,
    log: Vec<SavedTerrainChange>,
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
        spec: TerrainSpec,
        prompt: String,
        provider: String,
    ) -> Result<SculptStats, Vec<String>> {
        let area = self.target().ok_or_else(|| vec!["no area is selected".to_string()])?;
        let old = self.draft.take();
        if let Some(old) = &old {
            revert(world, &old.before);
        }
        match compile_in(world, &spec, area) {
            Ok(edit) => {
                let before = apply(world, &edit.edits);
                world.hold_saves = true;
                self.draft = Some(TerrainDraft { spec, prompt, provider, area, stats: edit.stats, before });
                Ok(edit.stats)
            }
            Err(errors) => {
                if let Some(mut old) = old
                    && let Ok(edit) = compile_in(world, &old.spec, old.area)
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

    /// Keeps the preview. Returns its id (for the shared undo history) and name.
    pub fn keep(&mut self, world: &mut VoxelWorld) -> Option<(u64, String)> {
        let draft = self.draft.take()?;
        world.hold_saves = false;
        let id = self.log.iter().map(|c| c.id + 1).max().unwrap_or(1);
        let name = draft.spec.name.clone();
        self.kept.push(Kept { id, name: name.clone(), before: draft.before });
        self.log.push(SavedTerrainChange {
            id,
            name: name.clone(),
            prompt: draft.prompt,
            provider: draft.provider,
            area: draft.area,
            spec: draft.spec,
        });
        self.save();
        Some((id, name))
    }

    pub fn discard(&mut self, world: &mut VoxelWorld) {
        if let Some(draft) = self.draft.take() {
            revert(world, &draft.before);
            world.hold_saves = false;
        }
    }

    /// Puts the ground back how it was before kept change `id`.
    pub fn undo(&mut self, world: &mut VoxelWorld, id: u64) -> Option<String> {
        let index = self.kept.iter().position(|k| k.id == id)?;
        let kept = self.kept.remove(index);
        revert(world, &kept.before);
        self.log.retain(|c| c.id != id);
        self.save();
        Some(format!("Put the ground back how it was before \"{}\".", kept.name))
    }

    fn save(&self) {
        let result = std::fs::create_dir_all("saves")
            .and_then(|_| std::fs::write(LOG_PATH, serde_json::to_string_pretty(&self.log).unwrap_or_default()));
        if let Err(e) = result {
            error!("couldn't save {LOG_PATH}: {e}");
        }
    }

    /// What the agent is told about the area: size, frame, a coarse height map, where
    /// the player is, and the preview it may be revising.
    pub fn context(&self, world: &VoxelWorld, player: Option<Vec3>, view: Option<Vec3>) -> Option<String> {
        let area = self.target()?;
        let size = area.size_m();
        let mut out = format!(
            "Selected area: {:.1} × {:.1} m (x 0..{:.1}, z 0..{:.1}). x runs along world east, z along world south.\n",
            size.x, size.y, size.x, size.y
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

fn compile_in(world: &VoxelWorld, spec: &TerrainSpec, area: Area) -> Result<TerrainEdit, Vec<String>> {
    let columns = Columns::read(area, |x, z, out| world.column(x, z, out))
        .ok_or_else(|| vec!["part of the area isn't loaded; the player needs to be closer".to_string()])?;
    sculpt::compile(spec, area, world.terrain, columns)
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

pub fn load_terrain_log(mut landscape: ResMut<Landscape>) {
    let Ok(text) = std::fs::read_to_string(LOG_PATH) else {
        return;
    };
    match serde_json::from_str(&text) {
        Ok(log) => landscape.log = log,
        Err(e) => error!("couldn't read {LOG_PATH}: {e}"),
    }
}

/// G mode: drag on the ground to select an area; a click selects a small square.
pub fn select_area(
    buttons: Res<ButtonInput<MouseButton>>,
    windows: Query<&Window, With<PrimaryWindow>>,
    cameras: Query<(&Camera, &GlobalTransform), With<Camera3d>>,
    egui: Res<EguiWantsInput>,
    world: Res<VoxelWorld>,
    chat: Res<Chat>,
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
    let cell = cursor_ray(&windows, &cameras, &egui).and_then(|(_, ray)| {
        let hit = world.raycast(ray, 60.0)?;
        Some(VoxelWorld::voxel_at(ray.get_point(hit.distance) - hit.normal * 0.01).xz())
    });
    if buttons.just_pressed(MouseButton::Left) {
        landscape.drag_from = cell;
    }
    let Some(from) = landscape.drag_from else {
        return;
    };
    let max = (limits::MAX_AREA_M / VOXEL_SIZE) as i32 - 1;
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
    let pulse = 3.0 + 1.0 * (time.elapsed_secs() * 3.0).sin();
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
    let mut line = |a: IVec2, b: IVec2, stride: i32| -> Vec<Vec3> {
        let steps = (b - a).abs().max_element();
        (0..=steps).step_by(stride as usize).filter_map(|s| height(a + (b - a).signum() * s)).collect()
    };

    let corners = [min, min + IVec2::new(size.x, 0), min + size, min + IVec2::new(0, size.y)];
    let stride = (2 * (size.x + size.y) / 300).max(1);
    let mut border: Vec<Vec3> = (0..4).flat_map(|i| line(corners[i], corners[(i + 1) % 4], stride)).collect();
    if let Some(&first) = border.first() {
        border.push(first);
    }
    // A grid line every meter (every 2 m on big areas), sampled every half meter.
    let spacing = if size.max_element() > 64 { 8 } else { 4 };
    let mut grid = Vec::new();
    for x in (spacing..size.x).step_by(spacing as usize) {
        grid.push(line(min + IVec2::new(x, 0), min + IVec2::new(x, size.y), 2));
    }
    for z in (spacing..size.y).step_by(spacing as usize) {
        grid.push(line(min + IVec2::new(0, z), min + IVec2::new(size.x, z), 2));
    }
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
