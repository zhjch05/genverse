//! The agent's tools, served over [`mcp`](super::mcp).
//!
//! - **Looking:** the build site, nearby builds, terrain, `search`, `query`, `look`, and
//!   `test_game`, which plays a game package on a copy of the world. These are in the
//!   *build frame* the agent designs in: build voxels (12.5 cm), origin at the build
//!   site's ground, +Y up, z = 0 the front row facing the player, x to the right as the
//!   player sees it. They change nothing.
//! - **Doing:** every [op](super::ops) a player has, as the player ([`act`]). They happen
//!   at once and the agent's folder follows, with two exceptions. What an op marks
//!   [`ask`](super::ops::Op::ask) waits for the player's yes ([`Ask`], a card in the
//!   chat). And what would swap the world under the agent (a trip elsewhere, hosting,
//!   joining) is kept in [`AfterTurn`] until its turn ends.

use std::fmt::Write;

use bevy::prelude::*;
use serde_json::{Value, json};

use genverse_rules::{
    build::BuildLimits,
    logic::{data::GameSpec, trial},
    settings::WorldRules,
};

use super::{
    Chat,
    creations::{Creations, SavedCreation, anchor_in_front},
    landscape::{Landscape, sample},
    mcp::{McpServer, ToolCall, ToolResult},
    ops::{self, Op},
    spec::{BuildSpec, VOXEL_SIZE, compile},
    worlds::{Travel, Trip},
};
use crate::{
    player::Player,
    voxel::{AIR, VoxelId},
    world::{
        VoxelWorld,
        ledger::Ledger,
        terrain::{self, block},
    },
};

pub const INSTRUCTIONS: &str = "Tools for the Genverse voxel world. Some look (the build site, search, query, \
look, test_game): for those, coordinates are build voxels (12.5 cm) in the frame of the build site: y=0 is the \
ground there, z=0 is the front row facing the player. The others do what the player can do from the chat and \
the Game window (worlds, portals, saves, undo, rules), as the player.";

/// How long the player has to answer an [`Ask`]; under the tool server's own wait.
const ASK_WAIT: std::time::Duration = std::time::Duration::from_secs(240);

/// Something the agent wants to do that the player is asked about first.
pub struct Ask {
    op: &'static Op,
    args: Value,
    /// What it is, for the card.
    pub what: String,
    reply: std::sync::mpsc::Sender<ToolResult>,
    since: std::time::Instant,
}

impl Ask {
    /// The player said yes: does it, and tells the agent how it went.
    pub fn allow(self, chat: &mut Chat, ledger: &mut Ledger, world: &mut VoxelWorld, creations: &mut Creations, landscape: &mut Landscape, at: [f32; 3]) {
        let result = act(self.op, self.args, chat, ledger, world, creations, landscape, at);
        info!("agent tool {} (allowed): {result:?}", self.op.name);
        let _ = self.reply.send(result);
    }

    /// The player said no, or never answered (`why`).
    pub fn refuse(self, why: &str) {
        info!("agent tool {}: {why}", self.op.name);
        let _ = self.reply.send(Err(why.into()));
    }
}

/// What the agent set going that waits for its turn to end: it would swap the world
/// it's working in.
#[derive(Default)]
pub struct AfterTurn {
    /// Ops to do then, in order.
    pub ops: Vec<(&'static str, Value)>,
    /// Where the player goes then.
    pub trip: Option<Trip>,
    pub stop_hosting: bool,
    /// What the agent left itself to do where the trip leads, and the request it's
    /// part of: it's started again there ([`Resume`]).
    pub then: Option<(String, String)>,
    /// The player was told it's waiting for their draft.
    pub told: bool,
}

/// A request that goes on in the world its trip leads to: once the player has arrived,
/// the agent is started again there with its note.
pub struct Resume {
    /// The world the trip left from.
    pub from: String,
    /// What's left to do, in the agent's words.
    pub note: String,
    /// The player's request it's part of.
    pub prompt: String,
    pub since: std::time::Instant,
}

impl AfterTurn {
    pub fn is_empty(&self) -> bool {
        self.ops.is_empty() && self.trip.is_none()
    }
}

/// Does an op for the agent, as the player, and brings the agent's folder up to date.
/// A trip the op asks for is held until the turn ends.
pub fn act(
    op: &'static Op,
    args: Value,
    chat: &mut Chat,
    ledger: &mut Ledger,
    world: &mut VoxelWorld,
    creations: &mut Creations,
    landscape: &mut Landscape,
    at: [f32; 3],
) -> ToolResult {
    if op.last {
        chat.after.ops.push((op.name, args));
        return Ok(format!("\"{}\" happens as soon as you answer. Do the rest of the work first, and tell the player it's on its way.", op.name));
    }
    let asked = chat.pending.as_ref().map(|p| (p.provider.label(), p.prompt.clone()));
    let then = args["then"].as_str().map(str::trim).filter(|note| !note.is_empty()).map(str::to_string);
    let edited = super::workspace::edited(ledger);
    let mut held = Travel::default();
    let result = {
        let cx = &mut ops::Context {
            ledger: &mut *ledger,
            world,
            creations,
            landscape,
            travel: &mut held,
            at,
            agent: asked.as_ref().map(|(agent, prompt)| (*agent, prompt.as_str())),
        };
        ops::run_op(op, &ops::Args::new(args), cx)
    };
    if let Err(why) = super::workspace::refresh(ledger, &edited) {
        warn!("couldn't bring the agent's folder up to date: {why}");
    }
    let mut text = result?;
    if let Some(trip) = held.next.take() {
        chat.after.trip = Some(trip);
        chat.after.stop_hosting |= held.stop_hosting;
        text = format!("{text} The player is taken there as soon as you answer (tell them you're taking them there): do the rest of the work here first.").trim().to_string();
        chat.after.then = then.zip(asked.map(|(_, prompt)| prompt));
        if chat.after.then.is_some() {
            text.push_str(" You'll be started again there with your note, to do what's left.");
        }
    }
    Ok(if text.is_empty() { "Done.".into() } else { text })
}

/// Where the next build goes, frozen while a request runs so tool answers stay consistent.
#[derive(Resource, Default, Clone, Copy)]
pub struct BuildSite {
    /// Bottom-center of the footprint.
    pub origin: Vec3,
    pub yaw: f32,
}

impl BuildSite {
    fn rotation(self) -> Quat {
        Quat::from_rotation_y(self.yaw)
    }

    /// Build-frame voxel coordinates (relative to the footprint center) to world space.
    fn to_world(self, local: Vec3) -> Vec3 {
        self.origin + self.rotation() * (local * VOXEL_SIZE)
    }

    fn to_local(self, world: Vec3) -> Vec3 {
        self.rotation().inverse() * (world - self.origin) / VOXEL_SIZE
    }
}

pub fn definitions(rules: &WorldRules) -> Value {
    let size = json!({ "type": "integer", "minimum": 2, "maximum": rules.build.max_size });
    let mut tools = json!([
        {
            "name": "get_build_site",
            "description": "Ground heights and surface types under a footprint centered on the build site, \
                plus where the player stands. Call this before designing anything that should fit the terrain.",
            "inputSchema": {
                "type": "object",
                "properties": { "width": size, "depth": size },
                "required": ["width", "depth"],
            },
        },
        {
            "name": "list_nearby_builds",
            "description": "Builds already placed near the site: id, name, size, position and turn in the build \
                frame, and the request that made them.",
            "inputSchema": {
                "type": "object",
                "properties": { "radius_m": { "type": "number", "minimum": 1, "maximum": 60 } },
            },
        },
        {
            "name": "get_build_spec",
            "description": "The full build JSON of a placed build (by id from list_nearby_builds), e.g. to match \
                its style or palette. Use id \"draft\" for the draft being revised.",
            "inputSchema": {
                "type": "object",
                "properties": { "id": { "type": "string" } },
                "required": ["id"],
            },
        },
        {
            "name": "sample_terrain",
            "description": "A height map of the ground, with surfaces and trees; levels relative to sea level. \
                With an area selected: a finer look at part of it, x, z, w, d in meters in the area frame (from its \
                corner). With \"world\": true (or no area selected): the rectangle of world meters from x, z (its \
                north-west corner) w wide and d deep, as change_land would see that area.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "x": { "type": "number" }, "z": { "type": "number" },
                    "w": { "type": "number", "minimum": 0.5 }, "d": { "type": "number", "minimum": 0.5 },
                    "world": { "type": "boolean" },
                },
                "required": ["x", "z", "w", "d"],
            },
        },
        {
            "name": "check_build",
            "description": "Validate a build against the limits and report how it sits on the ground at the site: \
                floating columns, voxels buried in terrain or under water, and overlaps with nearby builds. \
                Does not place anything.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "build": { "type": "object", "description": "Same shape as the \"build\" in your answer." },
                },
                "required": ["build"],
            },
        },
        {
            "name": "test_game",
            "description": "Play a game package in an empty flat test world, as a player, before answering: it is \
                checked and installed as your answer would be (on_start runs), the tester comes in at the spawn point \
                (on_join), then each step runs through the real rules. Returns what each step did (changes, messages, \
                regions entered, refusals and script errors), where the tester ends up, and a map and list of the \
                pieces afterwards. Steps: \"go <x> <z>\" (the tester walks there, in meters: regions and chasers \
                notice), \"hit <label or model>\" (attacks the nearest one: on_hit), \"interact\" or \"interact \
                <label or model>\" (E on the nearest labeled piece, or the nearest of those), \"wait <seconds>\" (time \
                passes: timers, walks, chases and spawners run; pieces more than 64 m from the tester sleep), \
                \"choose <choice id>\" (a pick in the open panel), \"close\", \"place <fixture>\" (puts it down, \
                each 32 m apart; the tester stands there after), or a key (\"enter\", \"north\", \"south\", \"east\", \
                \"west\", \"action\", \"reset\", \"leave\"; sent to the region taking keys the tester's pawn is in), \
                \"use <item> on <label>\" (right-click with the item in hand on that piece: on_use), \"say <chat line>\" \
                (on_chat), \"send <name> <json>\" (as if a player script sent it: on_message), \"press <key>\" and \"hold \
                <key+key> <seconds>\" (runs the player scripts frame by frame with those keys: how the hero moves, the \
                camera, and what they send), \"wait until night\" (or dawn, noon, dusk, midnight, 0-1). Every step takes a \
                second. Use it to play your game through as a player would, winning and losing, and fix what fails.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "game": { "type": "object", "description": "Optional: a change in the shape of the \"game\" in your answer, \
                        on top of the package files. Leave it out to test the files as you edited them." },
                    "steps": { "type": "array", "items": { "type": "string" }, "maxItems": 300 },
                },
                "required": ["steps"],
            },
        },
        {
            "name": "look",
            "description": "Takes a picture of the world as the player's game draws it, and saves it in your folder \
                (shots/): Read the file it names to see it. With no arguments it's the player's own view. Otherwise \
                the camera stands at from {x, y, z} and looks at {x, y, z} (meters; the camera goes back to the \
                player after). Use it to check how what you made looks: its size, colors, where it stands.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "from": { "type": "object", "properties": { "x": { "type": "number" }, "y": { "type": "number" }, "z": { "type": "number" } } },
                    "at": { "type": "object", "properties": { "x": { "type": "number" }, "y": { "type": "number" }, "z": { "type": "number" } } },
                },
            },
        },
        {
            "name": "search",
            "description": "Finds what's in the world by meaning and place: builds, land changes, game packages and \
                their files, labeled pieces and named places, best match first, with ids, positions (meters), what \
                they're about and the request that made them. \"text\" is what you're looking for (\"wolves\", \
                \"the merchant's hut\", \"bread\"); empty lists what's near. x and z (meters) rank nearer things \
                higher; radius leaves out what's further.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "text": { "type": "string" },
                    "x": { "type": "number" }, "z": { "type": "number" },
                    "radius": { "type": "number", "minimum": 1 },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 100 },
                },
                "required": ["text"],
            },
        },
        {
            "name": "query",
            "description": "Runs Lua against the live world with the game API (docs/game.md: get, find, near, \
                players, stat, count, ...) and returns what the code returns, as JSON: `return #find(\"wolf\")`, \
                `return near({x = 0, y = 9, z = 0}, 20)`. As the package \"package\" (default: world), so component \
                and stat names without \":\" are its. Anything it would change is only reported, never kept: use it \
                to look at what the game is doing, or to try a call out.",
            "inputSchema": {
                "type": "object",
                "properties": { "code": { "type": "string" }, "package": { "type": "string" } },
                "required": ["code"],
            },
        },
    ]);
    // What the player can do, the agent can do for them.
    if let Value::Array(tools) = &mut tools {
        tools.extend(ops::tools());
    }
    tools
}

fn search(args: &Value, ledger: &Ledger, hero: Option<Vec3>) -> Result<String, String> {
    let text = args["text"].as_str().unwrap_or_default();
    let num = |key: &str| args[key].as_f64().filter(|v| v.is_finite()).map(|v| v as f32);
    let near = match (num("x"), num("z")) {
        (Some(x), Some(z)) => Some([x, z]),
        _ => hero.map(|h| [h.x, h.z]),
    };
    let limit = args["limit"].as_u64().unwrap_or(20).clamp(1, 100) as usize;
    let records = genverse_rules::semantic::records(&ledger.log, &ledger.builds, &ledger.logic);
    let found = genverse_rules::semantic::search(&records, text, near, num("radius"), limit);
    if found.is_empty() {
        return Ok(format!("Nothing found for \"{text}\" (of {} things in the world).", records.len()));
    }
    Ok(serde_json::to_string_pretty(&found).unwrap_or_default())
}

fn query(args: &Value, ledger: &Ledger) -> Result<String, String> {
    let code = args["code"].as_str().ok_or("query needs \"code\"")?;
    let package = args["package"].as_str().unwrap_or("world");
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let ground = genverse_rules::log::TerrainGround { terrain: ledger.log.terrain(), edits: &ledger.edits };
    let (value, changes) = ledger.logic.query(package, &ledger.profile.id, code, now, &ledger.rules.logic, Some(&ground))?;
    let mut out = serde_json::to_string_pretty(&value).unwrap_or_default();
    if changes > 0 {
        out.push_str(&format!("\n(It would have made {changes} change(s); none were kept.)"));
    }
    Ok(out)
}

/// A picture the agent asked for (`look`): where the camera stands and looks (the
/// player's view if `None`), the file, and who's waiting.
pub struct PendingLook {
    view: Option<(Vec3, Vec3)>,
    path: std::path::PathBuf,
    reply: std::sync::mpsc::Sender<super::mcp::ToolResult>,
    frames: u32,
}

#[derive(Resource, Default)]
pub struct Looks(Option<PendingLook>);

/// Takes the picture: holds the camera where it was asked for a few frames, saves the
/// screen, and answers once the file is there.
pub fn take_looks(mut commands: Commands, mut looks: ResMut<Looks>, mut cameras: Query<&mut Transform, With<crate::player::FollowCamera>>) {
    let Some(look) = &mut looks.0 else { return };
    look.frames += 1;
    if let (Some((from, at)), Ok(mut camera)) = (look.view, cameras.single_mut()) {
        *camera = Transform::from_translation(from).looking_at(at, Vec3::Y);
    }
    // Let the view settle (and the ground stream in) before the picture.
    if look.frames == 20 {
        use bevy::render::view::screenshot::{Screenshot, save_to_disk};
        commands.spawn(Screenshot::primary_window()).observe(save_to_disk(look.path.clone()));
    }
    // Saving takes a few frames more; give the file time to be written in full.
    let saved = look.frames > 35 && std::fs::metadata(&look.path).is_ok_and(|m| m.len() > 0);
    if saved || look.frames > 600 {
        let look = looks.0.take().expect("checked above");
        let name = look.path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        let _ = look.reply.send(if saved { Ok(format!("Saved shots/{name}: Read it to see the picture.")) } else { Err("the picture didn't save".into()) });
    }
}

/// The world's ground and its builds, for a trial's walks.
struct Land<'a> {
    world: &'a VoxelWorld,
    builds: &'a genverse_rules::log::Builds,
}

impl genverse_rules::logic::Ground for Land<'_> {
    fn heights(&self, min: [i32; 2], step: i32, count: [i32; 2]) -> Option<Vec<Option<i32>>> {
        self.world.heights(min, step, count)
    }

    fn solids(&self, lo: [f32; 2], hi: [f32; 2]) -> Vec<genverse_rules::logic::solid::Solid<'_>> {
        genverse_rules::logic::solid::of_builds(self.builds, lo, hi).collect()
    }
}

/// Plays a change to the world's game logic (on top of what the world runs under that
/// name) through some steps, on a copy of the world as it is now: its pieces, timers and
/// players' things, its ground, the tester starting where the player stands. So a new
/// version's migration runs on the real state before anyone else sees it.
fn test_game(args: &Value, ledger: &Ledger, world: &VoxelWorld, hero: Option<Vec3>) -> Result<String, String> {
    // The package files as they are on disk, with a "game" given here on top.
    let given: Option<GameSpec> = match &args["game"] {
        Value::Null => None,
        game => Some(serde_json::from_value(game.clone()).map_err(|e| format!("game doesn't match the schema: {e}"))?),
    };
    let edited = match super::workspace::current() {
        Some(dir) => super::workspace::changed_game(&dir, ledger)?,
        None => None,
    };
    let change = super::workspace::merge(edited, given)
        .ok_or("nothing to test: the package files are as the world runs them (edit them first), and no \"game\" was given")?;
    let steps: Vec<String> = serde_json::from_value(args["steps"].clone()).map_err(|e| format!("steps must be strings: {e}"))?;
    let logic = &ledger.logic;
    let base = logic.installed.get(&change.name).and_then(|i| logic.package(i)).map(GameSpec::from_package);
    let spec = change.onto(base.as_ref());
    let version = logic.installed.get(&spec.name).map_or(1, |i| i.package.version + 1);
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let land = Land { world, builds: &ledger.builds };
    let copy = hero.map(|at| trial::Copy {
        logic: logic.clone(),
        rules: ledger.rules.logic,
        at: at.to_array(),
        now,
        next_change: ledger.log.changes.last().map_or(1, |c| c.id + 1),
        ground: &land,
    });
    let report = trial::play_in(&spec.to_package(version), &steps[..steps.len().min(300)], copy);
    let report = match &base {
        Some(_) => format!("Tested your change on top of the world's \"{}\" ({}).\n{report}", spec.name, change.change_summary(base.as_ref())),
        None => report,
    };
    info!("test_game ({} steps):\n{report}", steps.len());
    Ok(report)
}

/// Keeps the build site current while no request is running: the draft or selected
/// build if there is one, otherwise a spot in front of the player.
pub fn update_build_site(
    mut site: ResMut<BuildSite>,
    chat: Res<super::Chat>,
    creations: Res<Creations>,
    world: Res<VoxelWorld>,
    ledger: Res<Ledger>,
    players: Query<&Transform, With<Player>>,
    transforms: Query<&Transform>,
) {
    if chat.pending.is_some() {
        return;
    }
    let target = creations.draft.as_ref().map(|d| d.entity).or(creations.selected);
    if let Some(t) = target.and_then(|e| transforms.get(e).ok()) {
        *site = BuildSite { origin: t.translation, yaw: t.rotation.to_euler(EulerRot::YXZ).0 };
    } else if let Ok(player) = players.single() {
        let (origin, yaw) = anchor_in_front(player, &world, UVec3::splat(ledger.rules.build.max_size / 2));
        *site = BuildSite { origin, yaw };
    }
}

pub fn answer_tool_calls(
    server: Option<Res<McpServer>>,
    site: Res<BuildSite>,
    mut world: ResMut<VoxelWorld>,
    mut creations: ResMut<Creations>,
    mut landscape: ResMut<Landscape>,
    mut ledger: ResMut<Ledger>,
    mut chat: ResMut<Chat>,
    players: Query<&Transform, With<Player>>,
    transforms: Query<&Transform>,
    mut looks: ResMut<Looks>,
) {
    // A question nobody answered, or whose request is over.
    if chat.asking.as_ref().is_some_and(|ask| chat.pending.is_none() || ask.since.elapsed() > ASK_WAIT)
        && let Some(ask) = chat.asking.take()
    {
        ask.refuse("the player didn't answer");
    }
    let Some(server) = server else {
        return;
    };
    let calls = server.drain();
    if calls.is_empty() {
        return;
    }
    let hero = players.single().ok().map(|t| t.translation);
    for ToolCall { name, args, reply } in calls {
        // A picture takes a few frames: it answers when it's saved.
        if name == "look" {
            let point = |v: &Value| Some(Vec3::new(v["x"].as_f64()? as f32, v["y"].as_f64()? as f32, v["z"].as_f64()? as f32));
            let Some(dir) = super::workspace::current() else {
                let _ = reply.send(Err("there's no folder to save the picture in".into()));
                continue;
            };
            if looks.0.is_some() {
                let _ = reply.send(Err("a picture is already being taken".into()));
                continue;
            }
            let n = std::fs::read_dir(dir.join("shots")).map_or(0, |d| d.count()) + 1;
            let path = dir.join("shots").join(format!("look-{n}.png"));
            let _ = std::fs::create_dir_all(dir.join("shots"));
            let view = point(&args["from"]).map(|from| (from, point(&args["at"]).unwrap_or(from + Vec3::NEG_Z)));
            looks.0 = Some(PendingLook { view, path, reply, frames: 0 });
            info!("agent tool look: taking a picture");
            continue;
        }
        // Doing something, as the player.
        if let Some(op) = ops::tool(&name) {
            // `GENVERSE_KEEP=1` (scripted runs) says yes, as it keeps drafts.
            if op.ask && std::env::var("GENVERSE_KEEP").is_err() {
                if chat.asking.is_some() {
                    let _ = reply.send(Err("the player is still deciding about the last thing you asked for: one at a time".into()));
                    continue;
                }
                let line = ops::Args::new(args.clone()).line();
                let what = if line.is_empty() { op.about.to_string() } else { format!("{}: {line}", op.about) };
                info!("agent tool {name}: asking the player ({what})");
                chat.asking = Some(Ask { op, args, what, reply, since: std::time::Instant::now() });
                chat.open = true;
                continue;
            }
            let at = hero.map_or([0.0; 3], |h| h.to_array());
            let result = act(op, args, &mut chat, &mut ledger, &mut world, &mut creations, &mut landscape, at);
            info!("agent tool {name}: {result:?}");
            let _ = reply.send(result);
            continue;
        }
        let placed: Vec<_> = creations.placed.iter().filter_map(|(e, c)| transforms.get(*e).ok().map(|t| (*t, c))).collect();
        let ctx = Ctx { site: *site, world: &world, creations: &creations, placed: &placed, limits: &ledger.rules.build };
        let result = match name.as_str() {
            "get_build_site" => Ok(ctx.build_site(&args, players.single().ok())),
            "list_nearby_builds" => Ok(ctx.nearby(&args)),
            "get_build_spec" => ctx.spec(&args),
            "check_build" => ctx.check(&args),
            "sample_terrain" => sample_terrain(&world, &landscape, &args),
            "test_game" => test_game(&args, &ledger, &world, hero),
            "search" => search(&args, &ledger, hero),
            "query" => query(&args, &ledger),
            other => Err(format!("unknown tool {other}")),
        };
        info!("agent tool {name}: {}", if result.is_ok() { "ok" } else { "error" });
        let _ = reply.send(result);
    }
}

fn sample_terrain(world: &VoxelWorld, landscape: &Landscape, args: &Value) -> Result<String, String> {
    let num = |key: &str| args[key].as_f64().filter(|v| v.is_finite()).map(|v| v as f32);
    // Any rectangle of the world, as `change_land` takes it.
    if args["world"].as_bool() == Some(true) || landscape.target().is_none() {
        let (Some(x), Some(z), Some(w), Some(d)) = (num("x"), num("z"), num("w"), num("d")) else {
            return Err("give x, z, w and d in world meters (no area is selected)".into());
        };
        let area = ops::area_of(x, z, w.clamp(0.5, 64.0), d.clamp(0.5, 64.0));
        let size = area.size_m();
        return Ok(format!(
            "World rectangle from x={x:.1}, z={z:.1}, {:.1} × {:.1} m. Below, x and z run from its corner, as in change_land's \"terrain\".\n{}",
            size.x,
            size.y,
            sample(world, area, Vec2::ZERO, size, 24)
        ));
    }
    let area = landscape.target().ok_or("no area is selected")?;
    let size = area.size_m();
    let from = Vec2::new(num("x").unwrap_or(0.0), num("z").unwrap_or(0.0)).clamp(Vec2::ZERO, size - 0.25);
    let span = Vec2::new(num("w").unwrap_or(size.x), num("d").unwrap_or(size.y)).max(Vec2::splat(0.25)).min(size - from);
    Ok(sample(world, area, from, span, 24))
}

struct Ctx<'a> {
    limits: &'a BuildLimits,
    site: BuildSite,
    world: &'a VoxelWorld,
    creations: &'a Creations,
    /// Placed builds with their current transforms.
    placed: &'a [(Transform, &'a SavedCreation)],
}

/// What the ground is made of at the top of a column.
fn surface_code(id: VoxelId) -> char {
    match id {
        id if block::GRASS.contains(&id) => 'g',
        id if block::DIRT.contains(&id) => 'd',
        id if block::STONE.contains(&id) => 'r',
        id if block::SAND.contains(&id) => 's',
        id if block::BARK.contains(&id) => 't',
        id if block::LEAVES.contains(&id) => 'l',
        block::WATER => 'w',
        block::SNOW => 'n',
        _ => '?',
    }
}

const SURFACE_LEGEND: &str = "g grass, d dirt, r rock, s sand, w water (height is the water surface), \
t tree trunk, l leaves, n snow, ? not loaded";

impl Ctx<'_> {
    /// Top of the first ground voxel (terrain, water, trees) under a world point, as a
    /// build-frame height, with its surface code. Grass tufts and flowers don't count.
    fn ground(&self, world_xz: Vec3) -> (Option<f32>, char) {
        let start = VoxelWorld::voxel_at(Vec3::new(world_xz.x, self.site.origin.y + 6.0, world_xz.z));
        for dy in 0..120 {
            let p = start - IVec3::Y * dy;
            match self.world.get(p) {
                None => return (None, '?'),
                Some(AIR | block::TUFT) => {}
                Some(id) if block::FLOWERS.contains(&id) => {}
                Some(id) => {
                    let top = (p.y + 1) as f32 * terrain::VOXEL_SIZE;
                    return (Some((top - self.site.origin.y) / VOXEL_SIZE), surface_code(id));
                }
            }
        }
        (None, '?')
    }

    /// Ground under build-frame column `(x, z)` of a `width`×`depth` footprint.
    fn column_ground(&self, x: f32, z: f32, width: f32, depth: f32) -> (Option<f32>, char) {
        let local = Vec3::new(x + 0.5 - width / 2.0, 0.0, z + 0.5 - depth / 2.0);
        self.ground(self.site.to_world(local))
    }

    fn build_site(&self, args: &Value, player: Option<&Transform>) -> String {
        let dim = |key: &str| args[key].as_u64().unwrap_or(32).clamp(2, self.limits.max_size as u64) as u32;
        let (width, depth) = (dim("width"), dim("depth"));
        let mut out = String::new();
        let _ = writeln!(
            out,
            "Footprint {width}×{depth} build voxels centered on the site. x runs left→right as the player sees it, \
             z runs front→back (z=0 faces the player). Each entry is the ground height at that column relative to \
             y=0, in build voxels (8 per meter), followed by the surface: {SURFACE_LEGEND}. Terrain is 2 build \
             voxels per step, so every second column is shown."
        );
        if let Some(player) = player {
            let p = self.site.to_local(player.translation);
            let _ = writeln!(
                out,
                "The player stands at x={:.0}, z={:.0} (relative to the footprint center), feet at y={:.0}.",
                p.x + width as f32 / 2.0,
                p.z + depth as f32 / 2.0,
                p.y
            );
        }
        let _ = write!(out, "\n      ");
        for x in (0..width).step_by(2) {
            let _ = write!(out, "x{x:<3} ");
        }
        out.push('\n');
        for z in (0..depth).step_by(2) {
            let _ = write!(out, "z{z:<3}  ");
            for x in (0..width).step_by(2) {
                let (h, code) = self.column_ground(x as f32, z as f32, width as f32, depth as f32);
                match h {
                    Some(h) => {
                        let _ = write!(out, "{:<+4}{code} ", h.round() as i32);
                    }
                    None => out.push_str(" ?   "),
                }
            }
            out.push('\n');
        }
        if let Some(draft) = &self.creations.draft
            && draft.replaces.is_some()
        {
            let _ = writeln!(
                out,
                "\nYou are revising \"{}\" ({}×{}×{}); it stands on this site. get_build_spec id \"draft\" returns it.",
                draft.spec.name, draft.stats.size.x, draft.stats.size.y, draft.stats.size.z
            );
        }
        out
    }

    fn nearby(&self, args: &Value) -> String {
        let radius = args["radius_m"].as_f64().unwrap_or(24.0) as f32;
        let mut rows: Vec<_> = self
            .placed
            .iter()
            .filter_map(|(t, saved)| {
                let distance = t.translation.distance(self.site.origin);
                (distance <= radius).then_some((distance, t, saved))
            })
            .collect();
        rows.sort_by(|a, b| a.0.total_cmp(&b.0));
        if rows.is_empty() {
            return format!("No builds within {radius:.0} m of the site.");
        }
        let mut out = format!(
            "Builds within {radius:.0} m. Positions are their bottom-center relative to the site's footprint center, \
             in build voxels; turn is relative to the site's front.\n"
        );
        for (distance, t, c) in rows.into_iter().take(12) {
            let p = self.site.to_local(t.translation);
            let turn = (t.rotation.to_euler(EulerRot::YXZ).0 - self.site.yaw).to_degrees().rem_euclid(360.0);
            let _ = writeln!(
                out,
                "- id {} \"{}\" size {}×{}×{} at x={:+.0} y={:+.0} z={:+.0} ({distance:.1} m away), turned {:.0}°, \
                 made for \"{}\"",
                c.build, c.name, c.spec.size[0], c.spec.size[1], c.spec.size[2], p.x, p.y, p.z, turn, c.prompt
            );
        }
        out
    }

    fn spec(&self, args: &Value) -> Result<String, String> {
        let id = args["id"].as_str().map(str::to_string).or_else(|| args["id"].as_u64().map(|n| n.to_string()));
        let id = id.ok_or("pass an id from list_nearby_builds, or \"draft\"")?;
        let spec = if id == "draft" {
            self.creations.draft.as_ref().map(|d| &d.spec).ok_or("there is no draft")?
        } else {
            let n: u64 = id.parse().map_err(|_| format!("unknown id {id:?}"))?;
            self.creations.placed.iter().find(|(_, c)| c.build == n).map(|(_, c)| &c.spec).ok_or(format!("no build {n}"))?
        };
        serde_json::to_string(spec).map_err(|e| e.to_string())
    }

    fn check(&self, args: &Value) -> Result<String, String> {
        let spec: BuildSpec =
            serde_json::from_value(args["build"].clone()).map_err(|e| format!("build doesn't match the schema: {e}"))?;
        let build = compile(&spec, self.limits).map_err(|errors| format!("Rejected:\n- {}", errors.join("\n- ")))?;
        let s = build.stats;
        let grid = &build.model.grid;
        let (w, d) = (s.size.x as f32, s.size.z as f32);

        let (mut floating, mut max_gap, mut buried, mut wet, mut columns) = (0, 0.0_f32, 0, 0, 0);
        for z in 0..s.size.z as i32 {
            for x in 0..s.size.x as i32 {
                let ys: Vec<i32> = (0..s.size.y as i32).filter(|&y| grid.get(IVec3::new(x, y, z)) != AIR).collect();
                let Some(&lowest) = ys.first() else {
                    continue;
                };
                columns += 1;
                let (Some(ground), code) = self.column_ground(x as f32, z as f32, w, d) else {
                    continue;
                };
                let gap = lowest as f32 - ground;
                if gap >= 1.5 {
                    floating += 1;
                    max_gap = max_gap.max(gap);
                }
                let under = ys.iter().filter(|&&y| (y as f32) < ground - 0.5).count();
                if code == 'w' {
                    wet += under;
                } else {
                    buried += under;
                }
            }
        }

        let mut out = format!(
            "Valid: {}×{}×{} voxels, {} solid, {} visible faces, {} glowing (limits {} / {} / {}).\n",
            s.size.x, s.size.y, s.size.z, s.voxels, s.faces, s.glow,
            self.limits.max_voxels, self.limits.max_faces, self.limits.max_glow_voxels
        );
        let _ = writeln!(
            out,
            "On this site: {floating} of {columns} footprint columns float above the ground (largest gap {:.0} \
             voxels); {buried} voxels are buried in terrain; {wet} are under water.",
            max_gap
        );
        // Rough overlap check: square bounds that cover any rotation.
        let reach = |w: f32, d: f32| w.max(d) * VOXEL_SIZE / 2.0;
        let r = reach(w, d);
        let lo = self.site.origin - Vec3::new(r, 0.0, r);
        let hi = self.site.origin + Vec3::new(r, s.size.y as f32 * VOXEL_SIZE, r);
        let replaced = self.creations.draft.as_ref().and_then(|d| d.replaces.as_ref()).map(|r| r.build);
        let overlaps: Vec<_> = self
            .placed
            .iter()
            .filter(|(_, c)| Some(c.build) != replaced)
            .filter_map(|(t, c)| {
                let r = reach(c.spec.size[0] as f32, c.spec.size[2] as f32);
                let height = c.spec.size[1] as f32 * VOXEL_SIZE;
                let (blo, bhi) = (t.translation - Vec3::new(r, 0.0, r), t.translation + Vec3::new(r, height, r));
                (lo.cmplt(bhi).all() && blo.cmplt(hi).all()).then(|| format!("\"{}\" (id {})", c.name, c.build))
            })
            .collect();
        if overlaps.is_empty() {
            out.push_str("No nearby builds in the way.");
        } else {
            let _ = write!(out, "Its bounds overlap: {}.", overlaps.join(", "));
        }
        Ok(out)
    }
}
