//! The engine can carry a real game: Sokoban, written as a package script, played
//! through the log and replayed.

use cubewish_rules::{
    log::{Action, Provenance, WorldLog, replay},
    logic::{
        EntityId, Intent, Scope,
        data::{DefSpec, GameSpec, ScriptSpec},
    },
};

fn sokoban() -> GameSpec {
    GameSpec {
        name: "sokoban".into(),
        title: "Sokoban".into(),
        defs: vec![DefSpec { name: "board".into(), kind: "fixture".into(), title: "Sokoban board".into(), ..Default::default() }],
        scripts: vec![ScriptSpec { name: "main.luau".into(), source: include_str!("sokoban.luau").into() }],
    }
}

fn by(player: &str, at: u64) -> Provenance {
    Provenance::new(player, player, None, None, at)
}

#[test]
fn sokoban_plays_through_the_log() {
    let mut log = WorldLog { flat: true, ..WorldLog::new(7) };
    log.admins = vec!["admin".into()];
    let package = sokoban().to_package(1);
    assert!(package.check().is_ok(), "{:?}", package.check());
    let reference = package.reference();
    log.add_package(package);
    log.push(by("admin", 0), Action::InstallPackage { package: reference, scope: Scope::World });
    let place = Intent::Place { item: "sokoban:board".into(), position: [40.0, 9.0, 40.0] };
    let placed = log.push(by("ada", 1), Action::Intent { intent: place });
    let board = EntityId { change: placed, n: 0 };
    let mut t = 2;
    let mut key = |log: &mut WorldLog, key: &str| {
        t += 1;
        log.push(by("ada", t), Action::Intent { intent: Intent::Key { entity: board, key: key.into() } })
    };
    key(&mut log, "enter");
    // East twice pushes the first box onto its target; north into the wall does nothing;
    // round to the second box and down onto its target.
    for k in ["east", "east", "north", "north", "west", "south", "south"] {
        key(&mut log, k);
    }

    let state = replay(&log);
    assert!(state.problems.is_empty(), "{:?}", state.problems);
    let game = &state.logic.state;
    let label = |game: &cubewish_rules::logic::GameState| game.get(board).unwrap().components["label"].clone();
    assert_eq!(label(game), "Sokoban · solved!");
    // 6×8 board: 1 floor/zone + 24 walls + 2 targets + 2 boxes + the pawn.
    assert_eq!(game.len(), 30);
    let pawn = game.entities().find(|(_, e)| e.components.contains_key("pawn")).unwrap().1;
    assert_eq!(pawn.components["pawn"], "ada");

    // Reset puts the boxes back; leaving takes the pawn away. And it all replays the same.
    key(&mut log, "reset");
    key(&mut log, "leave");
    let state = replay(&log);
    assert!(state.problems.is_empty(), "{:?}", state.problems);
    assert_eq!(label(&state.logic.state), "Sokoban");
    assert_eq!(state.logic.state.len(), 29);
    let again: WorldLog = serde_json::from_str(&serde_json::to_string(&log).unwrap()).unwrap();
    let positions = |log: &WorldLog| replay(log).logic.state.entities().map(|(id, e)| (id, e.position)).collect::<Vec<_>>();
    assert_eq!(positions(&log), positions(&again));
}
