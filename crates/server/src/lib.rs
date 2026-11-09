//! A headless Cubewish server. It hosts one game (`docs/worlds.md`): the game's own
//! log (which worlds there are, who runs it) and each world's change log. Players
//! propose changes; it checks each against the world's rules (signature, land, limits,
//! budgets, rate limits), puts accepted ones in order, and sends them to the players in
//! that world. Land changes are compiled against the ground as it is when they arrive,
//! so a change made over a stale preview still does what was asked.
//!
//! Each player is in one world at a time ([`ToServer::Enter`] moves them) and hears
//! only that world's changes and people, plus the game's changes. Worlds load when
//! someone first enters them.
//!
//! No rendering and no game engine: just the rules crate, threads, and TCP. One thread
//! owns the game and handles events in order; each connection has a reader and a
//! writer thread.
//!
//! It also runs the games' timers ([`cubewish_rules::logic::clockwork`]): about twice a
//! second it fires those that are due in each loaded world and commits what they did
//! as one change per package, by [`WORLD`]. A timer that does nothing leaves no trace,
//! and one anchored to a game piece sleeps while no player is near it.
//!
//! On disk (the data directory): `game.json`, and `worlds/<name>/` with each world's
//! append-only log (`world.jsonl`, [`cubewish_rules::store`]) and its snapshot, saved
//! every [`SNAPSHOT_EVERY`] changes so restarting and entering only replay what came
//! after. A data directory from before games (one `world.jsonl`) becomes the world
//! `main`. With each land change the server sends fingerprints of the chunks it
//! touched; a player whose chunks disagree fetches the server's.

use std::{
    collections::HashMap,
    io,
    net::{SocketAddr, TcpListener, TcpStream},
    path::{Path, PathBuf},
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
    game::{self, GameAction, GameLog},
    identity::verify,
    log::{
        Action, Change, Provenance, RULES_VERSION, Replay, Snapshot, WORLD, WorldLog, WorldState, chunk_edits, chunk_of,
        chunk_prints, replay_from, signing_bytes,
    },
    logic::{Clockwork, Package},
    store::{self, LogFile},
};

pub struct ServerConfig {
    /// The game's directory: `game.json` and `worlds/`.
    pub data_dir: PathBuf,
    /// 0 picks a free port.
    pub port: u16,
    /// The terrain seed for a new game's first world.
    pub seed: u32,
    /// A new game's first world is flat and empty.
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
    /// The world they're in, once they've said hello.
    world: Option<String>,
    /// What others see of them, once they've sent it.
    presence: Option<Presence>,
}

/// One world of the game, loaded.
struct Place {
    file: LogFile,
    log: WorldLog,
    state: Replay,
    snapshot: Option<Snapshot>,
    /// Where its snapshot goes.
    snapshot_path: PathBuf,
    /// Which timers have run up to when.
    clock: Clockwork,
    /// Changes timers made since they were last sent.
    fresh: Vec<Change>,
}

/// Pieces walk on the world as the server has it.
impl cubewish_rules::logic::Ground for Place {
    fn heights(&self, min: [i32; 2], step: i32, count: [i32; 2]) -> Option<Vec<Option<i32>>> {
        self.state.heights(min, step, count)
    }
}

impl cubewish_rules::logic::TimerHost for Place {
    fn logic(&self) -> &cubewish_rules::logic::Logic {
        &self.state.logic
    }

    fn next_change(&self) -> u64 {
        self.log.changes.last().map_or(1, |c| c.id + 1)
    }

    fn record(&mut self, package: &str, now: u64, effects: Vec<cubewish_rules::logic::Effect>) -> Result<(), String> {
        let action = Action::Timers { package: package.into(), until: now, effects };
        let change = Change { id: self.next_change(), by: Provenance::new(WORLD, "World", None, None, now), action, sig: None };
        self.state.apply(&change, true)?;
        self.log.changes.push(change.clone());
        self.saved();
        self.fresh.push(change);
        Ok(())
    }
}

impl Place {
    /// The world as of its change `save` (a save), to load.
    fn state_of(&self, save: u64) -> WorldState {
        let dir = self.snapshot_path.parent().unwrap_or(Path::new("."));
        store::state_of_save(dir, &self.log, save, self.snapshot.as_ref())
    }

    /// Keeps the world as it is now (as of its last change, a save), so loading that save
    /// swaps it in.
    fn keep_state(&self) {
        let dir = self.snapshot_path.parent().unwrap_or(Path::new("."));
        if let Err(e) = store::write_state(dir, &Snapshot::take(&self.log, &self.state)) {
            eprintln!("couldn't keep the save in {}: {e}", dir.display());
        }
    }
}

struct Server {
    dir: PathBuf,
    game: GameLog,
    places: HashMap<String, Place>,
    conns: HashMap<u64, Conn>,
    players: Arc<AtomicUsize>,
}

/// Makes a data directory into a game, once: a world from before games becomes `main`.
fn open_game(config: &ServerConfig) -> io::Result<GameLog> {
    let dir = &config.data_dir;
    std::fs::create_dir_all(dir)?;
    if let Some(game) = store::read_game(dir)? {
        return Ok(game);
    }
    let name = dir.file_name().and_then(|n| game::clean_name(&n.to_string_lossy())).unwrap_or("game".into());
    let mut game = GameLog::new(&name);
    let by = Provenance::new(WORLD, "World", None, None, now());
    let main = dir.join("worlds").join("main");
    let (seed, flat, admins) = match LogFile::open(dir)? {
        (_, Some(log)) => {
            std::fs::create_dir_all(&main)?;
            for file in ["world.jsonl", "world.json.bak", "snapshot.json"] {
                if dir.join(file).exists() {
                    std::fs::rename(dir.join(file), main.join(file))?;
                }
            }
            (log.seed, log.flat, log.admins)
        }
        (_, None) => (config.seed, config.flat, Vec::new()),
    };
    if !admins.is_empty() {
        game.push(by.clone(), GameAction::SetAdmins { admins }).map_err(io::Error::other)?;
    }
    // A new game's first world is made by whoever runs it; here, the server.
    game.changes.push(game::GameChange { id: game.changes.len() as u64 + 1, by, action: GameAction::CreateWorld { name: "main".into(), seed, flat } });
    store::write_game(dir, &game)?;
    Ok(game)
}

pub fn start(config: ServerConfig) -> io::Result<Running> {
    let game = open_game(&config)?;
    println!("game {}: worlds {}", game.name, game.worlds().iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>().join(", "));
    let listener = TcpListener::bind(("0.0.0.0", config.port))?;
    let addr = listener.local_addr()?;
    let (events_tx, events) = mpsc::channel();
    thread::Builder::new().name("accept".into()).spawn(move || accept(listener, events_tx))?;
    let players = Arc::new(AtomicUsize::new(0));
    let mut server = Server { dir: config.data_dir, game, places: HashMap::new(), conns: HashMap::new(), players: players.clone() };
    // The first world is ready before anyone asks.
    if let Some(start) = server.game.start() {
        server.place(&start)?;
    }
    thread::Builder::new().name("game".into()).spawn(move || run(server, events))?;
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

fn run(mut server: Server, events: Receiver<Event>) {
    loop {
        let event = match events.recv_timeout(TIMER_POLL) {
            Ok(event) => event,
            Err(RecvTimeoutError::Timeout) => {
                server.run_timers(now());
                continue;
            }
            Err(RecvTimeoutError::Disconnected) => return,
        };
        match event {
            Event::Connected { conn, out, nonce } => {
                server.conns.insert(conn, Conn { out, nonce, player: None, world: None, presence: None });
            }
            Event::Left { conn } => {
                if let Some(c) = server.conns.remove(&conn)
                    && let (Some((player, name)), Some(world)) = (c.player, c.world)
                {
                    server.players.fetch_sub(1, Ordering::Relaxed);
                    println!("{name} left");
                    server.tell_world(&world, conn, ToClient::Gone { player });
                }
            }
            Event::Message { conn, message } => server.handle(conn, message),
        }
    }
}

impl Server {
    /// The world `name`, loaded from disk the first time it's asked for.
    fn place(&mut self, name: &str) -> io::Result<&mut Place> {
        if !self.places.contains_key(name) {
            let ground = self.game.world(name).ok_or_else(|| io::Error::other(format!("there's no world \"{name}\"")))?;
            let dir = self.dir.join("worlds").join(name);
            let (mut file, log) = LogFile::open(&dir)?;
            let mut log = log.unwrap_or(WorldLog { flat: ground.flat, ..WorldLog::new(ground.seed) });
            // The game's admins run every world in it.
            log.admins = self.game.admins();
            let snapshot_path = dir.join("snapshot.json");
            if log.migrate() {
                println!("world {name}: took the old undos out of its log");
                let _ = std::fs::remove_file(&snapshot_path);
            }
            file.sync(&log)?;
            let snapshot: Option<Snapshot> = std::fs::read_to_string(&snapshot_path)
                .ok()
                .and_then(|text| serde_json::from_str(&text).ok())
                .filter(|s: &Snapshot| s.fits(&log));
            let state = replay_from(&log, snapshot.as_ref());
            for problem in &state.problems {
                eprintln!("{name}: saved change no longer applies: {problem}");
            }
            println!("world {name}: {} changes, {} builds", log.changes.len(), state.builds.len());
            let clock = Clockwork::new(state.logic.timers_until);
            self.places.insert(name.to_string(), Place { file, log, state, snapshot, snapshot_path, clock, fresh: Vec::new() });
            // Nobody is in it yet, so the changes need sending to no one.
            let _ = self.sync_packages(name);
        }
        Ok(self.places.get_mut(name).expect("just loaded"))
    }

    /// Brings a loaded world's packages in line with the game's: returns the changes
    /// it made there.
    fn sync_packages(&mut self, name: &str) -> Vec<Change> {
        let Some(place) = self.places.get_mut(name) else { return Vec::new() };
        let mut out = Vec::new();
        let mut adopted = false;
        for sync in self.game.sync(&place.state.logic.installed) {
            match sync {
                game::Sync::Upgrade(package, scope) => {
                    let reference = package.reference();
                    place.log.packages.insert(reference.hash.clone(), package.clone());
                    place.state.logic.packages.insert(reference.hash.clone(), package);
                    match place.record(Action::InstallPackage { package: reference, scope }) {
                        Ok(change) => out.push(change),
                        Err(why) => eprintln!("{name}: couldn't move to the game's package: {why}"),
                    }
                }
                game::Sync::Remove(package) => {
                    if let Ok(change) = place.record(Action::UninstallPackage { name: package }) {
                        out.push(change);
                    }
                }
                game::Sync::Adopt(package) => {
                    let found = place.state.logic.installed.get(&package).and_then(|i| place.state.logic.package(i)).cloned();
                    if let Some(found) = found {
                        let by = Provenance::new(WORLD, "World", None, None, now());
                        adopted |= self.game.push(by, GameAction::AddPackage { package: Box::new(found) }).is_ok();
                    }
                }
            }
        }
        if adopted {
            self.game_changed(None);
        }
        out
    }

    /// Fires the timers due by `now` in every loaded world.
    fn run_timers(&mut self, now: u64) {
        let names: Vec<String> = self.places.keys().cloned().collect();
        for name in names {
            // Where its players are: timers anchored to game pieces run only near someone.
            let players: Vec<[f32; 3]> = self
                .conns
                .values()
                .filter(|c| c.player.is_some() && c.world.as_deref() == Some(name.as_str()))
                .filter_map(|c| c.presence.as_ref().map(|p| p.position))
                .collect();
            let Some(place) = self.places.get_mut(&name) else { continue };
            for change in place.run_timers(now, &players) {
                self.broadcast(&name, change, Vec::new(), None, u64::MAX, 0);
            }
        }
    }

    /// Puts the player on `conn` in `world`: what they carry comes along, then the
    /// welcome, and who's there.
    fn enter(&mut self, conn: u64, world: &str) -> Result<(), String> {
        self.place(world).map_err(|e| e.to_string())?;
        let (left, me) = self.conns.get(&conn).map(|c| (c.world.clone(), c.player.clone())).unwrap_or_default();
        if let (Some(left), Some((player, _))) = (left.filter(|l| l != world), me) {
            let (items, stats) = self
                .places
                .get(&left)
                .map(|p| (p.state.logic.state.carried(&player), p.state.logic.state.carried_stats(&player)))
                .unwrap_or_default();
            if !items.is_empty() || !stats.is_empty() {
                let depart = Action::Depart { player: player.clone(), to: world.to_string(), items: items.clone(), stats: stats.clone() };
                if let Some(Ok(change)) = self.places.get_mut(&left).map(|p| p.record(depart)) {
                    self.broadcast(&left, change, Vec::new(), None, u64::MAX, 0);
                }
                let arrive = Action::Arrive { player, from: left.clone(), items, stats };
                if let Some(Ok(change)) = self.places.get_mut(world).map(|p| p.record(arrive)) {
                    self.broadcast(world, change, Vec::new(), None, u64::MAX, 0);
                }
            }
        }
        let game = self.game.clone();
        let place = self.place(world).map_err(|e| e.to_string())?;
        let snapshot = place.snapshot.clone().filter(|s| s.fits(&place.log));
        let welcome = ToClient::Welcome { game, world: world.to_string(), log: place.log.clone(), snapshot };
        let Some(c) = self.conns.get_mut(&conn) else { return Ok(()) };
        let left = c.world.replace(world.to_string());
        let (out, me) = (c.out.clone(), c.player.clone());
        let _ = out.send(welcome);
        if let (Some(left), Some((player, _))) = (left.filter(|l| l != world), &me) {
            self.tell_world(&left, conn, ToClient::Gone { player: player.clone() });
        }
        // Who's here already, and that we came.
        for (other, c) in &self.conns {
            if *other != conn
                && c.world.as_deref() == Some(world)
                && let (Some((player, name)), Some(presence)) = (&c.player, &c.presence)
            {
                let _ = out.send(ToClient::Presence { player: player.clone(), name: name.clone(), presence: presence.clone() });
            }
        }
        if let (Some((player, name)), Some(presence)) = (me, self.conns.get(&conn).and_then(|c| c.presence.clone())) {
            self.tell_world(world, conn, ToClient::Presence { player, name, presence });
        }
        Ok(())
    }

    fn handle(&mut self, conn: u64, message: ToServer) {
        let Some(c) = self.conns.get(&conn) else { return };
        let (out, nonce, player, world) =
            (c.out.clone(), c.nonce.clone(), c.player.as_ref().map(|(id, _)| id.clone()), c.world.clone());
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
                // A new game's first player runs it.
                if self.game.admins().is_empty() {
                    let by = Provenance::new(&player, &name, None, None, now());
                    let _ = self.game.push(by, GameAction::SetAdmins { admins: vec![player.clone()] });
                    self.admins_changed();
                    self.game_changed(None);
                }
                println!("{name} joined ({}…)", &player[..8.min(player.len())]);
                if let Some(c) = self.conns.get_mut(&conn)
                    && c.player.replace((player, name)).is_none()
                {
                    self.players.fetch_add(1, Ordering::Relaxed);
                }
                let start = self.game.start().unwrap_or("main".into());
                if let Err(why) = self.enter(conn, &start) {
                    let _ = out.send(ToClient::Bye { why });
                }
            }
            ToServer::Enter { world: to } => {
                if player.is_none() {
                    return;
                }
                if self.game.world(&to).is_none() {
                    let _ = out.send(ToClient::Bye { why: format!("there's no world \"{to}\" in this game") });
                    return;
                }
                if let Err(why) = self.enter(conn, &to) {
                    eprintln!("couldn't open world {to}: {why}");
                }
            }
            ToServer::Presence(presence) => {
                let Some(c) = self.conns.get_mut(&conn) else { return };
                let (Some((player, name)), Some(world)) = (c.player.clone(), c.world.clone()) else { return };
                c.presence = Some(presence.clone());
                self.tell_world(&world, conn, ToClient::Presence { player, name, presence });
            }
            ToServer::FetchChunks { chunks } => {
                let Some(place) = world.and_then(|w| self.places.get(&w)) else { return };
                let chunks: Vec<_> = chunks.into_iter().take(256).map(cubewish_rules::IVec3::from_array).collect();
                let edits = chunk_edits(&place.state.edits, &chunks);
                let _ = out.send(ToClient::Chunks { chunks: edits.into_iter().map(|(c, e)| (c.to_array(), e)).collect() });
            }
            ToServer::Propose { request, by, action, sig, package } => {
                let result = match world.as_ref().and_then(|w| self.places.get_mut(w)) {
                    Some(place) => place.commit(player, by, action, sig, package),
                    None => Err("say hello first".into()),
                };
                match result {
                    Ok((changes, prints, package)) => {
                        let world = world.expect("in a world");
                        let (change, timers) = changes.split_last().expect("the change itself");
                        for timer in timers {
                            self.broadcast(&world, timer.clone(), Vec::new(), None, u64::MAX, 0);
                        }
                        self.broadcast(&world, change.clone(), prints, package, conn, request);
                    }
                    Err(why) => {
                        let _ = out.send(ToClient::Rejected { request, why });
                    }
                }
            }
            ToServer::ProposeGame { request, by, action, sig } => match self.change_game(player, by, action, sig) {
                Ok(()) => self.game_changed(Some((conn, request))),
                Err(why) => {
                    let _ = out.send(ToClient::Rejected { request, why });
                }
            },
        }
    }

    /// Checks a change to the game and makes it.
    fn change_game(&mut self, player: Option<String>, by: Provenance, action: GameAction, sig: String) -> Result<(), String> {
        let player = player.ok_or("say hello first")?;
        if by.player != player || !verify(&by.player, &game::signing_bytes(&by, &action), &sig) {
            return Err("the change isn't signed by you".into());
        }
        if let GameAction::Save { worlds, .. } = &action
            && worlds.iter().any(|(w, last)| self.game.world(w).is_none() || *last > self.last_change(w))
        {
            return Err("that save names changes the worlds don't have".into());
        }
        let by = Provenance { prompt: None, ..by };
        // Loading a game save: every world's load must be signed by the admin.
        let mut loads = Vec::new();
        if let GameAction::LoadSave { name, signed } = &action {
            let worlds = self.game.save_named(name).ok_or(format!("there's no game save \"{name}\""))?.clone();
            for (world, to) in worlds {
                if self.game.world(&world).is_none() {
                    continue;
                }
                let load = game::load_action(name, to);
                let sig = signed.get(&world).ok_or(format!("loading {world} isn't signed"))?;
                if !verify(&by.player, &signing_bytes(&by, &load), sig) {
                    return Err(format!("loading {world} isn't signed by you"));
                }
                loads.push((world, to, load, sig.clone()));
            }
        }
        // A save names where every world is; the server knows that best.
        let action = match action {
            GameAction::Save { name, worlds } if worlds.is_empty() => {
                let worlds = self.game.worlds().into_iter().map(|(w, _)| (w.clone(), self.last_change(&w))).collect();
                GameAction::Save { name, worlds }
            }
            other => other,
        };
        let by_for_loads = by.clone();
        self.game.push(by, action.clone())?;
        match &action {
            GameAction::CreateWorld { name, .. } => {
                self.place(name).map_err(|e| e.to_string())?;
            }
            GameAction::RemoveWorld { name } => {
                // Whoever is in it goes to the first world.
                let start = self.game.start().unwrap_or_default();
                let inside: Vec<u64> = self.conns.iter().filter(|(_, c)| c.world.as_deref() == Some(name.as_str())).map(|(id, _)| *id).collect();
                for conn in inside {
                    let _ = self.enter(conn, &start);
                }
                self.places.remove(name);
                let trash = self.dir.join("trash");
                let _ = std::fs::create_dir_all(&trash);
                let _ = std::fs::rename(self.dir.join("worlds").join(name), trash.join(format!("{name}-{}", now())));
            }
            GameAction::SetAdmins { .. } => self.admins_changed(),
            GameAction::Save { worlds, .. } => {
                // Each world keeps its state as of the save, so loading it never replays.
                for (world, last) in worlds.clone() {
                    if let Ok(place) = self.place(&world)
                        && place.log.changes.last().map_or(0, |c| c.id) == last
                    {
                        place.keep_state();
                    }
                }
            }
            GameAction::AddPackage { .. } | GameAction::RemovePackage { .. } => {
                // Loaded worlds follow at once; the others when they're next loaded.
                let names: Vec<String> = self.places.keys().cloned().collect();
                for name in names {
                    for change in self.sync_packages(&name) {
                        self.broadcast(&name, change, Vec::new(), None, u64::MAX, 0);
                    }
                }
            }
            GameAction::LoadSave { .. } => {
                for (world, to, load, sig) in loads {
                    // Loading may add changes of its own (package upgrades), so the
                    // last change is read after.
                    let Ok(place) = self.place(&world) else { continue };
                    let last = place.log.changes.last().map_or(0, |c| c.id);
                    if last <= to {
                        continue;
                    }
                    let Action::Load { save, name, .. } = load else { continue };
                    let state = Some(Box::new(place.state_of(to)));
                    let change = Change { id: last + 1, by: by_for_loads.clone(), action: Action::Load { save, name, state }, sig: Some(sig) };
                    if let Err(why) = place.state.apply(&change, true) {
                        eprintln!("{world}: couldn't load the save: {why}");
                        continue;
                    }
                    place.log.changes.push(change.clone());
                    place.saved();
                    self.broadcast(&world, change, Vec::new(), None, u64::MAX, 0);
                }
            }
        }
        println!("{} {:?}", player, action);
        Ok(())
    }

    /// The game's admins run every world: the loaded ones take them now.
    fn admins_changed(&mut self) {
        let admins = self.game.admins();
        for place in self.places.values_mut() {
            place.log.admins = admins.clone();
            place.state.admins = admins.clone();
            place.state.land.admins = admins.clone();
            place.saved();
        }
    }

    /// A world's last change: loaded, or as its file says.
    fn last_change(&self, world: &str) -> u64 {
        match self.places.get(world) {
            Some(place) => place.log.changes.last().map_or(0, |c| c.id),
            None => store::summary(&self.dir.join("worlds").join(world)).map_or(0, |s| s.changes as u64),
        }
    }

    /// Saves the game and tells everyone; `from` proposed it.
    fn game_changed(&mut self, from: Option<(u64, u64)>) {
        if let Err(e) = store::write_game(&self.dir, &self.game) {
            eprintln!("couldn't save the game: {e}");
        }
        for (conn, c) in &self.conns {
            if c.player.is_some() {
                let request = from.filter(|(f, _)| f == conn).map(|(_, r)| r);
                let _ = c.out.send(ToClient::Game { game: self.game.clone(), request });
            }
        }
    }

    /// Sends `message` to every player in `world` but the one on `from`.
    fn tell_world(&self, world: &str, from: u64, message: ToClient) {
        for (conn, c) in &self.conns {
            if *conn != from && c.player.is_some() && c.world.as_deref() == Some(world) {
                let _ = c.out.send(message.clone());
            }
        }
    }

    fn broadcast(&self, world: &str, change: Change, prints: Vec<([i32; 3], u64)>, package: Option<Package>, from: u64, request: u64) {
        for (conn, c) in &self.conns {
            if c.player.is_some() && c.world.as_deref() == Some(world) {
                let request = (*conn == from).then_some(request);
                let (change, prints, package) = (change.clone(), prints.clone(), package.clone());
                let _ = c.out.send(ToClient::Committed { change, request, prints, package });
            }
        }
    }
}

impl Place {
    /// Appends a change the server makes itself (by the world).
    fn record(&mut self, action: Action) -> Result<Change, String> {
        let id = self.log.changes.last().map_or(1, |c| c.id + 1);
        let change = Change { id, by: Provenance::new(WORLD, "World", None, None, now()), action, sig: None };
        self.state.apply(&change, true)?;
        self.log.changes.push(change.clone());
        self.saved();
        Ok(change)
    }

    /// Fires the timers due by `now` and commits what each package's did; returns the
    /// changes, to send.
    fn run_timers(&mut self, now: u64, players: &[[f32; 3]]) -> Vec<Change> {
        let mut clock = std::mem::take(&mut self.clock);
        let (seed, rules) = (self.log.seed, self.state.rules.logic);
        let run = clock.run(self, now, seed, &rules, players);
        self.clock = clock;
        for error in &run.errors {
            eprintln!("{error}");
        }
        std::mem::take(&mut self.fresh)
    }

    /// Checks a proposal and, if the rules allow it, appends it to the log.
    fn commit(
        &mut self,
        player: Option<String>,
        by: cubewish_rules::log::Provenance,
        action: Action,
        sig: String,
        package: Option<Package>,
    ) -> Result<(Vec<Change>, Vec<([i32; 3], u64)>, Option<Package>), String> {
        let player = player.ok_or("say hello first")?;
        if matches!(action, Action::Timers { .. }) {
            return Err("only the world runs timers".into());
        }
        // Whatever was due goes off first, so the change lands on the world as it is now.
        // Nobody's position is known here, so only timers not anchored to a piece run.
        let mut changes = self.run_timers(now(), &[]);
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
        let mut change = Change { id, by, action, sig: Some(sig) };
        // A load brings the save's state, which the server keeps; whatever came with the
        // proposal doesn't count.
        if let Action::Load { save, name, .. } = &change.action {
            if !self.log.is_admin(&player) {
                return Err("only admins load saves".into());
            }
            if !matches!(self.log.change(*save).map(|c| &c.action), Some(Action::Save { .. })) {
                return Err(format!("change {save} isn't a save"));
            }
            change.action = Action::Load { save: *save, name: name.clone(), state: Some(Box::new(self.state_of(*save))) };
        }
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
        let made = match self.state.apply(&change, true) {
            Ok(edits) => edits,
            Err(why) => {
                if let Some((hash, _)) = &new_package {
                    self.state.logic.packages.remove(hash);
                }
                return Err(why);
            }
        };
        if let Some((hash, package)) = &new_package {
            self.log.packages.insert(hash.clone(), package.clone());
        }
        self.log.changes.push(change.clone());
        if matches!(change.action, Action::Save { .. }) {
            self.keep_state();
        }
        let mut chunks: Vec<_> = made.iter().map(|(p, _)| chunk_of(*p)).collect();
        chunks.sort_unstable_by_key(|c| c.to_array());
        chunks.dedup();
        let prints = chunk_prints(&self.state.edits, &chunks);
        self.saved();
        println!("{} {}", change.by.player_name, change.action.describe());
        changes.push(change);
        Ok((changes, prints, new_package.map(|(_, p)| (*p).clone())))
    }

    fn take_snapshot(&mut self) {
        let snapshot = Snapshot::take(&self.log, &self.state);
        let path = &self.snapshot_path;
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
