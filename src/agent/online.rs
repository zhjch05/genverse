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
use genverse_net::{ClientEvent, ToClient, ToServer};
use genverse_rules::{
    IVec3,
    log::{Action, Change, chunk_of, chunk_prints, replay, replay_from},
    logic::Effect,
};

use super::{Chat, Role, creations::Creations, landscape::Landscape, ops::Reply, presence::Peers};
use crate::world::{VoxelWorld, ledger::Ledger};

/// Joins the server at `addr`; returns what to tell the player.
pub fn join(ledger: &mut Ledger, addr: &str) -> Reply {
    if ledger.is_online() {
        return Err("Already online; /leave first.".into());
    }
    match ledger.go_online(addr) {
        Ok(()) if genverse_net::internet::is_invite(addr) => Ok("Joining by invite… finding the host can take a few seconds.".into()),
        Ok(()) => Ok(format!("Joining {addr}…")),
        Err(e) => Err(format!("⚠ {e}")),
    }
}

/// Opens this world to others: starts a server in the game on the world's own save
/// (so changes made together are kept with it) and joins it. Others on the network
/// join at its address; anyone else, with its invite over the internet
/// ([`genverse_net::internet`]), while this game stays the server.
pub fn host(ledger: &mut Ledger) -> Reply {
    if ledger.is_online() {
        return Err("Already online; only your local world can be hosted.".into());
    }
    let config = genverse_server::ServerConfig {
        data_dir: crate::world::ledger::game_dir().into(),
        port: genverse_net::DEFAULT_PORT,
        seed: ledger.log.seed,
        flat: ledger.log.flat,
    };
    let running = match genverse_server::start(config) {
        Ok(running) => running,
        Err(e) => return Err(format!("⚠ Couldn't host: {e}")),
    };
    let port = running.addr.port();
    ledger.hosting = Some(running);
    if let Err(e) = ledger.go_online(&format!("127.0.0.1:{port}")) {
        return Err(format!("⚠ {e}"));
    }
    let lan = lan_ip().map_or(format!("this computer's address, port {port}"), |ip| format!("{ip}:{port}"));
    // The same key gives the same invite each time this player hosts.
    let internet = match genverse_net::internet::Host::start(port, ledger.derive_secret("internet host")) {
        Ok(door) => {
            info!("hosting over the internet with the invite {}", door.invite());
            let invite = " Friends anywhere join with your invite: copy it under Play together once it's ready.".to_string();
            ledger.internet = Some(door);
            invite
        }
        Err(e) => format!(" (Not open to the internet: {e}.)"),
    };
    Ok(format!("Hosting this world; Leave closes it. Others on your network join with {lan}.{internet}"))
}


/// Leaves the server for the local world; a hosted one closes on the way out.
pub fn leave(ledger: &mut Ledger, travel: &mut super::worlds::Travel) -> Reply {
    if !ledger.is_online() {
        return Err("You're in your local world.".into());
    }
    travel.stop_hosting = ledger.hosting.is_some();
    travel.next = Some(super::worlds::Trip::Local { game: crate::world::ledger::game_name(), world: crate::world::ledger::world_name() });
    Ok(String::new())
}

/// Where others on the network join the game this one hosts (`ip:port`), looked up
/// again every few seconds as networks change.
pub fn lan_address(port: u16) -> Option<String> {
    use std::{sync::Mutex, time::Instant};
    static CACHE: Mutex<Option<(Instant, Option<std::net::IpAddr>)>> = Mutex::new(None);
    let mut cache = CACHE.lock().ok()?;
    let ip = match *cache {
        Some((at, ip)) if at.elapsed().as_secs() < 5 => ip,
        _ => {
            let ip = lan_ip();
            *cache = Some((Instant::now(), ip));
            ip
        }
    };
    Some(format!("{}:{port}", ip?))
}

/// This computer's address on the local network: a private one on a real interface,
/// else the one a route to the internet would use (connecting a UDP socket sends
/// nothing; behind a VPN, that's the tunnel's).
fn lan_ip() -> Option<std::net::IpAddr> {
    if let Some(ip) = genverse_net::lan::local_ipv4() {
        return Some(ip.into());
    }
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
    (mut loading, mut play, mut heroes, mut worlds_window): (
        ResMut<super::loading::Loading>,
        ResMut<super::game::Play>,
        Query<(&mut Transform, &mut crate::player::Player)>,
        ResMut<super::worlds::WorldsWindow>,
    ),
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
                // Through a tunnel, the tunnel knows why better than the client does.
                let why = online.tunnel.as_ref().and_then(|t| t.error()).unwrap_or(why);
                if online.closed.is_none() {
                    chat.say(Role::System, format!("⚠ Lost the connection to {} ({why}); reconnecting…", crate::world::ledger::shown_addr(&online.addr)));
                }
                online.closed = Some(why);
                *retry_at = Some(time.elapsed_secs() + backoff(online.attempts));
                online.others.clear();
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
                if let Some(online) = &mut ledger.online {
                    let shown = if presence.name.trim().is_empty() { name.clone() } else { presence.name.trim().to_string() };
                    online.others.insert(player.clone(), (shown, presence.clone(), std::time::Instant::now()));
                }
                peers.seen(&mut commands, &assets, player, name, presence);
            }
            ClientEvent::Message(ToClient::Gone { player }) => {
                if let Some(online) = &mut ledger.online {
                    online.others.remove(&player);
                }
                peers.gone(&mut commands, &player);
            }
            ClientEvent::Message(ToClient::Chunks { chunks }) => {
                heal(&mut ledger, &mut world, chunks);
            }
            ClientEvent::Message(ToClient::Welcome { game, world: name, log, snapshot }) => {
                peers.clear(&mut commands);
                let Some(online) = &mut ledger.online else { continue };
                online.others.clear();
                // Worlds renamed while we were away are the same worlds.
                let renamed = |w: Option<String>| w.map(|w| game.current(&w).unwrap_or(w));
                online.world = renamed(online.world.take());
                online.shown = renamed(online.shown.take());
                // A reconnect lands in the game's first world: back to where we were.
                if let Some(wanted) = online.world.clone().filter(|w| *w != name && game.world(w).is_some()) {
                    online.game = Some(game);
                    online.client.send(genverse_net::ToServer::Enter { world: wanted });
                    continue;
                }
                let moved = online.shown.is_some() && online.shown.as_deref() != Some(name.as_str());
                online.game = Some(game);
                online.world = Some(name.clone());
                online.shown = Some(name.clone());
                online.attempts = 0;
                let from_portal = online.arriving_from.take();
                // Other ground, or another world: start over on it.
                let fresh = moved || log.terrain() != world.terrain;
                ledger.log = log;
                // After a reconnect: our changes in flight either made it into the log
                // (find them by signature) or were lost with the connection. In another
                // world, they're lost either way.
                let l = &mut *ledger;
                if let Some(online) = &mut l.online {
                    let pending: Vec<(u64, String)> = online.pending.drain().collect();
                    let mut lost = 0;
                    for (request, sig) in pending {
                        match l.log.changes.iter().find(|c| c.sig.as_deref() == Some(sig.as_str())).filter(|_| !moved) {
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
                if fresh {
                    world.reset(&mut commands, ledger.log.terrain(), full.edits.iter().map(|(p, id)| (*p, *id)));
                } else {
                    world.replace_edits(full.edits.iter().map(|(p, id)| (*p, *id)));
                }
                creations.resync(&mut commands, &full.builds);
                landscape.forget_history();
                ledger.adopt(&full);
                if fresh {
                    play.leave_world();
                    let spot = from_portal
                        .and_then(|from| super::worlds::arrival(&ledger, &from))
                        .unwrap_or_else(|| crate::player::spawn_point(ledger.log.terrain(), IVec2::ZERO));
                    if let Ok((mut transform, mut player)) = heroes.single_mut() {
                        transform.translation = spot;
                        player.arrive();
                    }
                    loading.restart(&name);
                }
                worlds_window.refresh();
                let addr = ledger.online.as_ref().map(|o| crate::world::ledger::shown_addr(&o.addr)).unwrap_or_default();
                chat.say(
                    Role::System,
                    format!("In {name} on {addr}: {} changes, {} builds.", ledger.log.changes.len(), full.builds.len()),
                );
                if let Some(online) = &mut ledger.online {
                    online.state = Some(full);
                }
            }
            ClientEvent::Message(ToClient::Game { game, request }) => {
                let Some(online) = &mut ledger.online else { continue };
                // The request is set only for whoever proposed the change.
                if request.is_some() && let Some(reply) = super::worlds::game_reply(game.changes.last().map(|c| &c.action)) {
                    chat.say(Role::System, reply);
                }
                // The world we're in renamed: we're still in it.
                let renamed = |w: Option<String>| w.map(|w| game.current(&w).unwrap_or(w));
                online.world = renamed(online.world.take());
                online.shown = renamed(online.shown.take());
                online.game = Some(game);
                worlds_window.refresh();
            }
            ClientEvent::Message(ToClient::Committed { change, request, prints, package }) => {
                if let Some(package) = package {
                    let hash = ledger.log.add_package(package);
                    let package = ledger.log.packages[&hash].clone();
                    if let Some(state) = ledger.online.as_mut().and_then(|o| o.state.as_mut()) {
                        state.logic.packages.insert(hash, package);
                    }
                }
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
    if let Action::Timers { effects, .. } = &change.action {
        for effect in effects {
            if let Effect::Notify { player, text } = effect
                && *player == ledger.profile.id
            {
                ledger.heard.push(text.clone());
            }
        }
    }
    let Some(state) = &mut online.state else {
        ledger.online = Some(online);
        return;
    };

    let applied = state.apply(&change, true).map_err(|why| warn!("{why}")).ok();
    match applied {
        Some(edits) => {
            match (&change.action, own) {
                // An undo: the voxels it set back, and builds it changed.
                (Action::Undo { .. }, _) => {
                    for cell in &state.last_cells {
                        world.restore(cell.pos(), cell.to.0, cell.to.1);
                    }
                    creations.resync(commands, &state.builds);
                }
                // A load: everything may differ.
                (Action::Load { .. }, _) => {
                    world.replace_edits(state.edits.iter().map(|(p, id)| (*p, *id)));
                    creations.resync(commands, &state.builds);
                    landscape.forget_history();
                }
                (Action::Terrain { name, .. }, Some(reference)) => {
                    if landscape.settle(world, reference, &edits) {
                        chat.say(
                            Role::System,
                            format!("The ground changed under your preview, so \"{name}\" was redone on the ground as it is now."),
                        );
                    }
                }
                // A game's builds and land changes: ours too, since games make them.
                (Action::Intent { .. } | Action::Timers { .. }, _) => {
                    for (p, id) in &edits {
                        world.set(*p, *id);
                    }
                    let built = state.last_world.as_ref().is_some_and(|(_, w)| {
                        w.iter().any(|c| matches!(c, genverse_rules::logic::WorldChange::Build { .. } | genverse_rules::logic::WorldChange::Unbuild { .. }))
                    });
                    if built {
                        creations.resync(commands, &state.builds);
                    }
                }
                (_, Some(_)) => {}
                // What someone else just built arrives the way one's own does.
                (Action::PlaceBuild(b), None) => creations.upsert_arriving(commands, b, None),
                (Action::UpdateBuild(b), None) => creations.upsert(commands, b),
                (Action::RemoveBuild { build }, None) => creations.remove(commands, *build),
                (_, None) => {
                    // `GENVERSE_DESYNC=1` drops some of others' land edits, to watch
                    // fingerprints catch it and the chunks heal.
                    let drift = std::env::var("GENVERSE_DESYNC").is_ok();
                    for (i, (p, id)) in edits.iter().enumerate() {
                        if drift && i % 7 == 0 {
                            state.edits.remove(p);
                            continue;
                        }
                        world.set(*p, *id);
                    }
                }
            }
            if own.is_none() && !matches!(change.action, Action::Intent { .. } | Action::Timers { .. }) {
                chat.say(Role::System, format!("{} {}.", change.by.player_name, change.action.describe()));
            }
        }
        // A change the local copy disagrees with (it shouldn't happen): rebuild from the log.
        None => {
            *state = replay(&ledger.log);
            world.replace_edits(state.edits.iter().map(|(p, id)| (*p, *id)));
            creations.resync(commands, &state.builds);
            landscape.forget_history();
            if own.is_none() && !matches!(change.action, Action::Intent { .. } | Action::Timers { .. }) {
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
