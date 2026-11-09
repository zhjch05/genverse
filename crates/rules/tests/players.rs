//! What games give players (stats, a HUD, panels, shops) and do to the world (builds and
//! land changes), through the log: checked, all or nothing, and charged to whoever
//! installed the game.

use cubewish_rules::{
    log::{Action, Provenance, Snapshot, WorldLog, replay, replay_from},
    logic::{
        EntityId, Intent, Scope, Widget,
        data::{DefSpec, GameSpec, ModelSpec, ScriptSpec},
        entity::SCRIPT_BUILDS,
        trial,
    },
};

fn by(player: &str, at: u64) -> Provenance {
    Provenance::new(player, player, None, None, at)
}

fn model(name: &str, size: [i64; 3]) -> ModelSpec {
    let json = format!(
        r##"{{"name":"{name}","size":[{},{},{}],"palette":[{{"name":"stone","hex":"#8a857e","material":"matte"}}],
            "shapes":[{{"kind":"box","color":"stone","from":[0,0,0],"to":[{},{},{}],"radius":null,"wobble":null}}]}}"##,
        size[0], size[1], size[2], size[0], size[1], size[2]
    );
    ModelSpec { name: name.into(), build: serde_json::from_str(&json).unwrap() }
}

fn game(name: &str, defs: Vec<DefSpec>, script: Option<&str>, models: Vec<ModelSpec>) -> GameSpec {
    GameSpec {
        remove: None, rename: None,
        name: name.into(),
        title: name.into(),
        defs,
        scripts: script.map(|s| vec![ScriptSpec { name: "main.luau".into(), source: s.into() }]).unwrap_or_default(),
        models,
    }
}

fn fixture(name: &str) -> DefSpec {
    DefSpec { name: name.into(), kind: "fixture".into(), title: name.into(), ..Default::default() }
}

/// A flat world run by `admin`, with `spec` installed everywhere.
fn world(spec: &GameSpec) -> WorldLog {
    let mut log = WorldLog { flat: true, ..WorldLog::new(7) };
    log.admins = vec!["admin".into()];
    let package = spec.to_package(1);
    assert!(package.check().is_ok(), "{:?}", package.check());
    let reference = package.reference();
    log.add_package(package);
    log.push(by("admin", 100), Action::InstallPackage { package: reference, scope: Scope::World });
    log
}

const INN: &str = r##"
function on_place(ev)
  spawn(ev.pos, { look = { model = "keeper" }, label = "Innkeeper", keeper = true })
end
function on_interact(ev)
  if not stat(ev.player, "gold") then
    set_stat(ev.player, "gold", 12)
    set_stat(ev.player, "hp", 3, 10)
  end
  hud(ev.player, "hp", { kind = "bar", label = "HP", stat = "hp", color = "#d84a4a" })
  hud("*", "quest", { kind = "text", text = "Rest at the inn" })
  hud(ev.player, "keys", { kind = "items", label = "Keys", item = "key" })
  show(ev.player, { id = "inn", title = "The innkeeper", text = "A room is 5 gold.", entity = ev.entity,
    choices = { { id = "room", label = "Rent a room", detail = "5 gold" }, { id = "bye", label = "Goodbye" } } })
end
function on_choose(ev)
  if ev.panel ~= "inn" then return end
  if ev.choice == "room" then
    if stat(ev.player, "gold") < 5 then error("not enough gold") end
    add_stat(ev.player, "gold", -5)
    set_stat(ev.player, "hp", stat_max(ev.player, "hp"))
    notify(ev.player, "You feel rested.")
  end
  close(ev.player)
end
"##;

#[test]
fn stats_hud_and_panels() {
    let spec = game("inn", vec![fixture("inn")], Some(INN), vec![model("keeper", [6, 12, 6])]);
    let mut log = world(&spec);
    let placed = log.push(by("ada", 101), Action::Intent { intent: Intent::Place { item: "inn:inn".into(), position: [0.0, 9.0, 0.0] } });
    let keeper = EntityId { change: placed, n: 0 };
    log.push(by("ada", 102), Action::Intent { intent: Intent::Interact { entity: keeper } });
    let state = replay(&log);
    assert!(state.problems.is_empty(), "{:?}", state.problems);
    let game = &state.logic.state;
    assert_eq!(game.stat("ada", "inn:hp").map(|s| (s.value, s.max)), Some((3.0, Some(10.0))));
    let panel = game.panel("ada").expect("the innkeeper talks");
    assert_eq!((panel.spec.id.as_str(), panel.spec.choices.len(), panel.spec.entity), ("inn", 2, Some(keeper)));
    // Everyone sees the quest; only Ada her HP.
    assert_eq!(game.hud("ada").count(), 3);
    assert!(game.hud("ada").any(|(_, w)| matches!(w, Widget::Items { item, .. } if item == "inn:key")));
    assert!(matches!(game.hud("bo").collect::<Vec<_>>().as_slice(), [(_, Widget::Text { .. })]));
    assert!(game.panel("bo").is_none());

    // A choice that isn't offered is refused; renting a room twice runs out of gold.
    log.push(by("ada", 103), Action::Intent { intent: Intent::Choose { choice: Some("steal".into()) } });
    log.push(by("ada", 104), Action::Intent { intent: Intent::Choose { choice: Some("room".into()) } });
    let state = replay(&log);
    assert!(state.problems.iter().any(|p| p.contains("isn't a choice")), "{:?}", state.problems);
    let game = &state.logic.state;
    assert_eq!(game.stat("ada", "inn:gold").map(|s| s.value), Some(7.0));
    assert_eq!(game.stat("ada", "inn:hp").map(|s| s.value), Some(10.0));
    assert!(game.panel("ada").is_none(), "closed after the choice");
    log.push(by("ada", 105), Action::Intent { intent: Intent::Interact { entity: keeper } });
    log.push(by("ada", 106), Action::Intent { intent: Intent::Choose { choice: Some("room".into()) } });
    log.push(by("ada", 107), Action::Intent { intent: Intent::Interact { entity: keeper } });
    log.push(by("ada", 108), Action::Intent { intent: Intent::Choose { choice: Some("room".into()) } });
    let state = replay(&log);
    assert!(state.problems.iter().any(|p| p.contains("not enough gold")), "{:?}", state.problems);
    assert_eq!(state.logic.state.stat("ada", "inn:gold").map(|s| s.value), Some(2.0));
    // The player can always close a panel.
    log.push(by("ada", 109), Action::Intent { intent: Intent::Choose { choice: None } });
    assert!(replay(&log).logic.state.panel("ada").is_none());
}

const MASON: &str = r##"
function on_place(ev)
  local wall = build("wall", { x = ev.pos.x + 3, y = ev.pos.y, z = ev.pos.z }, 1)
  spawn(ev.pos, { look = { model = "post" }, label = "Mason", mason = { wall = wall } })
  terrain({ x = ev.pos.x - 6, z = ev.pos.z - 6, w = 4, d = 4 },
    { name = "Pit", ops = { { kind = "lower", area = { shape = "rect", x = 2, z = 2, w = 2, d = 2 },
      points = nil, amount_m = 1, level_m = nil, depth_m = nil, width_m = nil, edge_m = nil,
      material = nil, item = nil, density = nil } } })
end
function on_interact(ev)
  local mason = get(ev.entity)
  if mason.c.mason.wall then
    unbuild(mason.c.mason.wall)
    set(ev.entity, "mason", {})
  else
    build("wall", ev.pos, 0)
    build("wall", { x = ev.pos.x - 3, y = ev.pos.y, z = ev.pos.z }, 0)
  end
end
"##;

#[test]
fn games_build_with_the_installers_budget() {
    let spec = game("mason", vec![fixture("mason")], Some(MASON), vec![model("wall", [16, 16, 4]), model("post", [4, 8, 4])]);
    let mut log = world(&spec);
    let placed = log.push(by("ada", 101), Action::Intent { intent: Intent::Place { item: "mason:mason".into(), position: [20.0, 9.0, 20.0] } });
    let mason = EntityId { change: placed, n: 0 };
    let state = replay(&log);
    assert!(state.problems.is_empty(), "{:?}", state.problems);
    // A solid build, charged to the admin who installed the game, not to Ada.
    let (id, build) = state.builds.iter().next().expect("the mason built a wall");
    assert!(*id >= SCRIPT_BUILDS);
    assert_eq!((build.name.as_str(), build.provider.as_str()), ("wall", "mason"));
    assert_eq!(state.footprints[id].owner, "admin");
    assert!(!state.edits.is_empty(), "the pit was dug");
    assert_eq!(state.logic.state.build_owner(*id), Some("mason"));

    // Snapshots keep games' builds: replay from one doesn't run the script again.
    let snapshot = Snapshot::take(&log, &state);
    let again = replay_from(&log, Some(&snapshot));
    assert_eq!(again.builds.keys().collect::<Vec<_>>(), state.builds.keys().collect::<Vec<_>>());
    assert_eq!(again.footprints[id].owner, "admin");

    // It takes its wall down.
    log.push(by("ada", 102), Action::Intent { intent: Intent::Interact { entity: mason } });
    assert!(replay(&log).builds.is_empty());
    // Two more walls with a budget of one: neither goes up, and nothing else changes.
    let mut tight = log.clone();
    tight.rules.budgets.player_builds = 1;
    tight.push(by("ada", 103), Action::Intent { intent: Intent::Interact { entity: mason } });
    let state = replay(&tight);
    assert!(state.problems.iter().any(|p| p.contains("mason:")), "{:?}", state.problems);
    assert!(state.builds.is_empty());
    log.push(by("ada", 103), Action::Intent { intent: Intent::Interact { entity: mason } });
    assert_eq!(replay(&log).builds.len(), 2);
}

#[test]
fn trials_show_panels_stats_and_builds() {
    let spec = game("inn", vec![fixture("inn")], Some(INN), vec![model("keeper", [6, 12, 6])]);
    let steps: Vec<String> = ["place inn", "interact", "choose room"].iter().map(|s| s.to_string()).collect();
    let report = trial::play(&spec.to_package(1), &steps);
    println!("{report}");
    assert!(report.contains("3. choose room: ok"), "{report}");
    assert!(report.contains("inn:gold = 7"), "{report}");
    let steps: Vec<String> = ["place inn", "interact innkeeper", "choose buy:0"].iter().map(|s| s.to_string()).collect();
    let report = trial::play(&spec.to_package(1), &steps);
    assert!(report.contains("Panel open: \"The innkeeper\" (inn)"), "{report}");
    // Says what was pressed, and what the panel does offer.
    assert!(report.contains("2. interact innkeeper (Innkeeper, "), "{report}");
    assert!(report.contains("\"buy:0\" isn't a choice in \"The innkeeper\" (it offers room, bye)"), "{report}");
    let spec = game("mason", vec![fixture("mason")], Some(MASON), vec![model("wall", [16, 16, 4]), model("post", [4, 8, 4])]);
    let report = trial::play(&spec.to_package(1), &["place mason".to_string()]);
    assert!(report.contains("built wall") && report.contains("reshaped 16x16 columns"), "{report}");
}

/// `take` reads like `give`: with the player first, as long as it's the player acting.
#[test]
fn take_reads_like_give() {
    let script = r#"
function on_place(ev)
  spawn(ev.pos, { look = { model = "keeper" }, label = "Purse", purse = true })
end
function on_interact(ev)
  give(ev.player, "coin", 5)
  take(ev.player, "coin", 2)
  take("coin")
  if ev.pos.x > 100 then take("someone else", "coin") end
end
"#;
    let spec = game("inn", vec![fixture("inn")], Some(script), vec![model("keeper", [6, 12, 6])]);
    let steps: Vec<String> = ["place inn", "interact"].iter().map(|s| s.to_string()).collect();
    let report = trial::play(&spec.to_package(1), &steps);
    assert!(report.contains("inn:coin ×2"), "{report}");
    let script = script.replace("ev.pos.x > 100", "true");
    let spec = game("inn", vec![fixture("inn")], Some(&script), vec![model("keeper", [6, 12, 6])]);
    let report = trial::play(&spec.to_package(1), &steps);
    assert!(report.contains("take: only from the player acting"), "{report}");
    assert!(!report.contains("stack traceback"), "{report}");
}
