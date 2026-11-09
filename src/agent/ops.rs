//! Everything a player can do to the game and its worlds, in one place. Each is an
//! [`Op`]: a name, its arguments, who may do it, and the code that does it. The chat's
//! `/commands` ([`super::commands`]), the Game window ([`super::worlds`]) and the history
//! panel all go through [`run`], so a thing that can be done one way can be done every
//! way, under the same checks. The agent's tools come from this list next.
//!
//! An op changes the world only through the [`Ledger`], as the player: the world's rules
//! (land, admins, rates) hold whoever asked.
//!
//! Adding something players can do: write its function here and add it to [`OPS`]. A new
//! kind of change in the log ([`Action`], [`GameAction`]) doesn't compile until
//! [`action_cover`] or [`game_cover`] says which op makes it, or why none does.

use bevy::prelude::*;
use genverse_rules::{
    game::GameAction,
    log::{Action, PortalTo},
    logic::Scope,
    settings::{LandMode, Look, RateLimits},
};
use serde_json::Value;

use super::{
    creations::Creations,
    landscape::Landscape,
    online,
    worlds::{Travel, Trip, fresh_seed},
};
use crate::world::{
    VoxelWorld,
    ledger::{self, Ledger, clean_name, game_name},
};

/// What an op works on.
pub struct Context<'a> {
    pub ledger: &'a mut Ledger,
    pub world: &'a mut VoxelWorld,
    pub creations: &'a mut Creations,
    pub landscape: &'a mut Landscape,
    /// Where to go next, for ops that change worlds.
    pub travel: &'a mut Travel,
    /// Where the player stands, for game commands (`on_chat`).
    pub at: [f32; 3],
}

/// What an op says back: what it did, or why it didn't.
pub type Reply = Result<String, String>;

/// A reply as a line for the chat.
pub fn said(reply: Reply) -> String {
    match reply {
        Ok(text) | Err(text) => text,
    }
}

/// How an argument is read from a chat line.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// One word.
    Word,
    /// Everything that's left, spaces and all (a save's name).
    Rest,
}

pub struct Param {
    pub name: &'static str,
    pub kind: Kind,
}

const fn word(name: &'static str) -> Param {
    Param { name, kind: Kind::Word }
}

const fn rest(name: &'static str) -> Param {
    Param { name, kind: Kind::Rest }
}

/// An op's arguments, by name.
#[derive(Default)]
pub struct Args(serde_json::Map<String, Value>);

impl Args {
    /// From a JSON object: `json!({"name": "dungeon"})`.
    pub fn new(value: Value) -> Self {
        match value {
            Value::Object(map) => Self(map),
            _ => Self::default(),
        }
    }

    /// From the words after a `/command`, in the order of its params.
    pub fn from_words(params: &[Param], words: &[&str]) -> Self {
        let mut map = serde_json::Map::new();
        for (i, param) in params.iter().enumerate() {
            let value = match param.kind {
                Kind::Word => words.get(i).map(|w| w.to_string()),
                Kind::Rest => Some(words.get(i..).unwrap_or_default().join(" ")).filter(|r| !r.is_empty()),
            };
            if let Some(value) = value {
                map.insert(param.name.to_string(), Value::String(value));
            }
        }
        Self(map)
    }

    /// An argument as text (numbers too); `None` if it's missing or empty.
    fn text(&self, name: &str) -> Option<String> {
        match self.0.get(name)? {
            Value::String(s) => Some(s.trim().to_string()).filter(|s| !s.is_empty()),
            Value::Number(n) => Some(n.to_string()),
            Value::Bool(b) => Some(b.to_string()),
            _ => None,
        }
    }

    /// An argument as a whole number.
    fn int(&self, name: &str) -> Option<u64> {
        match self.0.get(name)? {
            Value::Number(n) => n.as_u64(),
            Value::String(s) => s.trim().parse().ok(),
            _ => None,
        }
    }

    /// Flat ground unless it says natural.
    fn flat(&self) -> bool {
        self.text("ground").as_deref() != Some("natural")
    }
}

/// One thing a player can do.
pub struct Op {
    pub name: &'static str,
    /// What follows the name in the chat, e.g. `<id>`; empty for none.
    pub args: &'static str,
    pub about: &'static str,
    pub admin: bool,
    /// Typed in the chat as `/name`, and suggested there.
    pub slash: bool,
    /// Refused while a draft is open: it would change the world under it.
    pub settled: bool,
    pub params: &'static [Param],
    run: fn(&mut Context, &Args) -> Reply,
}

const fn op(name: &'static str, args: &'static str, about: &'static str, params: &'static [Param], run: fn(&mut Context, &Args) -> Reply) -> Op {
    Op { name, args, about, admin: false, slash: true, settled: false, params, run }
}

impl Op {
    const fn admin(mut self) -> Self {
        self.admin = true;
        self
    }

    const fn settled(mut self) -> Self {
        self.settled = true;
        self
    }

    /// Not a chat command: the Game window's only.
    const fn unlisted(mut self) -> Self {
        self.slash = false;
        self
    }
}

/// Every op. The chat suggests its commands in this order.
pub static OPS: &[Op] = &[
    op("history", "[n]", "The latest changes, with ids", &[word("n")], history),
    op("undo", "<id>", "Undo one of your changes (or an undo, to redo)", &[word("id")], undo).settled(),
    op("rules", "", "This world's rules", &[], rules),
    op("verify", "", "Check every change is signed by its player", &[], verify),
    op("play", "", "Join a world on your network, or open this one", &[], play),
    op("host", "", "Open this game, every world in it, to others", &[], host).settled(),
    op("join", "<host[:port]>", "Play in a hosted game", &[word("addr")], join).settled(),
    op("leave", "", "Back to your own game", &[], leave).settled(),
    op("packages", "", "The game rules in this world", &[], packages),
    op("save", "<name>", "Bookmark the world as it is now", &[rest("name")], save),
    op("saves", "", "The saves you can load or fork", &[], |cx, _| Ok(saves(cx.ledger))),
    op("fork", "<save> <world>", "Start a new world of your own from a save", &[word("save"), word("world")], fork),
    op("worlds", "", "This game's worlds (O opens the Game window)", &[], |cx, _| Ok(worlds(cx.ledger))),
    op("world", "<name>", "Go to another world of this game", &[word("name")], world).settled(),
    op("games", "", "Your games on this computer", &[], |_, _| Ok(games())),
    op("game", "<name>", "Go to another of your games", &[word("name")], game).settled(),
    op("newgame", "<name> [flat|natural]", "Make a new game and go there", &[word("name"), word("ground")], new_game).settled(),
    op("savegame", "<name>", "Save every world of the game at once", &[rest("name")], save_game),
    op("portal", "<world|game/world|host:port> | off", "Make the selected build a portal", &[rest("to")], portal),
    op("newworld", "<name> [flat|natural]", "Make a new world in this game", &[word("name"), word("ground")], new_world).admin(),
    op("removeworld", "<name>", "Take a world out of this game (to the trash)", &[word("name")], remove_world).admin(),
    op("loadgame", "<name>", "Put every world back to a game save", &[rest("name")], load_game).admin().settled(),
    op("load", "<name>", "Put the world back to a save, for everyone", &[rest("name")], load).admin().settled(),
    op("land", "open|parcels", "Whether land is open or claimed by parcel", &[word("mode")], land).admin(),
    op("rate", "<per min> [voxels/10 min] | off", "Limit how fast players change things", &[word("per_minute"), word("voxels")], rate).admin(),
    op("bevel", "<0-0.3> | off", "Round voxels' edges for everyone", &[word("bevel")], bevel).admin(),
    op("uninstall", "<name>", "Remove game rules from this world", &[word("name")], uninstall).admin(),
    op("help", "", "List the commands", &[], |_, _| Ok(help())),
    op("turn_on", "<name>", "Turn one of the game's packages on in this world", &[word("name")], turn_on).admin().unlisted(),
    op("remove_package", "<name>", "Take a package out of the game: every world stops running it", &[word("name")], remove_package).admin().unlisted(),
];

/// The chat's commands, in the order they're suggested.
pub fn commands() -> impl Iterator<Item = &'static Op> {
    OPS.iter().filter(|op| op.slash)
}

/// The command typed as `/name`.
pub fn command(name: &str) -> Option<&'static Op> {
    let name = if name == "revert" { "undo" } else { name };
    commands().find(|op| op.name == name)
}

/// Does `name` with `args` (a JSON object), under the checks every way in shares.
pub fn run(name: &str, args: Value, cx: &mut Context) -> Reply {
    let op = OPS.iter().find(|op| op.name == name).ok_or_else(|| format!("There's no \"{name}\" to do."))?;
    run_op(op, &Args::new(args), cx)
}

pub fn run_op(op: &Op, args: &Args, cx: &mut Context) -> Reply {
    if op.settled && (cx.creations.draft.is_some() || cx.landscape.draft.is_some()) {
        return Err("Keep or discard your draft first.".into());
    }
    if op.admin && !cx.ledger.land.is_admin(&cx.ledger.profile.id) {
        return Err("Only admins can do that.".into());
    }
    (op.run)(cx, args)
}

pub fn help() -> String {
    let line = |admin: bool| {
        commands()
            .filter(|c| c.admin == admin)
            .map(|c| if c.args.is_empty() { format!("/{}", c.name) } else { format!("/{} {}", c.name, c.args) })
            .collect::<Vec<_>>()
            .join(" · ")
    };
    format!("Commands: {} · admins: {}", line(false), line(true))
}

fn history(cx: &mut Context, args: &Args) -> Reply {
    let ledger = &*cx.ledger;
    let n = args.int("n").unwrap_or(12) as usize;
    let undone = ledger.log.undone();
    let lines: Vec<String> = ledger
        .log
        .changes
        .iter()
        .rev()
        .take(n.clamp(1, 50))
        .map(|c| {
            let mark = if undone.contains(&c.id) { " (undone)" } else { "" };
            format!("{}: {} by {}{mark}", c.id, c.action.describe(), ledger.describe(&c.by))
        })
        .collect();
    Ok(if lines.is_empty() { "No changes yet.".into() } else { format!("Latest changes:\n{}", lines.join("\n")) })
}

/// Undoes change `id`: your own, or anyone's for admins. Undoing an undo redoes.
fn undo(cx: &mut Context, args: &Args) -> Reply {
    let id = args.int("id").ok_or("Usage: /undo <change id> (see /history)")?;
    let Some(change) = cx.ledger.log.change(id) else {
        return Err(format!("There's no change {id}."));
    };
    let what = change.action.describe();
    let upgrade = matches!(&change.action, Action::InstallPackage { package, .. } if package.version > 1);
    cx.ledger.undo(id)?;
    Ok(if upgrade {
        // The old version is back; what the new one did to the world stays.
        format!("Undid change {id} ({what}). What it changed in the world stays: /load the save made before it to go fully back.")
    } else {
        format!("Undid change {id} ({what}).")
    })
}

fn verify(cx: &mut Context, _: &Args) -> Reply {
    let changes = &cx.ledger.log.changes;
    let bad: Vec<String> = changes.iter().filter(|c| !c.verified()).map(|c| c.id.to_string()).collect();
    if bad.is_empty() {
        Ok(format!("All {} changes are signed by the players they name.", changes.len()))
    } else {
        Err(format!("{} of {} changes aren't signed by their authors: {}", bad.len(), changes.len(), bad.join(", ")))
    }
}

/// Play together is the chat panel's: it looks for worlds on the network.
fn play(_: &mut Context, _: &Args) -> Reply {
    Ok("Looking for worlds on your network…".into())
}

fn host(cx: &mut Context, _: &Args) -> Reply {
    online::host(cx.ledger)
}

fn join(cx: &mut Context, args: &Args) -> Reply {
    let addr = args.text("addr").ok_or("Usage: /join <host[:port]>")?;
    online::join(cx.ledger, &addr)
}

fn leave(cx: &mut Context, _: &Args) -> Reply {
    online::leave(cx.ledger, cx.travel)
}

/// Bookmarks the world as it is now, keeping its state (with the voxel edits).
fn save(cx: &mut Context, args: &Args) -> Reply {
    let name = args.text("name").unwrap_or_default();
    if name.is_empty() || name.chars().count() > 40 {
        return Err("Usage: /save <name> (up to 40 characters)".into());
    }
    let old = cx.ledger.log.save_named(&name).map(|c| c.id);
    cx.ledger.record(None, None, Action::Save { name: name.clone() });
    cx.ledger.keep_saves(&cx.world.edit_list());
    Ok(match old {
        Some(old) => format!("Saved \"{name}\" (it replaces the save of that name from change {old})."),
        None => format!("Saved \"{name}\". An admin can /load it; anyone can /fork it into a world of their own."),
    })
}

fn saves(ledger: &Ledger) -> String {
    let mut seen = std::collections::HashSet::new();
    let lines: Vec<String> = ledger
        .log
        .saves()
        .rev()
        .filter(|(_, name)| seen.insert(name.to_lowercase()))
        .take(20)
        .map(|(c, name)| format!("\"{name}\": change {}, {}", c.id, ledger.describe(&c.by)))
        .collect();
    if lines.is_empty() { "No saves yet: /save <name>".into() } else { format!("Saves, newest first: {}", lines.join(" · ")) }
}

/// Admins: loads a save (by `name`, or its change `id`), for everyone. The world is
/// saved first, so loading that brings it back.
fn load(cx: &mut Context, args: &Args) -> Reply {
    let id = args.int("id").or_else(|| args.text("name").and_then(|name| cx.ledger.log.save_named(&name).map(|c| c.id)));
    let Some(id) = id else {
        return Err(format!("Usage: /load <name>. {}", saves(cx.ledger)));
    };
    let name = match cx.ledger.log.change(id).map(|c| &c.action) {
        Some(Action::Save { name }) => name.clone(),
        _ => return Err(format!("Change {id} isn't a save.")),
    };
    let before = cx.ledger.load(id, &cx.world.edit_list())?;
    Ok(format!("Loaded \"{name}\". The world as it was is saved as \"{before}\"."))
}

/// Starts a new local world from a save: it starts as the world was then, without the
/// history.
fn fork(cx: &mut Context, args: &Args) -> Reply {
    let (Some(save), Some(world)) = (args.text("save"), args.text("world")) else {
        return Err("Usage: /fork <save> <new world name>".into());
    };
    let ledger = &mut *cx.ledger;
    let Some(at) = ledger.log.save_named(&save).map(|c| c.id) else {
        return Err(format!("There's no save \"{save}\". {}", saves(ledger)));
    };
    if ledger.is_online() {
        return Err("Forking makes a world on this computer; do it in your own game.".into());
    }
    let state = ledger.state_of_save(at);
    let log = ledger.log.fork(at, &save, state, ledger.log.changes.last().map_or(0, |c| c.by.at));
    let name = ledger.create_world(&world, (log.seed, log.flat), Some(log))?;
    Ok(format!("Started the world {name} from \"{save}\"; this one carries on as it is. /world {name} goes there."))
}

/// This game's worlds, for the chat.
fn worlds(ledger: &Ledger) -> String {
    let here = super::worlds::current_title(ledger);
    let list: Vec<String> = match &ledger.online {
        Some(online) => online.game.as_ref().map(|g| g.worlds().into_iter().map(|(n, _)| n).collect()).unwrap_or_default(),
        None => ledger::list_worlds()
            .into_iter()
            .map(|w| format!("{} ({}, {} changes)", w.name, if w.flat { "flat" } else { "natural" }, w.changes))
            .collect(),
    };
    format!("You're in {here}. Worlds: {}. /world <name> goes; O opens the Game window.", list.join(" · "))
}

/// Goes to another world of this game; with no name, lists them.
fn world(cx: &mut Context, args: &Args) -> Reply {
    let Some(name) = args.text("name").and_then(|n| clean_name(&n)) else {
        return Ok(worlds(cx.ledger));
    };
    cx.travel.next = Some(if cx.ledger.is_online() { Trip::Remote { world: name } } else { Trip::Local { game: game_name(), world: name } });
    Ok(String::new())
}

/// The local games, for the chat.
fn games() -> String {
    let here = game_name();
    let list: Vec<String> = ledger::list_games()
        .iter()
        .map(|g| format!("{}{} ({} worlds)", g.name, if g.name == here { " ← here" } else { "" }, g.worlds))
        .collect();
    format!("Games: {}. /game <name> goes; /newgame <name> makes one.", list.join(" · "))
}

/// Goes to another local game; with no name, lists them.
fn game(cx: &mut Context, args: &Args) -> Reply {
    let Some(game) = args.text("name").and_then(|n| clean_name(&n)) else {
        return Ok(games());
    };
    match ledger::read_game(&game).start() {
        Some(world) => {
            cx.travel.next = Some(Trip::Local { game, world });
            Ok(String::new())
        }
        None => Err(format!("There's no game \"{game}\". {}", games())),
    }
}

/// Makes a new local game and goes there.
fn new_game(cx: &mut Context, args: &Args) -> Reply {
    let name = args.text("name").ok_or("Usage: /newgame <name> [flat|natural]")?;
    let flat = args.flat();
    let by = genverse_rules::log::Provenance::new(&cx.ledger.profile.id, &cx.ledger.profile.name, None, None, 0);
    let game = ledger::create_game(&name, flat, fresh_seed(flat), &by)?;
    cx.travel.next = Some(Trip::Local { game, world: "main".into() });
    Ok(String::new())
}

/// Makes a world in this game (on a server, asks it to) and, locally, goes there.
fn new_world(cx: &mut Context, args: &Args) -> Reply {
    let name = args.text("name").ok_or("Usage: /newworld <name> [flat|natural]")?;
    let flat = args.flat();
    let seed = fresh_seed(flat);
    if cx.ledger.is_online() {
        let name = clean_name(&name).ok_or("A world's name is letters, digits, - and _.")?;
        cx.ledger.propose_game(GameAction::CreateWorld { name: name.clone(), seed, flat })?;
        return Ok(format!("Asking the server to make {name}…"));
    }
    let name = cx.ledger.create_world(&name, (seed, flat), None)?;
    cx.travel.next = Some(Trip::Local { game: game_name(), world: name });
    Ok(String::new())
}

/// Takes a world out of this game; its files go to the trash.
fn remove_world(cx: &mut Context, args: &Args) -> Reply {
    let name = args.text("name").ok_or("Usage: /removeworld <name>")?;
    if cx.ledger.is_online() {
        cx.ledger.propose_game(GameAction::RemoveWorld { name: name.clone() })?;
        return Ok(format!("Asking the server to remove {name}…"));
    }
    cx.ledger.remove_world(&name)?;
    Ok(format!("Removed {name}; its files are in saves/trash."))
}

/// Saves every world of the game at once.
fn save_game(cx: &mut Context, args: &Args) -> Reply {
    let name = args.text("name").ok_or("Usage: /savegame <name>: saves every world of the game at once")?;
    save_game_as(cx, &name)
}

fn save_game_as(cx: &mut Context, name: &str) -> Reply {
    if cx.ledger.is_online() {
        // The server fills in where each world is.
        cx.ledger.propose_game(GameAction::Save { name: name.into(), worlds: Default::default() })?;
        return Ok("Saving the game…".into());
    }
    let saved = cx.ledger.save_game(name);
    cx.ledger.keep_saves(&cx.world.edit_list());
    Ok(format!("Saved the game as \"{name}\" ({} worlds).", saved?))
}

/// Puts every world of the game back to a game save, after saving the game as it is
/// (loading that brings it back).
fn load_game(cx: &mut Context, args: &Args) -> Reply {
    let name = args.text("name").ok_or("Usage: /loadgame <name>")?;
    let before: String = format!("before loading {name}").chars().take(40).collect();
    let _ = save_game_as(cx, &before);
    let n = cx.ledger.load_game(&name, &cx.world.edit_list())?;
    Ok(if cx.ledger.is_online() {
        format!("Asking the server to put {n} world(s) back to \"{name}\"…")
    } else {
        format!("Loaded the game save \"{name}\": {n} world(s) went back. The game as it was is saved as \"{before}\".")
    })
}

/// Makes a build (`build`, or the selected one) a portal to `to`; `off` unlinks it.
fn portal(cx: &mut Context, args: &Args) -> Reply {
    let Some(build) = args.int("build").or_else(|| cx.creations.selected().map(|b| b.build)) else {
        return Err("Select a build first (click it), then /portal <world or host:port>.".into());
    };
    let to = args.text("to").unwrap_or_default();
    if to == "off" {
        cx.ledger.record(None, None, Action::SetPortal { build, to: None });
        return Ok("It's an ordinary build again.".into());
    }
    let to = PortalTo::parse(&to).map_err(|why| format!("Usage: /portal <world or host:port> | off ({why})"))?;
    let label = to.label().to_string();
    cx.ledger.record(None, None, Action::SetPortal { build, to: Some(to) });
    Ok(format!("It's a portal to {label} now: walk up to it and press E."))
}

fn land(cx: &mut Context, args: &Args) -> Reply {
    let land = match args.text("mode").as_deref() {
        Some("open") => LandMode::Open,
        Some("parcels") => LandMode::Parcels,
        _ => return Err("Usage: /land open|parcels".into()),
    };
    let mut rules = cx.ledger.rules.clone();
    rules.land = land;
    cx.ledger.record(None, None, Action::SetRules { rules });
    Ok(format!("Land is now {}.", if land == LandMode::Open { "open to everyone" } else { "claimed by parcels" }))
}

fn rate(cx: &mut Context, args: &Args) -> Reply {
    const USAGE: &str = "Usage: /rate <changes per minute> [voxels per 10 minutes], or /rate off";
    let rates = match (args.text("per_minute").as_deref(), args.text("voxels")) {
        (Some("off"), None) => RateLimits::default(),
        (Some(n), voxels) => match (n.parse().ok(), voxels.map(|v| v.parse().ok())) {
            (Some(n), None) => RateLimits { changes_per_minute: Some(n), voxels_per_ten_minutes: None },
            (Some(n), Some(Some(v))) => RateLimits { changes_per_minute: Some(n), voxels_per_ten_minutes: Some(v) },
            _ => return Err(USAGE.into()),
        },
        (None, _) => return Err(USAGE.into()),
    };
    let mut rules = cx.ledger.rules.clone();
    rules.rates = rates;
    cx.ledger.record(None, None, Action::SetRules { rules });
    Ok(format!("Rate limits set. {}", rate_line(&rates)))
}

fn bevel(cx: &mut Context, args: &Args) -> Reply {
    const USAGE: &str = "Usage: /bevel <0-0.3>: how far voxels' edges are rounded, as a share of a voxel (/bevel off for sharp cubes)";
    let bevel = match args.text("bevel").as_deref() {
        Some("off") => 0.0,
        Some(n) => match n.parse::<f32>() {
            Ok(n) if (0.0..=Look::MAX_BEVEL).contains(&n) => n,
            _ => return Err(USAGE.into()),
        },
        None => return Err(USAGE.into()),
    };
    let mut rules = cx.ledger.rules.clone();
    rules.look.bevel = bevel;
    cx.ledger.record(None, None, Action::SetRules { rules });
    Ok(format!("{} for everyone in this world.", bevel_line(bevel)))
}

fn rate_line(rates: &RateLimits) -> String {
    match (rates.changes_per_minute, rates.voxels_per_ten_minutes) {
        (None, None) => "No rate limits.".into(),
        (n, v) => format!(
            "At most {} changes a minute and {} voxels per 10 minutes (admins exempt).",
            n.map_or("unlimited".into(), |n| n.to_string()),
            v.map_or("unlimited".into(), |v| v.to_string())
        ),
    }
}

fn bevel_line(bevel: f32) -> String {
    if bevel > 0.0 { format!("Voxel edges are rounded ({bevel})") } else { "Voxels have sharp edges".into() }
}

fn rules(cx: &mut Context, _: &Args) -> Reply {
    let ledger = &*cx.ledger;
    let r = &ledger.rules;
    let land = match r.land {
        LandMode::Open => "open: anyone changes anything outside the spawn",
        LandMode::Parcels => "parcels: land is claimed, and owners change it",
    };
    Ok(format!(
        "Land is {land}. Spawn: {:.0} m around the start. One build: up to {} voxels a side, {} voxels, {} faces. \
         One land change: areas up to {:.0} m, {} voxels. {} {}. Admins: {}.",
        r.spawn.radius_m,
        r.build.max_size,
        r.build.max_voxels,
        r.build.max_faces,
        r.terrain.max_area_m,
        r.terrain.max_changed_voxels,
        rate_line(&r.rates),
        bevel_line(r.look.bevel),
        ledger.log.admins.len()
    ))
}

/// The game's packages by name: the server's when online, this game's otherwise.
fn catalog(ledger: &Ledger) -> std::collections::BTreeMap<String, std::sync::Arc<genverse_rules::logic::Package>> {
    match &ledger.online {
        Some(online) => online.game.as_ref().map(|g| g.packages()).unwrap_or_default(),
        None => ledger::read_game(&game_name()).packages(),
    }
}

fn packages(cx: &mut Context, _: &Args) -> Reply {
    let ledger = &*cx.ledger;
    let logic = &ledger.logic;
    let off: Vec<String> = catalog(ledger).keys().filter(|n| !logic.installed.contains_key(*n)).cloned().collect();
    let off = if off.is_empty() { String::new() } else { format!("\nIn the game but off here: {} (turn them on from the Game window's Rules)", off.join(", ")) };
    if logic.installed.is_empty() {
        return Ok(format!("No game rules on in this world. Admins can ask the agent for a game.{off}"));
    }
    let lines: Vec<String> = logic
        .installed
        .values()
        .map(|i| {
            let title = logic.package(i).map_or(i.package.name.clone(), |p| p.manifest.title.clone());
            let place = match &i.scope {
                Scope::World => "the whole world".to_string(),
                Scope::Parcels { parcels } => format!("{} parcel(s)", parcels.len()),
            };
            let who = if i.by == ledger.profile.id { "you".to_string() } else { format!("{}…", &i.by[..8.min(i.by.len())]) };
            format!("{} \"{title}\" v{} for {place}, by {who}, {} piece(s)", i.package.name, i.package.version, logic.state.count(&i.package.name))
        })
        .collect();
    Ok(lines.join("\n") + &off)
}

/// Turns a package off in this world.
fn uninstall(cx: &mut Context, args: &Args) -> Reply {
    let name = args.text("name").ok_or("Usage: /uninstall <name> (see /packages)")?;
    cx.ledger.record(None, None, Action::UninstallPackage { name: name.clone() });
    Ok(format!("{name} is off in this world; its pieces stay, asleep, in case it comes back."))
}

/// Turns one of the game's packages on in this world.
fn turn_on(cx: &mut Context, args: &Args) -> Reply {
    let name = args.text("name").ok_or("Which package? /packages lists the game's.")?;
    let package = catalog(cx.ledger).remove(&name).ok_or_else(|| format!("The game has no package \"{name}\"."))?;
    let title = package.manifest.title.clone();
    match cx.ledger.install((*package).clone(), Scope::World, None, None) {
        Ok(_) => Ok(format!("{title} is on in this world.")),
        Err(why) => Err(format!("⚠ Couldn't turn {title} on: {why}.")),
    }
}

/// Takes a package out of the game: every world stops running it.
fn remove_package(cx: &mut Context, args: &Args) -> Reply {
    let name = args.text("name").ok_or("Which package? /packages lists the game's.")?;
    cx.ledger.set_game_package(GameAction::RemovePackage { name: name.clone() })?;
    Ok(format!("Took {name} out of the game; worlds stop running it."))
}

/// Whether an op makes a kind of change.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug)]
pub enum Cover {
    /// This op makes it.
    Op(&'static str),
    /// Players make it by hand today (placing a draft, the land panel); it gets an op
    /// when the agent works through ops.
    Later(&'static str),
    /// Nobody asks for it: the world records it, or only old logs have it.
    Never(&'static str),
}

/// Writes the op for each kind of change once, as a match (so a new kind doesn't
/// compile until it's here) and as a list (so a test can check the ops exist).
macro_rules! cover {
    ($covers:ident, $list:ident, $kind:ty { $($pattern:pat => $cover:expr,)* }) => {
        #[allow(dead_code)]
        pub fn $covers(change: &$kind) -> Cover {
            match change {
                $($pattern => $cover,)*
            }
        }

        #[allow(dead_code)]
        const $list: &[Cover] = &[$($cover,)*];
    };
}

cover!(action_cover, ACTION_COVER, Action {
    Action::PlaceBuild(..) => Cover::Later("placing a draft by hand"),
    Action::UpdateBuild(..) => Cover::Later("moving or revising a build by hand"),
    Action::RemoveBuild { .. } => Cover::Later("removing the selected build by hand"),
    Action::Terrain { .. } => Cover::Later("keeping a land preview"),
    Action::ClaimParcels { .. } => Cover::Later("the land panel's Claim"),
    Action::ReleaseParcels { .. } => Cover::Later("the land panel's Release"),
    Action::Undo { .. } => Cover::Op("undo"),
    Action::Revert { .. } => Cover::Never("only in old logs"),
    Action::Rollback { .. } => Cover::Never("only in old logs"),
    Action::SetRules { .. } => Cover::Op("land"),
    Action::LegacyVoxels { .. } => Cover::Never("only in old logs"),
    Action::InstallPackage { .. } => Cover::Op("turn_on"),
    Action::UninstallPackage { .. } => Cover::Op("uninstall"),
    Action::Intent { .. } => Cover::Never("playing the game: the player's own keys and clicks"),
    Action::Save { .. } => Cover::Op("save"),
    Action::Load { .. } => Cover::Op("load"),
    Action::Restore { .. } => Cover::Never("only in old logs"),
    Action::Depart { .. } => Cover::Never("recorded by the world when a player travels"),
    Action::Arrive { .. } => Cover::Never("recorded by the world when a player travels"),
    Action::SetPortal { .. } => Cover::Op("portal"),
    Action::Describe { .. } => Cover::Later("written from the agent's answer"),
    Action::Timers { .. } => Cover::Never("recorded by the world as timers go off"),
});

cover!(game_cover, GAME_COVER, GameAction {
    GameAction::CreateWorld { .. } => Cover::Op("newworld"),
    GameAction::RemoveWorld { .. } => Cover::Op("removeworld"),
    GameAction::SetAdmins { .. } => Cover::Later("set when a game is made; nothing changes it yet"),
    GameAction::Save { .. } => Cover::Op("savegame"),
    GameAction::AddPackage { .. } => Cover::Later("added when the agent installs a package"),
    GameAction::RemovePackage { .. } => Cover::Op("remove_package"),
    GameAction::LoadSave { .. } => Cover::Op("loadgame"),
});

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn names_are_unique_and_commands_are_plain_words() {
        let mut seen = std::collections::HashSet::new();
        for op in OPS {
            assert!(seen.insert(op.name), "two ops are called {}", op.name);
            assert!(!op.about.is_empty(), "{} says nothing about itself", op.name);
            assert_eq!(op.args.is_empty(), op.params.is_empty(), "{}: its usage and params disagree", op.name);
            if op.slash {
                assert!(op.name.chars().all(|c| c.is_ascii_lowercase()), "/{} can't be typed as one word", op.name);
            }
            // Only the last argument may take the rest of the line.
            let rests = op.params.iter().rev().skip(1).filter(|p| p.kind == Kind::Rest).count();
            assert_eq!(rests, 0, "{}: an argument after one that takes the rest", op.name);
        }
    }

    #[test]
    fn every_kind_of_change_names_an_op_that_exists() {
        for cover in ACTION_COVER.iter().chain(GAME_COVER) {
            match cover {
                Cover::Op(name) => assert!(OPS.iter().any(|op| op.name == *name), "no op called {name}"),
                Cover::Later(why) | Cover::Never(why) => assert!(!why.is_empty()),
            }
        }
    }

    #[test]
    fn chat_words_become_arguments() {
        let fork = command("fork").expect("fork is a command");
        let args = Args::from_words(fork.params, &["before", "copy"]);
        assert_eq!((args.text("save").as_deref(), args.text("world").as_deref()), (Some("before"), Some("copy")));

        let save = command("save").expect("save is a command");
        let args = Args::from_words(save.params, &["before", "the", "party"]);
        assert_eq!(args.text("name").as_deref(), Some("before the party"));
        assert_eq!(Args::from_words(save.params, &[]).text("name"), None);

        // The agent's and the window's arguments are JSON; numbers read either way.
        let args = Args::new(json!({"id": 42, "n": "7", "ground": "natural"}));
        assert_eq!((args.int("id"), args.int("n"), args.flat()), (Some(42), Some(7), false));
        assert_eq!(args.text("id").as_deref(), Some("42"));
        assert!(Args::new(json!({})).flat());
    }

    #[test]
    fn revert_is_still_undo() {
        assert_eq!(command("revert").map(|op| op.name), Some("undo"));
        assert!(command("turn_on").is_none(), "the window's ops aren't chat commands");
    }
}
