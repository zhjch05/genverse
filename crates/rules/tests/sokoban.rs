//! The engine can carry a real game: Sokoban, written as a package script, played
//! through the log and replayed.

use genverse_rules::{
    log::{Action, Provenance, Replay, WORLD, WorldLog, replay},
    logic::{
        EntityId, Intent, Scope,
        data::{DefSpec, GameSpec, ModelSpec, ScriptSpec},
    },
};

fn sokoban() -> GameSpec {
    GameSpec { shaders: Default::default(),
        remove: None, rename: None,
        name: "sokoban".into(),
        title: "Sokoban".into(),
        defs: vec![DefSpec { name: "board".into(), kind: "fixture".into(), title: "Sokoban board".into(), ..Default::default() }],
        scripts: vec![ScriptSpec { name: "main.luau".into(), source: include_str!("sokoban.luau").into() }],
        models: models(),
    }
}

/// The voxel models the pieces look like.
pub fn models() -> Vec<ModelSpec> {
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


/// `player` stands at `at` when the world's clock runs at `now`, as a server sees them:
/// regions they walk into hear about it.
fn stand(log: &mut WorldLog, player: &str, at: [f32; 3], now: u64) {
    use genverse_rules::logic::{Clockwork, Effect, Ground, LogicRules, TimerHost};
    struct Host<'a> {
        log: &'a mut WorldLog,
        state: Replay,
    }
    impl Ground for Host<'_> {
        fn heights(&self, _min: [i32; 2], _step: i32, _count: [i32; 2]) -> Option<Vec<Option<i32>>> {
            None
        }
    }
    impl TimerHost for Host<'_> {
        fn logic(&self) -> &genverse_rules::logic::Logic {
            &self.state.logic
        }
        fn next_change(&self) -> u64 {
            self.log.changes.last().unwrap().id + 1
        }
        fn record(&mut self, package: &str, now: u64, effects: Vec<Effect>) -> Result<(), String> {
            let id = self.log.push(by(WORLD, now), Action::Timers { package: package.into(), until: now, effects });
            let change = self.log.changes.iter().find(|c| c.id == id).unwrap().clone();
            self.state.apply(&change, false).map(|_| ())
        }
    }
    let seed = log.seed;
    let state = replay(log);
    let mut clock = Clockwork::new(now - 1);
    let run = clock.run(&mut Host { log, state }, now, seed, &LogicRules::default(), &[(player.to_string(), at)]);
    assert!(run.errors.is_empty(), "{:?}", run.errors);
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
    // Ada walks onto the board: she gets her piece.
    stand(&mut log, "ada", [40.0, 9.0, 40.0], 2);
    // East twice pushes the first box onto its target; north into the wall does nothing;
    // round to the second box and down onto its target.
    for k in ["east", "east", "north", "north", "west", "south", "south"] {
        key(&mut log, k);
    }

    let state = replay(&log);
    assert!(state.problems.is_empty(), "{:?}", state.problems);
    let game = &state.logic.state;
    let label = |game: &genverse_rules::logic::GameState| game.get(board).unwrap().components["label"].clone();
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

#[test]
fn trials_show_what_a_package_does() {
    let package = sokoban().to_package(1);
    let steps: Vec<String> = ["place board", "east", "east", "jump"].iter().map(|s| s.to_string()).collect();
    let report = genverse_rules::logic::trial::play(&package, &steps);
    println!("{report}");
    // The board, 24 walls, 2 targets and 2 boxes.
    assert!(report.contains("1. place board: ok, 29 change(s)"), "{report}");
    assert!(report.contains("entered the Sokoban"), "walking on gets the piece:\n{report}");
    assert!(report.contains("3. east: ok"), "{report}");
    assert!(report.contains("4. jump: REFUSED"), "{report}");
    assert!(report.contains("  w...@b.w"), "the pushed box shows on the map:\n{report}");
    let mut broken = sokoban();
    broken.scripts[0].source = "function on_place(ev) spawn(ev.pos, { look = { model = 'barrel' } }) end".into();
    let report = genverse_rules::logic::trial::play(&broken.to_package(1), &steps[..1]);
    assert!(report.contains("REFUSED") && report.contains("models (box, floor, target, wall)"), "{report}");
}
