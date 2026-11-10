//! The history panel (H): every change in the world, newest first, with who made it
//! and how, like a version history. Undo your own changes (admins: anyone's) while
//! nothing has changed there since, redo what you undid, and (admins) load saves.
//!
//! Undos and loads are changes like any other, applied on top of the world as it is:
//! [`follow_ledger`] makes the world's voxels and builds follow them, offline.

use bevy::prelude::*;
use bevy_egui::{EguiContexts, egui};
use egui_phosphor::regular as icon;
use serde_json::json;

use super::{
    Chat, Role,
    creations::Creations,
    landscape::Landscape,
    ops::{self, Context},
    presence::color_of,
};
use crate::{
    player::InputBlocked,
    voxel::VoxelId,
    world::{VoxelWorld, ledger::Ledger},
};
use genverse_rules::log::Action;

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
    Load(u64),
}

/// H opens and closes it; `GENVERSE_HISTORY=1` opens it at start.
pub fn toggle_history(
    binds: Res<crate::bindings::Bindings>,
    blocked: Res<InputBlocked>,
    mut history: ResMut<History>,
    mut started: Local<bool>,
) {
    if !*started {
        *started = true;
        history.open = std::env::var("GENVERSE_HISTORY").is_ok();
    }
    if !blocked.0 && binds.just_pressed("history") {
        history.open = !history.open;
    }
}

pub fn history_window(
    mut contexts: EguiContexts,
    mut history: ResMut<History>,
    mut ledger: ResMut<Ledger>,
    mut world: ResMut<VoxelWorld>,
    mut creations: ResMut<Creations>,
    mut landscape: ResMut<Landscape>,
    mut chat: ResMut<Chat>,
    mut travel: ResMut<super::worlds::Travel>,
    mut binds: ResMut<crate::bindings::Bindings>,
    mut graphics: ResMut<crate::graphics::Graphics>,
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
                        // Undoing an undo redoes what it undid.
                        let label = if matches!(c.action, Action::Undo { .. }) { "Redo" } else { "Undo" };
                        let may = ledger.undoable.get(&c.id).is_some_and(|r| r.by == me || admin);
                        if may && ui.small_button(label).clicked() {
                            pick = Some(Pick::Undo(c.id));
                        }
                        if admin
                            && matches!(c.action, Action::Save { .. })
                            && ui.small_button("Load").on_hover_text("Put the world back to this save, for everyone (it's saved first)").clicked()
                        {
                            pick = Some(Pick::Load(c.id));
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
        ledger: &mut ledger,
        world: &mut world,
        creations: &mut creations,
        landscape: &mut landscape,
        travel: &mut travel,
        at: [0.0; 3],
        agent: None,
        binds: &mut binds,
        graphics: &mut graphics,
    };
    let reply = ops::said(match pick {
        Pick::Undo(id) => ops::run("undo", json!({ "id": id }), cx),
        Pick::Load(id) => ops::run("load", json!({ "id": id }), cx),
    });
    chat.say(Role::System, reply);
    Ok(())
}

/// Offline, makes the world's voxels and builds follow what undos and loads did in the
/// ledger (it doesn't keep voxels), and keeps saves' states (they need the voxels).
/// Online, the server's changes do this as they arrive.
pub fn follow_ledger(
    mut commands: Commands,
    mut ledger: ResMut<Ledger>,
    mut world: ResMut<VoxelWorld>,
    mut creations: ResMut<Creations>,
    mut landscape: ResMut<Landscape>,
) {
    let follow = &ledger.follow;
    if follow.cells.is_empty() && !follow.builds && follow.edits.is_none() && follow.keep.is_empty() {
        return;
    }
    let follow = std::mem::take(&mut ledger.follow);
    if let Some(edits) = follow.edits {
        world.replace_edits(edits.iter().map(|[x, y, z, id]| (IVec3::new(*x, *y, *z), *id as VoxelId)));
        landscape.forget_history();
    }
    for cell in &follow.cells {
        world.restore(cell.pos(), cell.to.0, cell.to.1);
    }
    if follow.builds {
        creations.resync(&mut commands, &ledger.builds);
    }
    if !follow.keep.is_empty() {
        ledger.follow.keep = follow.keep;
        let edits = world.edit_list();
        ledger.keep_saves(&edits);
    }
}
