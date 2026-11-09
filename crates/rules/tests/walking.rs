//! Pieces that move on their own: walking, routines, wandering and spawners. The engine
//! runs them on timers, records what they did, and replay never looks for a path again.

use cubewish_rules::{
    log::{Action, Provenance, Replay, WORLD, WorldLog, replay},
    logic::{
        Clockwork, Effect, EntityId, Ground, Intent, LogicRules, Scope, TimerHost,
        data::{DefSpec, GameSpec, ModelSpec, ScriptSpec, Stop},
        walk::position_at,
    },
    terrain::{FLAT_GROUND, VOXEL_SIZE},
};

fn by(player: &str, at: u64) -> Provenance {
    Provenance::new(player, player, None, None, at)
}

fn model(name: &str) -> ModelSpec {
    let build = r##"{"name":"Piece","size":[4,8,4],"palette":[{"name":"skin","hex":"#e0b090","material":"matte"}],
        "shapes":[{"kind":"box","color":"skin","from":[0,0,0],"to":[4,8,4],"radius":null,"wobble":null}]}"##;
    ModelSpec { name: name.into(), build: serde_json::from_str(build).unwrap() }
}

/// Where pieces stand in a flat world.
const Y: f32 = (FLAT_GROUND + 1) as f32 * VOXEL_SIZE;

/// A flat world with `game` installed everywhere.
fn world(game: &GameSpec) -> WorldLog {
    let mut log = WorldLog { flat: true, ..WorldLog::new(7) };
    log.admins = vec!["admin".into()];
    let package = game.to_package(1);
    assert!(package.check().is_ok(), "{:?}", package.check());
    let reference = package.reference();
    log.add_package(package);
    log.push(by("admin", 100), Action::InstallPackage { package: reference, scope: Scope::World });
    log
}

/// Runs the timers due by `now` with players at `players`, as a server does.
fn tick(clock: &mut Clockwork, log: &mut WorldLog, now: u64, players: &[[f32; 3]]) -> Vec<String> {
    struct Host<'a> {
        log: &'a mut WorldLog,
        state: Replay,
    }
    impl Ground for Host<'_> {
        fn heights(&self, min: [i32; 2], step: i32, count: [i32; 2]) -> Option<Vec<Option<i32>>> {
            self.state.heights(min, step, count)
        }
    }
    impl TimerHost for Host<'_> {
        fn logic(&self) -> &cubewish_rules::logic::Logic {
            &self.state.logic
        }
        fn next_change(&self) -> u64 {
            self.log.changes.last().unwrap().id + 1
        }
        fn record(&mut self, package: &str, now: u64, effects: Vec<Effect>) -> Result<(), String> {
            let id = self.log.push(by(WORLD, now), Action::Timers { package: package.into(), until: now, effects });
            let change = self.log.changes.iter().find(|c| c.id == id).unwrap().clone();
            self.state.apply(&change, true).map(|_| ())
        }
    }
    let seed = log.seed;
    let state = replay(log);
    let mut host = Host { log, state };
    let players: Vec<(String, [f32; 3])> = players.iter().enumerate().map(|(i, p)| (format!("p{i}"), *p)).collect();
    let run = clock.run(&mut host, now, seed, &LogicRules::default(), &players);
    run.errors
}

const WALKER: &str = r#"
function on_place(ev)
  spawn(ev.pos, { look = { model = "walker" }, label = "Walker", walker = true })
end
function on_interact(ev)
  local me = get(ev.entity)
  walk(ev.entity, { x = me.pos.x + 6, y = me.pos.y, z = me.pos.z }, 2)
end
function on_arrive(ev)
  set(ev.entity, "label", ev.stuck and "Stuck" or "Arrived")
end
"#;

#[test]
fn a_walk_is_one_change_and_ends_in_on_arrive() {
    let game = GameSpec {
        remove: None, rename: None,
        name: "town".into(),
        title: "Town".into(),
        defs: vec![DefSpec { name: "walker".into(), kind: "fixture".into(), title: "Walker".into(), ..Default::default() }],
        scripts: vec![ScriptSpec { name: "main.luau".into(), source: WALKER.into() }],
        models: vec![model("walker")],
    };
    let mut log = world(&game);
    let placed = log.push(by("ada", 100), Action::Intent { intent: Intent::Place { item: "town:walker".into(), position: [20.0, Y, 20.0] } });
    let id = EntityId { change: placed, n: 0 };
    log.push(by("ada", 101), Action::Intent { intent: Intent::Interact { entity: id } });
    let here = [[20.0, Y, 20.0]];
    let mut clock = Clockwork::new(100);

    // The engine finds the way: one change holds the whole walk.
    let before = log.changes.len();
    assert!(tick(&mut clock, &mut log, 102, &here).is_empty());
    assert_eq!(log.changes.len(), before + 1);
    let walker = replay(&log).logic.state.get(id).unwrap().clone();
    assert_eq!(walker.components["walk"]["start"], 102);
    assert_eq!(walker.position, [20.0, Y, 20.0], "it stands where it set off until it arrives");
    // Players see it on its way.
    assert_eq!(position_at(&walker, 103.5), [23.0, Y, 20.0]);
    // Nothing more until it gets there.
    assert!(tick(&mut clock, &mut log, 104, &here).is_empty());
    assert_eq!(log.changes.len(), before + 1);
    tick(&mut clock, &mut log, 105, &here);
    let state = replay(&log);
    assert!(state.problems.is_empty(), "{:?}", state.problems);
    let walker = state.logic.state.get(id).unwrap();
    assert_eq!(walker.position, [26.0, Y, 20.0]);
    assert_eq!(walker.components["label"], "Arrived");
    assert!(!walker.components.contains_key("walk"));
    assert_eq!(state.logic.state.timers().count(), 0);
}

#[test]
fn spawners_and_routines_need_no_script() {
    let def = |name: &str, kind: &str| DefSpec { name: name.into(), kind: kind.into(), title: name.into(), ..Default::default() };
    let game = GameSpec {
        remove: None, rename: None,
        name: "vale".into(),
        title: "Vale".into(),
        defs: vec![
            DefSpec { model: Some("slime".into()), spawns: Some("Slime".into()), max: Some(2), every_seconds: Some(10), radius: Some(4.0), ..def("pool", "spawner") },
            DefSpec {
                model: Some("baker".into()),
                routine: Some(vec![Stop { at: "0:00".into(), to: [0.0, 0.0] }, Stop { at: "12:00".into(), to: [10.0, 0.0] }]),
                ..def("baker", "npc")
            },
        ],
        scripts: Vec::new(),
        models: vec![model("slime"), model("baker")],
    };
    // Day 10 begins at 12000 (days are 1200 s).
    let start = 12_000;
    let mut log = world(&game);
    log.push(by("ada", start), Action::Intent { intent: Intent::Place { item: "vale:pool".into(), position: [40.0, Y, 0.0] } });
    let baker = log.push(by("ada", start), Action::Intent { intent: Intent::Place { item: "vale:baker".into(), position: [0.0, Y, 0.0] } });
    let baker = EntityId { change: baker, n: 0 };
    let mut clock = Clockwork::new(start);
    let near = [[20.0, Y, 0.0]];
    let slimes = |log: &WorldLog| replay(log).logic.state.entities().filter(|(_, e)| e.components.contains_key("spawned")).count();

    // Nobody near: nothing spawns.
    assert!(tick(&mut clock, &mut log, start + 30, &[[500.0, Y, 500.0]]).is_empty());
    assert_eq!(slimes(&log), 0);
    // Someone comes: slimes appear, never more than two, and wander.
    for t in (start + 31..start + 120).step_by(5) {
        let errors = tick(&mut clock, &mut log, t, &near);
        assert!(errors.is_empty(), "{errors:?}");
    }
    assert_eq!(slimes(&log), 2);
    let state = replay(&log);
    assert!(state.problems.is_empty(), "{:?}", state.problems);
    assert!(state.logic.state.entities().any(|(_, e)| e.components.contains_key("spawned") && e.components.contains_key("walk")));

    // At noon the baker walks to the second stop, 10 m east.
    for t in (start + 120..start + 606).step_by(5) {
        let errors = tick(&mut clock, &mut log, t, &near);
        assert!(errors.is_empty(), "{errors:?}");
    }
    let walking = replay(&log).logic.state.get(baker).unwrap().clone();
    assert!(walking.components.contains_key("walk"), "{:?}", walking.components);
    tick(&mut clock, &mut log, start + 640, &near);
    let state = replay(&log);
    assert_eq!(state.logic.state.get(baker).unwrap().position, [10.0, Y, 0.0]);
    assert!(state.problems.is_empty(), "{:?}", state.problems);
}

#[test]
fn a_scripted_game_puts_down_data_spawners_and_the_trial_shows_them() {
    let def = |name: &str, kind: &str| DefSpec { name: name.into(), kind: kind.into(), title: name.into(), ..Default::default() };
    let game = GameSpec {
        remove: None, rename: None,
        name: "vale".into(),
        title: "Vale".into(),
        defs: vec![
            DefSpec { model: Some("slime".into()), spawns: Some("Slime".into()), max: Some(3), every_seconds: Some(10), radius: Some(4.0), ..def("pool", "spawner") },
            def("stall", "fixture"),
        ],
        scripts: vec![ScriptSpec { name: "main.luau".into(), source: "function on_place(ev) end".into() }],
        models: vec![model("slime")],
    };
    let steps: Vec<String> = ["place pool", "wait 45"].iter().map(|s| s.to_string()).collect();
    let report = cubewish_rules::logic::trial::play(&game.to_package(1), &steps);
    assert!(report.contains("1. place pool: ok"), "{report}");
    assert_eq!(report.matches("\"spawned\"").count(), 3, "{report}");
}

/// A world that is the game: it sets itself up on install, greets players, a wolf
/// guards a clearing and chases whoever walks in, and its bite sends them back to camp.
const NIGHT: &str = r#"
function on_start(ev)
  spawn_point({x = 0, y = 8.75, z = 0})
  building(false)
  local wolf = spawn({x = 20, y = 8.75, z = 0}, { look = { model = "wolf" }, label = "Wolf", wolf = true })
  region(wolf, {radius = 8})
end
function on_join(ev)
  set_stat(ev.player, "hp", 10, 10)
  hud(ev.player, "hp", { kind = "bar", label = "HP", stat = "hp" })
  notify(ev.player, "Survive the night.")
end
function on_enter(ev)
  chase(ev.entity, ev.player, {speed = 4, reach = 1.5})
end
function on_catch(ev)
  local hp = add_stat(ev.player, "hp", -4)
  if hp <= 0 then
    teleport(ev.player, {x = 0, y = 8.75, z = 0})
    set_stat(ev.player, "hp", 10)
    chase(ev.entity)
    notify(ev.player, "The wolf got you. Back to camp.")
  end
end
function on_hit(ev)
  despawn(ev.entity)
  notify(ev.player, "You drove the wolf off!")
end
"#;

#[test]
fn the_world_is_the_game() {
    let game = GameSpec {
        remove: None, rename: None,
        name: "night".into(),
        title: "Night".into(),
        defs: Vec::new(),
        scripts: vec![ScriptSpec { name: "main.luau".into(), source: NIGHT.into() }],
        models: vec![model("wolf")],
    };
    // In the trial: set up on install, the tester joins, walks into the wolf's region,
    // gets chased, bitten until sent back to camp; then hits the wolf.
    let steps: Vec<String> = ["go 15 0", "wait 12", "go 3 0", "hit wolf"].iter().map(|s| s.to_string()).collect();
    let report = cubewish_rules::logic::trial::play(&game.to_package(1), &steps);
    println!("{report}");
    assert!(report.contains("on_start:"), "{report}");
    assert!(report.contains("joined") && report.contains("\"Survive the night.\""), "{report}");
    assert!(report.contains("entered"), "{report}");
    assert!(report.contains("\"The wolf got you. Back to camp.\""), "{report}");
    assert!(report.contains("\"You drove the wolf off!\""), "{report}");

    // Through the log, as a server runs it: players can't build while the game runs.
    let mut log = world(&game);
    let mut clock = Clockwork::new(100);
    assert!(tick(&mut clock, &mut log, 101, &[[0.0, Y, 0.0]]).is_empty());
    let state = replay(&log);
    assert_eq!(state.logic.state.stat("p0", "night:hp").map(|s| s.value), Some(10.0));
    assert!(!state.logic.building_allowed());
    assert_eq!(state.logic.state.teleport("*").map(|t| t.position), Some([0.0, 8.75, 0.0]));
    let lamp = serde_json::from_str(
        r##"{"name":"Lamp","size":[3,4,3],"palette":[{"name":"iron","hex":"#3d4248","material":"metal"}],
            "shapes":[{"kind":"box","color":"iron","from":[0,0,0],"to":[3,4,3],"radius":null,"wobble":null}]}"##,
    )
    .unwrap();
    let build = cubewish_rules::log::BuildState {
        build: 1,
        name: "Lamp".into(),
        prompt: String::new(),
        provider: String::new(),
        spec: lamp,
        position: [100.0, 10.0, 100.0],
        yaw: 0.0,
    };
    log.push(by("bo", 102), Action::PlaceBuild(build));
    assert!(replay(&log).problems.last().unwrap().contains("only admins build"));
    // Walking into the wolf's region sets it running at them.
    tick(&mut clock, &mut log, 103, &[[14.0, Y, 0.0]]);
    let wolf = replay(&log).logic.state.entities().find(|(_, e)| e.components.contains_key("night:wolf")).map(|(_, e)| e.clone()).unwrap();
    assert_eq!(wolf.components["chase"]["player"], "p0");
}
