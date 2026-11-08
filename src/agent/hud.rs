//! What games put in front of the player (`cubewish_rules::logic::entity`): the HUD
//! (bars and counters for their stats, lines of text) at the top of the view, and the
//! panel a game opened for them (dialogue, a menu, a shop), whose choices go back to the
//! game as [`Intent::Choose`]. Offline, it also draws what games did to the world:
//! their builds, and their land changes worked out on the ground as it is here.
//!
//! Script it: `CUBEWISH_PLAY="use; choose buy:0; close"` picks in the open panel.

use bevy::prelude::*;
use bevy_egui::{EguiContexts, egui};
use cubewish_rules::{
    log::ground_columns,
    logic::{Intent, Widget, WorldChange},
    sculpt,
};

use super::{
    Chat,
    creations::Creations,
    game::{Play, report},
    theme,
};
use crate::world::{VoxelWorld, ledger::Ledger};

/// The widgets on the local player's screen, and the panel they have open.
pub fn game_hud(mut contexts: EguiContexts, time: Res<Time>, mut ledger: ResMut<Ledger>, mut play: ResMut<Play>, mut chat: ResMut<Chat>) -> Result {
    let ctx = contexts.ctx_mut()?;
    let p = theme::palette(ctx);
    let covered = chat.covered * ctx.viewport_rect().width();
    let me = ledger.profile.id.clone();
    let state = &ledger.logic.state;

    let widgets: Vec<(String, Widget)> = state.hud(&me).map(|(id, w)| (id.to_string(), w.clone())).collect();
    if !widgets.is_empty() {
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
                        for (id, widget) in &widgets {
                            let stat = |name: &str| state.stat(&me, name);
                            match widget {
                                Widget::Bar { label, stat: name, color } => {
                                    let s = stat(name);
                                    let (value, max) = s.map_or((0.0, 1.0), |s| (s.value, s.max.unwrap_or(s.value.max(1.0))));
                                    ui.horizontal(|ui| {
                                        ui.label(egui::RichText::new(label).color(p.text));
                                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                            ui.label(egui::RichText::new(format!("{} / {}", number(value), number(max))).small().color(p.muted));
                                        });
                                    });
                                    bar(ui, (value / max) as f32, color.as_deref().and_then(hex).unwrap_or(p.primary), id);
                                }
                                Widget::Counter { label, stat: name } => {
                                    let value = stat(name).map_or(0.0, |s| s.value);
                                    ui.horizontal(|ui| {
                                        ui.label(egui::RichText::new(label).color(p.text));
                                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                            ui.label(egui::RichText::new(number(value)).strong().color(p.strong));
                                        });
                                    });
                                }
                                Widget::Items { label, item } => {
                                    let n = state.items(&me, item);
                                    ui.horizontal(|ui| {
                                        ui.label(egui::RichText::new(label).color(p.text));
                                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                            ui.label(egui::RichText::new(n.to_string()).strong().color(p.strong));
                                        });
                                    });
                                }
                                Widget::Text { text } => {
                                    ui.label(egui::RichText::new(text).color(p.text));
                                }
                            }
                        }
                    });
            });
    }

    let Some(panel) = state.panel(&me).cloned() else { return Ok(()) };
    let mut pick: Option<Option<String>> = None;
    egui::Window::new(&panel.spec.title)
        .id(egui::Id::new("game_panel"))
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, egui::vec2(-covered / 2.0, 0.0))
        .default_width(360.0)
        .show(ctx, |ui| {
            if !panel.spec.text.is_empty() {
                ui.label(egui::RichText::new(&panel.spec.text).color(p.text));
                ui.add_space(8.0);
            }
            for choice in &panel.spec.choices {
                ui.horizontal(|ui| {
                    if ui.add(egui::Button::new(&choice.label).min_size(egui::vec2(220.0, 0.0))).clicked() {
                        pick = Some(Some(choice.id.clone()));
                    }
                    if !choice.detail.is_empty() {
                        ui.label(egui::RichText::new(&choice.detail).small().color(p.muted));
                    }
                });
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

fn number(v: f64) -> String {
    if v.fract().abs() < 1e-9 { format!("{v:.0}") } else { format!("{v:.1}") }
}

fn hex(text: &str) -> Option<egui::Color32> {
    let t = text.trim_start_matches('#');
    let v = u32::from_str_radix(t, 16).ok().filter(|_| t.len() == 6)?;
    Some(egui::Color32::from_rgb((v >> 16) as u8, (v >> 8) as u8, v as u8))
}

fn bar(ui: &mut egui::Ui, share: f32, color: egui::Color32, _id: &str) {
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
                WorldChange::Terrain { package, area, spec } => {
                    let ground = ground_columns(world.terrain, area, &world.edits_in_area(area));
                    match sculpt::compile(&spec, area, world.terrain, ground, &ledger.rules.terrain) {
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
