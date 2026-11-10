//! Every value that can be tuned, in one list: the world's rules (limits, budgets, the
//! clock, how heroes move) and the player's own options (graphics). Each is a
//! [`Setting`]: a name like `hero.run_speed`, what it is, its range, whose it is
//! ([`Scope`]), and where it's kept.
//!
//! The chat (`/set hero.run_speed 10`), the Settings window and the agent all change
//! them through here, so a value that can be tuned one way can be tuned every way, and
//! is checked the same. A world's settings are its admins' to change and are recorded
//! in its log like any change (and undone like one); a player's are theirs, kept with
//! their preferences.
//!
//! The values themselves stay where the game reads them ([`WorldRules`], [`Graphics`]);
//! a setting is a path into one of them. A field added to either doesn't pass the tests
//! until it's a setting here, or listed as not one with the reason ([`NOT_SETTINGS`]).

use genverse_rules::settings::WorldRules;
use serde_json::{Value, json};

use crate::graphics::Graphics;

/// Whose a setting is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Scope {
    /// The world's, for everyone in it: its admins change it, and it's in the log.
    World,
    /// The player's own, on their machine.
    Player,
}

/// What a setting holds, and what it may be.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Kind {
    /// A whole number.
    Whole { min: u64, max: u64 },
    /// A whole number, or no limit at all ("off").
    Limit { max: u64 },
    Number { min: f64, max: f64 },
    Flag,
    /// One of these, as they're kept.
    Choice(&'static [&'static str]),
}

/// Where a setting is kept.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Store {
    Rules,
    Graphics,
}

pub struct Setting {
    pub name: &'static str,
    pub title: &'static str,
    pub kind: Kind,
    store: Store,
    /// Where it is in its store, as a JSON pointer.
    path: &'static str,
}

impl Setting {
    pub fn scope(&self) -> Scope {
        match self.store {
            Store::Rules => Scope::World,
            Store::Graphics => Scope::Player,
        }
    }

    /// The first part of its name: settings are shown in these groups.
    pub fn group(&self) -> &'static str {
        self.name.rsplit_once('.').map_or(self.name, |(group, _)| group)
    }
}

const fn rule(name: &'static str, path: &'static str, kind: Kind, title: &'static str) -> Setting {
    Setting { name, title, kind, store: Store::Rules, path }
}

const fn graphic(name: &'static str, path: &'static str, kind: Kind, title: &'static str) -> Setting {
    Setting { name, title, kind, store: Store::Graphics, path }
}

const fn whole(min: u64, max: u64) -> Kind {
    Kind::Whole { min, max }
}

const fn number(min: f64, max: f64) -> Kind {
    Kind::Number { min, max }
}

const LEVELS: Kind = Kind::Choice(&["Off", "Low", "Medium", "High", "Ultra"]);

/// Every setting, in the order they're listed.
pub static SETTINGS: &[Setting] = &[
    rule("hero.walk_speed", "/hero/walk_speed", number(0.5, 20.0), "How fast heroes walk, in meters a second"),
    rule("hero.run_speed", "/hero/run_speed", number(0.5, 40.0), "How fast heroes run, in meters a second"),
    rule("hero.jump_speed", "/hero/jump_speed", number(0.0, 30.0), "How fast a jump leaves the ground, in meters a second (7.5 goes a little over a meter high)"),
    rule("hero.gravity", "/hero/gravity", number(1.0, 100.0), "How fast heroes fall, in meters a second each second"),
    rule("world.land", "/land", Kind::Choice(&["open", "parcels"]), "Whether land is open to everyone or claimed parcel by parcel"),
    rule("world.spawn.radius_m", "/spawn/radius_m", number(0.0, 256.0), "How far around the start only admins build, in meters"),
    rule("world.look.bevel", "/look/bevel", number(0.0, 0.3), "How far voxels' edges are rounded, as a share of a voxel (0 is sharp cubes)"),
    rule("world.build.max_size", "/build/max_size", whole(8, 256), "The longest side of one build, in build voxels (8 to a meter)"),
    rule("world.build.max_shapes", "/build/max_shapes", whole(1, 4096), "The most shapes in one build"),
    rule("world.build.max_colors", "/build/max_colors", whole(1, 255), "The most colors in one build"),
    rule("world.build.max_voxels", "/build/max_voxels", whole(1_000, 4_000_000), "The most solid voxels in one build"),
    rule("world.build.max_faces", "/build/max_faces", whole(1_000, 2_000_000), "The most visible faces in one build: what it costs to draw"),
    rule("world.build.max_glow_voxels", "/build/max_glow_voxels", whole(0, 200_000), "The most glowing voxels in one build"),
    rule("world.build.max_glass_voxels", "/build/max_glass_voxels", whole(0, 1_000_000), "The most glass voxels in one build"),
    rule("world.terrain.max_area_m", "/terrain/max_area_m", number(4.0, 128.0), "The longest side of one land change, in meters"),
    rule("world.terrain.max_ops", "/terrain/max_ops", whole(1, 200), "The most operations in one land change"),
    rule("world.terrain.max_height_change_m", "/terrain/max_height_change_m", number(0.5, 64.0), "The most one land change raises, lowers or digs, in meters"),
    rule("world.terrain.max_path_points", "/terrain/max_path_points", whole(2, 256), "The most points in one path"),
    rule("world.terrain.max_trees", "/terrain/max_trees", whole(0, 2_000), "The most trees one land change plants"),
    rule("world.terrain.max_changed_voxels", "/terrain/max_changed_voxels", whole(1_000, 20_000_000), "The most voxels one land change changes"),
    rule("world.budgets.parcel_builds", "/budgets/parcel_builds", whole(0, 10_000), "The most builds on one parcel"),
    rule("world.budgets.parcel_faces", "/budgets/parcel_faces", whole(0, 100_000_000), "The most visible faces of builds on one parcel"),
    rule("world.budgets.parcel_voxels", "/budgets/parcel_voxels", whole(0, 1_000_000_000), "The most build voxels on one parcel"),
    rule("world.budgets.parcel_glow", "/budgets/parcel_glow", whole(0, 10_000_000), "The most glowing voxels on one parcel"),
    rule("world.budgets.player_builds", "/budgets/player_builds", whole(0, 1_000_000), "The most builds one player has"),
    rule("world.budgets.player_voxels", "/budgets/player_voxels", whole(0, 10_000_000_000), "The most build voxels one player has"),
    rule("world.budgets.player_terrain", "/budgets/player_terrain", whole(0, 10_000_000_000), "The most land voxels one player has changed, in all"),
    rule("world.rates.changes_per_minute", "/rates/changes_per_minute", Kind::Limit { max: 100_000 }, "The most changes a player makes in a minute (off: no limit; admins are exempt)"),
    rule("world.rates.voxels_per_ten_minutes", "/rates/voxels_per_ten_minutes", Kind::Limit { max: 1_000_000_000 }, "The most voxels a player places or changes in ten minutes (off: no limit)"),
    rule("world.logic.mode", "/logic/mode", Kind::Choice(&["open", "admin_only", "locked"]), "Who may install and change game packages: anyone, admins, or nobody"),
    rule("world.logic.max_entities", "/logic/max_entities", whole(0, 1_000_000), "The most game pieces one package has"),
    rule("world.logic.max_effects", "/logic/max_effects", whole(1, 100_000), "The most changes one player action causes, over all packages"),
    rule("world.clock.day_seconds", "/logic/clock/day_seconds", whole(10, 604_800), "How long a game day lasts, in real seconds"),
    rule("world.clock.days_per_season", "/logic/clock/days_per_season", whole(1, 1_000), "How many game days a season lasts"),
    graphic("graphics.antialias", "/antialias", Kind::Choice(&["Off", "Fxaa", "Taa"]), "How edges are smoothed"),
    graphic("graphics.ambient_occlusion", "/ambient_occlusion", LEVELS, "Soft shading where surfaces meet"),
    graphic("graphics.shadows", "/shadows", LEVELS, "How detailed the sun's shadows are"),
    graphic("graphics.contact_shadows", "/contact_shadows", Kind::Flag, "Small shadows where things touch the ground"),
    graphic("graphics.bloom", "/bloom", Kind::Flag, "A glow around bright things"),
    graphic("graphics.render_scale", "/render_scale", number(0.5, 1.0), "The share of the window's pixels the world is drawn at"),
    graphic("graphics.auto_scale", "/auto_scale", Kind::Flag, "Lower the render scale while frames run slow"),
];

/// Fields of the stores that aren't settings, and why.
#[cfg_attr(not(test), allow(dead_code))]
pub const NOT_SETTINGS: &[(Store, &str, &str)] = &[
    (Store::Rules, "/spawn/center", "where the world starts: moving it would move the protected spawn off what's built there"),
    (Store::Rules, "/logic/clock/epoch", "when day 0 began: games set the time of day with set_time"),
];

/// The setting called `name`: its whole name, in any case, or its last part if only
/// one setting ends that way ("run_speed").
pub fn find(name: &str) -> Result<&'static Setting, String> {
    let name = name.trim().to_lowercase();
    if let Some(setting) = SETTINGS.iter().find(|s| s.name == name) {
        return Ok(setting);
    }
    let ends: Vec<&Setting> = SETTINGS.iter().filter(|s| s.name.rsplit('.').next() == Some(name.as_str())).collect();
    match ends.as_slice() {
        [one] => Ok(one),
        [] => Err(format!("There's no setting \"{name}\" (/settings lists them, like hero.run_speed).")),
        many => Err(format!("\"{name}\" could be {}: give its whole name.", many.iter().map(|s| s.name).collect::<Vec<_>>().join(" or "))),
    }
}

/// The stores as they are now, to read settings from.
pub struct Stores<'a> {
    pub rules: &'a WorldRules,
    pub graphics: &'a Graphics,
}

/// A store with a setting changed, for whoever keeps it to put in place.
pub enum Changed {
    /// The world's rules: recorded as a change, by an admin.
    Rules(Box<WorldRules>),
    /// The player's graphics: put in place and kept with their preferences.
    Graphics(Graphics),
}

/// The world's rules as JSON with every field there (the parts left out of the log
/// while they're the default are put back).
fn rules_json(rules: &WorldRules) -> Value {
    let mut all = serde_json::to_value(rules).unwrap_or_default();
    for (part, value) in [("look", json!(rules.look)), ("logic", json!(rules.logic)), ("hero", json!(rules.hero))] {
        all[part] = value;
    }
    all
}

fn store_json(store: Store, stores: &Stores) -> Value {
    match store {
        Store::Rules => rules_json(stores.rules),
        Store::Graphics => serde_json::to_value(stores.graphics).unwrap_or_default(),
    }
}

/// What `setting` is now.
pub fn value(setting: &Setting, stores: &Stores) -> Value {
    store_json(setting.store, stores).pointer(setting.path).cloned().unwrap_or(Value::Null)
}

/// What `setting` starts as.
pub fn default(setting: &Setting) -> Value {
    value(setting, &Stores { rules: &WorldRules::default(), graphics: &Graphics::default() })
}

/// A value as a player reads it: "12", "0.25", "on", "off" for no limit.
pub fn show(setting: &Setting, value: &Value) -> String {
    match (setting.kind, value) {
        (Kind::Limit { .. }, Value::Null) => "off".into(),
        (Kind::Flag, Value::Bool(on)) => if *on { "on" } else { "off" }.into(),
        (Kind::Number { .. }, Value::Number(n)) => {
            // Kept as 32-bit floats: 0.1 comes back as 0.10000000149.
            let text = format!("{:.3}", n.as_f64().unwrap_or_default());
            text.trim_end_matches('0').trim_end_matches('.').to_string()
        }
        (_, Value::String(text)) => text.clone(),
        (_, other) => other.to_string(),
    }
}

/// What a setting may be, in words.
pub fn range(setting: &Setting) -> String {
    match setting.kind {
        Kind::Whole { min, max } => format!("{min} to {max}"),
        Kind::Limit { max } => format!("0 to {max}, or off"),
        Kind::Number { min, max } => format!("{min} to {max}"),
        Kind::Flag => "on or off".into(),
        Kind::Choice(choices) => choices.join(", "),
    }
}

/// Reads what someone gave for `setting` (as text or JSON) into the value to keep, or
/// says what it may be. "default" is what it starts as.
pub fn parse(setting: &Setting, given: &Value) -> Result<Value, String> {
    let text = match given {
        Value::String(text) => text.trim().to_lowercase(),
        Value::Null => String::new(),
        other => other.to_string(),
    };
    if text == "default" {
        return Ok(default(setting));
    }
    let wrong = || format!("{} is {}: \"{text}\" isn't.", setting.name, range(setting));
    match setting.kind {
        Kind::Whole { min, max } => text.parse::<u64>().ok().filter(|n| (min..=max).contains(n)).map(|n| json!(n)).ok_or_else(wrong),
        Kind::Limit { .. } if ["off", "none", "unlimited", "null"].contains(&text.as_str()) => Ok(Value::Null),
        Kind::Limit { max } => text.parse::<u64>().ok().filter(|n| *n <= max).map(|n| json!(n)).ok_or_else(wrong),
        Kind::Number { min, max } => text.parse::<f64>().ok().filter(|n| n.is_finite() && (min..=max).contains(n)).map(|n| json!(n)).ok_or_else(wrong),
        Kind::Flag => match text.as_str() {
            "on" | "true" | "yes" | "1" => Ok(json!(true)),
            "off" | "false" | "no" | "0" => Ok(json!(false)),
            _ => Err(wrong()),
        },
        Kind::Choice(choices) => choices.iter().find(|c| c.eq_ignore_ascii_case(&text)).map(|c| json!(c)).ok_or_else(wrong),
    }
}

/// The store `setting` is in, with it set to `value` (from [`parse`]).
pub fn with(setting: &Setting, stores: &Stores, value: Value) -> Result<Changed, String> {
    let mut all = store_json(setting.store, stores);
    *all.pointer_mut(setting.path).ok_or(format!("{} has nowhere to go", setting.name))? = value;
    let misfit = |e: serde_json::Error| format!("{} can't be that: {e}", setting.name);
    Ok(match setting.store {
        Store::Rules => Changed::Rules(Box::new(serde_json::from_value(all).map_err(misfit)?)),
        Store::Graphics => Changed::Graphics(serde_json::from_value(all).map_err(misfit)?),
    })
}

/// The settings whose names contain `filter` (all of them, if it's empty), a line each
/// by group, with what each is now.
pub fn list(stores: &Stores, filter: &str) -> String {
    let filter = filter.trim().to_lowercase();
    let mut out = String::new();
    let mut group = "";
    for setting in SETTINGS.iter().filter(|s| s.name.contains(&filter)) {
        if setting.group() != group {
            group = setting.group();
            let whose = match setting.scope() {
                Scope::World => "this world's, for everyone in it: admins change them",
                Scope::Player => "the player's own",
            };
            out.push_str(&format!("{}{group} ({whose})\n", if out.is_empty() { "" } else { "\n" }));
        }
        let now = value(setting, stores);
        let changed = if now == default(setting) { String::new() } else { format!(", default {}", show(setting, &default(setting))) };
        out.push_str(&format!("- {} = {} ({}; {}{changed})\n", setting.name, show(setting, &now), setting.title, range(setting)));
    }
    if out.is_empty() { format!("No setting has \"{filter}\" in its name.") } else { out }
}

/// The settings for the agent's folder: the world's with what they are now, and the
/// player's own by name (the `settings` tool has their values).
pub fn describe(rules: &WorldRules) -> String {
    let stores = Stores { rules, graphics: &Graphics::default() };
    let mut out = String::from(
        "# Settings\n\nEvery value that can be tuned, by name. The `set` tool changes one (`settings` lists them live); \
         a world's setting is for everyone in it, needs the player to be an admin, and is undone like any change.\n",
    );
    let mut group = "";
    for setting in SETTINGS {
        if setting.group() != group {
            group = setting.group();
            let whose = match setting.scope() {
                Scope::World => "this world's",
                Scope::Player => "the player's own: `settings` shows what they are now",
            };
            out.push_str(&format!("\n## {group} ({whose})\n\n"));
        }
        match setting.scope() {
            Scope::World => out.push_str(&format!("- {} = {}: {} ({})\n", setting.name, show(setting, &value(setting, &stores)), setting.title, range(setting))),
            Scope::Player => out.push_str(&format!("- {}: {} ({})\n", setting.name, setting.title, range(setting))),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every leaf under `value`, as JSON pointers.
    fn leaves(value: &Value, at: String, out: &mut Vec<String>) {
        match value {
            Value::Object(map) => map.iter().for_each(|(key, v)| leaves(v, format!("{at}/{key}"), out)),
            _ => out.push(at),
        }
    }

    #[test]
    fn every_field_of_the_stores_is_a_setting_or_says_why_not() {
        let stores = Stores { rules: &WorldRules::default(), graphics: &Graphics::default() };
        for store in [Store::Rules, Store::Graphics] {
            let mut fields = Vec::new();
            leaves(&store_json(store, &stores), String::new(), &mut fields);
            assert!(!fields.is_empty());
            for field in fields {
                let set = SETTINGS.iter().any(|s| s.store == store && s.path == field);
                let excused = NOT_SETTINGS.iter().any(|(s, path, why)| *s == store && *path == field && !why.is_empty());
                assert!(set != excused, "{store:?}{field} must be a setting, or listed in NOT_SETTINGS with the reason (and not both)");
            }
        }
    }

    #[test]
    fn every_setting_reads_writes_and_starts_inside_its_range() {
        let stores = Stores { rules: &WorldRules::default(), graphics: &Graphics::default() };
        let mut names = std::collections::HashSet::new();
        for setting in SETTINGS {
            assert!(names.insert(setting.name), "two settings are called {}", setting.name);
            assert!(setting.name.contains('.') && !setting.title.is_empty(), "{}", setting.name);
            let start = default(setting);
            assert!(matches!(setting.kind, Kind::Limit { .. }) || !start.is_null(), "{} isn't where its path says", setting.name);
            // What it shows as reads back as the same value, and goes back in its store.
            let again = parse(setting, &json!(show(setting, &start))).unwrap_or_else(|why| panic!("{why}"));
            assert_eq!(show(setting, &again), show(setting, &start), "{}", setting.name);
            assert!(with(setting, &stores, again).is_ok(), "{} doesn't fit back", setting.name);
        }
    }

    #[test]
    fn values_are_checked_and_land_in_the_right_store() {
        let stores = Stores { rules: &WorldRules::default(), graphics: &Graphics::default() };
        let run = find("hero.run_speed").expect("it's a setting");
        assert_eq!(find("RUN_SPEED").map(|s| s.name), Ok("hero.run_speed"));
        assert!(find("max_voxels").is_ok() && find("mode").is_ok() && find("nope").is_err());
        assert_eq!(run.scope(), Scope::World);
        assert!(parse(run, &json!("fast")).is_err() && parse(run, &json!(400)).is_err());
        match with(run, &stores, parse(run, &json!("14")).expect("in range")).expect("it fits") {
            Changed::Rules(rules) => {
                assert_eq!(rules.hero.run_speed, 14.0);
                // Only that changed, and the log still leaves out what's default.
                assert_eq!(WorldRules { hero: Default::default(), ..*rules }, WorldRules::default());
            }
            Changed::Graphics(_) => panic!("hero settings are the world's"),
        }
        let rate = find("world.rates.changes_per_minute").expect("it's a setting");
        assert_eq!(show(rate, &value(rate, &stores)), "off");
        assert_eq!(parse(rate, &json!(30)), Ok(json!(30)));
        assert_eq!(parse(rate, &json!("off")), Ok(Value::Null));
        let bloom = find("graphics.bloom").expect("it's a setting");
        assert_eq!(bloom.scope(), Scope::Player);
        match with(bloom, &stores, parse(bloom, &json!("off")).expect("a flag")).expect("it fits") {
            Changed::Graphics(graphics) => assert!(!graphics.bloom),
            Changed::Rules(_) => panic!("graphics are the player's"),
        }
        let shadows = find("graphics.shadows").expect("it's a setting");
        assert_eq!(parse(shadows, &json!("ultra")), Ok(json!("Ultra")));
        assert_eq!(parse(run, &json!("default")), Ok(default(run)));
        assert!(list(&stores, "hero").contains("hero.jump_speed = 7.5") && !list(&stores, "hero").contains("graphics"));
    }
}
