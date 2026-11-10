//! The world as a folder the agent works in, the way a coding agent works in a
//! repository: it lists, reads and searches files to see what's there, and changes the
//! world's game logic by editing the package files in place. The folder is written fresh
//! before each request from the ledger (it's a view, never the truth), and the packages
//! the agent changed are read back afterwards as a change to install.
//!
//! ```text
//! README.md            how the folder works
//! WORLD.md             the world at a glance: where the player is, places, features
//! GAME.md              the game around it: its worlds, saves, packages, who the player is
//! places.jsonl         what everything means (`genverse_rules::semantic`), one per line
//! builds/<id>.json     each build: name, about, request, position (and <id>.spec.json)
//! pieces.jsonl         every game piece: id, package, position, components
//! players.json         players' stats and items
//! history.md           recent changes and the requests behind them
//! packages/<name>/     package.json (title, defs), *.luau files, models/<model>.json
//! docs/                the scripting reference
//! ```
//!
//! When the agent changes the world while it works (an op: a save, an undo, a new
//! world), [`refresh`] rewrites the view so the files stay true, leaving alone the
//! packages the agent is in the middle of editing.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Write as _,
    path::{Path, PathBuf},
    sync::Mutex,
};

use serde_json::{Value, json};

use genverse_rules::{
    build::BuildSpec,
    logic::data::{DefSpec, GameSpec, ModelSpec, ScriptSpec},
    semantic::{self, Record},
};

use crate::world::ledger::Ledger;

/// The folder of the request running now, for tools that read it (`test_game` tries
/// the packages as they are on disk).
static CURRENT: Mutex<Option<Current>> = Mutex::new(None);

/// The folder of the request running now, and what it was told that the ledger doesn't
/// hold, to write the view again.
#[derive(Clone)]
struct Current {
    dir: PathBuf,
    here: Option<String>,
    errors: Vec<String>,
}

pub fn current() -> Option<PathBuf> {
    CURRENT.lock().ok().and_then(|c| c.as_ref().map(|c| c.dir.clone()))
}

/// Where a world's folder goes: the agent's own scratch space, by game and world, and
/// by where this copy of the game keeps its saves (two copies never share a folder).
fn folder(ledger: &Ledger) -> PathBuf {
    let (game, world) = crate::world::ledger::place();
    let world = ledger.online.as_ref().and_then(|o| o.world.clone()).unwrap_or(world);
    let home = std::env::current_dir().map(|d| d.to_string_lossy().to_string()).unwrap_or_default();
    let home = format!("{:08x}", genverse_rules::log::fnv1a(home.as_bytes()) as u32);
    std::env::temp_dir().join("genverse-agent").join(home).join(clean(&game)).join(clean(&world))
}

fn clean(name: &str) -> String {
    name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect()
}

fn put(path: &Path, text: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("can't create {}: {e}", parent.display()))?;
    }
    std::fs::write(path, text).map_err(|e| format!("can't write {}: {e}", path.display()))
}

fn pretty(value: &impl serde::Serialize) -> String {
    serde_json::to_string_pretty(value).unwrap_or_default() + "\n"
}

/// Writes the world's folder for a request, and returns it. `here` says where the
/// player stands and the ground around them; `errors` are what scripts hit lately.
pub fn write(ledger: &Ledger, here: Option<&str>, errors: &[String]) -> Result<PathBuf, String> {
    let dir = folder(ledger);
    if let Ok(mut current) = CURRENT.lock() {
        *current = None;
    }
    // Only ever our own folder, under the agent's scratch space.
    if dir.exists() {
        std::fs::remove_dir_all(&dir).map_err(|e| format!("can't clear {}: {e}", dir.display()))?;
    }
    put(&dir.join("README.md"), README)?;
    put(&dir.join("docs/game.md"), super::prompt::GAME_DOC)?;
    let mut changes = String::from("# What changed in the scripting API\n\nA package records the API it was written for (`api`); WORLD.md says which are behind.\n");
    for (version, what) in genverse_rules::logic::package::API_CHANGES {
        let _ = write!(changes, "\n## {version}\n\n{what}\n");
    }
    put(&dir.join("docs/changes.md"), &changes)?;
    view(&dir, ledger, here, errors)?;
    packages(&dir, ledger, &BTreeSet::new())?;
    if let Ok(mut current) = CURRENT.lock() {
        *current = Some(Current { dir: dir.clone(), here: here.map(str::to_string), errors: errors.to_vec() });
    }
    Ok(dir)
}

/// The package folders the agent has changed so far in this request (or left unreadable,
/// mid-edit): [`refresh`] keeps them as they are.
pub fn edited(ledger: &Ledger) -> BTreeSet<String> {
    let Some(dir) = current() else { return BTreeSet::new() };
    let Ok(entries) = std::fs::read_dir(dir.join("packages")) else { return BTreeSet::new() };
    entries
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().to_string();
            let base = ledger.logic.installed.get(&name).and_then(|i| ledger.logic.package(i)).map(GameSpec::from_package);
            (!matches!(diff(&entry.path(), &name, base.as_ref()), Ok(None))).then_some(name)
        })
        .collect()
}

/// Writes the view again after the agent changed the world mid-request, so what it
/// reads next is true. `edited` are the packages it had changed before ([`edited`]):
/// those stay as it left them; the others follow the world (a package turned off goes,
/// one turned on appears).
pub fn refresh(ledger: &Ledger, edited: &BTreeSet<String>) -> Result<(), String> {
    let Some(current) = CURRENT.lock().ok().and_then(|c| c.clone()) else { return Ok(()) };
    view(&current.dir, ledger, current.here.as_deref(), &current.errors)?;
    packages(&current.dir, ledger, edited)
}

/// The files that show the world: everything but the packages and the docs.
fn view(dir: &Path, ledger: &Ledger, here: Option<&str>, errors: &[String]) -> Result<(), String> {
    let records = semantic::records(&ledger.log, &ledger.builds, &ledger.logic);
    put(&dir.join("WORLD.md"), &overview(ledger, here, errors, &records))?;
    put(&dir.join("GAME.md"), &game(ledger))?;
    let mut places = String::new();
    for r in &records {
        places.push_str(&serde_json::to_string(r).unwrap_or_default());
        places.push('\n');
    }
    put(&dir.join("places.jsonl"), &places)?;
    // Builds that are gone leave no file behind.
    let _ = std::fs::remove_dir_all(dir.join("builds"));
    for (id, b) in ledger.builds.iter() {
        let record = records.iter().find(|r| r.of == format!("build:{id}"));
        let summary = json!({
            "id": id,
            "name": b.name,
            "about": b.spec.about,
            "request": record.map(|r| r.prompt.as_str()).filter(|p| !p.is_empty()).unwrap_or(&b.prompt),
            "by": record.map(|r| r.by.as_str()),
            "position": b.position,
            "yaw": b.yaw,
            "size_voxels": b.spec.size,
            "portal": ledger.portals.get(id).map(|p| p.label().to_string()),
        });
        put(&dir.join(format!("builds/{id}.json")), &pretty(&summary))?;
        put(&dir.join(format!("builds/{id}.spec.json")), &pretty(&b.spec))?;
    }
    let mut pieces = String::new();
    for (id, e) in ledger.logic.state.entities() {
        let line = json!({ "id": format!("{}.{}", id.change, id.n), "package": e.package, "pos": e.position, "c": e.components });
        pieces.push_str(&line.to_string());
        pieces.push('\n');
    }
    put(&dir.join("pieces.jsonl"), &pieces)?;
    let state = &ledger.logic.state;
    let players: BTreeMap<String, Value> = state
        .players()
        .into_iter()
        .map(|p| {
            let stats: BTreeMap<&str, f64> = state.stats(&p).map(|(n, s)| (n, s.value)).collect();
            let items: BTreeMap<&str, u32> = state.inventory(&p).collect();
            let me = p == ledger.profile.id;
            let v = json!({ "you_are_talking_to": me, "stats": stats, "items": items });
            (p, v)
        })
        .collect();
    put(&dir.join("players.json"), &pretty(&players))?;
    put(&dir.join("history.md"), &history(ledger))
}

/// The packages the world runs, as folders; those in `keep` are left as they are.
fn packages(dir: &Path, ledger: &Ledger, keep: &BTreeSet<String>) -> Result<(), String> {
    let root = dir.join("packages");
    if let Ok(entries) = std::fs::read_dir(&root) {
        for entry in entries.flatten() {
            if entry.path().is_dir() && !keep.contains(entry.file_name().to_string_lossy().as_ref()) {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
    }
    for install in ledger.logic.installed.values() {
        let Some(package) = ledger.logic.package(install) else { continue };
        let spec = GameSpec::from_package(package);
        if keep.contains(&spec.name) {
            continue;
        }
        let root = root.join(&spec.name);
        let manifest = json!({ "name": spec.name, "title": spec.title, "version": package.manifest.version, "defs": spec.defs });
        put(&root.join("package.json"), &pretty(&manifest))?;
        for script in &spec.scripts {
            put(&root.join(&script.name), &script.source)?;
        }
        for model in &spec.models {
            put(&root.join("models").join(format!("{}.json", model.name)), &pretty(&model.build))?;
        }
    }
    std::fs::create_dir_all(&root).map_err(|e| e.to_string())
}

/// The game around this world: what the Game window shows a player.
fn game(ledger: &Ledger) -> String {
    use crate::world::ledger as files;
    let (local_game, local_world) = files::place();
    let log = match &ledger.online {
        Some(online) => online.game.clone(),
        None => Some(files::read_game(&local_game)),
    };
    let name = match &ledger.online {
        Some(online) => online.game.as_ref().map_or(online.addr.clone(), |g| g.name.clone()),
        None => local_game,
    };
    let world = ledger.online.as_ref().and_then(|o| o.world.clone()).unwrap_or(local_world);
    let mut out = format!("# The game \"{name}\"\n\n");
    let shared = match (&ledger.hosting, &ledger.online) {
        (Some(server), _) => format!("The player hosts it from this computer (port {}): others can join every world in it.", server.addr.port()),
        (None, Some(online)) => format!("It's hosted at {}: the player joined it, and others may be playing.", online.addr),
        (None, None) => "It's on the player's computer, not shared (host opens it to others).".into(),
    };
    let role = if ledger.land.is_admin(&ledger.profile.id) {
        "an admin of this game: every tool is theirs to use"
    } else {
        "not an admin of this game: tools for admins are refused"
    };
    let _ = writeln!(out, "{shared} The player is {}, {role}.\n", ledger.profile.name);

    out.push_str("## Worlds\n\n");
    let here = |n: &str| if n == world { " ← the player is here" } else { "" };
    match &ledger.online {
        Some(_) => {
            for (n, ground) in log.as_ref().map(|g| g.worlds()).unwrap_or_default() {
                let _ = writeln!(out, "- {n} ({}){}", if ground.flat { "flat" } else { "natural" }, here(&n));
            }
        }
        None => {
            for w in files::list_worlds() {
                let _ = writeln!(out, "- {} ({}, {} changes){}", w.name, if w.flat { "flat" } else { "natural" }, w.changes, here(&w.name));
            }
        }
    }

    out.push_str("\n## Saves of this world, newest first\n\n");
    let mut seen = std::collections::HashSet::new();
    let saves: Vec<String> = ledger
        .log
        .saves()
        .rev()
        .filter(|(_, n)| seen.insert(n.to_lowercase()))
        .take(20)
        .map(|(c, n)| format!("- \"{n}\" (change {}, by {})", c.id, c.by.player_name))
        .collect();
    out.push_str(&if saves.is_empty() { "None yet.\n".to_string() } else { saves.join("\n") + "\n" });

    out.push_str("\n## Saves of the whole game, newest first\n\n");
    let mut seen = std::collections::HashSet::new();
    let saves: Vec<String> = log
        .iter()
        .flat_map(|g| g.saves())
        .rev()
        .filter(|(_, n, _)| seen.insert(n.to_lowercase()))
        .take(20)
        .map(|(_, n, worlds)| format!("- \"{n}\" ({} worlds)", worlds.len()))
        .collect();
    out.push_str(&if saves.is_empty() { "None yet.\n".to_string() } else { saves.join("\n") + "\n" });

    out.push_str("\n## The game's packages\n\nInstalled once for the game; each world turns on the ones it plays.\n\n");
    let catalog = log.as_ref().map(|g| g.packages()).unwrap_or_default();
    if catalog.is_empty() {
        out.push_str("None yet.\n");
    }
    for (n, package) in &catalog {
        let on = if ledger.logic.installed.contains_key(n) { "on in this world" } else { "off in this world (turn_on)" };
        let _ = writeln!(out, "- {n} (\"{}\", v{}): {on}", package.manifest.title, package.manifest.version);
    }

    out.push_str("\n## Portals in this world\n\n");
    if ledger.portals.is_empty() {
        out.push_str("None yet.\n");
    }
    for (id, to) in &ledger.portals {
        let build = ledger.builds.get(id).map_or("a build", |b| b.name.as_str());
        let _ = writeln!(out, "- build {id} \"{build}\" → {}", to.label());
    }
    out
}

/// The world at a glance.
fn overview(ledger: &Ledger, here: Option<&str>, errors: &[String], records: &[Record]) -> String {
    let (game, world) = crate::world::ledger::place();
    let mut out = format!("# {world} (a world of the game \"{game}\")\n\n");
    if let Some(here) = here {
        let _ = writeln!(out, "## Where the player is\n\n{here}");
    }
    let count = |kind: &str| records.iter().filter(|r| r.kind == kind).count();
    let _ = writeln!(
        out,
        "## What's here\n\n{} builds, {} land changes, {} game pieces with labels, {} named places. \
         Every one is a line in places.jsonl; the `search` tool finds them by meaning and place.\n",
        count("build"),
        count("land"),
        count("piece"),
        count("place")
    );
    let places: Vec<&Record> = records.iter().filter(|r| r.kind == "place").collect();
    if !places.is_empty() {
        out.push_str("Named places:\n");
        for p in places {
            let at = p.at.map_or(String::new(), |a| format!(" at ({:.0}, {:.0})", a[0], a[2]));
            let _ = writeln!(out, "- {}{at}: {}", p.name, p.about);
        }
        out.push('\n');
    }
    out.push_str("## Game logic\n\n");
    if ledger.logic.installed.is_empty() {
        out.push_str("None yet. A new package goes in packages/world/ (see docs/game.md).\n");
    }
    for (name, install) in &ledger.logic.installed {
        let Some(package) = ledger.logic.package(install) else { continue };
        let _ = writeln!(out, "Package \"{name}\" (\"{}\", v{}), in packages/{name}/:", package.manifest.title, package.manifest.version);
        if package.manifest.api < genverse_rules::logic::package::API {
            let _ = writeln!(
                out,
                "  ⚠ written for scripting API {} (it's {} now): read docs/changes.md and bring its files up to date when you change it",
                package.manifest.api,
                genverse_rules::logic::package::API
            );
        }
        for file in package.scripts.keys() {
            let about = records.iter().find(|r| r.of == format!("feature:{name}/{file}")).map(|r| r.about.as_str()).unwrap_or_default();
            let pieces = ledger.logic.state.entities().filter(|(_, e)| e.package == *name && e.components.get("feature").and_then(|f| f.as_str()) == Some(file.as_str())).count();
            let _ = writeln!(out, "- {file}: {} ({pieces} pieces)", if about.is_empty() { "(no opening comment)" } else { about });
            // Calls to what the game no longer has: the feature is broken until they go.
            let source = &package.scripts[file];
            for (name, instead) in genverse_rules::logic::script::REMOVED {
                if source.contains(&format!("{name}(")) {
                    let _ = writeln!(out, "  ⚠ calls {name}(), which the game no longer has (use {instead}): this feature doesn't work until that's fixed");
                }
            }
        }
    }
    if !errors.is_empty() {
        let _ = writeln!(out, "\nRecent errors from these scripts while players played:\n- {}", errors.join("\n- "));
    }
    if !ledger.load_problems.is_empty() {
        let _ = writeln!(
            out,
            "\nSaved changes that no longer applied when the world loaded (fix what they meant to do):\n- {}",
            ledger.load_problems.iter().take(10).cloned().collect::<Vec<_>>().join("\n- ")
        );
    }
    let names: Vec<String> = match &ledger.online {
        Some(online) => online.game.as_ref().map(|g| g.worlds().into_iter().map(|(n, _)| n).collect()).unwrap_or_default(),
        None => crate::world::ledger::list_worlds().into_iter().map(|w| w.name).collect(),
    };
    let names: Vec<String> = names.into_iter().filter(|n| *n != world).collect();
    let _ = writeln!(
        out,
        "\n## Other worlds of this game\n\n{} (for portals, by name; another game of the player's is \"game/world\").",
        if names.is_empty() { "None yet".into() } else { names.join(", ") }
    );
    out
}

/// The last changes, newest last, with the requests behind them.
fn history(ledger: &Ledger) -> String {
    let mut out = String::from("# Recent changes\n\n");
    let changes = &ledger.log.changes;
    for c in changes.iter().filter(|c| !matches!(c.action, genverse_rules::log::Action::Timers { .. })).rev().take(60).collect::<Vec<_>>().into_iter().rev() {
        let request = c.by.prompt.as_deref().map(|p| format!(" — asked: \"{}\"", p.replace('\n', " "))).unwrap_or_default();
        let agent = c.by.agent.as_deref().map(|a| format!(" via {a}")).unwrap_or_default();
        let _ = writeln!(out, "- {} by {}{agent}: {}{request}", c.id, c.by.player_name, c.action.describe());
    }
    out
}

/// What the agent changed in the packages, as a change to install: new and changed
/// files, defs and models, and what it took out (by name). `None` if nothing changed.
/// Only one package may change in one request.
pub fn changed_game(dir: &Path, ledger: &Ledger) -> Result<Option<GameSpec>, String> {
    let root = dir.join("packages");
    let Ok(entries) = std::fs::read_dir(&root) else { return Ok(None) };
    let mut changes = Vec::new();
    for entry in entries.flatten() {
        if !entry.path().is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        let base = ledger.logic.installed.get(&name).and_then(|i| ledger.logic.package(i)).map(GameSpec::from_package);
        if let Some(change) = diff(&entry.path(), &name, base.as_ref())? {
            changes.push(change);
        }
    }
    match changes.len() {
        0 => Ok(None),
        1 => Ok(changes.pop()),
        _ => Err(format!(
            "you changed {} packages ({}); change one per request",
            changes.len(),
            changes.iter().map(|c| c.name.as_str()).collect::<Vec<_>>().join(", ")
        )),
    }
}

fn diff(root: &Path, name: &str, base: Option<&GameSpec>) -> Result<Option<GameSpec>, String> {
    let manifest: Value = match std::fs::read_to_string(root.join("package.json")) {
        Ok(text) => serde_json::from_str(&text).map_err(|e| format!("packages/{name}/package.json isn't JSON: {e}"))?,
        Err(_) => json!({}),
    };
    let title = manifest["title"].as_str().unwrap_or_default().to_string();
    let defs: Vec<DefSpec> = serde_json::from_value(manifest["defs"].clone()).or_else(|e| if manifest["defs"].is_null() { Ok(Vec::new()) } else { Err(e) }).map_err(|e| {
        format!("packages/{name}/package.json: \"defs\" is a list of {{\"name\", \"kind\": \"item\", \"title\", \"model\"}}: {e}")
    })?;
    let mut scripts = Vec::new();
    let mut models = Vec::new();
    for entry in std::fs::read_dir(root).map_err(|e| e.to_string())?.flatten() {
        let file = entry.file_name().to_string_lossy().to_string();
        if file.ends_with(".luau") || file.ends_with(".lua") {
            let source = std::fs::read_to_string(entry.path()).map_err(|e| e.to_string())?;
            scripts.push(ScriptSpec { name: file, source });
        }
    }
    if let Ok(entries) = std::fs::read_dir(root.join("models")) {
        for entry in entries.flatten() {
            let file = entry.file_name().to_string_lossy().to_string();
            let Some(model) = file.strip_suffix(".json") else { continue };
            let text = std::fs::read_to_string(entry.path()).map_err(|e| e.to_string())?;
            let build: BuildSpec = serde_json::from_str(&text).map_err(|e| format!("packages/{name}/models/{file} isn't a model (the \"build\" format): {e}"))?;
            models.push(ModelSpec { name: model.to_string(), build });
        }
    }
    scripts.sort_by(|a, b| a.name.cmp(&b.name));
    models.sort_by(|a, b| a.name.cmp(&b.name));
    let Some(base) = base else {
        if scripts.is_empty() && models.is_empty() && defs.is_empty() {
            return Ok(None);
        }
        return Ok(Some(GameSpec { name: name.into(), title, defs, scripts, models, remove: None, rename: None }));
    };
    let mut change = GameSpec { name: name.into(), title: String::new(), defs: Vec::new(), scripts: Vec::new(), models: Vec::new(), remove: None, rename: None };
    let mut gone = Vec::new();
    if !title.trim().is_empty() && title != base.title {
        change.title = title;
    }
    change.defs = defs.iter().filter(|d| !base.defs.contains(d)).cloned().collect();
    gone.extend(base.defs.iter().filter(|d| !defs.iter().any(|n| n.name == d.name)).map(|d| d.name.clone()));
    change.scripts = scripts.iter().filter(|s| !base.scripts.contains(s)).cloned().collect();
    gone.extend(base.scripts.iter().filter(|s| !scripts.iter().any(|n| n.name == s.name)).map(|s| s.name.clone()));
    change.models = models.iter().filter(|m| !base.models.contains(m)).cloned().collect();
    gone.extend(base.models.iter().filter(|m| !models.iter().any(|n| n.name == m.name)).map(|m| m.name.clone()));
    if !gone.is_empty() {
        change.remove = Some(gone);
    }
    let unchanged = change.title.is_empty() && change.defs.is_empty() && change.scripts.is_empty() && change.models.is_empty() && change.remove.is_none();
    Ok((!unchanged).then_some(change))
}

/// Puts a change from the answer's "game" on top of what the agent edited in files:
/// the answer's names win, and its removals and renames add to the files'.
pub fn merge(files: Option<GameSpec>, answer: Option<GameSpec>) -> Option<GameSpec> {
    let (files, answer) = match (files, answer) {
        (None, a) => return a,
        (f, None) => return f,
        (Some(f), Some(a)) if f.name != a.name => return Some(a),
        (Some(f), Some(a)) => (f, a),
    };
    let mut out = files;
    if !answer.title.trim().is_empty() {
        out.title = answer.title;
    }
    out.defs.retain(|d| !answer.defs.iter().any(|n| n.name == d.name));
    out.defs.extend(answer.defs);
    out.scripts.retain(|s| !answer.scripts.iter().any(|n| n.name == s.name));
    out.scripts.extend(answer.scripts);
    out.models.retain(|m| !answer.models.iter().any(|n| n.name == m.name));
    out.models.extend(answer.models);
    if let Some(remove) = answer.remove {
        out.remove.get_or_insert_with(Vec::new).extend(remove);
    }
    out.rename = answer.rename;
    Some(out)
}

const README: &str = r#"# This world, as files

This folder is a view of the Genverse world you're working in, written fresh for this
request. Read and search it like a repository: Glob, Grep and Read.

- WORLD.md: where the player is, the ground around them, named places, the game's
  features and recent script errors. Start here.
- GAME.md: the game this world is part of: its worlds, saves and packages, whether
  it's shared, and whether the player is an admin.
- places.jsonl: everything in the world and what it means, one JSON object per line:
  builds, land changes, game packages and their files, labeled pieces, named places.
  Grep it ("wolf", "bakery"), or use the `search` tool to find things by meaning and
  distance.
- builds/<id>.json: one build: name, what it's about, the request that made it, where
  it stands. builds/<id>.spec.json is its voxel spec.
- pieces.jsonl: every game piece: id, package, position, components.
- players.json: players' stats and items.
- history.md: recent changes, who made them, and what they asked for.
- packages/<name>/: the game logic, as it runs now. package.json has the title and the
  defs (items); each .luau file is a feature; models/<model>.json are the voxel models
  pieces are drawn as.
- docs/game.md: the scripting reference. Read it before writing game logic.

To change the game logic, edit the files under packages/ in place (Edit, Write): change
a feature's .luau file, add a new file for a new feature, add or change a model, add a
def to package.json. What you change there is installed as the next version when you
answer; test it first with `test_game` (it tries the files as they are now). Start each
.luau file with a comment saying what the feature is. Files elsewhere in this folder are
for reading: changing them changes nothing.

Tools beyond the files:
- query: runs Lua with the game API against the live world and returns what it
  returns: `return #find("wolf")`, `return near({x = 0, y = 9, z = 0}, 20)`.
- search: finds things by meaning, near a place: "where are the wolves", "anything
  about bread near me".
- test_game: plays your change as a player would, on a copy of the world.
- The rest do things, as the player would from the chat or the Game window: make a
  world, go to one, link a portal, save, undo, change the rules. They happen at once
  (the files here follow), except going elsewhere, which waits until you finish.
"#;
