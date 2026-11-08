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
//! It also runs the games' timers ([`cubewish_rules::logic::clockwork`]): about twice a
//! second it fires those that are due and commits what they did as one change per
//! package, by [`WORLD`]. A timer that does nothing leaves no trace, and one anchored to
//! a game piece sleeps while no player is near it.
//!
//! The log is kept append-only (`world.jsonl`, [`cubewish_rules::store`]), so saving a
//! change writes only that change.
//!
//! Every [`SNAPSHOT_EVERY`] changes the server saves a snapshot of the world's voxels
//! and games (`snapshot.json`), so restarting and joining only replay the changes after
//! it, without running old game scripts again. With
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
        mpsc::{self, Receiver, RecvTimeoutError, Sender},
    },
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use cubewish_net::{Presence, ToClient, ToServer, VERSION, hello_bytes, read_messages, write_message};
use cubewish_rules::{
    identity::verify,
    log::{
        Action, Change, Provenance, RULES_VERSION, Replay, Snapshot, WORLD, WorldLog, chunk_edits, chunk_of, chunk_prints,
        replay, replay_from, signing_bytes,
    },
    logic::{Clockwork, Package},
    store::LogFile,
};

pub struct ServerConfig {
    /// Where the world's log (`world.jsonl`) and snapshot are kept.
    pub data_dir: PathBuf,
    /// 0 picks a free port.
    pub port: u16,
    /// The terrain seed for a new world.
    pub seed: u32,
    /// A new world is flat and empty.
    pub flat: bool,
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
    file: LogFile,
    log: WorldLog,
    state: Replay,
    snapshot: Option<Snapshot>,
    conns: HashMap<u64, Conn>,
    players: Arc<AtomicUsize>,
    /// Which timers have run up to when.
    clock: Clockwork,
}

pub fn start(config: ServerConfig) -> io::Result<Running> {
    std::fs::create_dir_all(&config.data_dir)?;
    let path = config.data_dir.join("snapshot.json");
    let (mut file, log) = LogFile::open(&config.data_dir)?;
    let log = match log {
        Some(log) => log,
        None => {
            let log = WorldLog { flat: config.flat, ..WorldLog::new(config.seed) };
            file.rewrite(&log)?;
            log
        }
    };
    let snapshot: Option<Snapshot> = std::fs::read_to_string(&path)
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
    let clock = Clockwork::new(state.logic.timers_until);
    let world = World { path, file, log, state, snapshot, conns: HashMap::new(), players: players.clone(), clock };
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

/// How often the server looks for timers to run.
const TIMER_POLL: Duration = Duration::from_millis(500);

fn run(mut world: World, events: Receiver<Event>) {
    loop {
        let event = match events.recv_timeout(TIMER_POLL) {
            Ok(event) => event,
            Err(RecvTimeoutError::Timeout) => {
                world.run_timers(now());
                continue;
            }
            Err(RecvTimeoutError::Disconnected) => return,
        };
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
    /// Fires the timers due by `now` and commits what each package's did.
    fn run_timers(&mut self, now: u64) {
        // Where the players are: timers anchored to game pieces run only near someone.
        let players: Vec<[f32; 3]> = self.conns.values().filter(|c| c.player.is_some()).filter_map(|c| c.presence.as_ref().map(|p| p.position)).collect();
        let first = self.log.changes.last().map_or(1, |c| c.id + 1);
        let run = self.clock.run(&mut self.state.logic, first, now, self.log.seed, &self.state.rules.logic, &players);
        for error in &run.errors {
            eprintln!("{error}");
        }
        for (id, package, effects) in run.changes {
            let action = Action::Timers { package, until: now, effects };
            let change = Change { id, by: Provenance::new(WORLD, "World", None, None, now), action, sig: None };
            self.log.changes.push(change.clone());
            self.saved();
            self.broadcast(change, Vec::new(), None, u64::MAX, 0);
        }
    }

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
                    self.saved();
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
            ToServer::Propose { request, by, action, sig, package } => match self.commit(player, by, action, sig, package) {
                Ok((change, prints, package)) => self.broadcast(change, prints, package, conn, request),
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
        package: Option<Package>,
    ) -> Result<(Change, Vec<([i32; 3], u64)>, Option<Package>), String> {
        let player = player.ok_or("say hello first")?;
        if matches!(action, Action::Timers { .. }) {
            return Err("only the world runs timers".into());
        }
        // Whatever was due goes off first, so the change lands on the world as it is now.
        self.run_timers(now());
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
        // A package that comes along must be the one the change installs; it's kept
        // only if the change goes through.
        let new_package = match (&change.action, package) {
            (Action::InstallPackage { package: named, .. }, Some(package)) => {
                if package.hash() != named.hash {
                    return Err("that package doesn't match the one the change names".into());
                }
                package.check().map_err(|e| e.join("; "))?;
                let fresh = !self.log.packages.contains_key(&named.hash);
                fresh.then(|| (named.hash.clone(), Arc::new(package)))
            }
            (_, Some(_)) => return Err("only installing a package brings one along".into()),
            (_, None) => None,
        };
        if let Some((hash, package)) = &new_package {
            self.state.logic.packages.insert(hash.clone(), package.clone());
        }
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
            _ => match self.state.apply(&change, true) {
                Ok(edits) => made = edits,
                Err(why) => {
                    if let Some((hash, _)) = &new_package {
                        self.state.logic.packages.remove(hash);
                    }
                    return Err(why);
                }
            },
        }
        if let Some((hash, package)) = &new_package {
            self.log.packages.insert(hash.clone(), package.clone());
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
        self.saved();
        println!("{} {}", change.by.player_name, change.action.describe());
        Ok((change, prints, new_package.map(|(_, p)| (*p).clone())))
    }

    fn take_snapshot(&mut self) {
        let snapshot = Snapshot::take(&self.log, &self.state);
        let path = &self.path;
        let tmp = path.with_extension("json.tmp");
        let result = serde_json::to_string(&snapshot)
            .map_err(io::Error::other)
            .and_then(|t| std::fs::write(&tmp, t))
            .and_then(|_| std::fs::rename(&tmp, path));
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

    fn broadcast(&self, change: Change, prints: Vec<([i32; 3], u64)>, package: Option<Package>, from: u64, request: u64) {
        for (conn, c) in &self.conns {
            if c.player.is_some() {
                let request = (*conn == from).then_some(request);
                let (change, prints, package) = (change.clone(), prints.clone(), package.clone());
                let _ = c.out.send(ToClient::Committed { change, request, prints, package });
            }
        }
    }

    /// Saves what the log gained, and now and then a snapshot.
    fn saved(&mut self) {
        if let Err(e) = self.file.sync(&self.log) {
            eprintln!("couldn't save {}: {e}", self.file.path().display());
        }
        if self.log.changes.len().is_multiple_of(SNAPSHOT_EVERY) {
            self.take_snapshot();
        }
    }
}
