//! The wire protocol between Cubelark clients and a server.
//!
//! The server holds the world's change log and is the only one who orders changes.
//! A client proposes a change (signed by its player); the server checks it against
//! the rules, appends it, and sends it to everyone, or tells the proposer why not.
//! Later changes win, like Figma's multiplayer: no merging, just one order.
//!
//! Messages are JSON, one per line, over TCP: simple to read and debug, and the rules
//! crate already serializes everything. [`Client`] runs the connection on background
//! threads for the game.

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

use cubelark_rules::log::{Action, Change, Provenance, WorldLog};
use serde::{Deserialize, Serialize};

/// Bumped when the protocol changes incompatibly.
pub const VERSION: u32 = 1;
pub const DEFAULT_PORT: u16 = 7878;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToServer {
    /// The answer to [`ToClient::Challenge`]: who this is, proven by signing the nonce.
    Hello { version: u32, player: String, name: String, sig: String },
    /// A change the player wants made. `request` is the client's own number for it,
    /// echoed in the answer; `sig` is the player's signature of `by` and `action`.
    Propose { request: u64, by: Provenance, action: Action, sig: String },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToClient {
    /// Sent on connect; the client signs `nonce` to prove who it is.
    Challenge { version: u32, nonce: String },
    /// The world so far.
    Welcome { log: WorldLog },
    /// A change the server accepted, for everyone. `request` is set only for the player
    /// who proposed it.
    Committed { change: Change, request: Option<u64> },
    /// Why the server refused a player's proposal.
    Rejected { request: u64, why: String },
    /// The server is closing this connection.
    Bye { why: String },
}

/// What a player signs to log in.
pub fn hello_bytes(nonce: &str, player: &str, name: &str) -> Vec<u8> {
    format!("cubelark hello {nonce} {player} {name}").into_bytes()
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
    /// The connection ended, and why.
    Closed(String),
}

/// A connection to a server, run on background threads. Messages in and out go
/// through channels, so the game polls it once a frame.
pub struct Client {
    events: Mutex<Receiver<ClientEvent>>,
    outgoing: Sender<ToServer>,
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
        let (events_tx, events) = mpsc::channel();
        let (outgoing, outgoing_rx) = mpsc::channel::<ToServer>();

        let hello = outgoing.clone();
        let reader_events = events_tx.clone();
        thread::Builder::new()
            .name("net-read".into())
            .spawn(move || {
                for message in read_messages::<ToClient>(stream) {
                    match message {
                        Ok(ToClient::Challenge { version, nonce }) => {
                            if version != VERSION {
                                let why = format!("the server speaks protocol {version}, this game {VERSION}");
                                let _ = reader_events.send(ClientEvent::Closed(why));
                                return;
                            }
                            let sig = sign(&hello_bytes(&nonce, &player, &name));
                            let login = ToServer::Hello { version: VERSION, player: player.clone(), name: name.clone(), sig };
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
        Ok(Self { events: Mutex::new(events), outgoing })
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
