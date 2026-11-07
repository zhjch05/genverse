//! A headless Cubelark server. It owns the world's change log: players propose
//! changes, it checks each against the world's rules (signature, land, limits,
//! budgets, rate limits), puts accepted ones in order, and sends them to everyone.
//! Land changes are compiled against the ground as it is when they arrive, so a change
//! made over a stale preview still does what was asked.
//!
//! No rendering and no game engine: just the rules crate, threads, and TCP. One thread
//! owns the world and handles events in order; each connection has a reader and a
//! writer thread.

use std::{
    collections::HashMap,
    io,
    net::{SocketAddr, TcpListener, TcpStream},
    path::PathBuf,
    sync::mpsc::{self, Receiver, Sender},
    thread,
    time::{SystemTime, UNIX_EPOCH},
};

use cubelark_net::{ToClient, ToServer, VERSION, hello_bytes, read_messages, write_message};
use cubelark_rules::{
    identity::verify,
    log::{Action, Change, RULES_VERSION, Replay, WorldLog, replay, signing_bytes},
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
}

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
}

struct World {
    path: PathBuf,
    log: WorldLog,
    state: Replay,
    conns: HashMap<u64, Conn>,
}

pub fn start(config: ServerConfig) -> io::Result<Running> {
    std::fs::create_dir_all(&config.data_dir)?;
    let path = config.data_dir.join("world.json");
    let log = match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text).map_err(io::Error::other)?,
        Err(_) => WorldLog::new(config.seed),
    };
    let state = replay(&log);
    for problem in &state.problems {
        eprintln!("saved change no longer applies: {problem}");
    }
    println!("world: {} changes, {} builds", log.changes.len(), state.builds.len());

    let listener = TcpListener::bind(("0.0.0.0", config.port))?;
    let addr = listener.local_addr()?;
    let (events_tx, events) = mpsc::channel();
    thread::Builder::new().name("accept".into()).spawn(move || accept(listener, events_tx))?;
    let world = World { path, log, state, conns: HashMap::new() };
    thread::Builder::new().name("world".into()).spawn(move || run(world, events))?;
    Ok(Running { addr })
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
                world.conns.insert(conn, Conn { out, nonce, player: None });
            }
            Event::Left { conn } => {
                if let Some((_, name)) = world.conns.remove(&conn).and_then(|c| c.player) {
                    println!("{name} left");
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
                if let Some(c) = self.conns.get_mut(&conn) {
                    c.player = Some((player, name));
                }
                let _ = out.send(ToClient::Welcome { log: self.log.clone() });
            }
            ToServer::Propose { request, by, action, sig } => match self.commit(player, by, action, sig) {
                Ok(change) => self.broadcast(change, conn, request),
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
        by: cubelark_rules::log::Provenance,
        action: Action,
        sig: String,
    ) -> Result<Change, String> {
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
        let by = cubelark_rules::log::Provenance { prompt: None, ..by };
        let id = self.log.changes.last().map_or(1, |c| c.id + 1);
        let change = Change { id, by, action, sig: Some(sig) };
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
                self.state.apply(&change, true)?;
            }
        }
        self.log.changes.push(change.clone());
        // Undos change which changes count: rebuild from the log.
        if matches!(change.action, Action::Revert { .. } | Action::Rollback { .. }) {
            self.state = replay(&self.log);
        }
        self.save();
        println!("{} {}", change.by.player_name, change.action.describe());
        Ok(change)
    }

    fn broadcast(&self, change: Change, from: u64, request: u64) {
        for (conn, c) in &self.conns {
            if c.player.is_some() {
                let request = (*conn == from).then_some(request);
                let _ = c.out.send(ToClient::Committed { change: change.clone(), request });
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
