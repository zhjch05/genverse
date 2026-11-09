//! The wire protocol between Genverse clients and a server.
//!
//! A server hosts one game: its worlds, each with its own change log, and the game's
//! own log (which worlds there are, who runs it). Each player is in one world at a
//! time and hears only that world's changes and people, plus the game's changes.
//! The server holds the logs and is the only one who orders changes.
//! A client proposes a change (signed by its player); the server checks it against
//! the rules, appends it, and sends it to everyone, or tells the proposer why not.
//! Later changes win, like Figma's multiplayer: no merging, just one order.
//!
//! Messages are JSON, one per line, over TCP: simple to read and debug, and the rules
//! crate already serializes everything. [`Client`] runs the connection on background
//! threads for the game.

pub mod lan;

use std::{
    io::{BufRead, BufReader, Write},
    net::TcpStream,
    sync::{
        Mutex,
        mpsc::{self, Receiver, RecvTimeoutError, Sender},
    },
    thread,
    time::Duration,
};

use genverse_rules::{
    game::{GameAction, GameLog},
    log::{Action, Change, Provenance, RULES_VERSION, Snapshot, WorldLog},
    logic::Package,
    sculpt::Area,
};
use serde::{Deserialize, Serialize};

/// Bumped when the protocol changes incompatibly.
pub const VERSION: u32 = 6;
pub const DEFAULT_PORT: u16 = 7878;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToServer {
    /// The answer to [`ToClient::Challenge`]: who this is, proven by signing the nonce.
    /// `rules` is the client's [`RULES_VERSION`]: both sides must turn changes into the
    /// same voxels.
    Hello { version: u32, rules: u32, player: String, name: String, sig: String },
    /// A change the player wants made. `request` is the client's own number for it,
    /// echoed in the answer; `sig` is the player's signature of `by` and `action`.
    /// Installing a package the server doesn't have yet brings the package along (the
    /// change names it by hash, so it's signed too).
    Propose {
        request: u64,
        by: Provenance,
        action: Action,
        sig: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        package: Option<Package>,
    },
    /// Goes to another world of the game; the server answers with its welcome.
    Enter { world: String },
    /// A change to the game itself (a new world, a game save), signed like a change.
    ProposeGame { request: u64, by: Provenance, action: GameAction, sig: String },
    /// Where the player is and what they're doing, for others to see. Not kept.
    Presence(Presence),
    /// The edits in these chunks, because ours disagree with the server's fingerprints.
    FetchChunks { chunks: Vec<[i32; 3]> },
}

/// What others see of a player, live: like Figma's cursors and selections. Sent when it
/// changes, and never stored.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Presence {
    /// Feet, in world meters.
    pub position: [f32; 3],
    pub yaw: f32,
    /// The land they've selected, or are previewing a change to.
    pub area: Option<Area>,
    /// They're previewing a land change in `area` (not kept yet).
    pub previewing: bool,
    /// The ghost of a build draft: bottom-center, turn, and size in meters.
    pub draft: Option<([f32; 3], f32, [f32; 3])>,
    /// The placed build they've selected.
    pub selected: Option<u64>,
    /// E.g. "Claude Code is designing…".
    pub status: Option<String>,
    /// The name they go by now, which can change while they play; empty means the one
    /// they joined with.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    /// The marker color they picked, as an index into the game's set; none means the one
    /// their id gives them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<u8>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToClient {
    /// Sent on connect; the client signs `nonce` to prove who it is.
    Challenge { version: u32, rules: u32, nonce: String },
    /// The world the player is in so far (named `world` in `game`), with a snapshot to
    /// replay from when there is one. Sent on joining and on entering another world.
    Welcome { game: GameLog, world: String, log: WorldLog, snapshot: Option<Snapshot> },
    /// The game changed (a world made or removed, a save): for everyone in it.
    Game { game: GameLog, request: Option<u64> },
    /// A change the server accepted, for everyone. `request` is set only for the player
    /// who proposed it. `prints` fingerprints the chunks it changed, as the server has
    /// them, so players can check theirs.
    /// `package` comes with the change that first installs it.
    Committed {
        change: Change,
        request: Option<u64>,
        prints: Vec<([i32; 3], u64)>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        package: Option<Package>,
    },
    /// The edits in the chunks a player asked for, as `[x, y, z, block]`.
    Chunks { chunks: Vec<([i32; 3], Vec<[i32; 4]>)> },
    /// Why the server refused a player's proposal.
    Rejected { request: u64, why: String },
    /// The server is closing this connection.
    Bye { why: String },
    /// Another player's presence (sent when it changes, and to newcomers).
    Presence { player: String, name: String, presence: Presence },
    /// A player left.
    Gone { player: String },
}

/// What a player signs to log in.
pub fn hello_bytes(nonce: &str, player: &str, name: &str) -> Vec<u8> {
    format!("genverse hello {nonce} {player} {name}").into_bytes()
}

/// Writes one message as a line.
pub fn write_message(stream: &mut impl Write, message: &impl Serialize) -> std::io::Result<()> {
    let mut line = serde_json::to_string(message)?;
    line.push('\n');
    stream.write_all(line.as_bytes())?;
    stream.flush()
}

/// Reads messages line by line until the stream ends or a line doesn't parse.
pub fn read_messages<T: for<'de> Deserialize<'de>>(stream: TcpStream) -> impl Iterator<Item = Result<T, String>> {
    BufReader::new(stream).lines().map(|line| {
        let line = line.map_err(|e| e.to_string())?;
        serde_json::from_str(&line).map_err(|e| format!("bad message: {e}"))
    })
}

pub enum ClientEvent {
    Message(ToClient),
    /// The connection ended, and why. Worth reconnecting.
    Closed(String),
    /// This game and the server can't play together (versions differ). Not worth
    /// reconnecting.
    Refused(String),
}

/// A connection to a server, run on background threads. Messages in and out go
/// through channels, so the game polls it once a frame.
pub struct Client {
    events: Mutex<Receiver<ClientEvent>>,
    outgoing: Sender<ToServer>,
    /// To hang up when the client is dropped (the reader thread holds the socket open).
    socket: TcpStream,
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.socket.shutdown(std::net::Shutdown::Both);
    }
}

impl Client {
    /// Connects to `addr` as `player` (a public key) named `name`; `sign` signs the
    /// login challenge with the player's secret key.
    pub fn connect(
        addr: &str,
        player: String,
        name: String,
        sign: impl Fn(&[u8]) -> String + Send + 'static,
    ) -> Result<Self, String> {
        let stream = TcpStream::connect(addr).map_err(|e| format!("couldn't reach {addr}: {e}"))?;
        let _ = stream.set_nodelay(true);
        let mut writer = stream.try_clone().map_err(|e| e.to_string())?;
        let socket = stream.try_clone().map_err(|e| e.to_string())?;
        let (events_tx, events) = mpsc::channel();
        let (outgoing, outgoing_rx) = mpsc::channel::<ToServer>();

        let hello = outgoing.clone();
        let reader_events = events_tx.clone();
        thread::Builder::new()
            .name("net-read".into())
            .spawn(move || {
                for message in read_messages::<ToClient>(stream) {
                    match message {
                        Ok(ToClient::Challenge { version, rules, nonce }) => {
                            let mismatch = if version != VERSION {
                                Some(format!("the server speaks protocol {version}, this game {VERSION}"))
                            } else if rules != RULES_VERSION {
                                Some(format!("the server runs world rules {rules}, this game {RULES_VERSION}; update both"))
                            } else {
                                None
                            };
                            if let Some(why) = mismatch {
                                let _ = reader_events.send(ClientEvent::Refused(why));
                                return;
                            }
                            let sig = sign(&hello_bytes(&nonce, &player, &name));
                            let login = ToServer::Hello {
                                version: VERSION,
                                rules: RULES_VERSION,
                                player: player.clone(),
                                name: name.clone(),
                                sig,
                            };
                            let _ = hello.send(login);
                        }
                        Ok(message) => {
                            if reader_events.send(ClientEvent::Message(message)).is_err() {
                                return;
                            }
                        }
                        Err(e) => {
                            let _ = reader_events.send(ClientEvent::Closed(e));
                            return;
                        }
                    }
                }
                let _ = reader_events.send(ClientEvent::Closed("the server closed the connection".into()));
            })
            .map_err(|e| e.to_string())?;
        thread::Builder::new()
            .name("net-write".into())
            .spawn(move || {
                for message in outgoing_rx {
                    if let Err(e) = write_message(&mut writer, &message) {
                        let _ = events_tx.send(ClientEvent::Closed(e.to_string()));
                        return;
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(Self { events: Mutex::new(events), outgoing, socket })
    }

    pub fn send(&self, message: ToServer) {
        let _ = self.outgoing.send(message);
    }

    /// Everything that arrived since the last poll.
    pub fn poll(&self) -> Vec<ClientEvent> {
        self.events.lock().map(|rx| rx.try_iter().collect()).unwrap_or_default()
    }

    /// Waits up to `timeout` for the next event.
    pub fn recv_timeout(&self, timeout: Duration) -> Result<ClientEvent, RecvTimeoutError> {
        self.events.lock().map_err(|_| RecvTimeoutError::Disconnected)?.recv_timeout(timeout)
    }
}

#[cfg(test)]
mod tests {
    use std::net::TcpListener;

    use super::*;

    #[test]
    fn mismatched_rules_are_refused_not_retried() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let challenge = ToClient::Challenge { version: VERSION, rules: RULES_VERSION + 1, nonce: "n".into() };
            write_message(&mut stream, &challenge).unwrap();
            std::thread::sleep(Duration::from_millis(500));
        });
        let client = Client::connect(&addr, "p".into(), "P".into(), |_| String::new()).unwrap();
        match client.recv_timeout(Duration::from_secs(5)) {
            Ok(ClientEvent::Refused(why)) => assert!(why.contains("world rules")),
            _ => panic!("expected a refusal"),
        }
    }
}
