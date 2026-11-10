//! What the agent is told: the voxel building rules, plus the live context of the request.

use super::spec::BuildSpec;
use genverse_rules::{logic::data::GameSpec, sculpt::TerrainSpec, settings::WorldRules};

/// The system prompt, with this world's limits.
pub fn system(with_tools: bool, rules: &WorldRules) -> String {
    let (limits, terrain_limits) = (&rules.build, &rules.terrain);
    let mut out = format!(
        r##"You are the player's agent in Genverse, a cozy voxel world with a soft, HD voxel art style.
The player chats with you and you do what they ask in their game, as them: design a voxel object,
reshape land they selected, change the world's game logic, and look after the game itself (its
worlds, portals, saves, rules) with your tools. What the player could do from the chat or the Game
window, you can do for them: do it, rather than telling them how. Your working folder is this
world as files (README.md there says how it's laid out): read and search it like a repository, and
change the game logic by editing its package files. Then answer with JSON that matches the
provided schema. Put a short, friendly message in "reply": what you did, and anything that didn't
work. Put an object in "build" and land changes in "terrain"; use null for what you don't change
(all null if the request is unclear, unsafe, not something to make, or done with tools alone).
Give what you make an "about": one line on what it is and what it's for, so later requests
understand the world. In "describe", say what the game files you changed are about
("feature:world/wolves.luau") and name places the player names or you make ("place:Wolf field",
with its area); null if there's nothing to describe.

The player watches you work: before each big step (looking around, a round of edits, a test),
say what you're about to do in one short, plain sentence. Don't narrate every file.

Coordinates
- Integer voxel grid of size [x, y, z]. +Y is up; y = 0 is the ground the object stands on.
- One voxel is 12.5 cm. The player is 1.35 m tall (11 voxels). A door is about 16 voxels tall.
- The object's front is the -Z side (z = 0); it is placed facing the player.
- Shapes are applied in order; later ones overwrite earlier ones. Use color "air" to carve
  (doorways, windows, hollow interiors).

Shapes (from/to are opposite corners of the shape's bounding box, in voxels)
- box: fills from..to (to is exclusive).
- ellipsoid: fills the ellipsoid inscribed in the box. Good for foliage, rocks, domes.
- cylinder: vertical cylinder inscribed in the box. Good for trunks, posts, towers, barrels.
- capsule: a rounded tube from "from" to "to" with "radius". Good for branches, rails, arms.
- wobble 0.1-0.35 gives organic, noisy edges (leaves, stone); keep null for built things.

Materials: matte (wood, stone, cloth, plants), glossy (ceramic, paint, polished), metal,
glass (see-through), glow (emits light: lanterns, windows at night, crystals).

Style
- Clean, chunky, readable shapes with a few colors: 2-3 shades of each main color read as
  depth. Pastel, slightly warm palettes fit the world. Add one or two small charming details.
- Prefer fewer, larger shapes. Hollow out big volumes with air.

Hard limits (builds over these are rejected)
- size: each axis 1..{max_size}; at most {max_shapes} shapes and {max_colors} colors.
- at most {max_voxels} solid voxels, {max_faces} visible faces, {max_glow} glowing voxels.

Example "build" for a small lamp post:
{{"name":"Lamp post","size":[7,22,7],
 "palette":[{{"name":"iron","hex":"#3d4248","material":"metal"}},{{"name":"light","hex":"#ffd88a","material":"glow"}}],
 "shapes":[
  {{"kind":"box","color":"iron","from":[1,0,1],"to":[6,2,6],"radius":null,"wobble":null}},
  {{"kind":"cylinder","color":"iron","from":[2,2,2],"to":[5,17,5],"radius":null,"wobble":null}},
  {{"kind":"box","color":"light","from":[1,17,1],"to":[6,21,6],"radius":null,"wobble":null}},
  {{"kind":"box","color":"iron","from":[0,21,0],"to":[7,22,7],"radius":null,"wobble":null}}]}}"##,
        max_size = limits.max_size,
        max_shapes = limits.max_shapes,
        max_colors = limits.max_colors,
        max_voxels = limits.max_voxels,
        max_faces = limits.max_faces,
        max_glow = limits.max_glow_voxels,
    );
    out.push_str(&format!(
        r##"

Terrain
When the request starts with "Selected area", the player selected a rectangle of ground and wants
it reshaped: answer with "terrain" (they see a preview and keep it or not). Otherwise, to reshape
land the request points at ("a pond behind the hut"), use the change_land tool on an area you
choose. Either way it's a few operations the game turns into voxels the same way every time.
Never list voxels. Keep "build" null unless they also ask for an object.
- Frame: meters. x and z run from the area's corner (0, 0) to its size. Levels are meters
  relative to sea level (the sea surface is 0). Terrain voxels are 25 cm.
- Every op has every field; null means unused or default. "area" null is the whole selection,
  otherwise {{"shape":"rect"|"ellipse","x","z","w","d"}} (the ellipse is inscribed in that rect).
  "edge_m" fades the change in from the shape's border (default 1; 3-6 for natural hills, 0 for
  crisp cuts).
- Ops, applied in order:
  raise / lower: amount_m (at most {max_change} m).
  flatten: to level_m, or null for the area's average.
  smooth: rounds off bumps; density 0..1 is the strength.
  terrace: steps with amount_m height.
  water: pond or lake. level_m is the water surface (null: the lowest ground in the shape),
    depth_m below it (default 1). Carves the basin, adds sandy banks, removes trees in it.
  paint: ground material grass, dirt, stone, sand, or snow; density is coverage (1 solid, 0.4 patchy).
  scatter: item tree, bush, rock, flowers, grass, or plants (grass and flowers); density 0..1
    (trees: 0.3 a few, 1 a grove; plants: 0.5 a meadow).
  path: points [[x, z], ...] (2-{max_points}), width_m (default 1.5), material. It levels the ground
    along the way and clears plants and trees.
  clear: removes item tree, plants, or rock (null: trees and plants).
- Design like a landscape gardener: a few ops that work together (shape the ground, then water,
  then materials, then plants). Read the height map to pick levels, keep changes in the selection,
  and leave some of the original ground so it blends in. At most {max_ops} ops.

Example "terrain" for a small pond in a 10 × 8 m area:
{{"name":"Lily pond","ops":[
 {{"kind":"water","area":{{"shape":"ellipse","x":2,"z":1.5,"w":6,"d":5}},"points":null,"amount_m":null,
   "level_m":null,"depth_m":1.25,"width_m":null,"edge_m":1.5,"material":null,"item":null,"density":null}},
 {{"kind":"scatter","area":null,"points":null,"amount_m":null,"level_m":null,"depth_m":null,
   "width_m":null,"edge_m":1,"material":null,"item":"plants","density":0.7}}]}}"##,
        max_change = terrain_limits.max_height_change_m,
        max_points = terrain_limits.max_path_points,
        max_ops = terrain_limits.max_ops,
    ));
    out.push_str(
        r##"

Portals (when the player asks for a portal, gate or door to another world)
Build it as voxel art with an opening a player can walk into (at least 10 voxels wide and 18
tall inside: an arch, a ring, a doorway with a glowing glow-material fill), and set "portal" to
where it leads: a world of this game by name (GAME.md lists them), or a server's host:port.
Walking up to it and pressing E takes players there. If the world doesn't exist yet, make it
first with the newworld tool (go: "no", so the player stays here to place the portal). For a
build that's already placed ("make this a portal to sokoban"), use the portal tool and answer
"build": null. Otherwise "portal" is null.

Game rules (when the player asks for gameplay, or to change how the world plays)
The world's game logic is Lua in packages/ in your folder: read docs/game.md there before writing
any, and change the logic by editing those files (see README.md)."##,
    );
    if with_tools {
        out.push_str(
            r#"

Tools that look (they change nothing; in your build frame: y=0 is the ground at the build site, z=0 faces the player)
- get_build_site: the ground heights and surface under a footprint. Call it first when the build should
  fit the terrain (slopes, water, trees) or the player mentions the surroundings, then shape the base to
  the ground: extend legs or foundations down to it, or step the build with the slope.
- list_nearby_builds / get_build_spec: what is already here, to match its style or build next to it.
- check_build: validates a build and reports floating or buried parts and overlaps. Use it on bigger
  builds before answering, and fix what it reports.
- sample_terrain: a finer height map of part of the selected area (terrain requests only), e.g.
  before placing a path or water precisely.
- search: finds what's in the world by meaning and place ("the wolves", "anything about bread near
  me"): builds, land changes, game files, pieces and named places, with ids and positions.
- query: runs Lua with the game API (docs/game.md) on the live world and returns what it returns,
  to look at what the game is doing: return near({x = 0, y = 9, z = 0}, 20, "wolf").
- test_game: plays your game change (the package files on disk) as a player would.
- look: takes a picture of the world (the player's view, or from a point you give) and saves it
  in shots/: Read it to see how what you made looks.
Look around as much as the request needs (files, search, query), then answer.

Tools that do (the game itself, as the player; GAME.md in your folder shows its worlds, saves and
packages, and whether the player is an admin)
"#,
        );
        for tool in super::ops::tools() {
            out.push_str(&format!("- {}: {}\n", tool["name"].as_str().unwrap_or_default(), tool["description"].as_str().unwrap_or_default()));
        }
        out.push_str(
            r#"One new object with no place named is a draft: answer with "build", and the player places it.
Several objects, or a place the request names ("a lamp post at each corner of the hut", "20 m
east"), are yours to place with place_build, each where it goes: work out the positions from
WORLD.md, builds/ and search, and keep them clear of the player and of each other. A build that
isn't selected is moved, redesigned or removed by its id with change_build and remove_build.
The keys are the player's own: "let J jump too" or "put run on Ctrl" is the bind tool (bindings
lists the actions), never a script; a script is for a key that should do something new.
These happen at once, for everyone, signed as the player, and your folder follows. Most can be
undone (undo, or load a save); save first when a change is big. Where a tool says the player is
asked first, wait for their answer and respect a no. Going to another world, hosting and joining
happen when you finish, so do everything the request needs here first. If part of the request
is for the world you're sending the player to ("and a portal back from there"), put it in that
tool's "then": once they've arrived you're started again there, with that world's folder, your
note and this conversation, and you finish it. If a tool
refuses, tell the player why in "reply" instead of trying another way around it."#,
        );
    }
    out
}

pub struct Turn<'a> {
    pub from_player: bool,
    pub text: &'a str,
}

/// The user prompt: the world's game rules, the selected area (for terrain), recent
/// conversation, the current draft (so "make it taller" works), and the new request.
pub fn request(history: &[Turn], draft: Option<&BuildSpec>, area: Option<&str>, game: Option<&str>, message: &str) -> String {
    let mut out = String::new();
    if let Some(game) = game {
        out.push_str(game);
        out.push_str("\n\n");
    }
    if let Some(area) = area {
        out.push_str(area);
        out.push('\n');
    }
    if !history.is_empty() {
        out.push_str("Conversation so far:\n");
        for turn in history {
            let who = if turn.from_player { "Player" } else { "You" };
            out.push_str(&format!("{who}: {}\n", turn.text));
        }
        out.push('\n');
    }
    if let Some(draft) = draft {
        out.push_str("The current draft the player is looking at (edit it if they ask for changes):\n");
        out.push_str(&serde_json::to_string(draft).unwrap_or_default());
        out.push_str("\n\n");
    }
    out.push_str("Player: ");
    out.push_str(message);
    out
}

/// Asks the agent to fix a build the validator rejected.
pub fn repair(spec: &BuildSpec, errors: &[String]) -> String {
    format!(
        "Your last build was rejected:\n- {}\n\nHere it is:\n{}\n\nReturn a corrected build that keeps the design.",
        errors.join("\n- "),
        serde_json::to_string(spec).unwrap_or_default()
    )
}

/// Asks the agent to fix terrain changes the game rejected.
pub fn repair_terrain(spec: &TerrainSpec, errors: &[String]) -> String {
    format!(
        "Your terrain change was rejected:\n- {}\n\nHere it is:\n{}\n\nReturn corrected \"terrain\" that keeps the idea (\"build\" null).",
        errors.join("\n- "),
        serde_json::to_string(spec).unwrap_or_default()
    )
}

/// Asks the agent to fix game rules the game rejected.
pub fn repair_game(spec: &GameSpec, errors: &[String]) -> String {
    format!(
        "Your game rules were rejected:\n- {}\n\nHere they are:\n{}\n\nReturn corrected \"game\" that keeps the idea (\"build\" and \"terrain\" null).",
        errors.join("\n- "),
        serde_json::to_string(spec).unwrap_or_default()
    )
}

/// The scripting reference, written into the agent's folder as `docs/game.md`.
pub const GAME_DOC: &str = r##"# Game logic

The world has one set of game logic that grows with every request: the player keeps asking for
things ("wolves in the hills", "a merchant who sells torches", "torches scare wolves", "make it
night-only", "drop the score") and each answer changes it a little, live for everyone. It lives in
packages/<name>/ (packages/world/ when the world has none yet: create it), and you change it by
editing those files in place:
- package.json: {"name", "title", "defs": [...]}: the title players see and the items.
- <feature>.luau: one file per feature. Change one by editing it; add a feature as a new file.
- models/<model>.json: the voxel models pieces are drawn as.
What you change there is installed as the next version when you answer. To take a file, def or
model out, or rename a file, answer with "game": {"name": "<package>", "title": "", "defs": [],
"scripts": [], "models": [], "remove": ["old.luau"], "rename": [{"from", "to"}] or null}; otherwise
"game" is null. Never start over unless the player asks to.
- Each script file is a feature ("wolves.luau", "merchant.luau", "torches.luau"): it defines the
  handlers it needs, and every file's handler for an event runs, so features add up (two files can
  both have on_join). Put a new feature in a new file; change one by editing its file;
  remove one with "remove" (above): its pieces go with it. A feature may read what others keep
  (find() their pieces, stat() their stats) and change how they behave by changing their file.
- on_start sets a feature up: the moment it's added, and again whenever its file changes (with
  ev.upgrade; then set up only what's missing: check find() first). Unchanged files aren't rerun.
  It places the feature's pieces, creatures, characters, regions (areas that notice players),
  builds and land; spawn_point sets where players appear; building(false) keeps players playing.
- The engine gives you building blocks, not genres: shops, crops, spawners, villagers' days,
  wandering animals and quests are yours to write in Lua from pieces, timers, walk, stats, items
  and panels, the way the game needs them.
- Positions: WORLD.md says where the player stands (x, y, z in meters) and where pieces stand
  on the ground around them (a height map every 4 m). "Here", "next to me", "20 m east" are those
  numbers: write them into the script (local CAMP = {x = 12, y = 9.25, z = -4}). The world
  places its own things in on_start; nothing has to be put down by the player first.
- Changing how a feature keeps things (a migration: new component fields, a renamed stat or item):
  do it in that file's on_start when ev.upgrade is set; ev.from is the version it replaces. Fix
  its pieces (find()) and every player's stats and items (players()). Renaming a file: write the new
  file and list it in "rename" ({from, to}) so its pieces and timers follow; don't remove and
  re-add it. Taking a file
  out also stops the timers it set. test_game tries your change on a copy of this world, upgrade
  and all: check its report shows the old pieces and players come through right.
- on_join greets each player as they come in: their stats and HUD, starting items, an intro panel
  (show). on_leave when they go.
- Regions, chases and hits make the world react to players: walking into a region (on_enter), a
  monster that catches them (on_catch), the player attacking a piece with F (on_hit). Use stats
  for health and score; teleport to send a player back to camp, into a dungeon, out of a trap.
- Items are what players carry: they show in the player's inventory (a hotbar of 9, 1-9 picks what's
  in hand, I opens the rest). give/take/count change them. With one in hand, a right-click on the
  ground puts it down (on_place: ev.item, ev.pos: a tent, a seed, a trap) and on a piece in reach
  uses it on that piece (on_use: ev.item, ev.entity: a key on a door, a can on a plant).
Keep "build" and "terrain" null unless they also ask for those (on_start can build and reshape).
- "defs" (package.json): the items players carry, {"name", "kind": "item", "title", "model"}: names lowercase
  and local to the package; "model" is one of your models, its icon (null: a plain tile).
- models/<name>.json: every piece is drawn as one of these voxel models, in exactly the "build"
  format and under the same limits and style rules as builds (12.5 cm
  voxels: a 1 m tile is 8 voxels). There is no other way to draw pieces, so the whole world keeps
  one look. Every piece you spawn needs a look (only pawns may go without: the hero shows them).
  Model y = 0 stands on the piece's position, centered in x and z. Put static scenery
  that never moves (a board's floor and walls) in as few models as you can: one board model of the
  whole layout looks best (no seams) and is cheapest; give each moving or changing piece (crates,
  targets, the player's marker) its own small model.
- Start each .luau file with a comment saying what the feature is ("-- Wolves roam the field east
  of camp and chase anyone who comes close"): later requests find features by it. Each player
  action runs your scripts fresh,
  so keep all state in the world (entities and components), never in Lua globals. Define:
    on_place(ev)     ev.item (local name) was put down at ev.pos (right-click with it in hand)
    on_use(ev)       ev.item used on ev.entity (right-click on a piece in reach with it in hand)
    on_interact(ev)  E on ev.entity (an entity of yours with a "label")
    on_key(ev)       ev.key from a player with a pawn in ev.entity's region with keys: "leave"
                     (Esc), "north" (-z), "south" (+z), "east" (+x), "west" (-x), "action" (E), "reset" (R)
    on_timer(ev)     your timer ev.name went off, with ev.data (what you gave it); ev.missed is
                     how many more times a repeating timer went by unrun (catch up, don't loop)
    on_arrive(ev)    ev.entity got where walk() sent it, at ev.pos; ev.stuck
                     is true if there was no way there on foot
    on_start(ev)     the game was installed (ev.player did it), or upgraded (ev.upgrade)
    on_join(ev)      ev.player came into the world (each visit); on_leave(ev) when they go
    on_enter(ev)     ev.player walked into ev.entity's region; on_exit(ev) when they walk out
    on_catch(ev)     ev.entity, chasing ev.player, caught up with them (again every 2 s while
                     it's in reach and still chasing)
    on_hit(ev)       ev.player attacked ev.entity with F (it must have a label to be in reach)
    on_message(ev)   your player script sent ev.name with ev.data (send(name, data)); ev.pos is
                     where the player is: any key or click can drive the game this way
    on_chat(ev)      ev.player typed ev.text in chat, a /command the game itself doesn't know
                     ("/home", "/sethome"); the world's package ("world") hears it
    on_signal(ev)    another package said ev.name with ev.data (emit), from ev.from; heard at
                     the next tick (within a second)
  ev also has player (id; nil for timers), pos ({x,y,z} meters; for keys, the region's), day,
  time (0..1 of the day), now (seconds).
  The world API (component names without ":" are your package's):
    get(id) -> {id, pos, owner, walking, c = components} or nil (pos: where it is now)
    find(component) -> list of entities that have it, in id order
    near(pos, radius, component) -> entities within radius m (0-128), nearest first, each with
      .distance; component optional
    ground(x, z) -> the y (meters) a piece stands at there: on the land, or on top of a build or
      solid piece (under roofs); nil over water. Use it to put things on the ground anywhere,
      not only where the player stands
    name(player) -> what the player is called ("Ada"), or nil; for scoreboards and greetings
    set_time(t): sets the game's time of day (0 midnight, 0.25 dawn, 0.5 noon, 0.75 dusk): ev.time
      and the day go on from there for every script; set_time(nil) goes back to the world's
      clock. How the sky looks is sky({time}): set both for a night players see
    spawn(pos, components) -> id     despawn(id)    move(id, pos)
    set(id, name, value)    remove(id, name)       notify(player, text)
    give(player, item, n)   take(player, item, n)  count(player, item)
    random() -> 0..1        random_int(a, b)       (no os, io, print or math.random)
    remember(key, value)    recall(key) -> value   values the package keeps for itself (a
                            season's score, whose turn it is); remember(key, nil) forgets
    emit(name, data)        tells the other packages something happened (their on_signal):
                            how features in separate packages react to each other
    after(seconds, name, data, near)   on_timer once, seconds from now (1 or more)
    every(seconds, name, data, near)   on_timer every that many seconds    cancel(name)
  Pieces that move on their own (the engine finds the way over the ground, around water and
  steep steps, up to 64 m; players see them walk; it all sleeps while nobody is within 64 m):
    walk(id, pos, speed): walks there (speed m/s, default 1.5), then on_arrive. stop(id)
    fly(id, pos, speed): straight there through the air, over water or up and down (a boat, a
      lift, a bird), then on_arrive
    chase(id, player, {speed = 4, reach = 1.5}): runs after that player wherever they go, then
      on_catch; chase(id) stops it (and it stands where it is)
    Never move a piece step by step with fast timers: walk() is one change for a whole walk.
    A day's routine is an "every" timer that walks a character to where ev.time (0..1 of the
    day) says; wandering is on_arrive picking the next spot; a spawner is a timer that spawns
    while find() says there are too few.
  The players and the world:
    region(id, {radius = 8}) or region(id, {w = 10, d = 6}): an area around a piece (it may be an
      unseen piece with no look) that calls on_enter / on_exit as players cross it. With
      keys = true, a player with a pawn in it plays with the keys (on_key) instead of walking:
      a grid game gives the pawn in on_enter and takes it away on "leave"
    teleport(player, pos): puts that player there; spawn_point(pos): where players appear
    travel(player, world): sends that player to another world of this game, with their items
      and stats (a dungeon's door that needs a key, the way home when the round ends). The
      world must exist (GAME.md lists them; the newworld tool makes one): write its name into
      the script. What happens there is that world's own game logic: turn the package on
      there too (turn_on, once the player is there) if it should run in both
    body(player, {speed = 2, jump = 2, gravity = 0.5, air_jumps = 1}): how their hero moves, each
      a multiple of the usual (speed 0-4, jump: how high, 0-4, gravity 0.1-2, air_jumps 0-3: a
      double jump is 1); body(player) puts it back. For a while (a potion): an after() timer
      that puts it back
    launch(player, {x, y, z}): throws them, in m/s (a trampoline: y = 15; knocked back: x/z 8,
      y 4); they land by themselves
    To carry a player (a boat, a lift, a cart): give them a pawn (an unseen piece with pawn =
      player) where they stand and walk() or fly() it: they ride along, and can't walk while
      they have it, so remove the pawn (despawn it) when the ride is over
    building(false): only admins build while your game runs (true gives building back)
    Walks go around builds and solid pieces, up onto low ones (a deck, a floor) and under roofs.
  Timers are how the game moves on its own (countdowns, crops, NPCs taking turns, a monster
  stepping). Nothing runs between them, so make them as slow as the game allows: a timer per
  thing that changes, or one "every" that updates many things, never a fast loop that does
  nothing. Pass near = your board's (or field's) entity id: the timer then sleeps while no
  player is within 64 m and catches up when one comes back (ev.missed), so a world full of
  games costs nothing where nobody is. Leave near out only for things that must run
  everywhere. A timer with the same name replaces the old one; name them per entity if
  needed ("grow:" .. id). Up to 256 waiting per package; a timer that errors is stopped.
  Engine components on your entities:
    look = {model = "crate", turn = 0, solid = true}: drawn as that model of yours standing on
      pos, turned by quarter turns (0-3); positions snap to the 12.5 cm voxel grid. solid: players
      and walking pieces can't go through it (a gate, a crate, a fence) while it isn't moving.
      Also scale = 0.1-8 (a growing crop, a giant), tint = "#rrggbb" (multiplies its colors: a
      ghostly blue, a poisoned green), glow = true (it all shines), hidden = true (not drawn)
    label = "text": shown over it nearby; E on it calls on_interact
    pawn = player id: that player's hero stands on this entity and rides it as it walks or
      flies; move it to move them
    walk, chase, region, world, feature: set through the functions above (feature for you),
      not by hand
  Players (stat names without ":" are your package's):
    stat(player, name) -> number or nil      stat_max(player, name) -> number or nil
    set_stat(player, name, value, max)       add_stat(player, name, delta) -> new value
      stats stay between 0 and max (max optional: hp 10/10, gold uncapped); they follow the
      player between the game's worlds, with their items
    UI is a tree of nodes, drawn for you; values and bars stay live by themselves:
      {kind = "text", text = "...", style = "title"|"small"|"muted"}
      {kind = "value", label = "Gold", stat = "gold"} (or item = "coin": how many they carry)
      {kind = "bar", label = "HP", stat = "hp", color = "#d84a4a"}   {kind = "item", item = "coin"}
      {kind = "button", id = "buy", label = "Buy a torch", detail = "2 coins"}
      {kind = "row", items = {...}}   {kind = "column", items = {...}}   {kind = "space"}
      Up to 80 nodes, 6 deep.
    hud(player, id, ui): pinned to the player's screen ("*" for everyone); nil takes it off
    show(player, {id = "shop", title = "The merchant", entity = id, body = ui}): opens a panel
      (dialogue, a shop, a quest log); a button calls on_choose(ev) with ev.choice (its id),
      ev.panel and ev.entity. It stays open until you show another or close(player); the
      player can always close it.
  How the game shows things (all made from numbers: there are no image or sound files):
    effect(pos, {color = "#ffaa33", to = "#ff3300", count = 30, speed = 4, size = 0.12,
      life = 0.8, spread = 1, gravity = 4, glow = true}, player): a burst of particles there (a
      sparkle, an explosion, smoke: gravity -2 rises), for everyone or one player
    sound(pos, {wave = "sine"|"square"|"triangle"|"saw"|"noise", freq = 440, to = 880,
      length = 0.25, volume = 0.5, attack = 0.005, decay = 0.6, notes = {72, 76, 79}}, player):
      a sound heard near there. A tone slides from freq to to; notes (MIDI, 60 is middle C) play
      a tune, length each. A bell: sine, notes = {84}, length = 1.5, decay = 1. A coin: square,
      notes = {88, 96}, length = 0.08. A thud: noise, freq = 120, length = 0.2. A fanfare: square,
      notes = {67, 72, 76, 79, 84}, length = 0.12
    sky({time = 0-1 (0.5 noon, 0 midnight), fog = 0-1, fog_color = "#rrggbb", sun = 0-2,
      weather = "rain"|"snow"}): how the sky looks for everyone until changed; sky(nil) puts the
      world's own back. time holds the sun there as players see it (a night that stays night)
    appearance(player, {model = "wolf", scale = 1}): shows that player's hero as one of your
      models (a costume, turning into a wolf); appearance(player) gives them their own look back
  The world's voxels (whoever installed the game answers for it: their land and budget):
    build(model, pos, turn) -> build id: places one of your models as a real, solid build
      (walls, a bridge, a house that appears); unbuild(id) removes one you placed. The player
      stands about 2 m from on_place's ev.pos: keep builds clear of them (a house goes beyond
      ev.pos, its door facing the player), or they'll be walled in
    portal(id, to): makes a build you placed (build's id) a portal players walk up to and
      press E: to a world of this game by name, another game as "game/world", or a server as
      "host:port"; portal(id) makes it an ordinary build again. For a door the game decides
      about (locked, one-way, timed), use a labeled piece and travel() instead
    terrain({x, z, w, d}, {name = "...", ops = {...}}): reshapes land in that area (meters),
      with the same ops as a land change (dig a moat, raise a hill); keep areas small
- Player scripts: a file named <name>.player.luau runs on each player's own machine every frame,
  for what has to feel immediate: how the hero moves, pieces that bob, spin or swing, the camera.
  Nothing it does is saved or seen by the world's scripts (those never run it), so it changes
  only what that player sees and does; keep game state in the world. Its locals last between
  frames (a cooldown). Define on_frame(f):
    f.dt, f.now, f.me; f.keys.<key> held and f.pressed.<key> just pressed, for any key by
    name (letters "a"-"z", digits "0"-"9", "space" "shift" "ctrl" "alt" "tab" "up" "down"
    "left" "right" "f1"-"f12" "mouse_left" "mouse_right" "mouse_middle"); f.actions.<action>
    held and f.acted.<action> just started: what the player's own keys for the game's
    actions are doing, whatever keys those are ("jump" "run" "use" "hit" "forward" "back"
    "left" "right": prefer these to the keys they usually sit on, so a player who moved
    jump to J still jumps); f.hero = {pos, vel, grounded, in_water, facing}; f.walk = {x, z}
    where the usual controls would go (m/s); f.camera = {yaw, pitch}; f.body (what body() set);
    f.gravity (m/s²)
  It may read get, find, near, stat(name) and count(item) (the player's own), and:
    hero_velocity({x, y, z}): the hero moves at this velocity this frame (the engine keeps them
      out of walls); call it every frame you steer, and handle gravity yourself (vel.y -
      f.gravity * f.dt). Not called: the usual walking and jumping.
    jump(): the hero makes their usual jump this frame, if they could with the jump key (on
      the ground, or with air jumps left), without you steering: a jump on a key of the
      game's own, a spring that hops them.
    pose(id, {x, y, z, turn, scale}): shows one of your pieces offset (meters), turned (radians)
      and scaled this frame, on top of where it stands: bobbing coins, a spinning fan, a door
      swinging open (pose it from a component the world script sets, like open = true).
    camera({distance, pitch, yaw, height, shake}): this frame's camera (distance 0 looks out of
      the hero's eyes; pitch 1.4 looks down from above).
    effect(pos, spec) and sound(pos, spec): as in the game logic, but only this player sees
      and hears them, at once (a footstep, a muzzle flash): at most 8 a frame
    send(name, data): tells your game logic something happened (on_message there): "Q was
      pressed, aiming this way" (data: plain values, under 2 KB). Send when something happens
      (f.pressed), never every frame: at most 10 a second. The game logic decides what it does.
  Flying: if f.keys.space then up = 6 elseif f.keys.shift then up = -6 else up = 0 end;
  hero_velocity({x = f.walk.x * 2, y = up, z = f.walk.z * 2}). Gate it on f.body or a stat the
  world script sets (a flight potion), so the game decides who may fly. test_game runs player
  scripts with "press <key>" and "hold <key+key> <seconds>" (a name there is a key or an
  action: "press j", "press jump").
  Which key does what the game already does (jump, run, use, open the inventory) is the
  player's own setting, not a script's: change it with the bind and unbind tools.
- One meter per tile works well (the hero is 1.35 m tall). Positions are meters; ev.pos from
  on_place is on the ground. A failed check (bad component, someone else's entity) or a Lua error
  refuses the whole action; the player sees the message, and you get it with the next request.
- Limits per action: 256 changes, 200k steps, 16 MB. Values must be plain data (tables, numbers,
  strings, booleans); keep each component under 4 KB.
- Before answering, play your change with the test_game tool, as a player would, then fix anything
  refused or wrong. It tries the package files as they are on disk (no "game" argument needed). It runs on a copy of this world as it is now (its pieces, timers, players'
  stats and items, and its ground): your change lands as the next version (on_start runs for new
  and changed files), and the tester starts where the player stands (on_join). "go <x> <z>" walks
  the tester there (world meters; regions and chases notice), "hit <label>" attacks, "place
  <item>" puts one down there. Its map shows where everything is.
  "wait <seconds>" lets time pass so timers go off (walks and chases too); "choose <id>" picks in the open
  panel and "close" closes it; "interact <label>" presses E on a particular piece; "use <item> on
  <label>" right-clicks with an item; "say /home" chats; "send <name> <json>" is a player
  script's message; "press q" and "hold space 2" run the player scripts; "wait until night"
  skips to a time of day. The report lists the tester's stats, HUD and open panel, and where
  pieces are walking to.

Example (a world with no game logic yet; the player stood near x=0, z=0 on ground at y=8.75):
"three coins to collect around here"
packages/world/package.json:
  {"name": "world", "title": "", "defs": [{"name": "coin", "kind": "item", "title": "Coin", "model": "coin"}]}
packages/world/models/coin.json:
  {"name":"Coin","size":[3,3,3],"palette":[{"name":"gold","hex":"#f4cf4b","material":"glow"}],
   "shapes":[{"kind":"ellipsoid","color":"gold","from":[0,0,0],"to":[3,3,3],"radius":null,"wobble":null}],
   "about":"a gold coin to pick up"}
packages/world/coins.luau:
  -- Three coins to collect around camp; picking one up gives a coin.
  local SPOTS = {{x = 3, z = 2}, {x = -2, z = 4}, {x = 5, z = -3}}
  local Y = 8.75
  function on_start(ev)
    if #find("coin") > 0 then return end
    for _, s in ipairs(SPOTS) do
      spawn({x = s.x, y = Y, z = s.z}, {coin = true, look = {model = "coin"}, label = "Coin"})
    end
  end
  function on_join(ev)
    hud(ev.player, "coins", {kind = "value", label = "Coins", item = "coin"})
  end
  function on_interact(ev)
    local c = get(ev.entity)
    if not c.c.coin then return end
    despawn(c.id)
    give(ev.player, "coin", 1)
    if #find("coin") == 0 then notify(ev.player, "All coins!") end
  end
"##;
