//! Playing on a server: applying what it sends, every frame.
//!
//! - **Welcome:** the server's log becomes the world (replayed in full).
//! - **Committed:** one change, applied on top. Someone else's builds and land appear;
//!   our own were already shown, except that our land change is swapped for the
//!   server's version of it (compiled against the ground as the server had it).
//!   Undos (reverts, rollbacks) rebuild the world from the log.
//! - **Rejected:** what we showed is taken back, with the server's reason.
//! - **Fingerprints:** each land change comes with the server's fingerprints of the
//!   chunks it touched; chunks that disagree with ours are fetched and replaced.
//! - **Dropped:** we reconnect with growing pauses; the welcome that follows tells which
//!   of our changes in flight made it, and the rest are taken back.

use bevy::prelude::*;
use cubelark_net::{ClientEvent, ToClient, ToServer};
use cubelark_rules::{
    IVec3,
    log::{Action, Change, chunk_of, chunk_prints, replay, replay_from},
};

use super::{Chat, Role, creations::Creations, landscape::Landscape, presence::Peers};
use crate::world::{VoxelWorld, ledger::Ledger};

/// Joins the server at `addr`; returns what to tell the player.
pub fn join(ledger: &mut Ledger, addr: &str) -> String {
    if ledger.is_online() {
        return "Already online; /leave first.".into();
    }
    match ledger.go_online(addr) {
        Ok(()) => format!("Joining {addr}…"),
        Err(e) => format!("⚠ {e}"),
    }
}

/// Opens this world to others: starts a server in the game on the world's own save
/// (so changes made together are kept with it) and joins it.
pub fn host(ledger: &mut Ledger) -> String {
    if ledger.is_online() {
        return "Already online; only your local world can be hosted.".into();
    }
    let config = cubelark_server::ServerConfig {
        data_dir: "saves".into(),
        port: cubelark_net::DEFAULT_PORT,
        seed: ledger.log.seed,
    };
    let running = match cubelark_server::start(config) {
        Ok(running) => running,
        Err(e) => return format!("⚠ Couldn't host: {e}"),
    };
    ledger.hosting = Some((running.addr, running.players));
    let port = running.addr.port();
    if let Err(e) = ledger.go_online(&format!("127.0.0.1:{port}")) {
        return format!("⚠ {e}");
    }
    let lan = lan_ip().map_or(format!("this computer's address, port {port}"), |ip| format!("{ip}:{port}"));
    format!("Hosting this world until you quit. Others on your network join with {lan}.")
}

/// Leaves the server for the local world.
pub fn leave(
    commands: &mut Commands,
    ledger: &mut Ledger,
    world: &mut VoxelWorld,
    creations: &mut Creations,
    landscape: &mut Landscape,
) -> String {
    if !ledger.is_online() {
        return "You're in your local world.".into();
    }
    if ledger.hosting.is_some() {
        return "You're hosting this world; it closes when you quit.".into();
    }
    let full = ledger.go_offline();
    world.replace_edits(full.edits.iter().map(|(p, id)| (*p, *id)));
    creations.resync(commands, &full.builds);
    landscape.forget_history();
    "Back in your local world.".into()
}

/// This computer's address on the local network: the one a route to the internet would
/// use (connecting a UDP socket sends nothing).
fn lan_ip() -> Option<std::net::IpAddr> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    Some(socket.local_addr().ok()?.ip())
}

/// Seconds before reconnect attempt `n` (0-based): 2, 4, 8, … up to 30.
fn backoff(n: u32) -> f32 {
    (2.0 * 2f32.powi(n.min(4) as i32)).min(30.0)
}

pub fn sync_online(
    mut commands: Commands,
    time: Res<Time>,
    mut retry_at: Local<Option<f32>>,
    mut ledger: ResMut<Ledger>,
    mut world: ResMut<VoxelWorld>,
    mut creations: ResMut<Creations>,
    mut landscape: ResMut<Landscape>,
    mut chat: ResMut<Chat>,
    mut peers: ResMut<Peers>,
    assets: Res<AssetServer>,
) {
    let Some(online) = &ledger.online else {
        *retry_at = None;
        peers.clear(&mut commands);
        return;
    };
    // Reconnect when it's time.
    if online.closed.is_some()
        && let Some(at) = *retry_at
        && time.elapsed_secs() >= at
    {
        match ledger.reconnect() {
            Ok(()) => *retry_at = None,
            Err(e) => {
                let online = ledger.online.as_mut().expect("checked above");
                online.attempts += 1;
                *retry_at = Some(time.elapsed_secs() + backoff(online.attempts));
                info!("reconnect failed: {e}");
            }
        }
        return;
    }
    let events = online.client.poll();
    for event in events {
        match event {
            ClientEvent::Closed(why) => {
                let Some(online) = &mut ledger.online else { continue };
                if online.closed.is_none() {
                    chat.say(Role::System, format!("⚠ Lost the connection to {} ({why}); reconnecting…", online.addr));
                }
                online.closed = Some(why);
                *retry_at = Some(time.elapsed_secs() + backoff(online.attempts));
                peers.clear(&mut commands);
            }
            ClientEvent::Refused(why) => {
                chat.say(Role::System, format!("⚠ Can't play on this server: {why}"));
                if let Some(online) = &mut ledger.online {
                    online.closed = Some(why);
                }
                *retry_at = None;
            }
            ClientEvent::Message(ToClient::Challenge { .. }) => {}
            ClientEvent::Message(ToClient::Bye { why }) => chat.say(Role::System, format!("⚠ The server says: {why}")),
            ClientEvent::Message(ToClient::Presence { player, name, presence }) => {
                peers.seen(&mut commands, &assets, player, name, presence);
            }
            ClientEvent::Message(ToClient::Gone { player }) => peers.gone(&mut commands, &player),
            ClientEvent::Message(ToClient::Chunks { chunks }) => {
                heal(&mut ledger, &mut world, chunks);
            }
            ClientEvent::Message(ToClient::Welcome { log, snapshot }) => {
                peers.clear(&mut commands);
                if log.seed != world.terrain.seed {
                    chat.say(Role::System, "⚠ This server's world has a different seed; can't join it.");
                    continue;
                }
                ledger.log = log;
                // After a reconnect: our changes in flight either made it into the log
                // (find them by signature) or were lost with the connection.
                let l = &mut *ledger;
                if let Some(online) = &mut l.online {
                    online.attempts = 0;
                    let pending: Vec<(u64, String)> = online.pending.drain().collect();
                    let mut lost = 0;
                    for (request, sig) in pending {
                        match l.log.changes.iter().find(|c| c.sig.as_deref() == Some(sig.as_str())) {
                            Some(change) => {
                                online.committed.insert(request, change.id);
                            }
                            None => {
                                creations.retract(&mut commands, request);
                                landscape.refused(&mut world, request);
                                lost += 1;
                            }
                        }
                    }
                    if lost > 0 {
                        chat.say(Role::System, format!("⚠ {lost} change(s) didn't reach the server before the connection dropped."));
                    }
                }
                let full = replay_from(&ledger.log, snapshot.as_ref());
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
            ClientEvent::Message(ToClient::Committed { change, request, prints }) => {
                committed(&mut commands, &mut ledger, &mut world, &mut creations, &mut landscape, &mut chat, change, request);
                check_prints(&ledger, prints);
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
    let own = request.filter(|r| online.pending.remove(r).is_some());
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
                    // `CUBELARK_DESYNC=1` drops some of others' land edits, to watch
                    // fingerprints catch it and the chunks heal.
                    let drift = std::env::var("CUBELARK_DESYNC").is_ok();
                    for (i, (p, id)) in edits.iter().enumerate() {
                        if drift && i % 7 == 0 {
                            state.edits.remove(p);
                            continue;
                        }
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

/// Compares the server's fingerprints of the chunks a change touched with ours, and
/// asks for the chunks that differ.
fn check_prints(ledger: &Ledger, prints: Vec<([i32; 3], u64)>) {
    let Some(online) = &ledger.online else { return };
    let Some(state) = &online.state else { return };
    if prints.is_empty() {
        return;
    }
    let chunks: Vec<IVec3> = prints.iter().map(|(c, _)| IVec3::from_array(*c)).collect();
    let ours = chunk_prints(&state.edits, &chunks);
    let differ: Vec<[i32; 3]> =
        prints.iter().filter(|print| !ours.contains(print)).map(|(c, _)| *c).collect();
    if !differ.is_empty() {
        warn!("{} chunk(s) differ from the server's; fetching them", differ.len());
        online.client.send(ToServer::FetchChunks { chunks: differ });
    }
}

/// Replaces our edits in `chunks` with the server's, and regenerates those chunks.
fn heal(ledger: &mut Ledger, world: &mut VoxelWorld, chunks: Vec<([i32; 3], Vec<[i32; 4]>)>) {
    let Some(state) = ledger.online.as_mut().and_then(|o| o.state.as_mut()) else { return };
    for (chunk, edits) in &chunks {
        let chunk = IVec3::from_array(*chunk);
        state.edits.retain(|p, _| chunk_of(*p) != chunk);
        state.edits.extend(edits.iter().map(|[x, y, z, id]| (IVec3::new(*x, *y, *z), *id as u8)));
    }
    world.replace_edits(state.edits.iter().map(|(p, id)| (*p, *id)));
    info!("healed {} chunk(s) from the server", chunks.len());
}
