//! A world's game logic grows request by request: each change adds to the package by
//! name, every script file is a feature whose handlers all run, a new version starts
//! only its new and changed files, and a file taken out takes its pieces with it.

use genverse_rules::{
    log::{Action, Provenance, WorldLog, replay},
    logic::{
        Intent, Scope,
        data::{GameSpec, ModelSpec, ScriptSpec},
    },
};

fn by(player: &str, at: u64) -> Provenance {
    Provenance::new(player, player, None, None, at)
}

fn model(name: &str) -> ModelSpec {
    let build = r##"{"name":"Piece","size":[4,8,4],"palette":[{"name":"fur","hex":"#777777","material":"matte"}],
        "shapes":[{"kind":"box","color":"fur","from":[0,0,0],"to":[4,8,4],"radius":null,"wobble":null}]}"##;
    ModelSpec { name: name.into(), build: serde_json::from_str(build).unwrap() }
}

/// A change to the world's package: only these files and models.
fn change(scripts: &[(&str, &str)], models: &[&str], remove: Option<Vec<String>>) -> GameSpec {
    GameSpec { shaders: Default::default(),
        name: "world".into(),
        title: String::new(),
        defs: Vec::new(),
        scripts: scripts.iter().map(|(n, s)| ScriptSpec { name: n.to_string(), source: s.to_string() }).collect(),
        models: models.iter().map(|m| model(m)).collect(),
        remove,
        rename: None,
    }
}

/// Applies `change` to what `log`'s world runs, installing the result as the next version.
fn apply(log: &mut WorldLog, change: &GameSpec, at: u64) {
    let state = replay(log);
    let base = state.logic.installed.get("world").and_then(|i| state.logic.package(i)).map(GameSpec::from_package);
    let version = state.logic.installed.get("world").map_or(1, |i| i.package.version + 1);
    let package = change.onto(base.as_ref()).to_package(version);
    assert!(package.check().is_ok(), "{:?}", package.check());
    let reference = package.reference();
    log.add_package(package);
    log.push(by("admin", at), Action::InstallPackage { package: reference, scope: Scope::World });
}

const WOLVES: &str = r#"
function on_start(ev)
  if #find("wolf") == 0 then
    spawn({x = 20, y = 8.75, z = 0}, { look = { model = "wolf" }, label = "Wolf", wolf = true })
  end
end
function on_interact(ev)
  if get(ev.entity).c.wolf then notify(ev.player, "Grr.") end
end
"#;

const MERCHANT: &str = r#"
function on_start(ev)
  spawn({x = 2, y = 8.75, z = 0}, { look = { model = "merchant" }, label = "Merchant", merchant = true })
end
function on_interact(ev)
  if get(ev.entity).c.merchant then give(ev.player, "torch", 1) end
end
"#;

#[test]
fn each_request_adds_to_the_worlds_game() {
    let mut log = WorldLog { flat: true, ..WorldLog::new(7) };
    log.admins = vec!["admin".into()];
    let pieces = |log: &WorldLog, c: &str| replay(log).logic.state.entities().filter(|(_, e)| e.components.contains_key(c)).count();

    // "Wolves in the hills."
    apply(&mut log, &change(&[("wolves.luau", WOLVES)], &["wolf"], None), 100);
    assert_eq!(pieces(&log, "world:wolf"), 1);
    // "Add a merchant": a new file; the wolves stay, and aren't set up twice.
    apply(&mut log, &change(&[("merchant.luau", MERCHANT)], &["merchant"], None), 110);
    let state = replay(&log);
    assert!(state.problems.is_empty(), "{:?}", state.problems);
    assert_eq!(pieces(&log, "world:wolf"), 1);
    assert_eq!(pieces(&log, "world:merchant"), 1);
    let package = state.logic.package(&state.logic.installed["world"]).unwrap();
    assert_eq!(package.scripts.keys().cloned().collect::<Vec<_>>(), vec!["merchant.luau", "wolves.luau"]);
    assert_eq!(package.manifest.version, 2);
    // Both files hear E: each answers for its own pieces.
    let merchant = state.logic.state.entities().find(|(_, e)| e.components.contains_key("world:merchant")).unwrap().0;
    log.push(by("ada", 120), Action::Intent { intent: Intent::Interact { entity: merchant } });
    assert_eq!(replay(&log).logic.state.items("ada", "world:torch"), 1);
    // Pieces remember their feature.
    let wolf = replay(&log).logic.state.entities().find(|(_, e)| e.components.contains_key("world:wolf")).map(|(_, e)| e.clone()).unwrap();
    assert_eq!(wolf.components["feature"], "wolves.luau");

    // "Take the wolves out": their pieces go; the merchant stays.
    apply(&mut log, &change(&[], &[], Some(vec!["wolves.luau".into(), "wolf".into()])), 130);
    let state = replay(&log);
    assert!(state.problems.is_empty(), "{:?}", state.problems);
    assert_eq!(pieces(&log, "world:wolf"), 0);
    assert_eq!(pieces(&log, "world:merchant"), 1);
    let package = state.logic.package(&state.logic.installed["world"]).unwrap();
    assert!(!package.scripts.contains_key("wolves.luau") && !package.models.contains_key("wolf"));

    // A change's summary says what it did.
    let base = GameSpec::from_package(package);
    let summary = change(&[("merchant.luau", MERCHANT), ("night.luau", "")], &[], Some(vec!["merchant".into()])).change_summary(Some(&base));
    assert_eq!(summary, "added night.luau; changed merchant.luau; removed merchant");
}

const PACK_V1: &str = r#"
function on_start(ev)
  if #find("wolf") == 0 then
    spawn({x = 20, y = 8.75, z = 0}, { look = { model = "wolf" }, label = "Wolf", wolf = { bites = 0 } })
  end
  every(30, "howl", nil)
end
function on_interact(ev)
  set_stat(ev.player, "gold", 5)
end
"#;

/// v2 keeps wolves' strength in a new field, and calls gold "coins".
const PACK_V2: &str = r#"
function on_start(ev)
  if ev.upgrade and ev.from == 1 then
    for _, w in ipairs(find("wolf")) do
      set(w.id, "wolf", { bites = w.c.wolf.bites, strength = 3 })
    end
    for _, p in ipairs(players()) do
      local gold = stat(p, "gold")
      if gold then set_stat(p, "coins", gold); set_stat(p, "gold", 0) end
    end
  end
end
"#;

#[test]
fn upgrades_migrate_rename_and_clean_up() {
    let mut log = WorldLog { flat: true, ..WorldLog::new(7) };
    log.admins = vec!["admin".into()];
    apply(&mut log, &change(&[("wolves.luau", PACK_V1)], &["wolf"], None), 100);
    let wolf = replay(&log).logic.state.entities().find(|(_, e)| e.components.contains_key("world:wolf")).unwrap().0;
    log.push(by("ada", 105), Action::Intent { intent: Intent::Interact { entity: wolf } });
    let state = replay(&log);
    assert_eq!(state.logic.state.stat("ada", "world:gold").map(|s| s.value), Some(5.0));
    assert!(state.logic.state.timers().any(|(_, n, t)| n == "howl" && t.feature.as_deref() == Some("wolves.luau")));

    // v2, renamed to pack.luau: the wolf and its timer follow, and the migration runs on
    // the wolf and on Ada (who isn't here).
    let mut v2 = change(&[("pack.luau", PACK_V2)], &[], None);
    v2.rename = Some(vec![genverse_rules::logic::data::Rename { from: "wolves.luau".into(), to: "pack.luau".into() }]);
    apply(&mut log, &v2, 110);
    let state = replay(&log);
    assert!(state.problems.is_empty(), "{:?}", state.problems);
    let w = state.logic.state.get(wolf).expect("the wolf stays");
    assert_eq!(w.components["feature"], "pack.luau");
    assert_eq!(w.components["world:wolf"]["strength"], 3);
    assert_eq!(state.logic.state.stat("ada", "world:coins").map(|s| s.value), Some(5.0));
    assert!(state.logic.state.timers().any(|(_, n, t)| n == "howl" && t.feature.as_deref() == Some("pack.luau")));

    // Taking pack.luau out takes the wolf and stops its timer.
    apply(&mut log, &change(&[("camp.luau", "function on_join(ev) end")], &[], Some(vec!["pack.luau".into()])), 120);
    let state = replay(&log);
    assert!(state.logic.state.get(wolf).is_none());
    assert!(!state.logic.state.timers().any(|(_, n, _)| n == "howl"));
}

#[test]
fn test_game_tries_a_change_on_a_copy_of_the_world() {
    use genverse_rules::logic::{NoGround, trial};
    let mut log = WorldLog { flat: true, ..WorldLog::new(7) };
    log.admins = vec!["admin".into()];
    apply(&mut log, &change(&[("wolves.luau", PACK_V1)], &["wolf"], None), 100);
    let wolf = replay(&log).logic.state.entities().find(|(_, e)| e.components.contains_key("world:wolf")).unwrap().0;
    log.push(by("ada", 105), Action::Intent { intent: Intent::Interact { entity: wolf } });
    let state = replay(&log);
    let base = GameSpec::from_package(state.logic.package(&state.logic.installed["world"]).unwrap());
    let next = change(&[("wolves.luau", PACK_V2)], &[], None).onto(Some(&base)).to_package(2);
    let copy = trial::Copy { logic: state.logic.clone(), rules: Default::default(), at: [18.0, 8.75, 0.0], now: 200, next_change: 10, ground: &NoGround };
    let report = trial::play_in(&next, &["go 19 0".to_string()], Some(copy));
    println!("{report}");
    assert!(report.contains("Tried on a copy of this world"), "{report}");
    assert!(report.contains("An upgrade from v1 to v2: on_start runs for wolves.luau"), "{report}");
    assert!(report.contains("\"strength\":3"), "{report}");
}
