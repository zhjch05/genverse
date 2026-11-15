//! A player joins a real server by invite, over the internet: through iroh and n0's
//! public relays, as a friend elsewhere would. Needs the internet, so it's ignored by
//! default: `cargo test -p genverse_server --test internet -- --ignored`.

use std::time::{Duration, Instant};

use genverse_net::{
    Client, ClientEvent, ToClient,
    internet::{Host, Tunnel},
};
use genverse_rules::identity::Keypair;
use genverse_server::{ServerConfig, start};

#[test]
#[ignore = "needs the internet"]
fn a_player_joins_by_invite() {
    let dir = std::env::temp_dir().join(format!("genverse-server-internet-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let server = start(ServerConfig {
        data_dir: dir.clone(),
        port: 0,
        seed: 1337,
        flat: true,
    })
    .unwrap();
    let host_key = Keypair::from_seed([1; 32]);
    let door = Host::start(server.addr.port(), host_key.derive("internet host")).unwrap();
    let started = Instant::now();
    while !door.online() && started.elapsed() < Duration::from_secs(40) {
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(door.online(), "the host never reached a relay");

    let tunnel = Tunnel::start(door.invite(), [2; 32]).unwrap();
    let key = Keypair::from_seed([3; 32]);
    let signer = key.clone();
    let client = Client::connect(
        &tunnel.addr().to_string(),
        key.player_id(),
        "Ada".into(),
        move |m| signer.sign(m),
    )
    .unwrap();
    match client.recv_timeout(Duration::from_secs(30)) {
        Ok(ClientEvent::Message(ToClient::Welcome { world, .. })) => assert_eq!(world, "main"),
        Ok(ClientEvent::Closed(why)) => panic!("closed: {why}; tunnel: {:?}", tunnel.error()),
        Ok(ClientEvent::Message(other)) => panic!("expected a welcome, got {other:?}"),
        _ => panic!("no welcome; tunnel: {:?}", tunnel.error()),
    }
    assert_eq!(server.players.load(std::sync::atomic::Ordering::Relaxed), 1);
    drop(client);
    drop(door);
    server.stop();
    let _ = std::fs::remove_dir_all(&dir);
}
