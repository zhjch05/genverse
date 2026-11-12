//! Finding games on the local network, like Minecraft's "Open to LAN": a hosting game
//! broadcasts a small [`Beacon`] every second, and every game listens for them. Nothing
//! leaves the local network, and the server itself doesn't change.

use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver},
    },
    thread,
    time::Duration,
};

use genverse_rules::log::RULES_VERSION;
use serde::{Deserialize, Serialize};
use socket2::{Domain, Protocol, Socket, Type};

use crate::VERSION;

/// The UDP port beacons go to.
pub const LAN_PORT: u16 = 7877;
const MAGIC: &str = "genverse";
const EVERY: Duration = Duration::from_secs(1);

/// What a hosting game says about its world.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Beacon {
    pub magic: String,
    /// Random per hosted world, so a world heard on several interfaces is listed once.
    pub id: u64,
    pub protocol: u32,
    pub rules: u32,
    pub world: String,
    pub host: String,
    /// The game server's TCP port.
    pub port: u16,
    pub players: u32,
}

impl Beacon {
    pub fn new(id: u64, world: String, host: String, port: u16, players: u32) -> Self {
        Self { magic: MAGIC.into(), id, protocol: VERSION, rules: RULES_VERSION, world, host, port, players }
    }

    /// Whether this game can play on it.
    pub fn compatible(&self) -> bool {
        self.protocol == VERSION && self.rules == RULES_VERSION
    }
}

/// Every address a broadcast should go to: each IPv4 interface's broadcast address,
/// plus the all-hosts one (which some systems only send out of one interface).
fn broadcast_addresses() -> Vec<Ipv4Addr> {
    let mut out = vec![Ipv4Addr::BROADCAST];
    if let Ok(interfaces) = if_addrs::get_if_addrs() {
        for interface in interfaces {
            if let if_addrs::IfAddr::V4(v4) = interface.addr
                && !v4.ip.is_loopback()
                && let Some(broadcast) = v4.broadcast
                && !out.contains(&broadcast)
            {
                out.push(broadcast);
            }
        }
    }
    out
}

/// Broadcasts a beacon every second until dropped. Update it with [`Self::set`].
pub struct Announcer {
    beacon: Arc<Mutex<Beacon>>,
    stop: Arc<AtomicBool>,
}

impl Announcer {
    pub fn start(beacon: Beacon) -> std::io::Result<Self> {
        let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
        socket.set_broadcast(true)?;
        let beacon = Arc::new(Mutex::new(beacon));
        let stop = Arc::new(AtomicBool::new(false));
        let (shared, stopped) = (beacon.clone(), stop.clone());
        thread::Builder::new().name("lan-announce".into()).spawn(move || {
            while !stopped.load(Ordering::Relaxed) {
                let bytes = shared.lock().map(|b| serde_json::to_vec(&*b).unwrap_or_default()).unwrap_or_default();
                // Interfaces come and go (Wi-Fi, VPNs), so look again each time.
                for address in broadcast_addresses() {
                    let _ = socket.send_to(&bytes, SocketAddrV4::new(address, LAN_PORT));
                }
                thread::sleep(EVERY);
            }
        })?;
        Ok(Self { beacon, stop })
    }

    pub fn set(&self, beacon: Beacon) {
        if let Ok(mut b) = self.beacon.lock() {
            *b = beacon;
        }
    }
}

impl Drop for Announcer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Hears beacons. Several games on one machine can listen at once.
pub struct Listener {
    heard: Mutex<Receiver<(Beacon, SocketAddr)>>,
}

impl Listener {
    pub fn start() -> std::io::Result<Self> {
        let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
        socket.set_reuse_address(true)?;
        #[cfg(unix)]
        socket.set_reuse_port(true)?;
        socket.set_broadcast(true)?;
        socket.bind(&SocketAddr::from((Ipv4Addr::UNSPECIFIED, LAN_PORT)).into())?;
        let socket: UdpSocket = socket.into();
        let (tx, heard) = mpsc::channel();
        thread::Builder::new().name("lan-listen".into()).spawn(move || {
            let mut buffer = [0u8; 1024];
            while let Ok((n, from)) = socket.recv_from(&mut buffer) {
                let Ok(beacon) = serde_json::from_slice::<Beacon>(&buffer[..n]) else { continue };
                if beacon.magic != MAGIC {
                    continue;
                }
                // The game is reached on the beacon's port, at the address it came from.
                let at = SocketAddr::new(from.ip(), beacon.port);
                if tx.send((beacon, at)).is_err() {
                    return;
                }
            }
        })?;
        Ok(Self { heard: Mutex::new(heard) })
    }

    /// Beacons heard since the last poll, with the address to join each world at.
    pub fn poll(&self) -> Vec<(Beacon, SocketAddr)> {
        self.heard.lock().map(|rx| rx.try_iter().collect()).unwrap_or_default()
    }
}

/// This computer's address on the local network, as others on it reach it: a private
/// address on a real network interface (Wi-Fi, Ethernet) before any other, so a VPN's
/// tunnel or a virtual machine's bridge isn't mistaken for it.
pub fn local_ipv4() -> Option<Ipv4Addr> {
    let interfaces = if_addrs::get_if_addrs().ok()?;
    let rank = |name: &str, ip: Ipv4Addr| {
        let virtual_ = ["utun", "tun", "tap", "ppp", "ipsec", "bridge", "vmnet", "vmenet", "vbox", "anpi", "ap", "docker", "veth", "wg", "zt", "tailscale", "llw", "awdl"]
            .iter()
            .any(|p| name.starts_with(p));
        match (ip.is_private(), virtual_) {
            (true, false) => 0,
            (true, true) => 2,
            (false, false) => 1,
            (false, true) => 3,
        }
    };
    interfaces
        .iter()
        .filter_map(|i| match i.addr {
            if_addrs::IfAddr::V4(ref v4) if !v4.ip.is_loopback() && !v4.ip.is_link_local() => Some((rank(&i.name, v4.ip), v4.ip)),
            _ => None,
        })
        .filter(|(rank, _)| *rank < 3)
        .min_by_key(|(rank, _)| *rank)
        .map(|(_, ip)| ip)
}

/// Whether `ip` is this machine (so a world hosted here can be joined on loopback).
pub fn is_local(ip: IpAddr) -> bool {
    ip.is_loopback()
        || if_addrs::get_if_addrs().is_ok_and(|interfaces| interfaces.iter().any(|i| i.ip() == ip))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn beacons_are_heard_by_every_listener() {
        let (a, b) = (Listener::start().unwrap(), Listener::start().unwrap());
        let beacon = Beacon::new(7, "Test world".into(), "Ada".into(), 4242, 2);
        let announcer = Announcer::start(beacon.clone()).unwrap();
        let heard = |listener: &Listener| {
            for _ in 0..40 {
                if let Some(found) = listener.poll().into_iter().find(|(b, _)| b.port == 4242) {
                    return Some(found);
                }
                thread::sleep(Duration::from_millis(100));
            }
            None
        };
        let (got, at) = heard(&a).expect("the first listener heard nothing");
        assert_eq!(got, beacon);
        assert!(got.compatible());
        assert_eq!(at.port(), 4242);
        assert!(heard(&b).is_some(), "the second listener heard nothing");
        drop(announcer);
    }
}
