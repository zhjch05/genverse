//! Games move on their own through timers: they go off live, what they do lands in the
//! log, and replay applies it without running them again.

use cubewish_rules::{
    log::{Action, Provenance, Snapshot, WORLD, WorldLog, replay, replay_from},
    logic::{
        Clockwork, Effect, EntityId, Intent, LogicRules, Scope, TimerHost,
        data::{DefSpec, GameSpec, ModelSpec, ScriptSpec},
        trial,
    },
};

/// A lamp that counts down: placed, it rings in 5 s and blinks every 2 s.
fn lamp(source: &str) -> GameSpec {
    let model = r##"{"name":"Lamp","size":[4,8,4],"palette":[{"name":"glow","hex":"#ffd27a","material":"glow"}],
        "shapes":[{"kind":"box","color":"glow","from":[0,0,0],"to":[4,8,4],"radius":null,"wobble":null}]}"##;
    GameSpec {
        name: "lamp".into(),
        title: "Lamp".into(),
        defs: vec![DefSpec { name: "lamp".into(), kind: "fixture".into(), title: "Lamp".into(), ..Default::default() }],
        scripts: vec![ScriptSpec { name: "main.luau".into(), source: source.into() }],
        models: vec![ModelSpec { name: "lamp".into(), build: serde_json::from_str(model).unwrap() }],
    }
}

const LAMP: &str = r#"
function on_place(ev)
  local id = spawn(ev.pos, { look = { model = "lamp" }, lamp = { blinks = 0 }, label = "Lamp" })
  after(5, "ring", { lamp = id, who = ev.player })
  every(2, "blink", { lamp = id })
end

function on_timer(ev)
  local lamp = get(ev.data.lamp)
  if ev.name == "ring" then
    notify(ev.data.who, "Ring!")
    set(lamp.id, "label", "Rang")
  elseif ev.name == "blink" then
    set(lamp.id, "lamp", { blinks = lamp.c.lamp.blinks + 1 + ev.missed })
    if lamp.c.lamp.blinks + 1 + ev.missed >= 4 then cancel("blink") end
  end
end
"#;

fn by(player: &str, at: u64) -> Provenance {
    Provenance::new(player, player, None, None, at)
}

#[test]
fn timers_go_off_into_the_log_and_replay() {
    let mut log = WorldLog { flat: true, ..WorldLog::new(7) };
    log.admins = vec!["admin".into()];
    let package = lamp(LAMP).to_package(1);
    assert!(package.check().is_ok(), "{:?}", package.check());
    let reference = package.reference();
    log.add_package(package);
    log.push(by("admin", 100), Action::InstallPackage { package: reference, scope: Scope::World });
    let place = Intent::Place { item: "lamp:lamp".into(), position: [4.0, 9.0, 4.0] };
    let placed = log.push(by("ada", 100), Action::Intent { intent: place });
    let id = EntityId { change: placed, n: 0 };

    // Runs the timers due by `now`, as a server does, recording what they did.
    let mut clock = Clockwork::new(100);
    let mut tick = |log: &mut WorldLog, now: u64| -> Vec<String> { run(&mut clock, log, now, &[]) };
    let blinks = |log: &WorldLog| replay(log).logic.state.get(id).unwrap().components["lamp:lamp"]["blinks"].as_u64().unwrap();

    // Nothing is due yet: nothing is recorded.
    let before = log.changes.len();
    assert!(tick(&mut log, 101).is_empty());
    assert_eq!(log.changes.len(), before);
    // At 102 the first blink.
    tick(&mut log, 102);
    assert_eq!(blinks(&log), 1);
    // Nobody ran it for a while: one firing catches up (103..=105 holds the blink at 104,
    // and the ring at 105).
    assert_eq!(tick(&mut log, 105), vec!["Ring!".to_string()]);
    assert_eq!(blinks(&log), 2);
    let state = replay(&log);
    assert!(state.problems.is_empty(), "{:?}", state.problems);
    assert_eq!(state.logic.state.get(id).unwrap().components["label"], "Rang");
    // The ring went off once and is gone; the blink keeps going.
    let timers: Vec<_> = state.logic.state.timers().map(|(_, n, _)| n.to_string()).collect();
    assert_eq!(timers, vec!["blink"]);
    // Six seconds unrun: three blinks missed fold into one firing, which cancels it.
    tick(&mut log, 111);
    assert_eq!(blinks(&log), 5);
    assert_eq!(replay(&log).logic.state.timers().count(), 0);
    assert!(tick(&mut log, 200).is_empty());

    // The log says what happened, so it replays the same, through JSON too.
    let again: WorldLog = serde_json::from_str(&serde_json::to_string(&log).unwrap()).unwrap();
    assert_eq!(replay(&again).logic.state.get(id), replay(&log).logic.state.get(id));
}

/// Runs `clock` on the log's world at `now`, with players at `players`, and records
/// what went off. Returns what was said.
fn run(clock: &mut Clockwork, log: &mut WorldLog, now: u64, players: &[[f32; 3]]) -> Vec<String> {
    /// A world as a server keeps it: the log, and its replay.
    struct Host<'a> {
        log: &'a mut WorldLog,
        state: cubewish_rules::log::Replay,
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
            self.state.apply(&change, false).map(|_| ())
        }
    }
    let seed = log.seed;
    let state = replay(log);
    let mut host = Host { log, state };
    let run = clock.run(&mut host, now, seed, &LogicRules::default(), players);
    assert!(run.errors.is_empty(), "{:?}", run.errors);
    run.notes.into_iter().map(|(_, t)| t).collect()
}

#[test]
fn timers_near_a_board_sleep_while_nobody_is_there() {
    let source = r#"
    function on_place(ev)
      local id = spawn(ev.pos, { look = { model = "lamp" }, lamp = { blinks = 0 } })
      every(10, "blink", nil, id)
    end
    function on_timer(ev)
      local lamp = find("lamp")[1]
      set(lamp.id, "lamp", { blinks = lamp.c.lamp.blinks + 1 + ev.missed })
    end
    "#;
    let mut log = WorldLog { flat: true, ..WorldLog::new(7) };
    log.admins = vec!["admin".into()];
    let package = lamp(source).to_package(1);
    let reference = package.reference();
    log.add_package(package);
    log.push(by("admin", 100), Action::InstallPackage { package: reference, scope: Scope::World });
    let placed = log.push(by("ada", 100), Action::Intent { intent: Intent::Place { item: "lamp:lamp".into(), position: [0.0, 9.0, 0.0] } });
    let id = EntityId { change: placed, n: 0 };
    let blinks = |log: &WorldLog| replay(log).logic.state.get(id).unwrap().components["lamp:lamp"]["blinks"].as_u64().unwrap();
    let (near, far) = ([10.0, 9.0, 0.0], [500.0, 9.0, 0.0]);
    let mut clock = Clockwork::new(100);
    run(&mut clock, &mut log, 110, &[near]);
    assert_eq!(blinks(&log), 1);
    // Ada walks away for 100 s: nothing runs, nothing is recorded.
    let before = log.changes.len();
    for now in [120, 150, 210] {
        run(&mut clock, &mut log, now, &[far]);
    }
    assert_eq!(log.changes.len(), before);
    // Back: one firing for all ten blinks she was away for.
    run(&mut clock, &mut log, 211, &[near]);
    assert_eq!(blinks(&log), 11);
    assert_eq!(log.changes.len(), before + 1);
}

#[test]
fn snapshots_keep_the_games() {
    let mut log = WorldLog { flat: true, ..WorldLog::new(7) };
    log.admins = vec!["admin".into()];
    let package = lamp(LAMP).to_package(1);
    let reference = package.reference();
    log.add_package(package);
    log.push(by("admin", 100), Action::InstallPackage { package: reference, scope: Scope::World });
    log.push(by("ada", 100), Action::Intent { intent: Intent::Place { item: "lamp:lamp".into(), position: [0.0, 9.0, 0.0] } });
    let mut clock = Clockwork::new(100);
    run(&mut clock, &mut log, 103, &[]);
    let snapshot = Snapshot::take(&log, &replay(&log));
    let snapshot: Snapshot = serde_json::from_str(&serde_json::to_string(&snapshot).unwrap()).unwrap();
    run(&mut clock, &mut log, 106, &[]);
    log.push(by("ada", 107), Action::Intent { intent: Intent::Place { item: "lamp:lamp".into(), position: [4.0, 9.0, 0.0] } });
    let (full, fast) = (replay(&log), replay_from(&log, Some(&snapshot)));
    assert!(fast.problems.is_empty(), "{:?}", fast.problems);
    assert_eq!(fast.logic.state, full.logic.state);
    assert_eq!(fast.logic.timers_until, full.logic.timers_until);
}

#[test]
fn timers_are_the_worlds() {
    let mut log = WorldLog { flat: true, ..WorldLog::new(7) };
    log.admins = vec!["admin".into()];
    let package = lamp(LAMP).to_package(1);
    let reference = package.reference();
    log.add_package(package);
    log.push(by("admin", 100), Action::InstallPackage { package: reference, scope: Scope::World });
    let placed = log.push(by("ada", 100), Action::Intent { intent: Intent::Place { item: "lamp:lamp".into(), position: [0.0; 3] } });
    // A player can't make timers say what they like.
    let forged = Effect::Set { entity: EntityId { change: placed, n: 0 }, component: "label".into(), value: "Mine".into() };
    log.push(by("ada", 101), Action::Timers { package: "lamp".into(), until: 101, effects: vec![forged.clone()] });
    let state = replay(&log);
    assert!(state.problems.iter().any(|p| p.contains("only the world runs timers")), "{:?}", state.problems);
    // And what the world records is checked like anything a package does.
    let other = Effect::Set { entity: EntityId { change: placed, n: 0 }, component: "fish:x".into(), value: 1.into() };
    log.push(by(WORLD, 102), Action::Timers { package: "lamp".into(), until: 102, effects: vec![other] });
    assert!(replay(&log).problems.iter().any(|p| p.contains("belongs to fish")));
}

#[test]
fn a_failing_timer_stops_and_the_others_run() {
    let source = r#"
    function on_place(ev)
      spawn(ev.pos, { look = { model = "lamp" } })
      every(1, "broken")
      every(1, "fine")
      after(1, "slow")
    end
    function on_timer(ev)
      if ev.name == "broken" then error("oops") end
      if ev.name == "slow" then while true do end end
      notify("ada", ev.name)
    end
    "#;
    let mut log = WorldLog { flat: true, ..WorldLog::new(7) };
    log.admins = vec!["admin".into()];
    let package = lamp(source).to_package(1);
    let reference = package.reference();
    log.add_package(package);
    log.push(by("admin", 100), Action::InstallPackage { package: reference, scope: Scope::World });
    log.push(by("ada", 100), Action::Intent { intent: Intent::Place { item: "lamp:lamp".into(), position: [0.0; 3] } });
    let state = replay(&log);
    let fired = Clockwork::new(100).fire(&state.logic, "lamp", 3, 101, 7, &LogicRules::default(), &[]);
    assert_eq!(fired.notes, vec![("ada".to_string(), "fine".to_string())]);
    assert_eq!(fired.errors.len(), 2, "{:?}", fired.errors);
    assert!(fired.errors[0].contains("\"broken\" stopped") && fired.errors[0].contains("oops"));
    assert!(fired.errors[1].contains("ran too long"));
    log.push(by(WORLD, 101), Action::Timers { package: "lamp".into(), until: 101, effects: fired.effects });
    let state = replay(&log);
    let left: Vec<_> = state.logic.state.timers().map(|(_, n, _)| n.to_string()).collect();
    assert_eq!(left, vec!["fine"]);
}

#[test]
fn trials_wait_for_timers() {
    let package = lamp(LAMP).to_package(1);
    let steps: Vec<String> = ["place lamp", "wait 10"].iter().map(|s| s.to_string()).collect();
    let report = trial::play(&package, &steps);
    println!("{report}");
    assert!(report.contains("2. wait 10: timers went off"), "{report}");
    assert!(report.contains("\"Ring!\""), "{report}");
}
