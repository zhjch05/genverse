//! Minecraft-style block editing: break, place, and pick terrain voxels with a hotbar.
//!
//! Press B (or a number key) for block mode. Clicks are press-and-release without
//! dragging the camera: left breaks, right places against the face you point at,
//! middle picks that block into the hotbar. Edits are limited to the hero's reach.

use bevy::{prelude::*, window::PrimaryWindow};

use super::{VoxelWorld, terrain::{VOXEL_SIZE, block}};
use crate::{
    player::{self, InputBlocked, Player},
    voxel::{AIR, VoxelId},
};

/// How far from the hero's head blocks can be edited, in meters.
const REACH: f32 = 6.0;

pub struct BlockEditPlugin;

impl Plugin for BlockEditPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<BlockMode>()
            .add_systems(Startup, spawn_hotbar)
            .add_systems(Update, (block_keys, aim, edit_on_click, draw_target, update_hotbar).chain());
    }
}

struct Slot {
    name: &'static str,
    /// What placing puts down.
    place: VoxelId,
    /// Block ids that count as this slot when picking.
    matches: &'static [VoxelId],
}

const SLOTS: [Slot; 9] = [
    Slot { name: "Grass", place: block::GRASS[1], matches: &block::GRASS },
    Slot { name: "Dirt", place: block::DIRT[0], matches: &block::DIRT },
    Slot { name: "Stone", place: block::STONE[1], matches: &block::STONE },
    Slot { name: "Sand", place: block::SAND[0], matches: &block::SAND },
    Slot { name: "Wood", place: block::BARK[0], matches: &block::BARK },
    Slot { name: "Leaves", place: block::LEAVES[1], matches: &block::LEAVES },
    Slot { name: "Snow", place: block::SNOW, matches: &[block::SNOW] },
    Slot { name: "Water", place: block::WATER, matches: &[block::WATER] },
    Slot { name: "Flowers", place: block::FLOWERS[0], matches: &block::FLOWERS },
];

#[derive(Resource)]
pub struct BlockMode {
    pub active: bool,
    pub slot: usize,
    /// Edge length of the edit brush in voxels: 1 (25 cm) or 2 (50 cm).
    pub brush: i32,
    target: Option<Target>,
}

impl Default for BlockMode {
    fn default() -> Self {
        Self { active: false, slot: 0, brush: 2, target: None }
    }
}

/// The voxel under the cursor and the face it was hit on.
#[derive(Clone, Copy)]
struct Target {
    cell: IVec3,
    normal: IVec3,
}

/// The `brush`-sized, grid-aligned cube of voxels containing `cell`.
fn brush_cells(cell: IVec3, brush: i32) -> impl Iterator<Item = IVec3> {
    let base = cell.div_euclid(IVec3::splat(brush)) * brush;
    (0..brush).flat_map(move |z| (0..brush).flat_map(move |y| (0..brush).map(move |x| base + IVec3::new(x, y, z))))
}

fn block_keys(keys: Res<ButtonInput<KeyCode>>, blocked: Res<InputBlocked>, mut mode: ResMut<BlockMode>) {
    if blocked.0 {
        return;
    }
    if keys.just_pressed(KeyCode::KeyB) {
        mode.active = !mode.active;
    }
    const DIGITS: [KeyCode; 9] = [
        KeyCode::Digit1,
        KeyCode::Digit2,
        KeyCode::Digit3,
        KeyCode::Digit4,
        KeyCode::Digit5,
        KeyCode::Digit6,
        KeyCode::Digit7,
        KeyCode::Digit8,
        KeyCode::Digit9,
    ];
    if let Some(slot) = DIGITS.iter().position(|k| keys.just_pressed(*k)) {
        mode.slot = slot;
        mode.active = true;
    }
    if !mode.active {
        return;
    }
    if keys.just_pressed(KeyCode::KeyX) {
        mode.brush = if mode.brush == 1 { 2 } else { 1 };
    }
    if keys.just_pressed(KeyCode::Escape) {
        mode.active = false;
    }
}

/// Finds the block under the cursor, within reach.
fn aim(
    mut mode: ResMut<BlockMode>,
    world: Res<VoxelWorld>,
    windows: Query<&Window, With<PrimaryWindow>>,
    cameras: Query<(&Camera, &GlobalTransform), With<Camera3d>>,
    players: Query<&Transform, With<Player>>,
) {
    mode.target = None;
    if !mode.active {
        return;
    }
    let (Ok(window), Ok((camera, camera_transform)), Ok(hero)) = (windows.single(), cameras.single(), players.single())
    else {
        return;
    };
    let Some(ray) = window.cursor_position().and_then(|c| camera.viewport_to_world(camera_transform, c).ok()) else {
        return;
    };
    let Some(hit) = world.raycast(ray, 40.0) else {
        return;
    };
    let point = ray.get_point(hit.distance);
    if point.distance(hero.translation + Vec3::Y * 1.1) > REACH {
        return;
    }
    let normal = hit.normal.round().as_ivec3();
    // Step back a hair along the normal-facing side so the point is inside the hit voxel.
    let cell = VoxelWorld::voxel_at(point - hit.normal * 0.01);
    mode.target = Some(Target { cell, normal });
}

fn edit_on_click(
    buttons: Res<ButtonInput<MouseButton>>,
    windows: Query<&Window, With<PrimaryWindow>>,
    mut mode: ResMut<BlockMode>,
    mut world: ResMut<VoxelWorld>,
    players: Query<&Transform, With<Player>>,
    mut pressed_at: Local<[Option<Vec2>; 3]>,
) {
    let Some(cursor) = windows.single().ok().and_then(Window::cursor_position) else {
        return;
    };
    let mut clicked = [false; 3];
    for (i, button) in [MouseButton::Left, MouseButton::Right, MouseButton::Middle].into_iter().enumerate() {
        if buttons.just_pressed(button) {
            pressed_at[i] = Some(cursor);
        }
        if buttons.just_released(button) {
            clicked[i] = pressed_at[i].take().is_some_and(|start| start.distance(cursor) <= 5.0);
        }
    }
    let [left, right, middle] = clicked;
    let (true, Some(target)) = (mode.active, mode.target) else {
        return;
    };

    if left {
        for cell in brush_cells(target.cell, mode.brush) {
            if world.get(cell).is_some_and(|id| id != AIR) {
                world.set(cell, AIR);
            }
        }
    } else if right {
        let Ok(hero) = players.single() else {
            return;
        };
        let (lo, hi) = player::bounds(hero.translation);
        let id = SLOTS[mode.slot].place;
        let cells: Vec<_> = brush_cells(target.cell + target.normal, mode.brush).collect();
        let inside_hero = |c: IVec3| {
            let (min, max) = (c.as_vec3() * VOXEL_SIZE, (c + IVec3::ONE).as_vec3() * VOXEL_SIZE);
            min.cmplt(hi).all() && lo.cmplt(max).all()
        };
        // Never entomb the hero.
        if super::terrain::is_solid(id) && cells.iter().any(|c| inside_hero(*c)) {
            return;
        }
        for cell in cells {
            // Only fill open space (air, water, plants), never replace solid ground.
            if world.get(cell).is_some_and(|existing| !super::terrain::is_solid(existing)) {
                world.set(cell, id);
            }
        }
    } else if middle
        && let Some(id) = world.get(target.cell)
        && let Some(slot) = SLOTS.iter().position(|s| s.matches.contains(&id))
    {
        mode.slot = slot;
    }
}

fn draw_target(mode: Res<BlockMode>, mut gizmos: Gizmos) {
    let Some(target) = mode.target else {
        return;
    };
    let size = mode.brush as f32 * VOXEL_SIZE;
    let base = target.cell.div_euclid(IVec3::splat(mode.brush)) * mode.brush;
    let center = base.as_vec3() * VOXEL_SIZE + Vec3::splat(size / 2.0);
    gizmos.cube(Transform::from_translation(center).with_scale(Vec3::splat(size + 0.01)), Color::srgba(1.0, 1.0, 1.0, 0.8));
}

// ---------------------------------------------------------------------------
// Hotbar UI (Bevy UI, so it works on the web too)
// ---------------------------------------------------------------------------

#[derive(Component)]
struct Hotbar;

#[derive(Component)]
struct HotbarSlot(usize);

#[derive(Component)]
struct HotbarLabel;

const SELECTED: Color = Color::srgb(1.0, 0.95, 0.75);
const UNSELECTED: Color = Color::srgba(0.0, 0.0, 0.0, 0.5);

fn spawn_hotbar(mut commands: Commands, world: Res<VoxelWorld>) {
    let palette = world.palette();
    commands
        .spawn((
            Hotbar,
            Node {
                position_type: PositionType::Absolute,
                bottom: px(44),
                width: percent(100),
                flex_direction: FlexDirection::Column,
                align_items: AlignItems::Center,
                row_gap: px(6),
                ..default()
            },
            Visibility::Hidden,
        ))
        .with_children(|root| {
            root.spawn((
                HotbarLabel,
                Text::new(""),
                TextFont { font_size: FontSize::Px(13.0), ..default() },
                TextColor(Color::srgba(1.0, 1.0, 1.0, 0.9)),
                Node { padding: UiRect::axes(px(10), px(4)), ..default() },
                BackgroundColor(Color::srgba(0.0, 0.0, 0.0, 0.55)),
            ));
            root.spawn(Node { column_gap: px(4), ..default() }).with_children(|row| {
                for (i, slot) in SLOTS.iter().enumerate() {
                    let color: Color = palette.entry(slot.place).color.into();
                    row.spawn((
                        HotbarSlot(i),
                        Node {
                            width: px(44),
                            height: px(44),
                            border: UiRect::all(px(3)),
                            justify_content: JustifyContent::FlexEnd,
                            align_items: AlignItems::FlexStart,
                            padding: UiRect::all(px(2)),
                            ..default()
                        },
                        BackgroundColor(color),
                        BorderColor::all(UNSELECTED),
                    ))
                    .with_child((
                        Text::new((i + 1).to_string()),
                        TextFont { font_size: FontSize::Px(11.0), ..default() },
                        TextColor(Color::srgba(0.0, 0.0, 0.0, 0.7)),
                    ));
                }
            });
        });
}

fn update_hotbar(
    mode: Res<BlockMode>,
    mut hotbar: Query<&mut Visibility, With<Hotbar>>,
    mut slots: Query<(&HotbarSlot, &mut BorderColor)>,
    mut label: Query<&mut Text, With<HotbarLabel>>,
) {
    if !mode.is_changed() {
        return;
    }
    for mut visibility in &mut hotbar {
        *visibility = if mode.active { Visibility::Inherited } else { Visibility::Hidden };
    }
    for (slot, mut border) in &mut slots {
        *border = BorderColor::all(if slot.0 == mode.slot { SELECTED } else { UNSELECTED });
    }
    for mut text in &mut label {
        let brush = if mode.brush == 1 { "25 cm" } else { "50 cm" };
        text.0 = format!(
            "{}, brush {brush}    Left: break   Right: place   Middle: pick   1-9: block   X: brush   B/Esc: done",
            SLOTS[mode.slot].name
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brush_aligns_to_its_grid() {
        let cells: Vec<_> = brush_cells(IVec3::new(3, -1, 4), 2).collect();
        assert_eq!(cells.len(), 8);
        assert!(cells.contains(&IVec3::new(2, -2, 4)) && cells.contains(&IVec3::new(3, -1, 5)));
        assert_eq!(brush_cells(IVec3::new(3, 1, 4), 1).collect::<Vec<_>>(), vec![IVec3::new(3, 1, 4)]);
    }
}
