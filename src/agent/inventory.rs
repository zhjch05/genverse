//! The player's inventory, like Minecraft's: a hotbar of 9 slots at the bottom and 27
//! more behind it (I). What you carry is the game's (items, kept in the world's log by
//! the packages that give and take them); which slot holds what is yours alone, kept
//! between runs for each world.
//!
//! - **1-9** pick a hotbar slot: what's in it is in your hand, and its name shows.
//! - **Right-click** with something in hand: on the ground, puts it down (the game's
//!   `on_place`); on a piece in reach, uses it on that piece (`on_use`).
//! - **I** opens the inventory: click a stack to pick it up, click a slot to put it
//!   there (swapping with what's there). Esc or I closes it.

use bevy::{prelude::*, window::PrimaryWindow};
use bevy_egui::{EguiContexts, egui};

use genverse_rules::logic::{Intent, data, walk::position_at};

use super::{
    Chat,
    creations::Creations,
    cursor_ray,
    game::{self, Play},
    landscape::Landscape,
    prefs, theme,
};
use crate::{
    player::{InputBlocked, Player, PointerOverUi},
    world::{VoxelWorld, ledger::Ledger},
};

/// Hotbar slots, then the rest.
pub const HOTBAR: usize = 9;
pub const SLOTS: usize = 36;
/// How far a put-down item may be from the hero, in meters.
const PLACE_REACH: f32 = 8.0;
/// How close to a piece a right-click must land to use the item on it, in meters.
const ON_PIECE: f32 = 1.5;
/// How long the name of what's in hand shows after picking it.
const NAME_SECONDS: f32 = 2.0;
/// A slot's size, in the hotbar and as the inventory screen opens (resizing it scales
/// its slots between the two bounds).
const SLOT_SIZE: f32 = 46.0;
const SLOT_SIZES: std::ops::RangeInclusive<f32> = 32.0..=96.0;

#[derive(Resource)]
pub struct Inventory {
    /// Item names by slot: the hotbar first.
    slots: Vec<Option<String>>,
    /// The hotbar slot in hand.
    pub selected: usize,
    pub open: bool,
    /// The stack picked up in the inventory screen.
    cursor: Option<String>,
    /// Whose layout, in which world, it is: a different one loads its own.
    key: String,
    /// When the slot in hand last changed, to show its name.
    picked_at: Option<f32>,
}

impl Default for Inventory {
    fn default() -> Self {
        Self { slots: vec![None; SLOTS], selected: 0, open: false, cursor: None, key: String::new(), picked_at: None }
    }
}

impl Inventory {
    /// Puts hotbar slot `i` in hand.
    pub fn pick(&mut self, i: usize, now_s: f32) {
        self.selected = i.min(HOTBAR - 1);
        self.picked_at = Some(now_s);
    }

    /// What's in hand.
    pub fn held(&self) -> Option<&str> {
        self.slots[self.selected].as_deref()
    }

    fn save(&self) {
        prefs::set(&self.key, serde_json::json!(self.slots));
    }
}

/// Keeps the slots in line with what the player carries: new items go to the first
/// empty slot (hotbar first), items no longer carried leave theirs.
pub fn sync_inventory(ledger: Res<Ledger>, mut inventory: ResMut<Inventory>) {
    let key = format!("inventory/{}/{}", super::worlds::current_title(&ledger), ledger.profile.id);
    let inventory = &mut *inventory;
    if inventory.key != key {
        let saved: Option<Vec<Option<String>>> = prefs::get(&key).and_then(|v| serde_json::from_value(v).ok());
        inventory.slots = saved.filter(|s| s.len() == SLOTS).unwrap_or_else(|| vec![None; SLOTS]);
        inventory.key = key;
        inventory.cursor = None;
    }
    let carried: Vec<&str> = ledger.logic.state.inventory(&ledger.profile.id).map(|(item, _)| item).collect();
    let mut changed = false;
    for slot in &mut inventory.slots {
        if slot.as_deref().is_some_and(|item| !carried.contains(&item)) {
            *slot = None;
            changed = true;
        }
    }
    if inventory.cursor.as_deref().is_some_and(|item| !carried.contains(&item)) {
        inventory.cursor = None;
    }
    for item in carried {
        let placed = inventory.slots.iter().any(|s| s.as_deref() == Some(item)) || inventory.cursor.as_deref() == Some(item);
        if !placed && let Some(empty) = inventory.slots.iter_mut().find(|s| s.is_none()) {
            *empty = Some(item.to_string());
            changed = true;
        }
    }
    if changed {
        inventory.save();
    }
}

/// 1-9 pick a hotbar slot; I opens and closes the inventory (Esc closes it too).
pub fn inventory_keys(
    time: Res<Time>,
    keys: Res<ButtonInput<KeyCode>>,
    binds: Res<crate::bindings::Bindings>,
    blocked: Res<InputBlocked>,
    mut inventory: ResMut<Inventory>,
) {
    if blocked.0 {
        return;
    }
    let hands = ["hand_1", "hand_2", "hand_3", "hand_4", "hand_5", "hand_6", "hand_7", "hand_8", "hand_9"];
    if let Some(i) = hands.iter().position(|hand| binds.just_pressed(hand)) {
        inventory.selected = i;
        inventory.picked_at = Some(time.elapsed_secs());
    }
    if binds.just_pressed("inventory") || (inventory.open && keys.just_pressed(KeyCode::Escape)) {
        inventory.open = !inventory.open;
        // A stack in hand goes back where there's room.
        if !inventory.open && let Some(item) = inventory.cursor.take() {
            if let Some(empty) = inventory.slots.iter_mut().find(|s| s.is_none()) {
                *empty = Some(item);
            }
            inventory.save();
        }
    }
}

/// Right-click with something in hand: uses it on the piece clicked (in reach), or puts
/// it down on the ground clicked. Not while building or editing land, which have their
/// own right-click.
#[allow(clippy::too_many_arguments)]
pub fn use_held(
    time: Res<Time>,
    binds: Res<crate::bindings::Bindings>,
    windows: Query<&Window, With<PrimaryWindow>>,
    cameras: Query<(&Camera, &GlobalTransform), With<Camera3d>>,
    over_ui: Res<PointerOverUi>,
    world: Res<VoxelWorld>,
    heroes: Query<&Transform, With<Player>>,
    creations: Res<Creations>,
    landscape: Res<Landscape>,
    inventory: Res<Inventory>,
    mut ledger: ResMut<Ledger>,
    mut play: ResMut<Play>,
    mut chat: ResMut<Chat>,
    mut pressed: Local<Option<Vec2>>,
) {
    let Some(item) = inventory.held().map(str::to_string) else { return };
    let Some((cursor, ray)) = cursor_ray(&windows, &cameras, &over_ui) else { return };
    let busy = creations.draft.is_some() || creations.selected.is_some() || landscape.selecting || landscape.draft.is_some() || inventory.open;
    if binds.just_pressed("use_held") {
        *pressed = (!busy).then_some(cursor);
    }
    // A click, not a drag to look around.
    if !binds.just_released("use_held") || !pressed.take().is_some_and(|at| at.distance(cursor) <= 5.0) {
        return;
    }
    let now_s = time.elapsed_secs();
    let Some(hit) = world.raycast(ray, 64.0) else { return };
    let at = ray.get_point(hit.distance);
    if heroes.single().is_ok_and(|h| h.translation.distance(at) > PLACE_REACH) {
        play.toast("That's too far away; walk closer.", now_s);
        return;
    }
    // On a piece in reach: use it there.
    let on = play.near.filter(|id| {
        ledger.logic.state.get(*id).is_some_and(|e| Vec3::from_array(position_at(e, game::now_exact())).distance(at) <= ON_PIECE)
    });
    let result = match on {
        Some(target) => ledger.act(Intent::UseItem { item: item.clone(), target }),
        None if hit.normal.y < 0.5 => {
            play.toast("Put it on top of the ground.", now_s);
            return;
        }
        None => ledger.act(Intent::Place { item: item.clone(), position: at.to_array() }),
    };
    if let Ok(acted) = &result
        && acted.notes.is_empty()
        && on.is_none()
    {
        play.toast(format!("Put down {}.", game::item_title(&ledger.logic, &item)), now_s);
        return;
    }
    game::report(result, &ledger, &mut play, &mut chat, now_s);
}

/// An item's tile: the first color of its icon model if it names one, otherwise one
/// picked from its name.
fn tile_color(ledger: &Ledger, item: &str) -> egui::Color32 {
    let logic = &ledger.logic;
    let package = item.split(':').next().and_then(|ns| logic.installed.get(ns)).and_then(|i| logic.package(i));
    let hex = package.and_then(|p| {
        let model = data::icon(p, item)?;
        p.models.get(&model)?.palette.first().map(|c| c.hex.clone())
    });
    let parse = |hex: &str| {
        let h = hex.trim_start_matches('#');
        let byte = |i: usize| h.get(i..i + 2).and_then(|b| u8::from_str_radix(b, 16).ok());
        Some(egui::Color32::from_rgb(byte(0)?, byte(2)?, byte(4)?))
    };
    hex.and_then(|h| parse(&h)).unwrap_or_else(|| {
        let n = genverse_rules::log::fnv1a(item.as_bytes());
        egui::Color32::from_rgb(90 + (n % 120) as u8, 90 + ((n >> 8) % 120) as u8, 90 + ((n >> 16) % 120) as u8)
    })
}

/// "Torch" → "To"; "Iron sword" → "IS".
fn initials(title: &str) -> String {
    let words: Vec<&str> = title.split_whitespace().collect();
    match words.as_slice() {
        [one] => one.chars().take(2).collect(),
        many => many.iter().take(2).filter_map(|w| w.chars().next()).collect::<String>().to_uppercase(),
    }
}

/// One slot: the item's tile, its initials and count. Returns its response.
fn slot(ui: &mut egui::Ui, ledger: &Ledger, item: Option<&str>, chosen: bool, size: f32, p: &theme::Palette) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::click());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 6.0, p.overlay);
    let stroke = if chosen { egui::Stroke::new(2.5, p.strong) } else { egui::Stroke::new(1.0, p.line) };
    painter.rect_stroke(rect, 6.0, stroke, egui::StrokeKind::Inside);
    let Some(item) = item else { return response };
    let title = game::item_title(&ledger.logic, item);
    painter.rect_filled(rect.shrink(8.0), 4.0, tile_color(ledger, item));
    let font = |px: f32| egui::FontId::proportional(px * size / SLOT_SIZE);
    painter.text(rect.center(), egui::Align2::CENTER_CENTER, initials(&title), font(15.0), egui::Color32::WHITE);
    let count = ledger.logic.state.items(&ledger.profile.id, item);
    if count > 1 {
        painter.text(rect.right_bottom() - egui::vec2(4.0, 3.0), egui::Align2::RIGHT_BOTTOM, count.to_string(), font(13.0), p.strong);
    }
    response.on_hover_text(title)
}

/// The hotbar (with the day), the name of what's in hand when it changes, and the
/// inventory screen when it's open.
pub fn hotbar(mut contexts: EguiContexts, time: Res<Time>, chat: Res<Chat>, ledger: Res<Ledger>, mut inventory: ResMut<Inventory>) -> Result {
    let logic = &ledger.logic;
    let carries = logic.state.inventory(&ledger.profile.id).next().is_some();
    if logic.installed.is_empty() && !carries {
        return Ok(());
    }
    let ctx = contexts.ctx_mut()?;
    let p = theme::palette(ctx);
    let covered = chat.covered * ctx.viewport_rect().width();
    let now_s = time.elapsed_secs();
    let inventory = &mut *inventory;
    let mut clicked = None;
    egui::Area::new("hotbar".into()).anchor(egui::Align2::CENTER_BOTTOM, egui::vec2(-covered / 2.0, -96.0)).show(ctx, |ui| {
        ui.vertical_centered(|ui| {
            if let (Some(at), Some(item)) = (inventory.picked_at, inventory.held())
                && now_s - at < NAME_SECONDS
            {
                ui.label(egui::RichText::new(game::item_title(logic, item)).color(p.strong).strong());
            }
            let today = ledger.logic.time(&ledger.rules.logic, game::now());
            let season = genverse_rules::logic::clock::SEASONS[today.season];
            ui.label(egui::RichText::new(format!("Day {} · {season}", today.day_of_season + 1)).small().color(p.muted));
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                for i in 0..HOTBAR {
                    if slot(ui, &ledger, inventory.slots[i].as_deref(), i == inventory.selected, SLOT_SIZE, p).clicked() {
                        clicked = Some(i);
                    }
                }
            });
        });
    });
    if let Some(i) = clicked.filter(|_| !inventory.open) {
        inventory.selected = i;
        inventory.picked_at = Some(now_s);
    }
    if !inventory.open {
        return Ok(());
    }
    let mut open = true;
    // Wide enough for nine slots and their gaps, as it opens; resizing scales them.
    let width = HOTBAR as f32 * (SLOT_SIZE + 4.0);
    theme::centered(egui::Window::new("Inventory"), ctx, covered / 2.0)
        .open(&mut open)
        .collapsible(false)
        .default_width(width)
        .min_width(HOTBAR as f32 * (SLOT_SIZES.start() + 4.0))
        .max_width(HOTBAR as f32 * (SLOT_SIZES.end() + 4.0))
        .show(ctx, |ui| {
            let size = (ui.available_width() / HOTBAR as f32 - 4.0).clamp(*SLOT_SIZES.start(), *SLOT_SIZES.end());
            let mut row = |ui: &mut egui::Ui, from: usize| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    for i in from..from + HOTBAR {
                        if slot(ui, &ledger, inventory.slots[i].as_deref(), false, size, p).clicked() {
                            clicked = Some(i);
                        }
                    }
                });
            };
            for from in (HOTBAR..SLOTS).step_by(HOTBAR) {
                row(ui, from);
            }
            ui.add_space(6.0);
            row(ui, 0);
            if let Some(item) = &inventory.cursor {
                ui.label(egui::RichText::new(format!("Holding {}: click a slot to put it there", game::item_title(logic, item))).small().color(p.muted));
            } else {
                theme::note(ui, "Click a stack to pick it up, then a slot to put it there. 1-9 pick what's in your hand.");
            }
        });
    // Pick up, put down, or swap.
    if let Some(i) = clicked {
        std::mem::swap(&mut inventory.slots[i], &mut inventory.cursor);
        inventory.save();
    }
    if !open {
        inventory.open = false;
        if let Some(item) = inventory.cursor.take()
            && let Some(empty) = inventory.slots.iter_mut().find(|s| s.is_none())
        {
            *empty = Some(item);
        }
        inventory.save();
    }
    Ok(())
}
