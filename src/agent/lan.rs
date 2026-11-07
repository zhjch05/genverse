//! Playing together on the local network without typing addresses: games hear each
//! other's beacons ([`cubelark_net::lan`]), the panel lists the worlds nearby, and a
//! hosting game announces itself. **Play together** listens for a moment, then joins
//! the world it found, or hosts this one if there was none.

use std::{net::SocketAddr, sync::atomic::Ordering};

use bevy::prelude::*;
use cubelark_net::lan::{Announcer, Beacon, Listener, is_local};

use super::{Chat, Role, online};
use crate::world::ledger::Ledger;

/// Seconds a world stays listed after its last beacon.
const FORGET_AFTER: f32 = 4.0;
/// Seconds Play together listens before hosting.
const LISTEN_FOR: f32 = 2.5;

pub struct Found {
    pub beacon: Beacon,
    /// Where to join it.
    pub at: SocketAddr,
    seen: f32,
}

impl Found {
    /// The address to join at: loopback for a world hosted on this machine.
    pub fn address(&self) -> String {
        if is_local(self.at.ip()) { format!("127.0.0.1:{}", self.at.port()) } else { self.at.to_string() }
    }
}

#[derive(Resource, Default)]
pub struct Lan {
    listener: Option<Listener>,
    announcer: Option<Announcer>,
    /// Worlds heard recently, newest first.
    pub worlds: Vec<Found>,
    /// Play together is listening until this time.
    pub play_until: Option<f32>,
    /// Our hosted world's beacon id.
    id: u64,
}

impl Lan {
    pub fn play_together(&mut self, now: f32) {
        self.play_until = Some(now + LISTEN_FOR);
    }
}

pub fn start_listening(mut lan: ResMut<Lan>) {
    match Listener::start() {
        Ok(listener) => lan.listener = Some(listener),
        Err(e) => warn!("can't listen for games on the local network: {e}"),
    }
}

pub fn update_lan(time: Res<Time>, mut lan: ResMut<Lan>, mut ledger: ResMut<Ledger>, mut chat: ResMut<Chat>) {
    let now = time.elapsed_secs();
    if std::mem::take(&mut chat.play_together) {
        lan.play_together(now);
    }
    let hosting = ledger.hosting.as_ref().map(|(addr, players)| (addr.port(), players.load(Ordering::Relaxed)));

    // Worlds heard (not our own).
    let heard = lan.listener.as_ref().map(Listener::poll).unwrap_or_default();
    for (beacon, at) in heard {
        if hosting.is_some_and(|(port, _)| port == at.port()) && is_local(at.ip()) {
            continue;
        }
        // Heard on several interfaces: keep the first address, refresh the rest.
        let at = lan.worlds.iter().find(|w| w.beacon.id == beacon.id).map_or(at, |w| w.at);
        lan.worlds.retain(|w| w.beacon.id != beacon.id);
        lan.worlds.insert(0, Found { beacon, at, seen: now });
    }
    lan.worlds.retain(|w| now - w.seen < FORGET_AFTER);

    // Announce our world while hosting it.
    match hosting {
        Some((port, players)) => {
            if lan.id == 0 {
                let mut bytes = [0u8; 8];
                let _ = getrandom::fill(&mut bytes);
                lan.id = u64::from_le_bytes(bytes) | 1;
            }
            let name = ledger.profile.name.clone();
            let beacon = Beacon::new(lan.id, format!("{name}'s world"), name, port, players as u32);
            match &lan.announcer {
                Some(announcer) => announcer.set(beacon),
                None => match Announcer::start(beacon) {
                    Ok(announcer) => lan.announcer = Some(announcer),
                    Err(e) => warn!("can't announce this world on the local network: {e}"),
                },
            }
        }
        None => lan.announcer = None,
    }

    // Play together: join what's out there, or host.
    let Some(until) = lan.play_until else { return };
    let found = lan.worlds.iter().find(|w| w.beacon.compatible());
    if let Some(world) = found {
        let (name, address) = (world.beacon.world.clone(), world.address());
        lan.play_until = None;
        let reply = online::join(&mut ledger, &address);
        chat.say(Role::System, format!("Found {name} on your network. {reply}"));
    } else if now >= until {
        lan.play_until = None;
        let reply = online::host(&mut ledger);
        chat.say(Role::System, format!("No worlds on your network yet, so this one is open for others. {reply}"));
    }
}
