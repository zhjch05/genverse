//! Chat commands for looking after the world, until the history panel exists:
//!
//! - `/history [n]`: the latest changes, with ids, who made them, and how.
//! - `/revert <id>`: undo one of your own changes (anyone's, for admins).
//! - `/rules`: the world's rules.
//! - `/verify`: checks every change is signed by the player it names.
//! - `/host`, `/join <host[:port]>`, `/leave`: play together.
//! - Admins: `/rollback <player>` undoes everything a player did; `/restore <id>` puts
//!   the world back how it was right after change `id`; `/land open|parcels`;
//!   `/rate <changes per minute> [voxels per 10 minutes]` or `/rate off`; `/bevel <0-0.3>`
//!   or `/bevel off` rounds voxels' edges for everyone.
//!
//! - `/packages` lists the world's game rules; admins `/uninstall <name>` one.
//! - `/save <name>` bookmarks the world as it is (anyone); `/saves` lists them; admins
//!   `/load <name>` puts the world back exactly as it was then, for everyone (undoable);
//!   `/fork <save> <world>` starts a new local world from a save, leaving this one be.
//!
//! Anything that undoes changes other than your latest rebuilds the world from the log.

use bevy::prelude::*;
use cubewish_rules::{
    log::{Action, replay},
    logic::Scope,
    settings::{LandMode, Look, RateLimits},
};

use cubewish_rules::game::GameAction;

use super::{
    creations::Creations,
    landscape::Landscape,
    worlds::{Travel, Trip},
};
use crate::world::{VoxelWorld, ledger::Ledger};

pub struct Context<'a, 'w, 's> {
    pub commands: &'a mut Commands<'w, 's>,
    pub ledger: &'a mut Ledger,
    pub world: &'a mut VoxelWorld,
    pub creations: &'a mut Creations,
    pub landscape: &'a mut Landscape,
    /// Where to go next, for commands that change worlds.
    pub travel: &'a mut Travel,
}

/// A command, as the chat suggests it while you type.
pub struct Command {
    pub name: &'static str,
    /// What follows the name, e.g. `<id>`; empty for none.
    pub args: &'static str,
    pub about: &'static str,
    pub admin: bool,
}

const fn command(name: &'static str, args: &'static str, about: &'static str, admin: bool) -> Command {
    Command { name, args, about, admin }
}

/// Every command, in the order they're suggested. `/play` is the panel's own.
pub const COMMANDS: [Command; 30] = [
    command("history", "[n]", "The latest changes, with ids", false),
    command("revert", "<id>", "Undo one of your changes", false),
    command("rules", "", "This world's rules", false),
    command("verify", "", "Check every change is signed by its player", false),
    command("play", "", "Join a world on your network, or open this one", false),
    command("host", "", "Open this game, every world in it, to others", false),
    command("join", "<host[:port]>", "Play in a hosted game", false),
    command("leave", "", "Back to your own game", false),
    command("packages", "", "The game rules in this world", false),
    command("save", "<name>", "Bookmark the world as it is now", false),
    command("saves", "", "The saves you can load or fork", false),
    command("fork", "<save> <world>", "Start a new world of your own from a save", false),
    command("worlds", "", "This game's worlds (O opens the Game window)", false),
    command("world", "<name>", "Go to another world of this game", false),
    command("games", "", "Your games on this computer", false),
    command("game", "<name>", "Go to another of your games", false),
    command("newgame", "<name> [flat|natural]", "Make a new game and go there", false),
    command("savegame", "<name>", "Save every world of the game at once", false),
    command("portal", "<world|game/world|host:port> | off", "Make the selected build a portal", false),
    command("newworld", "<name> [flat|natural]", "Make a new world in this game", true),
    command("removeworld", "<name>", "Take a world out of this game (to the trash)", true),
    command("loadgame", "<name>", "Put every world back to a game save", true),
    command("load", "<name>", "Put the world back to a save, for everyone", true),
    command("rollback", "<player>", "Undo everything a player did", true),
    command("restore", "<id>", "Put the world back to right after a change", true),
    command("land", "open|parcels", "Whether land is open or claimed by parcel", true),
    command("rate", "<per min> [voxels/10 min] | off", "Limit how fast players change things", true),
    command("bevel", "<0-0.3> | off", "Round voxels' edges for everyone", true),
    command("uninstall", "<name>", "Remove game rules from this world", true),
    command("help", "", "List the commands", false),
];

fn help() -> String {
    let line = |admin: bool| {
        COMMANDS
            .iter()
            .filter(|c| c.admin == admin)
            .map(|c| if c.args.is_empty() { format!("/{}", c.name) } else { format!("/{} {}", c.name, c.args) })
            .collect::<Vec<_>>()
            .join(" · ")
    };
    format!("Commands: {} · admins: {}", line(false), line(true))
}

/// Runs `/command args` and returns the reply for the chat.
pub fn run(text: &str, cx: &mut Context) -> String {
    let mut words = text.trim_start_matches('/').split_whitespace();
    let name = words.next().unwrap_or_default().to_lowercase();
    let args: Vec<&str> = words.collect();
    let admin = cx.ledger.land.is_admin(&cx.ledger.profile.id);
    let busy = cx.creations.draft.is_some() || cx.landscape.draft.is_some();
    match name.as_str() {
        "history" => history(cx.ledger, args.first().and_then(|n| n.parse().ok()).unwrap_or(12)),
        "rules" => rules(cx.ledger),
        "verify" => {
            let changes = &cx.ledger.log.changes;
            let bad: Vec<String> = changes.iter().filter(|c| !c.verified()).map(|c| c.id.to_string()).collect();
            if bad.is_empty() {
                format!("All {} changes are signed by the players they name.", changes.len())
            } else {
                format!("{} of {} changes aren't signed by their authors: {}", bad.len(), changes.len(), bad.join(", "))
            }
        }
        "revert" | "rollback" | "restore" | "load" | "host" | "join" | "leave" | "world" | "game" | "newgame" | "loadgame" if busy => {
            "Keep or discard your draft first.".into()
        }
        "save" => save(cx.ledger, &args.join(" ")),
        "saves" => saves(cx.ledger),
        "fork" => match args.as_slice() {
            [save, world] => fork(cx.ledger, save, world),
            _ => "Usage: /fork <save> <new world name>".into(),
        },
        "host" => super::online::host(cx.ledger),
        "join" => match args.first() {
            Some(addr) => super::online::join(cx.ledger, addr),
            None => "Usage: /join <host[:port]>".into(),
        },
        "leave" => super::online::leave(cx.ledger, cx.travel),
        "worlds" => worlds(cx.ledger),
        "world" => match args.first().and_then(|n| crate::world::ledger::clean_name(n)) {
            Some(name) if cx.ledger.is_online() => {
                cx.travel.next = Some(Trip::Remote { world: name });
                String::new()
            }
            Some(name) => {
                cx.travel.next = Some(Trip::Local { game: crate::world::ledger::game_name(), world: name });
                String::new()
            }
            None => worlds(cx.ledger),
        },
        "games" => games(),
        "game" => match args.first().and_then(|n| crate::world::ledger::clean_name(n)) {
            Some(game) => match crate::world::ledger::read_game(&game).start() {
                Some(world) => {
                    cx.travel.next = Some(Trip::Local { game, world });
                    String::new()
                }
                None => format!("There's no game \"{game}\". {}", games()),
            },
            None => games(),
        },
        "newgame" => match args.first() {
            Some(name) => new_game(cx.ledger, cx.travel, name, !matches!(args.get(1).copied(), Some("natural"))),
            None => "Usage: /newgame <name> [flat|natural]".into(),
        },
        "savegame" if !args.is_empty() => save_game(cx.ledger, &args.join(" ")),
        "savegame" => "Usage: /savegame <name>: saves every world of the game at once".into(),
        "portal" => portal(cx, &args.join(" ")),
        "revert" => match args.first().and_then(|n| n.parse::<u64>().ok()) {
            Some(id) => revert(cx, id),
            None => "Usage: /revert <change id> (see /history)".into(),
        },
        "rollback" | "restore" | "load" | "land" | "rate" | "bevel" | "uninstall" | "newworld" | "removeworld" | "loadgame" if !admin => {
            "Only admins can do that.".into()
        }
        "newworld" => match args.first() {
            Some(name) => new_world(cx.ledger, cx.travel, name, !matches!(args.get(1).copied(), Some("natural"))),
            None => "Usage: /newworld <name> [flat|natural]".into(),
        },
        "removeworld" => match args.first() {
            Some(name) => remove_world(cx.ledger, name),
            None => "Usage: /removeworld <name>".into(),
        },
        "loadgame" if !args.is_empty() => load_game(cx.ledger, &args.join(" ")),
        "loadgame" => "Usage: /loadgame <name>".into(),
        "load" => match cx.ledger.log.save_named(&args.join(" ")).map(|c| c.id) {
            Some(id) if !args.is_empty() => restore(cx, id),
            _ => format!("Usage: /load <name>. {}", saves(cx.ledger)),
        },
        "packages" => packages(cx.ledger),
        "uninstall" => match args.first() {
            Some(name) => {
                cx.ledger.record(None, None, Action::UninstallPackage { name: name.to_string() });
                format!("Uninstalling {name}; its pieces stay, asleep, in case it comes back.")
            }
            None => "Usage: /uninstall <name> (see /packages)".into(),
        },
        "rollback" => match args.first() {
            Some(who) => rollback(cx, who),
            None => "Usage: /rollback <player name>".into(),
        },
        "restore" => match args.first().and_then(|n| n.parse::<u64>().ok()) {
            Some(id) => restore(cx, id),
            None => "Usage: /restore <change id>: the world goes back to how it was right after it".into(),
        },
        "land" => {
            let land = match args.first().copied() {
                Some("open") => LandMode::Open,
                Some("parcels") => LandMode::Parcels,
                _ => return "Usage: /land open|parcels".into(),
            };
            let mut rules = cx.ledger.rules.clone();
            rules.land = land;
            cx.ledger.record(None, None, Action::SetRules { rules });
            format!("Land is now {}.", if land == LandMode::Open { "open to everyone" } else { "claimed by parcels" })
        }
        "rate" => {
            let rates = match args.as_slice() {
                ["off"] => RateLimits::default(),
                [n, rest @ ..] => match (n.parse().ok(), rest.first().map(|v| v.parse().ok())) {
                    (Some(n), None) => RateLimits { changes_per_minute: Some(n), voxels_per_ten_minutes: None },
                    (Some(n), Some(Some(v))) => RateLimits { changes_per_minute: Some(n), voxels_per_ten_minutes: Some(v) },
                    _ => return "Usage: /rate <changes per minute> [voxels per 10 minutes], or /rate off".into(),
                },
                [] => return "Usage: /rate <changes per minute> [voxels per 10 minutes], or /rate off".into(),
            };
            let mut rules = cx.ledger.rules.clone();
            rules.rates = rates;
            cx.ledger.record(None, None, Action::SetRules { rules });
            format!("Rate limits set. {}", rate_line(&rates))
        }
        "bevel" => {
            const USAGE: &str = "Usage: /bevel <0-0.3>: how far voxels' edges are rounded, as a share of a voxel (/bevel off for sharp cubes)";
            let bevel = match args.first().copied() {
                Some("off") => 0.0,
                Some(n) => match n.parse::<f32>() {
                    Ok(n) if (0.0..=Look::MAX_BEVEL).contains(&n) => n,
                    _ => return USAGE.into(),
                },
                None => return USAGE.into(),
            };
            let mut rules = cx.ledger.rules.clone();
            rules.look.bevel = bevel;
            cx.ledger.record(None, None, Action::SetRules { rules });
            format!("{} for everyone in this world.", bevel_line(bevel))
        }
        _ => help(),
    }
}

/// Undoes change `id`: your own, or anyone's for admins.
pub fn revert(cx: &mut Context, id: u64) -> String {
    if !cx.ledger.log.changes.iter().any(|c| c.id == id) {
        return format!("There's no change {id}.");
    }
    if !cx.ledger.log.may_revert(&cx.ledger.profile.id, id) {
        return "You can only undo your own changes.".into();
    }
    if cx.ledger.log.undone().contains(&id) {
        return format!("Change {id} is already undone.");
    }
    cx.ledger.revert(id);
    rebuild(cx);
    format!("Undid change {id}.")
}

/// Admins: undoes every change in effect by the player `who` (a name, or an id or its
/// start).
pub fn rollback(cx: &mut Context, who: &str) -> String {
    if !cx.ledger.land.is_admin(&cx.ledger.profile.id) {
        return "Only admins can do that.".into();
    }
    let who = who.to_lowercase();
    let changes: Vec<u64> = cx
        .ledger
        .log
        .effective()
        .filter(|c| c.by.player_name.to_lowercase() == who || c.by.player.starts_with(&who))
        .map(|c| c.id)
        .collect();
    if changes.is_empty() {
        return format!("{who} has no changes in effect.");
    }
    let n = changes.len();
    cx.ledger.record(None, None, Action::Rollback { changes, note: format!("rollback of {who}") });
    rebuild(cx);
    format!("Rolled back {n} change(s) by {who}.")
}

/// Admins: puts the world back exactly as it was right after change `id` (a save, or
/// any change): everything after it stops counting, undos included.
pub fn restore(cx: &mut Context, id: u64) -> String {
    if !cx.ledger.land.is_admin(&cx.ledger.profile.id) {
        return "Only admins can do that.".into();
    }
    if !cx.ledger.log.changes.iter().any(|c| c.id == id) {
        return format!("There's no change {id}.");
    }
    let n = cx.ledger.log.changes.iter().filter(|c| c.id > id).count();
    if n == 0 {
        return format!("Nothing has changed since change {id}.");
    }
    let reference = cx.ledger.record(None, None, Action::Restore { to: id });
    rebuild(cx);
    let name = cx.ledger.log.changes.iter().find(|c| c.id == id).and_then(|c| match &c.action {
        Action::Save { name } => Some(format!("\"{name}\"")),
        _ => None,
    });
    let what = name.unwrap_or(format!("change {id}"));
    let undo = match cx.ledger.resolve(reference) {
        Some(change) => format!("/revert {change} brings them back"),
        None => "reverting the load (/history) brings them back".into(),
    };
    format!("Loaded {what}: {n} later change(s) no longer count; {undo}.")
}

/// This game's worlds, for the chat.
fn worlds(ledger: &Ledger) -> String {
    let here = super::worlds::current_title(ledger);
    let list: Vec<String> = match &ledger.online {
        Some(online) => online.game.as_ref().map(|g| g.worlds().into_iter().map(|(n, _)| n).collect()).unwrap_or_default(),
        None => crate::world::ledger::list_worlds()
            .into_iter()
            .map(|w| format!("{} ({}, {} changes)", w.name, if w.flat { "flat" } else { "natural" }, w.changes))
            .collect(),
    };
    format!("You're in {here}. Worlds: {}. /world <name> goes; O opens the Game window.", list.join(" · "))
}

/// Makes the selected build a portal to `to` (`off` unlinks it).
fn portal(cx: &mut Context, to: &str) -> String {
    let Some(build) = cx.creations.selected().map(|b| b.build) else {
        return "Select a build first (click it), then /portal <world or host:port>.".into();
    };
    if to.trim() == "off" {
        cx.ledger.record(None, None, Action::SetPortal { build, to: None });
        return "It's an ordinary build again.".into();
    }
    match cubewish_rules::log::PortalTo::parse(to) {
        Ok(to) => {
            let label = to.label().to_string();
            cx.ledger.record(None, None, Action::SetPortal { build, to: Some(to) });
            format!("It's a portal to {label} now: walk up to it and press E.")
        }
        Err(why) => format!("Usage: /portal <world or host:port> | off ({why})"),
    }
}

/// Bookmarks the world as it is now.
pub fn save(ledger: &mut Ledger, name: &str) -> String {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 40 {
        return "Usage: /save <name> (up to 40 characters)".into();
    }
    if let Some(old) = ledger.log.save_named(name) {
        let old = old.id;
        ledger.record(None, None, Action::Save { name: name.into() });
        return format!("Saved \"{name}\" (it replaces the save of that name from change {old}).");
    }
    ledger.record(None, None, Action::Save { name: name.into() });
    format!("Saved \"{name}\". An admin can /load it; anyone can /fork it into a world of their own.")
}

fn saves(ledger: &Ledger) -> String {
    let undone = ledger.log.undone();
    let mut seen = std::collections::HashSet::new();
    let lines: Vec<String> = ledger
        .log
        .saves()
        .rev()
        .filter(|(_, name)| seen.insert(name.to_lowercase()))
        .take(20)
        .map(|(c, name)| {
            let later = if undone.contains(&c.id) { " (after the current state)" } else { "" };
            format!("\"{name}\": change {}, {}{later}", c.id, ledger.describe(&c.by))
        })
        .collect();
    if lines.is_empty() { "No saves yet: /save <name>".into() } else { format!("Saves, newest first: {}", lines.join(" · ")) }
}

/// Starts a new local world from a save: the log up to it, with the local player made an
/// admin there too.
pub fn fork(ledger: &mut Ledger, save: &str, world: &str) -> String {
    let Some(at) = ledger.log.save_named(save).map(|c| c.id) else {
        return format!("There's no save \"{save}\". {}", saves(ledger));
    };
    if ledger.is_online() {
        return "Forking makes a world on this computer; do it in your own game.".into();
    }
    let log = ledger.log.fork(at);
    let n = log.changes.len();
    match ledger.create_world(world, (log.seed, log.flat), Some(log)) {
        Ok(name) => format!("Started the world {name} from \"{save}\" ({n} changes); this one carries on as it is. /world {name} goes there."),
        Err(why) => why,
    }
}

/// Makes a world in this game (on a server, asks it to) and, locally, goes there.
pub fn new_world(ledger: &mut Ledger, travel: &mut Travel, name: &str, flat: bool) -> String {
    let seed = super::worlds::fresh_seed(flat);
    if ledger.is_online() {
        let Some(name) = crate::world::ledger::clean_name(name) else { return "A world's name is letters, digits, - and _.".into() };
        return match ledger.propose_game(GameAction::CreateWorld { name: name.clone(), seed, flat }) {
            Ok(_) => format!("Asking the server to make {name}…"),
            Err(why) => why,
        };
    }
    match ledger.create_world(name, (seed, flat), None) {
        Ok(name) => {
            travel.next = Some(Trip::Local { game: crate::world::ledger::game_name(), world: name });
            String::new()
        }
        Err(why) => why,
    }
}

/// Takes a world out of this game; its files go to the trash.
pub fn remove_world(ledger: &mut Ledger, name: &str) -> String {
    if ledger.is_online() {
        return match ledger.propose_game(GameAction::RemoveWorld { name: name.into() }) {
            Ok(_) => format!("Asking the server to remove {name}…"),
            Err(why) => why,
        };
    }
    match ledger.remove_world(name) {
        Ok(()) => format!("Removed {name}; its files are in saves/trash."),
        Err(why) => why,
    }
}

/// Saves every world of the game at once.
pub fn save_game(ledger: &mut Ledger, name: &str) -> String {
    if ledger.is_online() {
        // The server fills in where each world is.
        let action = GameAction::Save { name: name.trim().into(), worlds: Default::default() };
        return match ledger.propose_game(action) {
            Ok(_) => "Saving the game…".into(),
            Err(why) => why,
        };
    }
    match ledger.save_game(name) {
        Ok(n) => format!("Saved the game as \"{}\" ({n} worlds).", name.trim()),
        Err(why) => why,
    }
}

/// Puts every world of the game back to a game save.
pub fn load_game(ledger: &mut Ledger, name: &str) -> String {
    if ledger.is_online() {
        return match ledger.load_game(name) {
            Ok(n) => format!("Asking the server to put {n} world(s) back to \"{}\"…", name.trim()),
            Err(why) => why,
        };
    }
    match ledger.load_game(name) {
        Ok(n) => format!("Loaded the game save \"{}\": {n} world(s) went back. Each can be undone in its history.", name.trim()),
        Err(why) => why,
    }
}

/// Makes a new local game and goes there.
pub fn new_game(ledger: &Ledger, travel: &mut Travel, name: &str, flat: bool) -> String {
    let by = cubewish_rules::log::Provenance::new(&ledger.profile.id, &ledger.profile.name, None, None, 0);
    match crate::world::ledger::create_game(name, flat, super::worlds::fresh_seed(flat), &by) {
        Ok(game) => {
            travel.next = Some(Trip::Local { game, world: "main".into() });
            String::new()
        }
        Err(why) => why,
    }
}

/// The local games, for the chat.
fn games() -> String {
    let here = crate::world::ledger::game_name();
    let list: Vec<String> = crate::world::ledger::list_games()
        .iter()
        .map(|g| format!("{}{} ({} worlds)", g.name, if g.name == here { " ← here" } else { "" }, g.worlds))
        .collect();
    format!("Games: {}. /game <name> goes; /newgame <name> makes one.", list.join(" · "))
}

/// Makes the world (voxels and builds) match the change log again.
pub fn rebuild(cx: &mut Context) {
    let full = replay(&cx.ledger.log);
    for problem in &full.problems {
        warn!("change no longer applies: {problem}");
    }
    cx.world.replace_edits(full.edits);
    cx.creations.resync(cx.commands, &full.builds);
    cx.landscape.forget_history();
    cx.ledger.builds = full.builds;
}

fn history(ledger: &Ledger, n: usize) -> String {
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
    if lines.is_empty() { "No changes yet.".into() } else { format!("Latest changes:\n{}", lines.join("\n")) }
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

fn rules(ledger: &Ledger) -> String {
    let r = &ledger.rules;
    let land = match r.land {
        LandMode::Open => "open: anyone changes anything outside the spawn",
        LandMode::Parcels => "parcels: land is claimed, and owners change it",
    };
    format!(
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
    )
}

fn packages(ledger: &crate::world::ledger::Ledger) -> String {
    let logic = &ledger.logic;
    let catalog = match &ledger.online {
        Some(online) => online.game.as_ref().map(|g| g.packages()).unwrap_or_default(),
        None => crate::world::ledger::read_game(&crate::world::ledger::game_name()).packages(),
    };
    let off: Vec<String> = catalog.keys().filter(|n| !logic.installed.contains_key(*n)).cloned().collect();
    let off = if off.is_empty() { String::new() } else { format!("\nIn the game but off here: {} (turn them on from the Game window's Rules)", off.join(", ")) };
    if logic.installed.is_empty() {
        return format!("No game rules on in this world. Admins can ask the agent for a game.{off}");
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
            let who = if i.by == ledger.profile.id { "you".to_string() } else { short_id(&i.by) };
            format!("{} \"{title}\" v{} for {place}, by {who}, {} piece(s)", i.package.name, i.package.version, logic.state.count(&i.package.name))
        })
        .collect();
    lines.join("\n") + &off
}

fn short_id(id: &str) -> String {
    format!("{}…", &id[..8.min(id.len())])
}
