//! The history panel (H): every change in the world, newest first, with who made it
//! and how, like a version history. Undo your own changes; admins can also put the
//! world back how it was after any change, or roll back everything a player did.

use bevy::prelude::*;
use bevy_egui::{EguiContexts, egui};
use cubewish_rules::log::Action;
use egui_phosphor::regular as icon;

use super::{
    Chat, Role,
    commands::{self, Context},
    creations::Creations,
    landscape::Landscape,
    presence::color_of,
};
use crate::{
    player::InputBlocked,
    world::{VoxelWorld, ledger::Ledger},
};

/// How many changes the panel lists at once.
const PAGE: usize = 200;

#[derive(Resource, Default)]
pub struct History {
    pub open: bool,
    /// Only this player's changes (by id), if set.
    filter: Option<String>,
}

enum Pick {
    Undo(u64),
    Restore(u64),
    Rollback(String, String),
}

/// H opens and closes it; `CUBEWISH_HISTORY=1` opens it at start.
pub fn toggle_history(
    keys: Res<ButtonInput<KeyCode>>,
    blocked: Res<InputBlocked>,
    mut history: ResMut<History>,
    mut started: Local<bool>,
) {
    if !*started {
        *started = true;
        history.open = std::env::var("CUBEWISH_HISTORY").is_ok();
    }
    if !blocked.0 && keys.just_pressed(KeyCode::KeyH) {
        history.open = !history.open;
    }
}

pub fn history_window(
    mut contexts: EguiContexts,
    mut history: ResMut<History>,
    mut commands: Commands,
    mut ledger: ResMut<Ledger>,
    mut world: ResMut<VoxelWorld>,
    mut creations: ResMut<Creations>,
    mut landscape: ResMut<Landscape>,
    mut chat: ResMut<Chat>,
    peers: Res<super::presence::Peers>,
) -> Result {
    if !history.open {
        return Ok(());
    }
    let ctx = contexts.ctx_mut()?;
    let me = ledger.profile.id.clone();
    let admin = ledger.land.is_admin(&me);
    let undone = ledger.log.undone();
    let mut pick = None;
    let mut open = true;

    // Everyone who has changed something, for the filter.
    let mut players: Vec<(String, String)> = Vec::new();
    for c in &ledger.log.changes {
        if !players.iter().any(|(id, _)| *id == c.by.player) {
            players.push((c.by.player.clone(), c.by.player_name.clone()));
        }
    }

    egui::Window::new(format!("{} History", icon::CLOCK_COUNTER_CLOCKWISE))
        .open(&mut open)
        .default_width(520.0)
        .default_height(420.0)
        .show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label(format!("{} changes", ledger.log.changes.len()));
                let shown = match &history.filter {
                    Some(id) => players.iter().find(|(p, _)| p == id).map_or("someone".into(), |(_, n)| n.clone()),
                    None => "Everyone".into(),
                };
                egui::ComboBox::from_id_salt("history_filter").selected_text(shown).show_ui(ui, |ui| {
                    ui.selectable_value(&mut history.filter, None, "Everyone");
                    for (id, name) in &players {
                        let name = if *id == me { format!("{name} (you)") } else { name.clone() };
                        ui.selectable_value(&mut history.filter, Some(id.clone()), name);
                    }
                });
            });
            ui.separator();
            egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                let changes = ledger.log.changes.iter().rev().filter(|c| history.filter.as_ref().is_none_or(|f| *f == c.by.player));
                for c in changes.take(PAGE) {
                    let gone = undone.contains(&c.id);
                    let [r, g, b, _] = color_of(&c.by.player, &peers, &me, chat.marker(&me)).to_srgba().to_u8_array();
                    ui.horizontal_wrapped(|ui| {
                        ui.label(egui::RichText::new(format!("{}", c.id)).monospace().weak());
                        let who = if c.by.player == me { "you".to_string() } else { c.by.player_name.clone() };
                        ui.label(egui::RichText::new(who).color(egui::Color32::from_rgb(r, g, b)).strong());
                        let what = egui::RichText::new(c.action.describe());
                        ui.label(if gone { what.strikethrough().weak() } else { what });
                        let how = ledger.describe(&c.by);
                        let how = how.split_once(", ").map_or(how.as_str(), |(_, when)| when).to_string();
                        let agent = c.by.agent.as_deref().map(|a| format!("with {a} · ")).unwrap_or_default();
                        ui.label(egui::RichText::new(format!("{agent}{how}")).small().weak());
                        let undo_kind = matches!(c.action, Action::Revert { .. } | Action::Rollback { .. });
                        if !gone && !undo_kind && ledger.log.may_revert(&me, c.id) && ui.small_button("Undo").clicked() {
                            pick = Some(Pick::Undo(c.id));
                        }
                        if admin {
                            if ui.small_button("Restore to here").on_hover_text("Undo everything after this change").clicked() {
                                pick = Some(Pick::Restore(c.id));
                            }
                            if c.by.player != me
                                && ui
                                    .small_button(format!("Roll back {}", c.by.player_name))
                                    .on_hover_text("Undo every change of theirs still in effect")
                                    .clicked()
                            {
                                pick = Some(Pick::Rollback(c.by.player.clone(), c.by.player_name.clone()));
                            }
                        }
                    });
                }
            });
        });
    history.open = open;

    let Some(pick) = pick else { return Ok(()) };
    if creations.draft.is_some() || landscape.draft.is_some() {
        chat.say(Role::System, "Keep or discard your draft first.");
        return Ok(());
    }
    let cx = &mut Context {
        commands: &mut commands,
        ledger: &mut ledger,
        world: &mut world,
        creations: &mut creations,
        landscape: &mut landscape,
    };
    let reply = match pick {
        Pick::Undo(id) => commands::revert(cx, id),
        Pick::Restore(id) => commands::restore(cx, id),
        Pick::Rollback(id, name) => {
            let reply = commands::rollback(cx, &id);
            reply.replace(&id.to_lowercase(), &name)
        }
    };
    chat.say(Role::System, reply);
    Ok(())
}
