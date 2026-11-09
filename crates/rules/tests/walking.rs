//! Pieces that move on their own: walking and chasing are the engine's (it runs them on
//! timers, records what they did, and replay never looks for a path again); routines,
//! wandering and spawners are a few lines of Lua on top.

use cubewish_rules::{
    log::{Action, Provenance, Replay, WORLD, WorldLog, replay},
    logic::{
        Clockwork, Effect, EntityId, Ground, Intent, LogicRules, Scope, TimerHost,
        data::{DefSpec, GameSpec, ModelSpec, ScriptSpec},
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
        fn solids(&self, lo: [f32; 2], hi: [f32; 2]) -> Vec<cubewish_rules::logic::solid::Solid<'_>> {
            self.state.solids(lo, hi)
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

/// A lift: using it makes the player quick and double-jumping, throws them, then carries
/// them 10 m up on a pawn that flies, and puts them back to normal at the top.
const LIFT: &str = r#"
function on_place(ev)
  spawn(ev.pos, { look = { model = "lift" }, label = "Lift" })
end
function on_interact(ev)
  body(ev.player, {speed = 2, air_jumps = 1})
  launch(ev.player, {x = 0, y = 15, z = 0})
  local p = get(ev.entity).pos
  local seat = spawn(p, { pawn = ev.player, rider = ev.player })
  fly(seat, {x = p.x, y = p.y + 10, z = p.z}, 2)
end
function on_arrive(ev)
  local rider = get(ev.entity).c.rider
  body(rider)
  despawn(ev.entity)
  notify(rider, "At the top")
end
"#;

#[test]
fn games_change_how_players_move_and_carry_them() {
    let game = GameSpec {
        remove: None, rename: None,
        name: "tower".into(),
        title: "Tower".into(),
        defs: vec![DefSpec { name: "lift".into(), kind: "fixture".into(), title: "Lift".into(), ..Default::default() }],
        scripts: vec![ScriptSpec { name: "main.luau".into(), source: LIFT.into() }],
        models: vec![model("lift")],
    };
    let mut log = world(&game);
    let placed = log.push(by("ada", 100), Action::Intent { intent: Intent::Place { item: "tower:lift".into(), position: [20.0, Y, 20.0] } });
    log.push(by("ada", 101), Action::Intent { intent: Intent::Interact { entity: EntityId { change: placed, n: 0 } } });
    let state = replay(&log);
    assert!(state.problems.is_empty(), "{:?}", state.problems);
    let body = state.logic.state.body("ada");
    assert_eq!((body.speed, body.jump, body.air_jumps), (2.0, 1.0, 1));
    assert_eq!(state.logic.state.launch("ada").unwrap().velocity, [0.0, 15.0, 0.0]);
    let seat = state.logic.state.entities().find(|(_, e)| e.components.contains_key("pawn")).unwrap().0;

    // It flies straight up, over nothing a walk could stand on, carrying her.
    let here = [[20.0, Y, 20.0]];
    let mut clock = Clockwork::new(100);
    tick(&mut clock, &mut log, 102, &here);
    let pawn = replay(&log).logic.state.get(seat).unwrap().clone();
    assert_eq!(position_at(&pawn, 103.5), [20.0, Y + 3.0, 20.0]);
    let said = tick(&mut clock, &mut log, 107, &here);
    let state = replay(&log);
    assert!(state.problems.is_empty(), "{:?}", state.problems);
    assert!(state.logic.state.get(seat).is_none(), "the ride is over: {said:?}");
    assert_eq!(state.logic.state.body("ada"), Default::default(), "and she moves as usual again");
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

/// What the engine used to do itself, in Lua: a pool that keeps two slimes about, which
/// hop from spot to spot, and a baker who goes to her stall at noon.
const VILLAGE: &str = r#"
local Y = 8.75
function on_start(ev)
  local pool = spawn({x = 40, y = Y, z = 0}, { look = { model = "slime" }, label = "Slime pool", pool = true })
  every(10, "spawn", { pool = pool }, pool)
  local baker = spawn({x = 0, y = Y, z = 0}, { look = { model = "baker" }, label = "Baker", baker = true })
  every(60, "day", { baker = baker }, baker)
end
local function hop(id)
  local s = get(id)
  local a = random() * 6.28
  walk(id, {x = s.pos.x + math.cos(a) * 3, y = Y, z = s.pos.z + math.sin(a) * 3}, 1)
end
function on_timer(ev)
  if ev.name == "spawn" then
    local pool = get(ev.data.pool)
    if #find("slime") < 2 then
      local a = random() * 6.28
      local s = spawn({x = pool.pos.x + math.cos(a) * 3, y = Y, z = pool.pos.z + math.sin(a) * 3},
        { look = { model = "slime" }, label = "Slime", slime = true })
      hop(s)
    end
  elseif ev.name == "day" then
    local baker = get(ev.data.baker)
    if ev.time >= 0.5 and not baker.walking and baker.pos.x < 9 then walk(baker.id, {x = 10, y = Y, z = 0}) end
  end
end
function on_arrive(ev)
  if get(ev.entity).c.slime then hop(ev.entity) end
end
"#;

#[test]
fn routines_wandering_and_spawners_are_lua() {
    let game = GameSpec {
        remove: None,
        rename: None,
        name: "vale".into(),
        title: "Vale".into(),
        defs: Vec::new(),
        scripts: vec![ScriptSpec { name: "village.luau".into(), source: VILLAGE.into() }],
        models: vec![model("slime"), model("baker")],
    };
    // Day 10 begins at 12000 (days are 1200 s): installed then.
    let start = 12_000;
    let mut log = WorldLog { flat: true, ..WorldLog::new(7) };
    log.admins = vec!["admin".into()];
    let package = game.to_package(1);
    assert!(package.check().is_ok(), "{:?}", package.check());
    let reference = package.reference();
    log.add_package(package);
    log.push(by("admin", start), Action::InstallPackage { package: reference, scope: Scope::World });
    let mut clock = Clockwork::new(start);
    let near = [[20.0, Y, 0.0]];
    let slimes = |log: &WorldLog| replay(log).logic.state.entities().filter(|(_, e)| e.components.contains_key("vale:slime")).count();
    for t in (start + 1..start + 120).step_by(5) {
        let errors = tick(&mut clock, &mut log, t, &near);
        assert!(errors.is_empty(), "{errors:?}");
    }
    assert_eq!(slimes(&log), 2);
    assert!(replay(&log).logic.state.entities().any(|(_, e)| e.components.contains_key("vale:slime") && e.components.contains_key("walk")));
    // At noon the baker walks to her stall.
    for t in (start + 120..start + 700).step_by(10) {
        tick(&mut clock, &mut log, t, &near);
    }
    let state = replay(&log);
    assert!(state.problems.is_empty(), "{:?}", state.problems);
    let baker = state.logic.state.entities().find(|(_, e)| e.components.contains_key("vale:baker")).map(|(_, e)| e.clone()).unwrap();
    assert_eq!(baker.position, [10.0, Y, 0.0]);
}

const FENCED: &str = r#"
function on_place(ev)
  spawn({ x = ev.pos.x + 3, y = ev.pos.y, z = ev.pos.z }, { look = { model = "fence", solid = true } })
  spawn(ev.pos, { look = { model = "walker" }, label = "Walker" })
end
function on_interact(ev)
  local me = get(ev.entity)
  walk(ev.entity, { x = me.pos.x + 6, y = me.pos.y, z = me.pos.z }, 2)
end
"#;

#[test]
fn walks_go_around_solid_pieces() {
    let mut fence = model("fence");
    // 25 cm thick, 2 m tall, 6 m long across the way.
    fence.build.size = vec![2, 16, 48];
    fence.build.shapes[0].to = vec![2.0, 16.0, 48.0];
    let game = GameSpec {
        remove: None, rename: None,
        name: "yard".into(),
        title: "Yard".into(),
        defs: vec![DefSpec { name: "walker".into(), kind: "fixture".into(), title: "Walker".into(), ..Default::default() }],
        scripts: vec![ScriptSpec { name: "main.luau".into(), source: FENCED.into() }],
        models: vec![model("walker"), fence],
    };
    let mut log = world(&game);
    let placed = log.push(by("ada", 100), Action::Intent { intent: Intent::Place { item: "yard:walker".into(), position: [20.0, Y, 20.0] } });
    let walker = EntityId { change: placed, n: 1 };
    log.push(by("ada", 101), Action::Intent { intent: Intent::Interact { entity: walker } });
    let mut clock = Clockwork::new(100);
    assert!(tick(&mut clock, &mut log, 102, &[[20.0, Y, 20.0]]).is_empty());
    let state = replay(&log);
    let path = state.logic.state.get(walker).unwrap().components["walk"]["path"].as_array().unwrap().clone();
    let z = |p: &serde_json::Value| p[2].as_f64().unwrap() as f32;
    assert!(path.iter().any(|p| (z(p) - 20.0).abs() >= 3.0), "around the fence's end: {path:?}");
}

#[test]
fn agents_query_the_world_without_changing_it() {
    let game = GameSpec {
        remove: None, rename: None,
        name: "town".into(),
        title: "Town".into(),
        defs: vec![DefSpec { name: "walker".into(), kind: "fixture".into(), title: "Walker".into(), ..Default::default() }],
        scripts: vec![ScriptSpec { name: "main.luau".into(), source: WALKER.into() }],
        models: vec![model("walker")],
    };
    let mut log = world(&game);
    for x in [20.0, 30.0, 90.0] {
        log.push(by("ada", 100), Action::Intent { intent: Intent::Place { item: "town:walker".into(), position: [x, Y, 20.0] } });
    }
    let state = replay(&log);
    let rules = LogicRules::default();
    let ask = |code: &str| state.logic.query("town", "ada", code, 200, &rules).unwrap();
    // The package's own names, and what's near: nearest first, with how far.
    assert_eq!(ask("return #find('walker')").0, 3);
    let near = ask("local n = near({x = 21, y = 9, z = 20}, 15, 'walker') return {#n, n[1].distance < n[2].distance}").0;
    assert_eq!(near, serde_json::json!([2, true]));
    // What it would change is counted, never kept.
    let (_, changes) = ask("despawn(find('walker')[1].id) return nil");
    assert_eq!(changes, 1);
    assert_eq!(state.logic.state.len(), 3);
    assert!(state.logic.query("town", "ada", "return near({x = 0, y = 0, z = 0}, 500)", 200, &rules).unwrap_err().contains("0-128"));
}

#[test]
fn packages_tell_each_other_and_keep_values() {
    let pkg = |name: &str, source: &str| GameSpec {
        remove: None, rename: None,
        name: name.into(),
        title: name.into(),
        defs: vec![DefSpec { name: "bell".into(), kind: "fixture".into(), title: "Bell".into(), ..Default::default() }],
        scripts: vec![ScriptSpec { name: "main.luau".into(), source: source.into() }],
        models: vec![model("bell")],
    };
    let bells = pkg("bells", r#"
        function on_place(ev) spawn(ev.pos, { look = { model = "bell" }, label = "Bell" }) end
        function on_interact(ev)
          remember("rings", (recall("rings") or 0) + 1)
          emit("rang", { times = recall("rings") })
        end
    "#);
    let town = pkg("town", r#"
        function on_signal(ev) remember("heard", ev.from .. ":" .. ev.name .. ":" .. ev.data.times) end
    "#);
    let mut log = world(&bells);
    let package = town.to_package(1);
    let reference = package.reference();
    log.add_package(package);
    log.push(by("admin", 100), Action::InstallPackage { package: reference, scope: Scope::World });
    let placed = log.push(by("ada", 101), Action::Intent { intent: Intent::Place { item: "bells:bell".into(), position: [20.0, Y, 20.0] } });
    let bell = EntityId { change: placed, n: 0 };
    log.push(by("ada", 102), Action::Intent { intent: Intent::Interact { entity: bell } });
    log.push(by("ada", 103), Action::Intent { intent: Intent::Interact { entity: bell } });
    // The town hears it at its next tick, recorded like any timer's work.
    let mut clock = Clockwork::new(103);
    tick(&mut clock, &mut log, 104, &[[20.0, Y, 20.0]]);
    let state = replay(&log);
    assert!(state.problems.is_empty(), "{:?}", state.problems);
    let rules = LogicRules::default();
    let ask = |package: &str, code: &str| state.logic.query(package, "ada", code, 200, &rules).unwrap().0;
    assert_eq!(ask("bells", "return recall('rings')"), 2);
    assert_eq!(ask("town", "return recall('heard')"), "bells:rang:2");
    // Heard once: nothing new next time.
    assert!(state.logic.state.unheard("town").next().is_none());
    // A script from before calling what the game no longer has hears what to use instead.
    let old = state.logic.query("town", "ada", "wander('1.0', {radius = 5}) return nil", 200, &rules).unwrap_err();
    assert!(old.contains("isn't in the game any more"), "{old}");
}

#[test]
fn trials_play_player_scripts_items_and_time() {
    let game = GameSpec {
        remove: None, rename: None,
        name: "sky".into(),
        title: "Sky".into(),
        defs: vec![
            DefSpec { name: "pad".into(), kind: "fixture".into(), title: "Pad".into(), ..Default::default() },
            DefSpec { name: "key".into(), kind: "item".into(), title: "Key".into(), ..Default::default() },
        ],
        scripts: vec![
            ScriptSpec { name: "main.luau".into(), source: r#"
                function on_place(ev) spawn(ev.pos, { look = { model = "pad" }, label = "Chest" }) end
                function on_join(ev) give(ev.player, "key", 1) end
                function on_use(ev) notify(ev.player, "opened with the " .. ev.item) end
                function on_message(ev) notify(ev.player, "dash " .. ev.data.dir) end
                function on_chat(ev) notify(ev.player, "heard " .. ev.text) end
            "#.into() },
            ScriptSpec { name: "jet.player.luau".into(), source: r#"
                function on_frame(f)
                  if f.keys.space then hero_velocity({x = 0, y = 4, z = 0}) end
                  if f.pressed.q then send("dash", {dir = "north"}) end
                end
            "#.into() },
        ],
        models: vec![model("pad")],
    };
    let steps: Vec<String> = ["place pad", "use key on Chest", "say /home", "hold space 1", "press q", "wait until night"].iter().map(|s| s.to_string()).collect();
    let report = cubewish_rules::logic::trial::play(&game.to_package(1), &steps);
    println!("{report}");
    assert!(report.contains("2. use key on Chest") && report.contains("opened with the key"), "{report}");
    assert!(report.contains("heard /home"), "{report}");
    assert!(report.contains("4.0 m up at most, steered in 30"), "{report}");
    assert!(report.contains("sent \"dash\": 1 change(s), said \"dash north\""), "{report}");
    assert!(report.contains("time of day 0.85"), "{report}");
}
