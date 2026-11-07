//! Playing on a server: applying what it sends, every frame.
//!
//! - **Welcome:** the server's log becomes the world (replayed in full).
//! - **Committed:** one change, applied on top. Someone else's builds and land appear;
//!   our own were already shown, except that our land change is swapped for the
//!   server's version of it (compiled against the ground as the server had it).
//!   Undos (reverts, rollbacks) rebuild the world from the log.
//! - **Rejected:** what we showed is taken back, with the server's reason.

use bevy::prelude::*;
use cubelark_net::{ClientEvent, ToClient};
use cubelark_rules::log::{Action, Change, replay};

use super::{Chat, Role, creations::Creations, landscape::Landscape};
use crate::world::{VoxelWorld, ledger::Ledger};

pub fn sync_online(
    mut commands: Commands,
    mut ledger: ResMut<Ledger>,
    mut world: ResMut<VoxelWorld>,
    mut creations: ResMut<Creations>,
    mut landscape: ResMut<Landscape>,
    mut chat: ResMut<Chat>,
) {
    let Some(online) = &ledger.online else {
        return;
    };
    let events = online.client.poll();
    for event in events {
        match event {
            ClientEvent::Closed(why) => {
                let addr = ledger.online.as_ref().map(|o| o.addr.clone()).unwrap_or_default();
                chat.say(Role::System, format!("⚠ Disconnected from {addr}: {why}"));
                if let Some(online) = &mut ledger.online {
                    online.closed = Some(why);
                }
            }
            ClientEvent::Message(ToClient::Challenge { .. }) => {}
            ClientEvent::Message(ToClient::Bye { why }) => chat.say(Role::System, format!("⚠ The server says: {why}")),
            ClientEvent::Message(ToClient::Welcome { log }) => {
                if log.seed != world.terrain.seed {
                    chat.say(Role::System, "⚠ This server's world has a different seed; can't join it.");
                    continue;
                }
                ledger.log = log;
                let full = replay(&ledger.log);
                world.replace_edits(full.edits.iter().map(|(p, id)| (*p, *id)));
                creations.resync(&mut commands, &full.builds);
                landscape.forget_history();
                ledger.adopt(&full);
                let addr = ledger.online.as_ref().map(|o| o.addr.clone()).unwrap_or_default();
                chat.say(
                    Role::System,
                    format!("Joined {addr}: {} changes, {} builds.", ledger.log.changes.len(), full.builds.len()),
                );
                if let Some(online) = &mut ledger.online {
                    online.state = Some(full);
                }
            }
            ClientEvent::Message(ToClient::Committed { change, request }) => {
                committed(&mut commands, &mut ledger, &mut world, &mut creations, &mut landscape, &mut chat, change, request);
            }
            ClientEvent::Message(ToClient::Rejected { request, why }) => {
                if let Some(online) = &mut ledger.online {
                    online.pending.remove(&request);
                }
                creations.retract(&mut commands, request);
                landscape.refused(&mut world, request);
                chat.say(Role::System, format!("⚠ The server refused that: {why}."));
            }
        }
    }
}

fn committed(
    commands: &mut Commands,
    ledger: &mut Ledger,
    world: &mut VoxelWorld,
    creations: &mut Creations,
    landscape: &mut Landscape,
    chat: &mut Chat,
    change: Change,
    request: Option<u64>,
) {
    let Some(mut online) = ledger.online.take() else {
        return;
    };
    let own = request.filter(|r| online.pending.remove(r));
    if let Some(reference) = own {
        online.committed.insert(reference, change.id);
    }
    ledger.log.changes.push(change.clone());
    let Some(state) = &mut online.state else {
        ledger.online = Some(online);
        return;
    };

    let incremental = !matches!(change.action, Action::Revert { .. } | Action::Rollback { .. });
    let applied = if incremental { state.apply(&change, true).map_err(|why| warn!("{why}")).ok() } else { None };
    match applied {
        Some(edits) => {
            match (&change.action, own) {
                (Action::Terrain { name, .. }, Some(reference)) => {
                    if landscape.settle(world, reference, &edits) {
                        chat.say(
                            Role::System,
                            format!("The ground changed under your preview, so \"{name}\" was redone on the ground as it is now."),
                        );
                    }
                }
                (_, Some(_)) => {}
                (Action::PlaceBuild(b) | Action::UpdateBuild(b), None) => creations.upsert(commands, b),
                (Action::RemoveBuild { build }, None) => creations.remove(commands, *build),
                (_, None) => {
                    for (p, id) in &edits {
                        world.set(*p, *id);
                    }
                }
            }
            if own.is_none() {
                chat.say(Role::System, format!("{} {}.", change.by.player_name, change.action.describe()));
            }
        }
        // An undo (or a change the local copy disagrees with): rebuild from the log.
        None => {
            *state = replay(&ledger.log);
            world.replace_edits(state.edits.iter().map(|(p, id)| (*p, *id)));
            creations.resync(commands, &state.builds);
            landscape.forget_history();
            if own.is_none() {
                chat.say(Role::System, format!("{} {}.", change.by.player_name, change.action.describe()));
            }
        }
    }
    ledger.adopt(state);
    ledger.online = Some(online);
}
