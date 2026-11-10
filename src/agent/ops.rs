//! Everything a player can do to the game and its worlds, in one place. Each is an
//! [`Op`]: a name, its arguments, who may do it, and the code that does it. The chat's
//! `/commands` ([`super::commands`]), the Game window ([`super::worlds`]) and the history
//! panel all go through [`run`], so a thing that can be done one way can be done every
//! way, under the same checks. The agent gets them as tools ([`tools`]; served by
//! [`super::tools`]), so what a player can do, the agent can do for them.
//!
//! An op changes the world only through the [`Ledger`], as the player: the world's rules
//! (land, admins, rates) hold whoever asked. For the agent, two more things hold:
//! - [`Op::ask`]: the player is asked first (what can't be undone, or takes them online).
//! - [`Op::last`]: it happens when the agent's turn ends, since it swaps the world the
//!   agent is working in. Trips to another world wait for the end of the turn too.
//!
//! Some ops are the agent's alone (`place_build`, `change_build`, `remove_build`,
//! `change_land`): what a player does by hand with a draft or a selection, done by saying
//! where.
//!
//! Adding something players can do: write its function here and add it to [`OPS`]. A new
//! kind of change in the log ([`Action`], [`GameAction`]) doesn't compile until
//! [`action_cover`] or [`game_cover`] says which op makes it, or why none does.

use bevy::prelude::*;
use genverse_rules::{
    game::GameAction,
    log::{Action, PortalTo},
    logic::Scope,
    sculpt::{Area, TerrainSpec},
    settings::{LandMode, Look, RateLimits},
};
use serde_json::Value;

use super::{
    creations::{Creations, SavedCreation},
    landscape::{Landscape, PreviewError, describe_stats},
    online,
    spec::{BuildSpec, VOXEL_SIZE as BUILD_VOXEL, compile},
    worlds::{Travel, Trip, fresh_seed},
};
use crate::bindings::Bindings;
use crate::world::{
    VoxelWorld,
    ledger::{self, Ledger, clean_name, game_name},
    terrain::VOXEL_SIZE as LAND_VOXEL,
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
    /// The agent doing this for the player and the request behind it, kept with the
    /// changes it makes; `None` when the player does it themself.
    pub agent: Option<(&'a str, &'a str)>,
    /// Which keys do what, for the ops that change them.
    pub binds: &'a mut Bindings,
}

impl Context<'_> {
    /// Records a change as the player (through the agent, if it's the agent's doing).
    fn record(&mut self, action: Action) {
        self.ledger.record(self.agent.map(|a| a.0), self.agent.map(|a| a.1), action);
    }
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
    /// Never typed in the chat: only given by name (the agent, the Game window).
    Named,
}

pub struct Param {
    pub name: &'static str,
    pub kind: Kind,
    /// What to give, for the agent.
    pub about: &'static str,
    pub optional: bool,
    /// Its JSON type, for the agent: "string", "number" or "object".
    pub json: &'static str,
}

const fn word(name: &'static str, about: &'static str) -> Param {
    Param { name, kind: Kind::Word, about, optional: false, json: "string" }
}

const fn rest(name: &'static str, about: &'static str) -> Param {
    Param { name, kind: Kind::Rest, about, optional: false, json: "string" }
}

const fn named(name: &'static str, about: &'static str) -> Param {
    Param { name, kind: Kind::Named, about, optional: true, json: "string" }
}

impl Param {
    const fn opt(mut self) -> Self {
        self.optional = true;
        self
    }

    const fn required(mut self) -> Self {
        self.optional = false;
        self
    }

    const fn number(mut self) -> Self {
        self.json = "number";
        self
    }

    const fn object(mut self) -> Self {
        self.json = "object";
        self
    }
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
                Kind::Named => None,
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

    /// An argument as a number (meters, turns).
    fn num(&self, name: &str) -> Option<f32> {
        let n = match self.0.get(name)? {
            Value::Number(n) => n.as_f64()? as f32,
            Value::String(s) => s.trim().parse().ok()?,
            _ => return None,
        };
        n.is_finite().then_some(n)
    }

    /// An argument that's a whole object (a build, land changes), also when it came as
    /// JSON in a string. `Ok(None)` if it's missing; `Err` says how it's malformed.
    fn object<T: serde::de::DeserializeOwned>(&self, name: &str) -> Result<Option<T>, String> {
        let value = match self.0.get(name) {
            None | Some(Value::Null) => return Ok(None),
            Some(Value::String(text)) => serde_json::from_str(text).map_err(|e| format!("\"{name}\" isn't JSON: {e}"))?,
            Some(value) => value.clone(),
        };
        serde_json::from_value(value).map(Some).map_err(|e| format!("\"{name}\" doesn't match its format: {e}"))
    }

    /// The arguments on one line, for asking the player about them.
    pub fn line(&self) -> String {
        self.0.keys().filter_map(|name| self.text(name)).collect::<Vec<_>>().join(" ")
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
    /// One of the agent's tools.
    pub agent: bool,
    /// When the agent does it, the player is asked first.
    pub ask: bool,
    /// When the agent does it, it happens as the agent's turn ends.
    pub last: bool,
    /// More about it, for the agent.
    pub note: &'static str,
    pub params: &'static [Param],
    run: fn(&mut Context, &Args) -> Reply,
}

const fn op(name: &'static str, args: &'static str, about: &'static str, params: &'static [Param], run: fn(&mut Context, &Args) -> Reply) -> Op {
    Op { name, args, about, admin: false, slash: true, settled: false, agent: true, ask: false, last: false, note: "", params, run }
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

    /// Not a chat command: the Game window's and the agent's.
    const fn unlisted(mut self) -> Self {
        self.slash = false;
        self
    }

    /// The chat's own: no use to the agent.
    const fn chat_only(mut self) -> Self {
        self.agent = false;
        self
    }

    const fn ask(mut self) -> Self {
        self.ask = true;
        self
    }

    const fn last(mut self) -> Self {
        self.last = true;
        self
    }

    const fn note(mut self, note: &'static str) -> Self {
        self.note = note;
        self
    }
}

/// What a trip's `then` is, for the agent.
const THEN: &str = "what's left of the request to do in that world, as a note to yourself: once the player is there you're started again, \
    in that world's folder, with this note and the conversation so far; leave it out if nothing is left";

/// How a key is named, for the agent.
const KEY: &str = "a key or mouse button by name: a letter or digit (j, 7), space, shift, ctrl, alt, tab, up, down, left, right, \
    delete, f1-f12, mouse_left, mouse_right, mouse_middle, mouse_back, mouse_forward (Enter and Esc are fixed)";

const KEYS_ARE_THE_PLAYERS: &str = "The player's own keys, kept on their machine: no other player's change, and not part of the world \
    (undo and saves don't touch them). The key hints and the Settings window's Controls follow.";

/// Every op. The chat suggests its commands in this order.
pub static OPS: &[Op] = &[
    op("history", "[n]", "The latest changes, with ids", &[word("n", "how many, up to 50 (default 12)").opt()], history),
    op("undo", "<id>", "Undo one of your changes (or an undo, to redo)", &[word("id", "the change's id (history lists them)")], undo)
        .settled()
        .note("Undoes one of the player's changes (anyone's, if they're an admin), if nothing has changed there since."),
    op("rules", "", "This world's rules", &[], rules),
    op("verify", "", "Check every change is signed by its player", &[], verify),
    op("play", "", "Join a world on your network, or open this one", &[], play).chat_only(),
    op("host", "", "Open this game, every world in it, to others", &[], host).settled().ask().last(),
    op("join", "<host[:port]>", "Play in a hosted game", &[word("addr", "the server: host or host:port")], join).settled().ask().last(),
    op("leave", "", "Back to your own game", &[], leave).settled().ask(),
    op("packages", "", "The game rules in this world", &[], packages),
    op("save", "<name>", "Bookmark the world as it is now", &[rest("name", "the save's name, up to 40 characters")], save)
        .note("Cheap: save before a change the player may want to take back."),
    op("saves", "", "The saves you can load or fork", &[], |cx, _| Ok(saves(cx.ledger))),
    op(
        "fork",
        "<save> <world>",
        "Start a new world of your own from a save",
        &[word("save", "the save's name"), word("world", "the new world's name: letters, digits, - and _")],
        fork,
    ),
    op("worlds", "", "This game's worlds (O opens the Game window)", &[], |cx, _| Ok(worlds(cx.ledger))),
    op("world", "<name>", "Go to another world of this game", &[word("name", "the world to go to"), named("then", THEN)], world)
        .settled()
        .note("Takes the player there. It must exist: newworld makes one."),
    op("games", "", "Your games on this computer", &[], |_, _| Ok(games())),
    op("game", "<name>", "Go to another of your games", &[word("name", "the game to go to")], game).settled(),
    op(
        "newgame",
        "<name> [flat|natural]",
        "Make a new game and go there",
        &[word("name", "letters, digits, - and _"), word("ground", "flat (the default: empty, for games and building) or natural (a landscape)").opt()],
        new_game,
    )
    .settled(),
    op("savegame", "<name>", "Save every world of the game at once", &[rest("name", "the save's name")], save_game),
    op(
        "portal",
        "<world|game/world|host:port> | off",
        "Make the selected build a portal",
        &[
            rest("to", "where it leads: a world of this game by name, another local game as game/world, a server as host:port; or off to unlink it"),
            named("build", "the build's id (builds/ in your folder); left out, the build the player selected"),
        ],
        portal,
    )
    .note("For a build that's already placed. Players walk up to it and press E."),
    op(
        "newworld",
        "<name> [flat|natural]",
        "Make a new world in this game",
        &[
            word("name", "letters, digits, - and _"),
            word("ground", "flat (the default: empty, for games and building) or natural (a landscape of its own)").opt(),
            named("go", "\"no\" makes it without taking the player there (to link a portal to it); left out, they go"),
            named("then", THEN),
        ],
        new_world,
    )
    .admin(),
    op("removeworld", "<name>", "Take a world out of this game (to the trash)", &[word("name", "the world to remove")], remove_world).admin().ask(),
    op("loadgame", "<name>", "Put every world back to a game save", &[rest("name", "the game save's name")], load_game).admin().settled().ask(),
    op(
        "load",
        "<name>",
        "Put the world back to a save, for everyone",
        &[rest("name", "the save's name (saves lists them)"), named("id", "the save's change id, instead of its name")],
        load,
    )
    .admin()
    .settled()
    .ask()
    .note("The world is saved first, so loading that save brings it back."),
    op("land", "open|parcels", "Whether land is open or claimed by parcel", &[word("mode", "open or parcels")], land).admin(),
    op(
        "rate",
        "<per min> [voxels/10 min] | off",
        "Limit how fast players change things",
        &[word("per_minute", "changes a minute each player may make, or off"), word("voxels", "voxels per 10 minutes").opt()],
        rate,
    )
    .admin(),
    op("bevel", "<0-0.3> | off", "Round voxels' edges for everyone", &[word("bevel", "0 to 0.3 of a voxel, or off for sharp cubes")], bevel).admin(),
    op("uninstall", "<name>", "Remove game rules from this world", &[word("name", "the package (packages lists them)")], uninstall)
        .admin()
        .note("Turns a whole package off here; its pieces stay, asleep. To take one feature out, delete its file instead."),
    op("help", "", "List the commands", &[], |_, _| Ok(help())).chat_only(),
    op("turn_on", "<name>", "Turn one of the game's packages on in this world", &[word("name", "the package (packages lists the game's)")], turn_on)
        .admin()
        .unlisted(),
    op(
        "remove_package",
        "<name>",
        "Take a package out of the game: every world stops running it",
        &[word("name", "the package")],
        remove_package,
    )
    .admin()
    .unlisted()
    .ask(),
    op("bindings", "", "Which keys do what", &[], |cx, _| Ok(cx.binds.list())).note("Every action and the keys and mouse buttons on it."),
    op(
        "bind",
        "<action> <key>",
        "Make a key do something, as well as the keys that already do",
        &[word("action", "what it should do: an action's name (bindings lists them: jump, run, use, chat, ...)"), word("key", KEY)],
        bind,
    )
    .note(KEYS_ARE_THE_PLAYERS),
    op(
        "unbind",
        "<action> [key]",
        "Take a key off an action",
        &[word("action", "the action's name"), word("key", "the key to take off it; left out, every key").opt()],
        unbind,
    )
    .note(KEYS_ARE_THE_PLAYERS),
    op("resetkeys", "[action]", "Put keys back to their defaults", &[word("action", "one action; left out, all of them").opt()], reset_keys)
        .note(KEYS_ARE_THE_PLAYERS),
    op(
        "place_build",
        "<build> <x> <z>",
        "Place an object in the world, where you say",
        &[
            named("build", "the object, in the \"build\" format of your answer").object().required(),
            named("x", "where its center goes: world meters, east").number().required(),
            named("z", "world meters, south").number().required(),
            named("y", "meters, its base; left out, the ground there (give it to stand on a build, or over water)").number(),
            named("turn", "quarter turns: 0 its front faces north (-z), 1 west (-x), 2 south (+z), 3 east (+x); left out, it faces the player").number(),
            named("portal", "makes it a portal there: a world's name, game/world or host:port"),
        ],
        place_build,
    )
    .unlisted()
    .note("It's placed at once, for everyone (undo takes it back), and gets an id. Use it for several objects, or one whose place the request names; for one new object with no place named, answer with \"build\" instead, so the player places it."),
    op(
        "change_build",
        "<id> [build] [x z]",
        "Move, turn or redesign a placed build",
        &[
            named("id", "the build's id (builds/ in your folder)").number().required(),
            named("build", "its new design, in the \"build\" format; left out, it keeps its look").object(),
            named("x", "world meters, east; left out, it stays").number(),
            named("z", "world meters, south").number(),
            named("y", "meters, its base; left out with a new x or z, the ground there").number(),
            named("turn", "quarter turns: 0 its front faces north (-z), 1 west, 2 south, 3 east").number(),
        ],
        change_build,
    )
    .unlisted()
    .note("It keeps its id, and its portal link if it has one."),
    op("remove_build", "<id>", "Remove a placed build", &[named("id", "the build's id (builds/ in your folder)").number().required()], remove_build)
        .unlisted()
        .note("Undo brings it back."),
    op(
        "change_land",
        "<x> <z> <w> <d> <terrain>",
        "Reshape an area of land you choose",
        &[
            named("x", "the area's west edge, world meters").number().required(),
            named("z", "its north edge, world meters").number().required(),
            named("w", "its width, east, in meters").number().required(),
            named("d", "its depth, south, in meters").number().required(),
            named("terrain", "the changes, in the \"terrain\" format of your answer: its x and z run from the area's corner").object().required(),
        ],
        change_land,
    )
    .unlisted()
    .note("Done at once, with no preview (undo puts the ground back), near enough to the player that the ground is loaded. Read the ground first with sample_terrain (world: true). When the request starts with \"Selected area\", answer with \"terrain\" instead, so the player sees a preview."),
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

/// The op behind one of the agent's tools.
pub fn tool(name: &str) -> Option<&'static Op> {
    OPS.iter().find(|op| op.agent && op.name == name)
}

/// The ops as tools for the agent (MCP `tools/list` entries): each op's name, what it
/// does and when, and its arguments.
pub fn tools() -> Vec<Value> {
    OPS.iter()
        .filter(|op| op.agent)
        .map(|op| {
            let mut about = format!("{}.", op.about);
            for (on, text) in [
                (!op.note.is_empty(), op.note),
                (op.admin, "Only if the player is an admin of this game."),
                (op.ask, "The player is asked first, and may say no."),
                (op.last, "It happens when you finish this turn."),
            ] {
                if on {
                    about.push(' ');
                    about.push_str(text);
                }
            }
            let properties: serde_json::Map<String, Value> =
                op.params.iter().map(|p| (p.name.to_string(), serde_json::json!({ "type": p.json, "description": p.about }))).collect();
            let required: Vec<&str> = op.params.iter().filter(|p| !p.optional).map(|p| p.name).collect();
            serde_json::json!({
                "name": op.name,
                "description": about,
                "inputSchema": { "type": "object", "properties": properties, "required": required },
            })
        })
        .collect()
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
    cx.record(Action::Save { name: name.clone() });
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
    if args.text("go").as_deref() == Some("no") {
        return Ok(format!("Made the world {name}."));
    }
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
        cx.record(Action::SetPortal { build, to: None });
        return Ok("It's an ordinary build again.".into());
    }
    let to = PortalTo::parse(&to).map_err(|why| format!("Usage: /portal <world or host:port> | off ({why})"))?;
    let label = to.label().to_string();
    cx.record(Action::SetPortal { build, to: Some(to) });
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
    cx.record(Action::SetRules { rules });
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
    cx.record(Action::SetRules { rules });
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
    cx.record(Action::SetRules { rules });
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
    cx.record(Action::UninstallPackage { name: name.clone() });
    Ok(format!("{name} is off in this world; its pieces stay, asleep, in case it comes back."))
}

/// Turns one of the game's packages on in this world.
fn turn_on(cx: &mut Context, args: &Args) -> Reply {
    let name = args.text("name").ok_or("Which package? /packages lists the game's.")?;
    let package = catalog(cx.ledger).remove(&name).ok_or_else(|| format!("The game has no package \"{name}\"."))?;
    let title = package.manifest.title.clone();
    match cx.ledger.install((*package).clone(), Scope::World, cx.agent.map(|a| a.0), cx.agent.map(|a| a.1)) {
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

/// Keeps the player's bindings between runs: what differs from the defaults.
pub fn keep_bindings(binds: &Bindings) {
    super::prefs::set("bindings", serde_json::json!(binds.changed()));
}

/// Takes on the bindings kept from before.
pub fn load_bindings(mut binds: ResMut<Bindings>) {
    let kept: std::collections::BTreeMap<String, Vec<String>> =
        super::prefs::get("bindings").and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default();
    binds.restore(kept.iter().map(|(action, inputs)| (action.as_str(), inputs.iter().map(String::as_str).collect())));
}

fn bind(cx: &mut Context, args: &Args) -> Reply {
    let (Some(action), Some(key)) = (args.text("action"), args.text("key")) else {
        return Err("Usage: /bind <action> <key>, like /bind jump j. /bindings lists the actions.".into());
    };
    let said = cx.binds.bind(&action, &key)?;
    keep_bindings(cx.binds);
    Ok(said)
}

fn unbind(cx: &mut Context, args: &Args) -> Reply {
    let action = args.text("action").ok_or("Usage: /unbind <action> [key]. /bindings lists the actions.")?;
    let said = cx.binds.unbind(&action, args.text("key").as_deref())?;
    keep_bindings(cx.binds);
    Ok(said)
}

fn reset_keys(cx: &mut Context, args: &Args) -> Reply {
    let said = cx.binds.reset(args.text("action").as_deref())?;
    keep_bindings(cx.binds);
    Ok(said)
}

/// The ground a build stands on at world meters `(x, z)`: the loaded ground, edits and
/// all, or the world's own terrain where it isn't loaded. `None` over water.
fn ground(cx: &Context, x: f32, z: f32) -> Option<f32> {
    use genverse_rules::logic::Ground;
    let (column_x, column_z) = ((x / LAND_VOXEL).floor() as i32, (z / LAND_VOXEL).floor() as i32);
    let top = match cx.world.heights([column_x, column_z], 1, [1, 1]) {
        Some(heights) => heights.first().copied().flatten()?,
        None => {
            let terrain = cx.ledger.log.terrain();
            let h = terrain.height(column_x, column_z);
            if !terrain.flat && h < terrain.sea_level() {
                return None;
            }
            h
        }
    };
    Some((top + 1) as f32 * LAND_VOXEL)
}

/// Where a build goes, from an op's `x`, `z`, `y` and `turn`: on the build grid, on the
/// ground unless `y` says otherwise, facing the player unless `turn` says otherwise.
/// What's left out stays as it `was`, for a build that's already placed.
fn spot(cx: &Context, args: &Args, was: Option<(Vec3, f32)>) -> Result<(Vec3, f32), String> {
    let snap = |v: f32| (v / BUILD_VOXEL).round() * BUILD_VOXEL;
    let moved = args.num("x").is_some() || args.num("z").is_some();
    let x = args.num("x").map(snap).or(was.map(|w| w.0.x)).ok_or("Give x and z: where it goes, in world meters.")?;
    let z = args.num("z").map(snap).or(was.map(|w| w.0.z)).ok_or("Give x and z: where it goes, in world meters.")?;
    let y = match (args.num("y"), was) {
        (Some(y), _) => snap(y),
        (None, Some(was)) if !moved => was.0.y,
        (None, _) => ground(cx, x, z).ok_or(format!("There's no ground at x={x}, z={z} (water): give y too, or pick another spot."))?,
    };
    let yaw = match (args.num("turn"), was) {
        (Some(turn), _) => turn.round().rem_euclid(4.0) * std::f32::consts::FRAC_PI_2,
        (None, Some(was)) => was.1,
        // Its front (-z, turned) toward the player.
        (None, None) => {
            let to_player = Vec2::new(cx.at[0] - x, cx.at[2] - z);
            (0..4)
                .map(|k| k as f32 * std::f32::consts::FRAC_PI_2)
                .max_by(|a, b| Vec2::new(-a.sin(), -a.cos()).dot(to_player).total_cmp(&Vec2::new(-b.sin(), -b.cos()).dot(to_player)))
                .unwrap_or(0.0)
        }
    };
    Ok((Vec3::new(x, y, z), yaw))
}

/// Refuses a build that would close in on the player where they stand.
fn clear_of_player(cx: &Context, position: Vec3, yaw: f32, size: &[i64]) -> Result<(), String> {
    let half = Vec3::new(size[0] as f32, size[1] as f32, size[2] as f32) * BUILD_VOXEL / 2.0;
    let local = Quat::from_rotation_y(-yaw) * (Vec3::from_array(cx.at) - position);
    let inside = local.x.abs() < half.x + 0.3 && local.z.abs() < half.z + 0.3 && local.y > -1.4 && local.y < half.y * 2.0;
    if inside {
        return Err(format!(
            "That's on top of the player (they stand at x={:.1}, z={:.1}): put it at least {:.1} m from them.",
            cx.at[0],
            cx.at[2],
            half.x.max(half.z) + 1.0
        ));
    }
    Ok(())
}

/// Places an object where the agent says, without a draft.
fn place_build(cx: &mut Context, args: &Args) -> Reply {
    let spec: BuildSpec = args.object("build")?.ok_or("place_build needs \"build\": the object, in the \"build\" format.")?;
    let built = compile(&spec, &cx.ledger.rules.build).map_err(|errors| format!("The build breaks the rules:\n- {}", errors.join("\n- ")))?;
    let (position, yaw) = spot(cx, args, None)?;
    clear_of_player(cx, position, yaw, &spec.size)?;
    let portal = match args.text("portal") {
        Some(to) => Some(PortalTo::parse(&to).map_err(|why| format!("The portal can't lead there: {why}."))?),
        None => None,
    };
    let (agent, prompt) = cx.agent.unwrap_or_default();
    let name = spec.name.clone();
    let saved = SavedCreation { build: 0, name: name.clone(), prompt: prompt.into(), provider: agent.into(), position: position.into(), yaw, spec };
    let build = cx.creations.place(cx.ledger, saved, built.stats, cx.agent)?;
    let mut said = format!("Placed \"{name}\" as build {build} at x={:.2}, y={:.2}, z={:.2}.", position.x, position.y, position.z);
    if let Some(to) = portal {
        said.push_str(&format!(" It's a portal to {}: players walk up to it and press E.", to.label()));
        cx.record(Action::SetPortal { build, to: Some(to) });
    }
    Ok(said)
}

/// Moves, turns or redesigns a placed build, by id.
fn change_build(cx: &mut Context, args: &Args) -> Reply {
    let id = args.int("id").ok_or("Which build? Give its id (builds/ in your folder lists them).")?;
    let before = cx.ledger.builds.get(&id).cloned().ok_or(format!("There's no build {id} (builds/ in your folder lists them)."))?;
    let design: Option<BuildSpec> = args.object("build")?;
    let moves = ["x", "z", "y", "turn"].iter().any(|key| args.num(key).is_some());
    if design.is_none() && !moves {
        return Err("Nothing to change: give a new \"build\", or x, z, y or turn.".into());
    }
    // A build that only moves keeps the look it was accepted with.
    let (spec, limits) = match &design {
        Some(spec) => (spec.clone(), cx.ledger.rules.build),
        None => (before.spec.clone(), genverse_rules::build::BuildLimits::ACCEPTED),
    };
    let built = compile(&spec, &limits).map_err(|errors| format!("The build breaks the rules:\n- {}", errors.join("\n- ")))?;
    let (position, yaw) = spot(cx, args, Some((before.position.into(), before.yaw)))?;
    clear_of_player(cx, position, yaw, &spec.size)?;
    let (agent, prompt) = cx.agent.unwrap_or_default();
    let (prompt, provider) = if design.is_some() { (prompt.to_string(), agent.to_string()) } else { (before.prompt.clone(), before.provider.clone()) };
    let name = spec.name.clone();
    let saved = SavedCreation { build: id, name: name.clone(), prompt, provider, position: position.into(), yaw, spec };
    cx.creations.change(cx.ledger, saved, built.stats, cx.agent)?;
    Ok(format!("Build {id} \"{name}\" now stands at x={:.2}, y={:.2}, z={:.2}{}.", position.x, position.y, position.z, if design.is_some() { ", with its new design" } else { "" }))
}

fn remove_build(cx: &mut Context, args: &Args) -> Reply {
    let id = args.int("id").ok_or("Which build? Give its id (builds/ in your folder lists them).")?;
    let name = cx.creations.delete(cx.ledger, id, cx.agent)?;
    Ok(format!("Removed \"{name}\" (build {id})."))
}

/// The columns under a rectangle of world meters.
pub fn area_of(x: f32, z: f32, w: f32, d: f32) -> Area {
    let column = |m: f32| (m / LAND_VOXEL).floor() as i32;
    let far = |from: f32, size: f32| ((from + size.max(LAND_VOXEL)) / LAND_VOXEL).ceil() as i32 - 1;
    Area::from_corners(IVec2::new(column(x), column(z)), IVec2::new(far(x, w), far(z, d)))
}

/// Reshapes an area the agent chooses, at once.
fn change_land(cx: &mut Context, args: &Args) -> Reply {
    let spec: TerrainSpec = args.object("terrain")?.ok_or("change_land needs \"terrain\": the changes, in the \"terrain\" format.")?;
    let (Some(x), Some(z), Some(w), Some(d)) = (args.num("x"), args.num("z"), args.num("w"), args.num("d")) else {
        return Err("Give the area: x and z (its north-west corner) and w and d (its size), in world meters.".into());
    };
    let area = area_of(x, z, w, d);
    let (agent, prompt) = cx.agent.unwrap_or_default();
    match cx.landscape.change(cx.world, cx.ledger, spec, area, prompt.into(), agent.into()) {
        Ok((name, stats)) => Ok(format!("Reshaped the land (\"{name}\"): {}.", describe_stats(area, &stats))),
        Err(PreviewError::Refused(why)) => Err(format!("Can't change this land: {why}.")),
        Err(PreviewError::Invalid(errors)) => Err(format!("The land change breaks the rules:\n- {}", errors.join("\n- "))),
    }
}

/// Whether an op makes a kind of change.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug)]
pub enum Cover {
    /// This op makes it.
    Op(&'static str),
    /// Players make it by hand today (the land panel) or the agent's answer does; no op
    /// yet.
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
    Action::PlaceBuild(..) => Cover::Op("place_build"),
    Action::UpdateBuild(..) => Cover::Op("change_build"),
    Action::RemoveBuild { .. } => Cover::Op("remove_build"),
    Action::Terrain { .. } => Cover::Op("change_land"),
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
                assert!(op.params.iter().all(|p| p.json == "string"), "/{} takes something that can't be typed", op.name);
            }
            // Nothing typed may follow an argument that takes the rest of the line.
            let typed: Vec<&Param> = op.params.iter().filter(|p| p.kind != Kind::Named).collect();
            let rests = typed.iter().rev().skip(1).filter(|p| p.kind == Kind::Rest).count();
            assert_eq!(rests, 0, "{}: an argument after one that takes the rest", op.name);
            assert!(op.params.iter().all(|p| !p.about.is_empty()), "{}: an argument the agent isn't told about", op.name);
            // Asking and waiting are for the agent's tools.
            assert!(op.agent || !(op.ask || op.last), "{} isn't a tool, so nothing asks or waits", op.name);
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
    fn ops_become_tools_for_the_agent() {
        let tools = tools();
        let named = |name: &str| tools.iter().find(|t| t["name"] == name);
        assert!(named("play").is_none() && named("help").is_none(), "the chat's own ops aren't tools");
        let portal = named("portal").expect("portal is a tool");
        assert_eq!(portal["inputSchema"]["required"], json!(["to"]));
        assert!(portal["inputSchema"]["properties"]["build"]["description"].as_str().is_some_and(|d| !d.is_empty()));
        let remove = named("removeworld").expect("removeworld is a tool");
        let about = remove["description"].as_str().unwrap_or_default();
        assert!(about.contains("asked first") && about.contains("admin"), "{about}");
        assert!(named("host").expect("host is a tool")["description"].as_str().unwrap_or_default().contains("when you finish"));
        // A name only the agent gives is never read from chat words.
        let op = command("portal").expect("portal is a command");
        let args = Args::from_words(op.params, &["dungeon", "7"]);
        assert_eq!((args.text("to").as_deref(), args.int("build")), (Some("dungeon 7"), None));
    }

    #[test]
    fn objects_and_numbers_come_as_json_or_text() {
        let args = Args::new(json!({ "x": 12.5, "z": "-3", "terrain": "{\"name\": \"Pond\", \"ops\": []}", "turn": 5 }));
        assert_eq!((args.num("x"), args.num("z"), args.num("y")), (Some(12.5), Some(-3.0), None));
        let terrain: TerrainSpec = args.object("terrain").expect("it parses").expect("it's there");
        assert_eq!(terrain.name, "Pond");
        assert!(args.object::<TerrainSpec>("missing").expect("missing is fine").is_none());
        assert!(Args::new(json!({ "terrain": "{" })).object::<TerrainSpec>("terrain").is_err());
        // A rectangle of meters covers the columns it touches: 2 m at 25 cm is 8 columns.
        let area = area_of(1.0, -2.0, 2.0, 2.0);
        assert_eq!((area.min, area.size), ([4, -8], [8, 8]));
        let place = tool("place_build").expect("place_build is a tool");
        assert!(!place.slash, "builds can't be typed");
        let schema = tools().into_iter().find(|t| t["name"] == "place_build").expect("it's listed");
        assert_eq!(schema["inputSchema"]["properties"]["build"]["type"], "object");
        assert_eq!(schema["inputSchema"]["required"], json!(["build", "x", "z"]));
    }

    #[test]
    fn trips_within_the_game_can_carry_a_note() {
        let tools = tools();
        for name in ["world", "newworld"] {
            let tool = tools.iter().find(|t| t["name"] == name).expect("it's a tool");
            assert!(tool["inputSchema"]["properties"]["then"]["description"].as_str().is_some_and(|d| d.contains("started again")), "{name}");
            assert!(!tool["inputSchema"]["required"].as_array().expect("a list").iter().any(|r| r == "then"), "{name}: the note is optional");
        }
        // The note is the agent's: a chat line never fills it.
        let op = command("world").expect("world is a command");
        assert_eq!(Args::from_words(op.params, &["dungeon", "build", "a", "gate"]).text("then"), None);
    }

    #[test]
    fn revert_is_still_undo() {
        assert_eq!(command("revert").map(|op| op.name), Some("undo"));
        assert!(command("turn_on").is_none(), "the window's ops aren't chat commands");
    }
}
