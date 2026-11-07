//! Two players on a real server over TCP: joining, committing, refusals, and undo rules.

use std::{
    sync::mpsc::RecvTimeoutError,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use cubelark_net::{Client, ClientEvent, ToClient, ToServer};
use cubelark_rules::{
    identity::Keypair,
    log::{Action, BuildState, Provenance, signing_bytes},
};
use cubelark_server::{ServerConfig, start};

struct Player {
    key: Keypair,
    name: &'static str,
    client: Client,
}

impl Player {
    fn join(addr: &str, seed: u8, name: &'static str) -> Self {
        let key = Keypair::from_seed([seed; 32]);
        let signer = key.clone();
        let client = Client::connect(addr, key.player_id(), name.into(), move |m| signer.sign(m)).unwrap();
        let player = Self { key, name, client };
        match player.next() {
            ToClient::Welcome { .. } => player,
            other => panic!("expected a welcome, got {other:?}"),
        }
    }

    fn next(&self) -> ToClient {
        match self.client.recv_timeout(Duration::from_secs(5)) {
            Ok(ClientEvent::Message(m)) => m,
            Ok(ClientEvent::Closed(why) | ClientEvent::Refused(why)) => panic!("{} was disconnected: {why}", self.name),
            Err(RecvTimeoutError::Timeout) => panic!("{} heard nothing", self.name),
            Err(e) => panic!("{e}"),
        }
    }

    fn propose(&self, request: u64, action: Action) {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        let by = Provenance::new(&self.key.player_id(), self.name, None, None, now);
        let sig = self.key.sign(&signing_bytes(&by, &action));
        self.client.send(ToServer::Propose { request, by, action, sig });
    }
}

fn lamp(build: u64, x: f32, z: f32) -> Action {
    let spec = serde_json::from_str(
        r##"{"name":"Lamp","size":[3,4,3],"palette":[{"name":"iron","hex":"#3d4248","material":"metal"}],
            "shapes":[{"kind":"box","color":"iron","from":[0,0,0],"to":[3,4,3],"radius":null,"wobble":null}]}"##,
    )
    .unwrap();
    Action::PlaceBuild(BuildState {
        build,
        name: "Lamp".into(),
        prompt: String::new(),
        provider: String::new(),
        spec,
        position: [x, 10.0, z],
        yaw: 0.0,
    })
}

#[test]
fn two_players_share_one_ordered_world() {
    let dir = std::env::temp_dir().join(format!("cubelark-server-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let server = start(ServerConfig { data_dir: dir.clone(), port: 0, seed: 1337 }).unwrap();
    let addr = server.addr.to_string();

    // Ada joins first and becomes the admin; Bo joins after.
    let ada = Player::join(&addr, 1, "Ada");
    let bo = Player::join(&addr, 2, "Bo");

    // Ada builds: both hear it, only Ada with her request number.
    ada.propose(7, lamp(1, 100.0, 100.0));
    let id = match ada.next() {
        ToClient::Committed { change, request: Some(7) } => change.id,
        other => panic!("{other:?}"),
    };
    assert!(matches!(bo.next(), ToClient::Committed { request: None, .. }));

    // Bo can't undo Ada's change, or build at the protected spawn.
    bo.propose(1, Action::Revert { change: id });
    assert!(matches!(bo.next(), ToClient::Rejected { request: 1, why } if why.contains("your own")));
    bo.propose(2, lamp(2, 0.0, 0.0));
    assert!(matches!(bo.next(), ToClient::Rejected { request: 2, why } if why.contains("spawn")));

    // A change signed by someone else is refused.
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
    let by = Provenance::new(&bo.key.player_id(), "Bo", None, None, now);
    let forged = ada.key.sign(&signing_bytes(&by, &lamp(3, 120.0, 100.0)));
    bo.client.send(ToServer::Propose { request: 3, by, action: lamp(3, 120.0, 100.0), sig: forged });
    assert!(matches!(bo.next(), ToClient::Rejected { request: 3, why } if why.contains("signed")));

    // Ada undoes her own change; everyone hears it.
    ada.propose(8, Action::Revert { change: id });
    assert!(matches!(ada.next(), ToClient::Committed { request: Some(8), .. }));
    assert!(matches!(bo.next(), ToClient::Committed { .. }));

    // A restarted server has the same log.
    drop((ada, bo));
    let again = start(ServerConfig { data_dir: dir.clone(), port: 0, seed: 1337 }).unwrap();
    let cy = Player::join(&again.addr.to_string(), 3, "Cy");
    drop(cy);
    let saved: cubelark_rules::log::WorldLog =
        serde_json::from_str(&std::fs::read_to_string(dir.join("world.json")).unwrap()).unwrap();
    assert_eq!(saved.changes.len(), 2);
    assert!(saved.changes.iter().all(|c| c.verified()));
    assert!(saved.changes.iter().all(|c| c.by.prompt.is_none()));
    let _ = std::fs::remove_dir_all(&dir);
}
