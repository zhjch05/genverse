//! Playing over the internet, the host's game still the server: an invite names the
//! hosting game's [iroh](https://iroh.computer) endpoint, which players dial from
//! anywhere. iroh connects the two games directly through their routers when it can,
//! and through n0's free public relays when it can't; either way it's encrypted.
//!
//! The protocol doesn't change: both ends bridge iroh to plain TCP on this machine.
//! The host's [`Host`] pipes each player to the game server on loopback, and a
//! joining game's [`Tunnel`] listens on loopback for the [`crate::Client`] and pipes
//! it to the host. So the server and the client only ever see TCP.

use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

use iroh::{
    Endpoint, EndpointId, SecretKey,
    dns::{BoxIter, DnsError, DnsResolver, Resolver, TxtRecordData},
    endpoint::{Builder, presets},
};
use tokio::{
    io::{copy_bidirectional, join},
    net::{TcpListener, TcpStream},
    runtime::Runtime,
};

/// The protocol name iroh connections use; bumped with [`crate::VERSION`] so a game
/// can't reach a host that speaks another protocol.
const ALPN: &[u8] = b"genverse/net/7";
const _: () = assert!(crate::VERSION == 7, "bump ALPN with VERSION");
/// What an invite starts with, so it reads as one and isn't taken for an address.
const PREFIX: &str = "gv1";
/// A dialing game opens its stream with this byte: a QUIC stream isn't announced to
/// the host until something is written on it, and the client waits for the server
/// to speak first.
const OPEN: u8 = 0x47;

/// The invite to the endpoint with this key.
fn invite(id: EndpointId) -> String {
    format!("{PREFIX}{id}")
}

/// The endpoint an invite names, if `text` is one.
pub fn parse_invite(text: &str) -> Option<EndpointId> {
    text.trim().strip_prefix(PREFIX)?.parse().ok()
}

/// Whether `text` is an invite rather than an address.
pub fn is_invite(text: &str) -> bool {
    parse_invite(text).is_some()
}

/// Bridges an iroh stream to a TCP one until either side ends.
async fn pipe(
    tcp: &mut TcpStream,
    send: iroh::endpoint::SendStream,
    recv: iroh::endpoint::RecvStream,
) {
    let _ = tcp.set_nodelay(true);
    let mut quic = join(recv, send);
    let _ = copy_bidirectional(tcp, &mut quic).await;
}

/// Looks names up the way the rest of the computer does first (the OS's resolver),
/// then with iroh's own DNS client. iroh's client asks the nameservers directly, which
/// VPNs and proxies that answer DNS themselves (fake-IP tunnels) may not answer at all,
/// leaving the relays unreachable. TXT records only come from iroh's client.
#[derive(Debug)]
struct OsFirst(DnsResolver);

/// How long iroh's own lookups may take.
const DNS_TIMEOUT: Duration = Duration::from_secs(3);

type Lookup<T> = std::pin::Pin<Box<dyn Future<Output = Result<BoxIter<T>, DnsError>> + Send>>;

async fn os_lookup(host: &str) -> Vec<IpAddr> {
    match tokio::net::lookup_host((host.trim_end_matches('.'), 0)).await {
        Ok(found) => found.map(|a| a.ip()).collect(),
        Err(_) => Vec::new(),
    }
}

impl OsFirst {
    fn lookup<T: Send + 'static>(&self, host: String, pick: fn(IpAddr) -> Option<T>) -> Lookup<T> {
        let iroh = self.0.clone();
        Box::pin(async move {
            let mut found: Vec<T> = os_lookup(&host)
                .await
                .into_iter()
                .filter_map(pick)
                .collect();
            if found.is_empty() {
                let v4 = std::any::TypeId::of::<T>() == std::any::TypeId::of::<Ipv4Addr>();
                found = if v4 {
                    iroh.lookup_ipv4(&host, DNS_TIMEOUT)
                        .await?
                        .filter_map(pick)
                        .collect()
                } else {
                    iroh.lookup_ipv6(&host, DNS_TIMEOUT)
                        .await?
                        .filter_map(pick)
                        .collect()
                };
            }
            Ok(Box::new(found.into_iter()) as BoxIter<T>)
        })
    }
}

impl Resolver for OsFirst {
    fn lookup_ipv4(&self, host: String) -> Lookup<Ipv4Addr> {
        self.lookup(host, |ip| match ip {
            IpAddr::V4(v4) => Some(v4),
            IpAddr::V6(_) => None,
        })
    }

    fn lookup_ipv6(&self, host: String) -> Lookup<Ipv6Addr> {
        self.lookup(host, |ip| match ip {
            IpAddr::V6(v6) => Some(v6),
            IpAddr::V4(_) => None,
        })
    }

    fn lookup_txt(&self, host: String) -> Lookup<TxtRecordData> {
        let iroh = self.0.clone();
        Box::pin(async move {
            let found: Vec<_> = iroh.lookup_txt(host, DNS_TIMEOUT).await?.collect();
            Ok(Box::new(found.into_iter()) as BoxIter<_>)
        })
    }

    fn clear_cache(&self) {
        self.0.clear_cache();
    }

    fn reset(&self) -> Box<dyn Resolver> {
        self.0.reset();
        Box::new(OsFirst(self.0.clone()))
    }
}

/// An endpoint with `secret` as its key, on n0's public relays and address lookup.
fn endpoint(secret: [u8; 32]) -> Builder {
    Endpoint::builder(presets::N0)
        .secret_key(SecretKey::from_bytes(&secret))
        .dns_resolver(DnsResolver::custom(OsFirst(DnsResolver::new())))
}

fn runtime() -> Result<Runtime, String> {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("internet")
        .enable_all()
        .build()
        .map_err(|e| format!("couldn't start networking: {e}"))
}

/// Closes the endpoint politely (so peers hear it) and stops the runtime, off the
/// caller's thread.
fn shut(runtime: Runtime, endpoint: Endpoint) {
    let _ = thread::Builder::new()
        .name("internet-close".into())
        .spawn(move || {
            runtime.block_on(async {
                let _ = tokio::time::timeout(Duration::from_secs(2), endpoint.close()).await;
            });
            runtime.shutdown_timeout(Duration::from_secs(1));
        });
}

/// A hosting game's door to the internet: players who dial its invite are piped to
/// the game server on `127.0.0.1:port`. Dropping it closes the door.
pub struct Host {
    invite: String,
    online: Arc<AtomicBool>,
    endpoint: Endpoint,
    runtime: Option<Runtime>,
}

impl Host {
    /// Opens the server on `port` to the internet. `secret` is the endpoint's key: the
    /// same key gives the same invite every time.
    pub fn start(port: u16, secret: [u8; 32]) -> Result<Self, String> {
        let runtime = runtime()?;
        let endpoint = runtime
            .block_on(endpoint(secret).alpns(vec![ALPN.to_vec()]).bind())
            .map_err(|e| format!("couldn't open to the internet: {e}"))?;
        let online = Arc::new(AtomicBool::new(false));
        let (ready, accepting) = (endpoint.clone(), endpoint.clone());
        let flag = online.clone();
        runtime.spawn(async move {
            ready.online().await;
            flag.store(true, Ordering::Relaxed);
        });
        runtime.spawn(async move {
            while let Some(incoming) = accepting.accept().await {
                tokio::spawn(async move {
                    let Ok(connection) = incoming.await else {
                        return;
                    };
                    let Ok((send, mut recv)) = connection.accept_bi().await else {
                        return;
                    };
                    let mut open = [0u8];
                    if recv.read_exact(&mut open).await.is_err() || open[0] != OPEN {
                        connection.close(1u32.into(), b"not genverse");
                        return;
                    }
                    let Ok(mut tcp) = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).await else {
                        connection.close(2u32.into(), b"the game isn't hosting");
                        return;
                    };
                    pipe(&mut tcp, send, recv).await;
                    connection.close(0u32.into(), b"bye");
                });
            }
        });
        Ok(Self {
            invite: invite(endpoint.id()),
            online,
            endpoint,
            runtime: Some(runtime),
        })
    }

    /// What others join with.
    pub fn invite(&self) -> &str {
        &self.invite
    }

    /// Whether it has reached a relay, so the invite works from anywhere.
    pub fn online(&self) -> bool {
        self.online.load(Ordering::Relaxed)
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            shut(runtime, self.endpoint.clone());
        }
    }
}

/// A joining game's way to a host on the internet: it listens on loopback, and each
/// connection the client makes there is piped to the host. It keeps listening, so the
/// client reconnects through it as it would to any server. Dropping it hangs up.
pub struct Tunnel {
    addr: SocketAddr,
    /// Why the last dial failed, if it did.
    error: Arc<Mutex<Option<String>>>,
    endpoint: Endpoint,
    runtime: Option<Runtime>,
}

impl Tunnel {
    /// A tunnel to the host `invite` names. `secret` is this end's key (any fresh one).
    pub fn start(invite: &str, secret: [u8; 32]) -> Result<Self, String> {
        let host = parse_invite(invite).ok_or("that isn't an invite")?;
        let runtime = runtime()?;
        let (endpoint, listener) = runtime.block_on(async {
            let endpoint = endpoint(secret)
                .bind()
                .await
                .map_err(|e| format!("couldn't reach the internet: {e}"))?;
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
                .await
                .map_err(|e| e.to_string())?;
            Ok::<_, String>((endpoint, listener))
        })?;
        let addr = listener.local_addr().map_err(|e| e.to_string())?;
        let error = Arc::new(Mutex::new(None));
        let (dialing, failed) = (endpoint.clone(), error.clone());
        runtime.spawn(async move {
            while let Ok((mut tcp, _)) = listener.accept().await {
                let (endpoint, failed) = (dialing.clone(), failed.clone());
                tokio::spawn(async move {
                    let fail =
                        |why: String| *failed.lock().unwrap_or_else(|e| e.into_inner()) = Some(why);
                    let connection = match endpoint.connect(host, ALPN).await {
                        Ok(connection) => connection,
                        Err(e) => return fail(format!("couldn't reach the host: {e}")),
                    };
                    let (mut send, recv) = match connection.open_bi().await {
                        Ok(stream) => stream,
                        Err(e) => return fail(format!("the host didn't answer: {e}")),
                    };
                    if let Err(e) = send.write_all(&[OPEN]).await {
                        return fail(format!("the host hung up: {e}"));
                    }
                    *failed.lock().unwrap_or_else(|e| e.into_inner()) = None;
                    pipe(&mut tcp, send, recv).await;
                    connection.close(0u32.into(), b"bye");
                });
            }
        });
        Ok(Self {
            addr,
            error,
            endpoint,
            runtime: Some(runtime),
        })
    }

    /// Where the client connects.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Why reaching the host last failed, if it did.
    pub fn error(&self) -> Option<String> {
        self.error.lock().ok()?.clone()
    }
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            shut(runtime, self.endpoint.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io::{BufRead, BufReader, Write},
        net::TcpListener as StdListener,
    };

    use super::*;

    #[test]
    fn invites_name_the_endpoint() {
        let id = SecretKey::from_bytes(&[7; 32]).public();
        let text = invite(id);
        assert!(is_invite(&text));
        assert_eq!(parse_invite(&format!("  {text}\n")), Some(id));
        assert!(!is_invite("192.168.1.5:7878"));
        assert!(!is_invite(&text[PREFIX.len()..]));
    }

    /// Dials a host through n0's relays and DNS, so it needs the internet:
    /// `cargo test -p genverse_net --features internet -- --ignored`.
    #[test]
    #[ignore = "needs the internet"]
    fn a_tunnel_reaches_the_host() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .with_test_writer()
            .try_init();
        // An echo "server" behind the host's door, line by line.
        let server = StdListener::bind("127.0.0.1:0").unwrap();
        let port = server.local_addr().unwrap().port();
        thread::spawn(move || {
            for stream in server.incoming().flatten() {
                thread::spawn(move || {
                    let mut writer = stream.try_clone().unwrap();
                    writer.write_all(b"hello\n").unwrap();
                    for line in BufReader::new(stream).lines().map_while(Result::ok) {
                        writeln!(writer, "echo {line}").unwrap();
                    }
                });
            }
        });
        let mut seed = [0u8; 32];
        seed[0] = 1;
        let host = Host::start(port, seed).unwrap();
        // Reaching a relay takes a few seconds, more on flaky networks (iroh retries).
        for _ in 0..400 {
            if host.online() {
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
        seed[0] = 2;
        let tunnel = Tunnel::start(host.invite(), seed).unwrap();
        // Twice: the tunnel keeps listening, as a reconnect needs.
        for round in 0..2 {
            let stream = std::net::TcpStream::connect(tunnel.addr()).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(30)))
                .unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            reader
                .read_line(&mut line)
                .unwrap_or_else(|e| panic!("round {round}: {e}; {:?}", tunnel.error()));
            assert_eq!(line, "hello\n", "{:?}", tunnel.error());
            writeln!(&stream, "ping {round}").unwrap();
            line.clear();
            reader.read_line(&mut line).unwrap();
            assert_eq!(line, format!("echo ping {round}\n"));
        }
    }
}
