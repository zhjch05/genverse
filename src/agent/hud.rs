//! What games put in front of the player (`genverse_rules::logic::ui`): the HUD (the
//! UI trees games pin to the screen, their values and bars live) at the bottom left, and
//! the panel a game opened for them (dialogue, a menu, a shop: any tree), whose buttons
//! go back to the game as [`Intent::Choose`]. Offline, it also draws what games did to the world:
//! their builds, and their land changes worked out on the ground as it is here.
//!
//! Script it: `GENVERSE_PLAY="use; choose buy:0; close"` picks in the open panel.

use bevy::prelude::*;
use bevy_egui::{EguiContexts, egui};
use genverse_rules::{
    log::ground_columns,
    logic::{Intent, Ui, WorldChange},
    sculpt,
};

use super::{
    Chat,
    creations::Creations,
    game::{Play, report},
    theme,
};
use crate::world::{VoxelWorld, ledger::Ledger};

/// What the local player's screen shows of the games, and the panel they have open.
pub fn game_hud(mut contexts: EguiContexts, time: Res<Time>, mut ledger: ResMut<Ledger>, mut play: ResMut<Play>, mut chat: ResMut<Chat>) -> Result {
    let ctx = contexts.ctx_mut()?;
    let p = theme::palette(ctx);
    let covered = chat.covered * ctx.viewport_rect().width();
    let me = ledger.profile.id.clone();
    let state = &ledger.logic.state;
    let live = Live { state, me: &me, logic: &ledger.logic };

    let pinned: Vec<(String, Ui)> = state.hud(&me).map(|(id, w)| (id.to_string(), w.clone())).collect();
    if !pinned.is_empty() {
        egui::Area::new("game_hud".into())
            .anchor(egui::Align2::LEFT_BOTTOM, egui::vec2(16.0, -170.0))
            .interactable(false)
            .show(ctx, |ui| {
                egui::Frame::new()
                    .fill(p.overlay)
                    .stroke(egui::Stroke::new(1.0, p.line))
                    .corner_radius(theme::ROUND + 1)
                    .inner_margin(egui::Margin::symmetric(12, 8))
                    .show(ui, |ui| {
                        ui.set_width(220.0);
                        for (_, node) in &pinned {
                            draw(ui, node, &live, p);
                        }
                    });
            });
    }

    let Some(panel) = state.panel(&me).cloned() else { return Ok(()) };
    let mut pick: Option<Option<String>> = None;
    theme::centered(egui::Window::new(&panel.spec.title), ctx, covered / 2.0)
        .id(egui::Id::new("game_panel"))
        .collapsible(false)
        .default_width(360.0)
        .show(ctx, |ui| {
            if let Some(id) = draw(ui, &panel.spec.body, &live, p) {
                pick = Some(Some(id));
            }
            ui.add_space(6.0);
            if ui.button("Close").clicked() {
                pick = Some(None);
            }
        });
    if let Some(choice) = pick {
        let result = ledger.act(Intent::Choose { choice });
        report(result, &ledger, &mut play, &mut chat, time.elapsed_secs());
    }
    Ok(())
}

/// What a tree's live values read: the local player's stats and items.
struct Live<'a> {
    state: &'a genverse_rules::logic::GameState,
    logic: &'a genverse_rules::logic::Logic,
    me: &'a str,
}

/// Draws a game's UI tree; returns the id of a button pressed.
fn draw(ui: &mut egui::Ui, node: &Ui, live: &Live, p: &theme::Palette) -> Option<String> {
    let line = |ui: &mut egui::Ui, label: &str, value: String| {
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new(label).color(p.text));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(egui::RichText::new(value).strong().color(p.strong));
            });
        });
    };
    match node {
        Ui::Text { text, style } => {
            let text = egui::RichText::new(text);
            let text = match style.as_deref() {
                Some("title") => text.strong().size(17.0).color(p.strong),
                Some("small") => text.small().color(p.text),
                Some("muted") => text.color(p.muted),
                _ => text.color(p.text),
            };
            ui.label(text);
        }
        Ui::Value { label, stat, item } => {
            let value = match (stat, item) {
                (Some(stat), _) => number(live.state.stat(live.me, stat).map_or(0.0, |s| s.value)),
                (None, Some(item)) => live.state.items(live.me, item).to_string(),
                _ => String::new(),
            };
            line(ui, label, value);
        }
        Ui::Bar { label, stat, color } => {
            let s = live.state.stat(live.me, stat);
            let (value, max) = s.map_or((0.0, 1.0), |s| (s.value, s.max.unwrap_or(s.value.max(1.0))));
            if !label.is_empty() {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(label).color(p.text));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(egui::RichText::new(format!("{} / {}", number(value), number(max))).small().color(p.muted));
                    });
                });
            }
            bar(ui, (value / max) as f32, color.as_deref().and_then(hex).unwrap_or(p.primary));
        }
        Ui::Item { item } => {
            let title = super::game::item_title(live.logic, item);
            line(ui, &title, live.state.items(live.me, item).to_string());
        }
        Ui::Button { id, label, detail } => {
            let mut pressed = None;
            ui.horizontal(|ui| {
                if ui.add(egui::Button::new(label).min_size(egui::vec2(180.0, 0.0))).clicked() {
                    pressed = Some(id.clone());
                }
                if !detail.is_empty() {
                    ui.label(egui::RichText::new(detail).small().color(p.muted));
                }
            });
            return pressed;
        }
        Ui::Row { items } => {
            return ui.horizontal_wrapped(|ui| items.iter().fold(None, |pressed, n| draw(ui, n, live, p).or(pressed))).inner;
        }
        Ui::Column { items } => {
            return ui.vertical(|ui| items.iter().fold(None, |pressed, n| draw(ui, n, live, p).or(pressed))).inner;
        }
        Ui::Space => {
            ui.add_space(8.0);
        }
    }
    None
}

fn number(v: f64) -> String {
    if v.fract().abs() < 1e-9 { format!("{v:.0}") } else { format!("{v:.1}") }
}

fn hex(text: &str) -> Option<egui::Color32> {
    let t = text.trim_start_matches('#');
    let v = u32::from_str_radix(t, 16).ok().filter(|_| t.len() == 6)?;
    Some(egui::Color32::from_rgb((v >> 16) as u8, (v >> 8) as u8, v as u8))
}

fn bar(ui: &mut egui::Ui, share: f32, color: egui::Color32) {
    let p = theme::palette(ui.ctx());
    let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 10.0), egui::Sense::hover());
    ui.painter().rect_filled(rect, 4.0, p.input);
    let fill = egui::Rect::from_min_size(rect.min, egui::vec2(rect.width() * share.clamp(0.0, 1.0), rect.height()));
    ui.painter().rect_filled(fill, 4.0, color);
    ui.add_space(4.0);
}

/// Draws what games did to the world, offline (online, the server's changes bring it):
/// builds come and go, and land changes are worked out on the ground as it is here.
pub fn draw_game_world(mut commands: Commands, mut ledger: ResMut<Ledger>, mut world: ResMut<VoxelWorld>, mut creations: ResMut<Creations>) {
    if ledger.pending_world.is_empty() {
        return;
    }
    let pending = std::mem::take(&mut ledger.pending_world);
    let mut builds = false;
    for (installer, changes) in pending {
        for change in changes {
            match change {
                WorldChange::Build { .. } | WorldChange::Unbuild { .. } => builds = true,
                // The ledger holds portals; nothing to draw.
                WorldChange::Portal { .. } => {}
                WorldChange::Terrain { package, area, spec } => {
                    let ground = ground_columns(world.terrain, area, &world.edits_in_area(area));
                    match sculpt::compile(&spec, area, world.terrain, ground, &ledger.rules.terrain, &ledger.rules.content) {
                        Ok(edit) => {
                            if let Ok((_, edits)) = ledger.land.authorize_terrain(&installer, area, edit.edits) {
                                for (p, id) in edits {
                                    world.set(p, id);
                                }
                            }
                        }
                        Err(errors) => warn!("{package}'s land change: {}", errors.join("; ")),
                    }
                }
            }
        }
    }
    if builds {
        creations.resync(&mut commands, &ledger.builds);
    }
}
