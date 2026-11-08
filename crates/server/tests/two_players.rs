//! Two players on a real server over TCP: joining, committing, refusals, and undo rules.

use std::{
    sync::mpsc::RecvTimeoutError,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use cubewish_net::{Client, ClientEvent, ToClient, ToServer};
use cubewish_rules::{
    identity::Keypair,
    log::{Action, BuildState, Provenance, signing_bytes},
    logic::{
        Effect, EntityId, Intent, Package, Scope,
        data::{DefSpec, GameSpec, ModelSpec, ScriptSpec},
    },
};
use cubewish_server::{ServerConfig, start};

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
        self.propose_with(request, action, None);
    }

    fn propose_with(&self, request: u64, action: Action, package: Option<Package>) {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        let by = Provenance::new(&self.key.player_id(), self.name, None, None, now);
        let sig = self.key.sign(&signing_bytes(&by, &action));
        self.client.send(ToServer::Propose { request, by, action, sig, package });
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

fn pond() -> Action {
    let op = serde_json::json!({
        "kind": "water", "area": {"shape": "ellipse", "x": 1.0, "z": 1.0, "w": 4.0, "d": 4.0}, "points": null,
        "amount_m": null, "level_m": null, "depth_m": 1.0, "width_m": null, "edge_m": 1.0, "material": null,
        "item": null, "density": null
    });
    Action::Terrain {
        name: "Pond".into(),
        area: cubewish_rules::sculpt::Area { min: [400, 400], size: [24, 24] },
        spec: serde_json::from_value(serde_json::json!({ "name": "Pond", "ops": [op] })).unwrap(),
        changed: 500,
    }
}

#[test]
fn two_players_share_one_ordered_world() {
    let dir = std::env::temp_dir().join(format!("cubewish-server-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let server = start(ServerConfig { data_dir: dir.clone(), port: 0, seed: 1337, flat: false }).unwrap();
    let addr = server.addr.to_string();

    // Ada joins first and becomes the admin; Bo joins after.
    let ada = Player::join(&addr, 1, "Ada");
    let bo = Player::join(&addr, 2, "Bo");

    // Ada builds: both hear it, only Ada with her request number.
    ada.propose(7, lamp(1, 100.0, 100.0));
    let id = match ada.next() {
        ToClient::Committed { change, request: Some(7), .. } => change.id,
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
    bo.client.send(ToServer::Propose { request: 3, by, action: lamp(3, 120.0, 100.0), sig: forged, package: None });
    assert!(matches!(bo.next(), ToClient::Rejected { request: 3, why } if why.contains("signed")));

    // Ada undoes her own change; everyone hears it.
    ada.propose(8, Action::Revert { change: id });
    assert!(matches!(ada.next(), ToClient::Committed { request: Some(8), .. }));
    assert!(matches!(bo.next(), ToClient::Committed { .. }));

    // A land change comes with fingerprints of the chunks it touched; fetching those
    // chunks gives edits that match them.
    ada.propose(9, pond());
    let prints = match ada.next() {
        ToClient::Committed { request: Some(9), prints, .. } => prints,
        other => panic!("{other:?}"),
    };
    assert!(!prints.is_empty());
    assert!(matches!(bo.next(), ToClient::Committed { prints: p, .. } if p == prints));
    bo.client.send(ToServer::FetchChunks { chunks: prints.iter().map(|(c, _)| *c).collect() });
    let fetched = match bo.next() {
        ToClient::Chunks { chunks } => chunks,
        other => panic!("{other:?}"),
    };
    let edits: std::collections::HashMap<_, _> = fetched
        .iter()
        .flat_map(|(_, e)| e.iter().map(|[x, y, z, id]| (cubewish_rules::IVec3::new(*x, *y, *z), *id as u8)))
        .collect();
    let chunks: Vec<_> = prints.iter().map(|(c, _)| cubewish_rules::IVec3::from_array(*c)).collect();
    assert_eq!(cubewish_rules::log::chunk_prints(&edits, &chunks), prints);

    // Presence goes to others, never to the sender, and isn't kept. It carries a rename
    // and a marker color made after joining.
    let here = cubewish_net::Presence {
        position: [1.0, 2.0, 3.0],
        status: Some("designing".into()),
        name: "Ada L.".into(),
        color: Some(3),
        ..Default::default()
    };
    ada.client.send(ToServer::Presence(here.clone()));
    match bo.next() {
        ToClient::Presence { name, presence, .. } => assert_eq!((name.as_str(), presence), ("Ada", here)),
        other => panic!("{other:?}"),
    }
    // A newcomer hears who's here; when someone leaves, others hear that too.
    let cy = Player::join(&addr, 3, "Cy");
    assert!(matches!(cy.next(), ToClient::Presence { name, .. } if name == "Ada"));
    drop(cy);
    assert!(matches!(ada.next(), ToClient::Gone { .. }));

    // A restarted server has the same log.
    drop((ada, bo));
    let again = start(ServerConfig { data_dir: dir.clone(), port: 0, seed: 1337, flat: false }).unwrap();
    let dee = Player::join(&again.addr.to_string(), 4, "Dee");
    drop(dee);
    let saved = cubewish_rules::store::LogFile::open(&dir).unwrap().1.unwrap();
    assert_eq!(saved.changes.len(), 3);
    assert!(saved.changes.iter().all(|c| c.verified()));
    assert!(saved.changes.iter().all(|c| c.by.prompt.is_none()));
    let _ = std::fs::remove_dir_all(&dir);
}

/// The rules crate's Sokoban test script, as a package.
fn sokoban() -> GameSpec {
    GameSpec {
        name: "sokoban".into(),
        title: "Sokoban".into(),
        defs: vec![DefSpec { name: "board".into(), kind: "fixture".into(), title: "Sokoban board".into(), ..Default::default() }],
        scripts: vec![ScriptSpec { name: "main.luau".into(), source: include_str!("../../rules/tests/sokoban.luau").into() }],
        models: models(),
    }
}

/// The voxel models the pieces look like (as in the rules crate's test).
fn models() -> Vec<ModelSpec> {
    let model = |name: &str, json: &str| ModelSpec { name: name.into(), build: serde_json::from_str(json).unwrap() };
    vec![
        model("floor", r##"{"name":"Floor","size":[64,1,48],"palette":[{"name":"sand","hex":"#d8c8a8","material":"matte"}],
            "shapes":[{"kind":"box","color":"sand","from":[0,0,0],"to":[64,1,48],"radius":null,"wobble":null}]}"##),
        model("wall", r##"{"name":"Wall","size":[8,8,8],"palette":[{"name":"stone","hex":"#8a857e","material":"matte"}],
            "shapes":[{"kind":"box","color":"stone","from":[0,0,0],"to":[8,8,8],"radius":null,"wobble":0.15}]}"##),
        model("box", r##"{"name":"Box","size":[6,6,6],"palette":[{"name":"wood","hex":"#b07a45","material":"matte"}],
            "shapes":[{"kind":"box","color":"wood","from":[0,0,0],"to":[6,6,6],"radius":null,"wobble":null}]}"##),
        model("target", r##"{"name":"Target","size":[6,1,6],"palette":[{"name":"mark","hex":"#e5534b","material":"glow"}],
            "shapes":[{"kind":"box","color":"mark","from":[0,0,0],"to":[6,1,6],"radius":null,"wobble":null}]}"##),
    ]
}


#[test]
fn packages_travel_with_the_change_that_installs_them() {
    let dir = std::env::temp_dir().join(format!("cubewish-packages-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let server = start(ServerConfig { data_dir: dir.clone(), port: 0, seed: 1337, flat: false }).unwrap();
    let addr = server.addr.to_string();
    let ada = Player::join(&addr, 1, "Ada");
    let bo = Player::join(&addr, 2, "Bo");

    let package = sokoban().to_package(1);
    let reference = package.reference();
    let install = Action::InstallPackage { package: reference.clone(), scope: Scope::World };
    // Only admins change the game's rules (Ada, the first to join, is one).
    bo.propose_with(1, install.clone(), Some(package.clone()));
    assert!(matches!(bo.next(), ToClient::Rejected { why, .. } if why.contains("only admins")));
    // Without the package, or with one that isn't the one named, it's refused.
    ada.propose(1, install.clone());
    assert!(matches!(ada.next(), ToClient::Rejected { why, .. } if why.contains("wasn't uploaded")));
    ada.propose_with(2, install.clone(), Some(sokoban().to_package(2)));
    assert!(matches!(ada.next(), ToClient::Rejected { why, .. } if why.contains("doesn't match")));
    // With it, everyone gets the package along with the change.
    ada.propose_with(3, install, Some(package.clone()));
    assert!(matches!(ada.next(), ToClient::Committed { request: Some(3), package: Some(_), .. }));
    match bo.next() {
        ToClient::Committed { package: Some(p), .. } => assert_eq!(p.hash(), reference.hash),
        other => panic!("{other:?}"),
    }

    // The server runs the scripts: Bo puts a board down, walks in, and pushes a box.
    let place = Action::Intent { intent: Intent::Place { item: "sokoban:board".into(), position: [60.0, 10.0, 60.0] } };
    bo.propose(4, place);
    let board = match bo.next() {
        ToClient::Committed { change, request: Some(4), package: None, .. } => EntityId { change: change.id, n: 0 },
        other => panic!("{other:?}"),
    };
    assert!(matches!(ada.next(), ToClient::Committed { request: None, .. }));
    for (request, key) in [(5, "enter"), (6, "east"), (7, "east")] {
        bo.propose(request, Action::Intent { intent: Intent::Key { entity: board, key: key.into() } });
        assert!(matches!(bo.next(), ToClient::Committed { request: Some(r), .. } if r == request));
    }
    // A key no game takes is refused.
    bo.propose(8, Action::Intent { intent: Intent::Key { entity: board, key: "jump".into() } });
    assert!(matches!(bo.next(), ToClient::Rejected { why, .. } if why.contains("isn't a key")));

    // A newcomer gets the package in the log.
    let cy = Player::join(&addr, 3, "Cy");
    drop((ada, bo, cy));
    let saved = cubewish_rules::store::LogFile::open(&dir).unwrap().1.unwrap();
    assert!(saved.packages.contains_key(&reference.hash));
    let state = cubewish_rules::log::replay(&saved);
    assert!(state.problems.is_empty(), "{:?}", state.problems);
    let bo_id = Keypair::from_seed([2; 32]).player_id();
    let game = &state.logic.state;
    assert!(game.entities().any(|(_, e)| e.components.get("pawn").is_some_and(|p| *p == bo_id.as_str())));
    let pushed = game.entities().filter_map(|(_, e)| e.components.get("sokoban:box")).any(|b| b["col"] == 6.0);
    assert!(pushed, "the first box sits on its target");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_server_runs_timers() {
    let dir = std::env::temp_dir().join(format!("cubewish-timers-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let server = start(ServerConfig { data_dir: dir.clone(), port: 0, seed: 1337, flat: true }).unwrap();
    let addr = server.addr.to_string();
    let ada = Player::join(&addr, 1, "Ada");
    let bo = Player::join(&addr, 2, "Bo");
    let script = r#"
    function on_place(ev)
      local id = spawn(ev.pos, { look = { model = "box" }, label = "Bell" })
      after(1, "ring", { bell = id, who = ev.player })
    end
    function on_timer(ev)
      set(ev.data.bell, "label", "Rang")
      notify(ev.data.who, "Ring!")
    end
    "#;
    let models = sokoban().models;
    let bell = GameSpec {
        name: "bell".into(),
        title: "Bell".into(),
        defs: vec![DefSpec { name: "bell".into(), kind: "fixture".into(), title: "Bell".into(), ..Default::default() }],
        scripts: vec![ScriptSpec { name: "main.luau".into(), source: script.into() }],
        models,
    };
    let package = bell.to_package(1);
    let install = Action::InstallPackage { package: package.reference(), scope: Scope::World };
    ada.propose_with(1, install, Some(package));
    assert!(matches!(ada.next(), ToClient::Committed { request: Some(1), .. }));
    assert!(matches!(bo.next(), ToClient::Committed { .. }));
    bo.propose(1, Action::Intent { intent: Intent::Place { item: "bell:bell".into(), position: [4.0, 9.0, 4.0] } });
    assert!(matches!(bo.next(), ToClient::Committed { request: Some(1), .. }));
    assert!(matches!(ada.next(), ToClient::Committed { .. }));
    // A second or two later, the world rings it, for everyone.
    let bo_id = Keypair::from_seed([2; 32]).player_id();
    for player in [&ada, &bo] {
        match player.next() {
            ToClient::Committed { change, request: None, .. } => {
                assert_eq!(change.by.player, cubewish_rules::log::WORLD);
                let Action::Timers { package, effects, .. } = change.action else { panic!("{:?}", change.action) };
                assert_eq!(package, "bell");
                assert!(effects.iter().any(|e| matches!(e, Effect::Notify { player, text } if *player == bo_id && text == "Ring!")));
            }
            other => panic!("{other:?}"),
        }
    }
    // Players can't make timers go off themselves.
    bo.propose(2, Action::Timers { package: "bell".into(), until: 0, effects: Vec::new() });
    assert!(matches!(bo.next(), ToClient::Rejected { why, .. } if why.contains("only the world")));
    drop((ada, bo));
    let saved = cubewish_rules::store::LogFile::open(&dir).unwrap().1.unwrap();
    let state = cubewish_rules::log::replay(&saved);
    assert!(state.problems.is_empty(), "{:?}", state.problems);
    assert!(state.logic.state.entities().any(|(_, e)| e.components["label"] == "Rang"));
    assert_eq!(state.logic.state.timers().count(), 0);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn saves_load_for_everyone() {
    let dir = std::env::temp_dir().join(format!("cubewish-saves-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let server = start(ServerConfig { data_dir: dir.clone(), port: 0, seed: 1337, flat: true }).unwrap();
    let addr = server.addr.to_string();
    let ada = Player::join(&addr, 1, "Ada");
    let bo = Player::join(&addr, 2, "Bo");
    let committed = |p: &Player| match p.next() {
        ToClient::Committed { change, .. } => change.id,
        other => panic!("{other:?}"),
    };
    ada.propose(1, lamp(1, 4.0, 4.0));
    committed(&ada);
    committed(&bo);
    // Anyone saves.
    bo.propose(1, Action::Save { name: "before".into() });
    let save = committed(&bo);
    committed(&ada);
    ada.propose(2, lamp(2, 12.0, 4.0));
    committed(&ada);
    committed(&bo);
    // Only admins load, for everyone.
    bo.propose(2, Action::Restore { to: save });
    assert!(matches!(bo.next(), ToClient::Rejected { why, .. } if why.contains("only admins")));
    ada.propose(3, Action::Restore { to: save });
    committed(&ada);
    committed(&bo);
    drop((ada, bo));
    let saved = cubewish_rules::store::LogFile::open(&dir).unwrap().1.unwrap();
    let state = cubewish_rules::log::replay(&saved);
    assert_eq!(state.builds.keys().copied().collect::<Vec<_>>(), vec![1]);
    let _ = std::fs::remove_dir_all(&dir);
}
