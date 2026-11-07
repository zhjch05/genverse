//! A headless Cubewish server. It owns the world's change log: players propose
//! changes, it checks each against the world's rules (signature, land, limits,
//! budgets, rate limits), puts accepted ones in order, and sends them to everyone.
//! Land changes are compiled against the ground as it is when they arrive, so a change
//! made over a stale preview still does what was asked.
//!
//! No rendering and no game engine: just the rules crate, threads, and TCP. One thread
//! owns the world and handles events in order; each connection has a reader and a
//! writer thread.
//!
//! Every [`SNAPSHOT_EVERY`] changes the server saves a snapshot of the world's voxels
//! (`snapshot.json`), so restarting and joining only replay the changes after it. With
//! each land change it sends fingerprints of the chunks it touched; a player whose
//! chunks disagree fetches the server's.

use std::{
    collections::HashMap,
    io,
    net::{SocketAddr, TcpListener, TcpStream},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread,
    time::{SystemTime, UNIX_EPOCH},
};

use cubewish_net::{Presence, ToClient, ToServer, VERSION, hello_bytes, read_messages, write_message};
use cubewish_rules::{
    identity::verify,
    log::{
        Action, Change, RULES_VERSION, Replay, Snapshot, WorldLog, chunk_edits, chunk_of, chunk_prints, replay, replay_from,
        signing_bytes,
    },
};

pub struct ServerConfig {
    /// Where the world's log is kept (`world.json`).
    pub data_dir: PathBuf,
    /// 0 picks a free port.
    pub port: u16,
    /// The terrain seed for a new world.
    pub seed: u32,
}

/// A running server.
pub struct Running {
    pub addr: SocketAddr,
    /// How many players are logged in.
    pub players: Arc<AtomicUsize>,
}

/// Changes between snapshots.
pub const SNAPSHOT_EVERY: usize = 50;

/// How far a change's timestamp may be from the server's clock, in seconds.
const CLOCK_SLACK: u64 = 600;

enum Event {
    Connected { conn: u64, out: Sender<ToClient>, nonce: String },
    Message { conn: u64, message: ToServer },
    Left { conn: u64 },
}

struct Conn {
    out: Sender<ToClient>,
    nonce: String,
    /// The player's id (public key) and name, once they've said hello.
    player: Option<(String, String)>,
    /// What others see of them, once they've sent it.
    presence: Option<Presence>,
}

struct World {
    path: PathBuf,
    log: WorldLog,
    state: Replay,
    snapshot: Option<Snapshot>,
    conns: HashMap<u64, Conn>,
    players: Arc<AtomicUsize>,
}

pub fn start(config: ServerConfig) -> io::Result<Running> {
    std::fs::create_dir_all(&config.data_dir)?;
    let path = config.data_dir.join("world.json");
    let log = match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text).map_err(io::Error::other)?,
        Err(_) => WorldLog::new(config.seed),
    };
    let snapshot: Option<Snapshot> = std::fs::read_to_string(config.data_dir.join("snapshot.json"))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .filter(|s: &Snapshot| s.fits(&log));
    let state = replay_from(&log, snapshot.as_ref());
    for problem in &state.problems {
        eprintln!("saved change no longer applies: {problem}");
    }
    println!(
        "world: {} changes, {} builds{}",
        log.changes.len(),
        state.builds.len(),
        snapshot.as_ref().map_or(String::new(), |s| format!(" (replayed from the snapshot at change {})", s.upto))
    );

    let listener = TcpListener::bind(("0.0.0.0", config.port))?;
    let addr = listener.local_addr()?;
    let (events_tx, events) = mpsc::channel();
    thread::Builder::new().name("accept".into()).spawn(move || accept(listener, events_tx))?;
    let players = Arc::new(AtomicUsize::new(0));
    let world = World { path, log, state, snapshot, conns: HashMap::new(), players: players.clone() };
    thread::Builder::new().name("world".into()).spawn(move || run(world, events))?;
    Ok(Running { addr, players })
}

fn accept(listener: TcpListener, events: Sender<Event>) {
    for (conn, stream) in listener.incoming().filter_map(Result::ok).enumerate() {
        let conn = conn as u64;
        if let Err(e) = serve(conn, stream, events.clone()) {
            eprintln!("connection {conn}: {e}");
        }
    }
}

/// Starts a connection's reader and writer threads.
fn serve(conn: u64, stream: TcpStream, events: Sender<Event>) -> io::Result<()> {
    let _ = stream.set_nodelay(true);
    let mut writer = stream.try_clone()?;
    let (out, outgoing) = mpsc::channel::<ToClient>();
    thread::Builder::new().name(format!("write-{conn}")).spawn(move || {
        for message in outgoing {
            if write_message(&mut writer, &message).is_err() {
                break;
            }
        }
    })?;
    let nonce = random_hex();
    let challenge = ToClient::Challenge { version: VERSION, rules: RULES_VERSION, nonce: nonce.clone() };
    out.send(challenge).map_err(io::Error::other)?;
    let _ = events.send(Event::Connected { conn, out, nonce });
    thread::Builder::new().name(format!("read-{conn}")).spawn(move || {
        for message in read_messages::<ToServer>(stream) {
            let Ok(message) = message else { break };
            if events.send(Event::Message { conn, message }).is_err() {
                return;
            }
        }
        let _ = events.send(Event::Left { conn });
    })?;
    Ok(())
}

fn random_hex() -> String {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).expect("the OS provides randomness");
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn run(mut world: World, events: Receiver<Event>) {
    for event in events {
        match event {
            Event::Connected { conn, out, nonce } => {
                world.conns.insert(conn, Conn { out, nonce, player: None, presence: None });
            }
            Event::Left { conn } => {
                if let Some((player, name)) = world.conns.remove(&conn).and_then(|c| c.player) {
                    world.players.fetch_sub(1, Ordering::Relaxed);
                    println!("{name} left");
                    world.tell_others(conn, ToClient::Gone { player });
                }
            }
            Event::Message { conn, message } => world.handle(conn, message),
        }
    }
}

impl World {
    fn handle(&mut self, conn: u64, message: ToServer) {
        let Some(c) = self.conns.get(&conn) else { return };
        let (out, nonce, player) = (c.out.clone(), c.nonce.clone(), c.player.as_ref().map(|(id, _)| id.clone()));
        match message {
            ToServer::Hello { version, rules, player, name, sig } => {
                if version != VERSION || rules != RULES_VERSION {
                    let why = format!("this server speaks protocol {VERSION} with world rules {RULES_VERSION}");
                    let _ = out.send(ToClient::Bye { why });
                    return;
                }
                if !verify(&player, &hello_bytes(&nonce, &player, &name), &sig) {
                    let _ = out.send(ToClient::Bye { why: "that login isn't signed by its player".into() });
                    return;
                }
                // A new world's first player runs it.
                if self.log.admins.is_empty() {
                    self.log.admins.push(player.clone());
                    self.state = replay(&self.log);
                    self.save();
                }
                println!("{name} joined ({}…)", &player[..8.min(player.len())]);
                if let Some(c) = self.conns.get_mut(&conn)
                    && c.player.replace((player, name)).is_none()
                {
                    self.players.fetch_add(1, Ordering::Relaxed);
                }
                let snapshot = self.snapshot.clone().filter(|s| s.fits(&self.log));
                let _ = out.send(ToClient::Welcome { log: self.log.clone(), snapshot });
                // Who's here already.
                for (other, c) in &self.conns {
                    if let (true, Some((player, name)), Some(presence)) = (*other != conn, &c.player, &c.presence) {
                        let presence = presence.clone();
                        let _ = out.send(ToClient::Presence { player: player.clone(), name: name.clone(), presence });
                    }
                }
            }
            ToServer::Presence(presence) => {
                let Some(c) = self.conns.get_mut(&conn) else { return };
                let Some((player, name)) = c.player.clone() else { return };
                c.presence = Some(presence.clone());
                self.tell_others(conn, ToClient::Presence { player, name, presence });
            }
            ToServer::FetchChunks { chunks } => {
                let chunks: Vec<_> = chunks.into_iter().take(256).map(cubewish_rules::IVec3::from_array).collect();
                let edits = chunk_edits(&self.state.edits, &chunks);
                let _ = out.send(ToClient::Chunks { chunks: edits.into_iter().map(|(c, e)| (c.to_array(), e)).collect() });
            }
            ToServer::Propose { request, by, action, sig } => match self.commit(player, by, action, sig) {
                Ok((change, prints)) => self.broadcast(change, prints, conn, request),
                Err(why) => {
                    let _ = out.send(ToClient::Rejected { request, why });
                }
            },
        }
    }

    /// Checks a proposal and, if the rules allow it, appends it to the log.
    fn commit(
        &mut self,
        player: Option<String>,
        by: cubewish_rules::log::Provenance,
        action: Action,
        sig: String,
    ) -> Result<(Change, Vec<([i32; 3], u64)>), String> {
        let player = player.ok_or("say hello first")?;
        if by.player != player {
            return Err("you can only make changes as yourself".into());
        }
        if !verify(&by.player, &signing_bytes(&by, &action), &sig) {
            return Err("the change isn't signed by you".into());
        }
        if by.at.abs_diff(now()) > CLOCK_SLACK {
            return Err("your clock is more than 10 minutes off from the server's".into());
        }
        // The request text stays on the player's machine; only its hash is shared.
        let by = cubewish_rules::log::Provenance { prompt: None, ..by };
        let id = self.log.changes.last().map_or(1, |c| c.id + 1);
        let change = Change { id, by, action, sig: Some(sig) };
        let mut made = Vec::new();
        match &change.action {
            Action::Revert { change: target } => {
                if !self.log.may_revert(&player, *target) {
                    return Err("you can only undo your own changes".into());
                }
                if self.log.undone().contains(target) {
                    return Err(format!("change {target} is already undone"));
                }
            }
            Action::Rollback { .. } if !self.log.is_admin(&player) => return Err("only admins roll back".into()),
            _ => {
                made = self.state.apply(&change, true)?;
            }
        }
        self.log.changes.push(change.clone());
        // Undos change which changes count: rebuild from the log.
        if matches!(change.action, Action::Revert { .. } | Action::Rollback { .. }) {
            self.state = replay(&self.log);
        }
        let mut chunks: Vec<_> = made.iter().map(|(p, _)| chunk_of(*p)).collect();
        chunks.sort_unstable_by_key(|c| c.to_array());
        chunks.dedup();
        let prints = chunk_prints(&self.state.edits, &chunks);
        self.save();
        if self.log.changes.len().is_multiple_of(SNAPSHOT_EVERY) {
            self.take_snapshot();
        }
        println!("{} {}", change.by.player_name, change.action.describe());
        Ok((change, prints))
    }

    fn take_snapshot(&mut self) {
        let snapshot = Snapshot::take(&self.log, &self.state);
        let path = self.path.with_file_name("snapshot.json");
        let result = serde_json::to_string(&snapshot).map_err(io::Error::other).and_then(|t| std::fs::write(&path, t));
        if let Err(e) = result {
            eprintln!("couldn't save {}: {e}", path.display());
        }
        println!("snapshot at change {}", snapshot.upto);
        self.snapshot = Some(snapshot);
    }

    /// Sends `message` to every logged-in player but the one on `from`.
    fn tell_others(&self, from: u64, message: ToClient) {
        for (conn, c) in &self.conns {
            if *conn != from && c.player.is_some() {
                let _ = c.out.send(message.clone());
            }
        }
    }

    fn broadcast(&self, change: Change, prints: Vec<([i32; 3], u64)>, from: u64, request: u64) {
        for (conn, c) in &self.conns {
            if c.player.is_some() {
                let request = (*conn == from).then_some(request);
                let _ = c.out.send(ToClient::Committed { change: change.clone(), request, prints: prints.clone() });
            }
        }
    }

    fn save(&self) {
        let result = serde_json::to_string(&self.log).map_err(io::Error::other).and_then(|t| std::fs::write(&self.path, t));
        if let Err(e) = result {
            eprintln!("couldn't save {}: {e}", self.path.display());
        }
    }
}
