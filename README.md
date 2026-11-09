# Cubewish

A small HD voxel game on Bevy 0.20 ECS: an endless, streamed voxel world and a chibi voxel
hero you walk around in third person. Voxels are small (25 cm terrain, 5 cm characters) and
lit like a modern diorama render: baked per-vertex AO, per-voxel color jitter, PBR surfaces,
soft cascaded shadows, SSAO, TAA, bloom, distance fog, and a physical sky.

![preview](docs/preview.png)

```sh
bevy run            # or: cargo run
bevy run web --open # in the browser (WebGPU)
```

Controls: `WASD`/arrows move, `Shift` runs, `Space` jumps, drag to look, scroll to zoom.

## Reshaping the land (desktop)

Cubewish is AI-native: you don't stack 25 cm voxels by hand, you say what the land should
become. Press `G`, drag across the ground to select an area (a click selects a 6 m square, up
to 48 m a side), then press `T` and describe it: "a pond with sandy banks", "a winding stone
path to the hill", "terraced hillside with a few trees".

The agent never returns voxels. It answers with a few terrain operations in the area's frame
(meters from its corner, levels relative to sea level): `raise`, `lower`, `flatten`, `smooth`,
`terrace`, `water`, `paint`, `scatter` (trees, bushes, rocks, flowers, grass), `path`, and
`clear`, each with an optional rectangle or ellipse and a soft edge. `src/world/sculpt.rs`
checks them against limits and turns them into voxels the same way every time.

The result is a live preview in the world: `Enter` keeps it, `Esc` puts the ground back
exactly, and further messages revise it. Kept changes join the undo history with builds and
go into the world's change log as operations, never voxels (see below). The area stays
selected for follow-ups; `Esc` clears it. Agents also get a `sample_terrain` tool for a finer
height map of part of the area.

Script one: `CUBEWISH_AREA=14 CUBEWISH_PROMPT="a small pond" cargo run` selects a 14 m square
in front of the hero and sends the message (add `CUBEWISH_KEEP=1` to keep the preview).

There's no hand editing of single voxels: land only changes through these described edits.
The edits are a sparse layer on top of the world seed (`src/world/mod.rs`): chunks apply them
whenever they generate, and changed chunks (plus neighbors sharing the border) remesh in the
background. Agents' `get_build_site` sees the reshaped terrain too.

## Games (desktop)

Cubewish has a small game engine that admins program by talking to their agent. Start an
empty world and ask for a game:

```sh
CUBEWISH_WORLD=sokoban cargo run   # a world of your game, flat and empty if it's new
```

Then press `T`: "make a basic Sokoban game". The agent answers with a *package*: definitions,
voxel models and Luau scripts. Before answering, it plays the game itself in an empty test
world with the `test_game` tool (steps like `place board`, `go 40 40`, `east`), which reports what
each step did plus a map of the pieces, and fixes what fails. The game then checks the package
and installs it for the whole world, live (`crates/rules/src/logic`, design in
`docs/game-logic.md`). Only admins change a world's games; everyone plays them.

- **Timers, not ticks:** games move on their own through timers (`after`, `every`,
  `on_timer`). Nothing runs between them, and the log keeps only what a timer changed. The
  server runs them; offline, your game does. A repeating timer that missed runs while nobody
  was there goes off once, with `ev.missed`.

- **One style:** game pieces can only be drawn as the package's voxel models, written in the
  same format, compiled by the same code and held to the same limits as builds. They're drawn
  the way builds are, and every piece stands on the 12.5 cm build voxel grid.

- **Inventory:** what you carry fills a Minecraft-like hotbar (1-9 picks what's in hand, I opens
  the rest). Right-click the ground puts what's in hand down (the game's `on_place`); on a piece
  in reach, uses it there (`on_use`).
- **Play:** walk onto a board. A game takes over your keys inside its *region with keys*. WASD or the arrow
  keys move you a tile (W is away from the camera), `R` resets, `E` is the game's action, and
  `Esc` leaves. Your hero stands on the *pawn* the game moves. Labeled pieces show their name
  nearby, and `E` uses the one in reach.
- **Change it live:** ask for changes ("make the level bigger", "count my moves"). The agent
  sees the world's games and the script errors players hit lately, so "fix it" is enough. A new
  version replaces the running one for everyone, and what's already placed keeps going. A
  version that doesn't pass the checks is never installed.
- **The engine:** scripts answer players' *intents* (put this here, use that, a key on a
  board). Each action runs them fresh in a sandboxed Luau VM: no files, OS or clock, a seeded
  random number generator, and limits on steps, memory and changes. Through a small API
  (`get`, `find`, `spawn`, `move`, `set`, `notify`, ...) they change only their own entities
  and items, and every change is checked as it happens. The engine draws entities from their
  `look` (one of the package's models), names them with `label`, and acts on `region` and
  `pawn`. Builds, and pieces whose look is `solid`, block the hero and walking pieces (walks go
  around them, onto low ones, under roofs). HUD and panels are UI trees the scripts build (text, values, bars, items,
  buttons, rows and columns), drawn by one renderer.
- **One world, many requests:** the world's game logic grows with each request ("wolves in
  the hills", "a merchant", "torches scare wolves"): the agent sends only what changes, each
  script file is a feature whose handlers all run, and taking a file out takes its pieces.
- **The world is the game:** installing a game runs its `on_start`, which sets the world up
  (pieces, monsters, regions, the spawn point) and can stop players building. Games see
  players come and go (`on_join`, `on_leave`) and walk into regions (`on_enter`), chase them
  (`on_catch`), teleport them, and answer F (`on_hit`), with health and score as stats.
  They change how heroes move (`body`: speed, jump, gravity, double jumps), throw them
  (`launch`), and carry them on a pawn that walks or flies (`fly`: boats, lifts).
- **Pieces that move on their own:** `walk` and `chase`. The engine finds the way over the
  ground and records the whole walk as one change; games show the piece walking along it.
  Routines, wandering and spawners are a few lines of Lua on top. Like timers, they sleep
  while nobody is near.
- **Building blocks, not genres:** the engine has no shops, crops or spawners of its own;
  the agent writes them in Lua for the game at hand.
- **The log is still the truth:** installs and every move are changes in the log, so games
  replay like the voxels do, across restarts, undo and servers. The server runs the same
  scripts, and a package travels with the change that installs it.
- `/packages` lists the world's games; admins can `/uninstall <name>` one (its pieces stay,
  asleep).
- A dedicated server's first world is flat with `--flat` (`CUBEWISH_SERVER=host cargo run`
  joins it; the ground follows whichever world you're in).
- Script a run: `CUBEWISH_WORLD=s CUBEWISH_PROMPT="wolves 20 m east of me" CUBEWISH_PLAY="go 15 0;
  wait 4; hit" cargo run`. It waits for the agent, then plays the steps.

## An open world, kept as a log

Cubewish is heading for shared, open worlds: anyone can build and reshape land with their own
agent, nothing is lost, and every change says who made it. Everything below works the same in
single player and on a server.

The world is the seed plus an ordered log of changes (`saves/world.jsonl`, append-only): builds placed,
moved, revised and removed, terrain changes, rule changes, undos, saves and loads. Each change records who
made it, which agent did if one did, a hash of the request, and when. A selected build shows
"made by you with Claude Code, 2 h ago".

- **Players are keys.** `saves/profile.json` holds your name and an ed25519 secret key; your
  player id is the public key, and every change is signed with it, so any server can check who
  made a change without accounts. `/verify` checks every signature. Keep that file private.
- **The log only moves forward.** Each change applies on top of the world as it is; terrain
  changes compile against the ground the earlier changes left. `snapshot.json` holds the whole
  world as of some change, so loading applies only the changes after it, never the whole log.
- **Undo is a change too:** it puts back what a change replaced, on the world as it is now, and
  is refused if something has changed there since (undo the later change first). You undo your
  own changes (admins: anyone's); undoing an undo redoes it. The latest 100 changes can be undone.
  Nothing is replayed to undo, and nothing leaves the history.
- **Saves are swapped in, not replayed.** A save keeps the world's state as of it
  (`states/<change>.json`); loading it (admins) is a change that carries that state. The world
  is saved first ("before loading …"), so loading that brings it back.
- **Each world sets its own rules** (`cubewish_rules::settings`), kept in the log so replay
  always applies the rules of the time: land mode, spawn, limits on one build or land change,
  budgets, and rate limits.
  - **Land:** open by default, so anyone changes anything outside the protected spawn (24 m
    around the start). A world can switch to **parcels** (`/land parcels`): 8 × 8 m plots that
    changes claim for their author, which only owners change. `P` shows the land around you.
  - **Budgets:** per parcel 12 builds, 40k visible build faces, 150k build voxels and 4k glowing
    voxels; per player 60 builds, 1.5M build voxels and 2M voxels of terrain changes.
  - **Rate limits:** off unless a world sets them (`/rate 10 200000`: 10 changes a minute, 200k
    voxels per 10 minutes). Admins are exempt.
- Every change is checked against the rules as of it (land, budgets, rates), so a log can't hold
  anything its authors weren't allowed to do.
- Saves from before the log, and profiles from before keys, are upgraded once (old files kept as
  `.bak`). Logs from before undo moved forward lose their old undos once, with what they undid,
  so the world stays the same.

**History** (`H`, or the clock button in the panel): every change, newest first, with who
made it, which agent, and when; filter by player. Undo your own changes from there (Redo on an
undo); admins can also **Load** a save.

Chat commands do the same and more: `/history [n]`, `/undo <id>`, `/rules`,
`/verify`, `/host`, `/join <host[:port]>`, `/play`, `/leave`, and for admins `/load <save>`, `/land open|parcels`,
`/rate <changes per minute> [voxels per 10 minutes]` or `/rate off`. Try them from someone else's
side with `CUBEWISH_AS=Bo cargo run`, a non-admin guest for one run; script them with
`CUBEWISH_PROMPT="/rate 5; /history"`.

**Saves** work like a strategy game's, on a world everyone shares: `/save <name>` bookmarks
the world as it is (anyone can), `/saves` lists them, and an admin's `/load <name>` puts the
world back exactly as it was at that save, for everyone, game state included. Nothing is
deleted: loading a later save, or reverting the load, brings the later changes back.
`/fork <save> <world>` starts a new world of your own from a save, leaving this one as it is.

**Games and worlds** (`O`, or the globe button): a *game* is what you host or join; it holds
*worlds*, each with its own ground and its own log. Your worlds live in the game `local`
(`saves/games/local/`; the first run moves the old ones there, Home becoming `home`). The Game
window lists the game's worlds (Enter; admins make new ones, flat or natural, and remove them
to `saves/trash/`), its saves (save every world at once, or this world; load; fork into a new
world), this world's portals, and your other games and servers (new game, join, host). Hosting
shares every world of the game; each player is in one world at a time and sees only the people
and changes there. Ask the agent for **a portal** ("a stone arch, a portal to sokoban") and it
builds one linked to that world; walk up to it and press E. Portals lead to a world of this game
(`sokoban`, works for everyone in it), another of your games (`game/world`), or a server
(`host:port`). What you carry goes with you between a game's worlds. Game rules belong to the
game: install a game once and turn it on in the worlds that play it (the Rules tab); a new
version reaches every world that runs it.

Games give players **stats** (HP, gold) shown on a **HUD**, open **panels** (dialogue, menus,
shops), and change the world itself: a script can place its models as
real builds and reshape land, within the budget of whoever installed it. How to test all of it:
`docs/testing-games.md`. Chat: `/worlds`, `/world`, `/newworld`, `/games`, `/game`, `/newgame`,
`/savegame`, `/loadgame`, `/portal`. Design: `docs/worlds.md`.

## Playing together

Two ways to host:

- **From the game:** **Host** in the panel (or `/host`) opens your world to others on port 7878.
  The server runs inside your game on your world's own save, so what you build together stays
  in your world. Others join with your address; it closes when you quit.
- **A dedicated server**, for a world that's always up:

  ```sh
  cargo run --release -p cubewish_server -- --port 7878 --data server-data
  ```

  It's a small program with no graphics (only the rules and protocol crates), so it runs on any
  machine with a terminal. Its whole game is `server-data/`: `game.json` and each world's
  `worlds/<name>/world.jsonl`; back those up (`snapshot.json` files only speed up loading). A
  `server-data/world.jsonl` from before games becomes the world `main`.

**On the same network, nobody types an address.** A hosting game announces itself to the
local network (a small UDP broadcast on port 7877, once a second: world name, host name, port,
player count), and every game lists the worlds it hears under the panel's address field, each
with **Join**. **Play together** (or `/play`) listens for a couple of seconds, then joins the
world it found, or hosts yours if there was none, so the first person to press it hosts and
everyone after joins. Guest Wi-Fi that isolates devices, or VPN and proxy apps, can block the
announcements; typing the address still works then. The first time you host, allow incoming
connections if macOS asks.

To join by address, type `host` or `host:port` next to **Join** in the panel, or use `/join host`, or
start with `CUBEWISH_SERVER=host cargo run`. **Leave** (`/leave`) goes back to your local world.
If the connection drops, the game reconnects by itself and works out which of your changes
in flight reached the server. A game and server with different world rules refuse to play
together instead of drifting apart.

Messages are signed but not yet encrypted, so for now host on a local network or a private
one (Tailscale, WireGuard), not the open internet.

The server holds the world's change log and is the only one who orders changes, like Figma's
multiplayer: players propose changes, it checks each against the world's rules (signature,
land, limits, budgets, rate limits), appends the ones it accepts, and sends them to everyone;
later changes win. A land change is compiled against the ground as it is when it arrives, so
if someone changed the ground under your preview, you get the server's version and a note.
Refused changes are taken back on your screen with the server's reason. The first player to
join a new world is its admin.

- Agents run on each player's own machine; the server only accepts finished, signed specs and
  never sees the request text (only its hash).
- The server is plain Rust with no game engine (`crates/server`); the protocol is JSON, one
  message per line, over TCP (`crates/net`). Login is a signed challenge.
- Online, your local world and saves aren't touched.
- **You see each other**, like Figma's live cursors: other players appear as heroes with a
  name tag in their own color, a ring at their feet, their selected area or build outlined,
  a ghost of the build they're drafting, a pulsing outline while they preview a land change,
  and "Claude Code is designing…" while their agent works. Presence is sent when it changes
  (at most ten times a second) and never stored.
- Rest the cursor on a build or on reshaped ground to see who changed it last (offline too).
- **Safety nets:** every 50 changes the server saves a snapshot of the whole world, so
  restarts and joins only apply what came after. Each land change carries fingerprints of the
  chunks it touched; if your chunks disagree (floating point can differ across machines), the
  game fetches the server's and regenerates just those.
- Not yet: encryption (TLS).

The web build is on hold while multiplayer is built (it may return as watch-only). It still
compiles but isn't tested.

Web builds use WebGPU, which the physical sky needs, so they need a WebGPU browser (recent
Chrome, Edge, Safari or Firefox). On the web, SSAO and model hot reload are off, and chunks
generate on a single thread, so the world fills in more slowly; `bevy run --release web`
helps a lot.

## Builder chat (desktop)

Press `Tab` for the builder panel, pick **Claude Code** or **Codex CLI**, and describe
something to build. The agent answers with a small JSON build spec; the game validates it,
turns it into voxels, and shows it in front of you as a draft. Placed builds are solid (you
can bump into and stand on them) and recorded in the world's change log.

- Drafts stick to the surface under your cursor, Minecraft-style: on top of what you point
  at, or against its side. Click to place, right-click or `Esc` to cancel, `Q`/`E` to turn,
  `R`/`F` to raise and lower. A red outline means it can't go there: it would overlap you, or
  the land or its budget doesn't allow it (the panel says which).
- Click a placed build to select it (cyan outline); a build you just placed stays selected.
  While one is selected, chat messages revise *that* build in place (Place keeps the
  change, Cancel edit restores it). A hint bar at the bottom shows the shortcuts that apply:

  | Context | Shortcuts |
  | --- | --- |
  | Anywhere | `T`/`Enter` talk to the agent · `G` select ground to reshape · `P` show parcels · click a build to select · middle-click to copy it · `Ctrl/Cmd+Z` undo · `Ctrl/Cmd+Shift+Z` redo |
  | Selecting ground | drag across the ground · right-drag look · `G`/`Esc` stop |
  | Area selected | `T` describe the change · `G` select again · `Esc` clear |
  | Land preview | `Enter` keep · `T` ask for changes · `Esc` put the ground back |
  | Selected build | `M` or double-click move · `Delete`/`Backspace` delete · `T` change it · `Esc`/right-click deselect |
  | Draft following the cursor | click place · `Q`/`E` turn · `R`/`F` raise/lower · `Esc`/right-click cancel |
  | Pinned revision | `Enter` keep · `M` pick it up · `T` ask for more · `Esc` cancel |

  Undo steps back through your placing, deleting, moving, and revising builds, kept land changes,
  portals and installs, newest first; one that something has changed on since says so and stays.

- Needs `claude` and/or `codex` on your `PATH`, already logged in.
- The agents get no other tools: no shell, no file edits, no other MCP servers, and no user config
  (`--tools ""` / `--sandbox read-only --ignore-user-config`). They run in an empty temp
  directory and can only return JSON matching `crates/rules/src/build_spec.schema.json`.
- Agents can look at the world through read-only tools the game serves over MCP
  (`src/agent/mcp.rs`, `src/agent/tools.rs`): `get_build_site` (ground heights and surfaces
  under the footprint), `list_nearby_builds`, `get_build_spec`, and `check_build` (limits plus
  floating/buried/overlap report). The server listens on `127.0.0.1` on a random port with a
  per-run bearer token, and the CLIs get only this server (Codex auto-approves just its tools).
- Every build passes the same limits a server would enforce (`crates/rules/src/build.rs`):
  size ≤ 64³ voxels at 12.5 cm, ≤ 200 shapes, ≤ 24 colors, ≤ 120k voxels, ≤ 60k visible
  faces, ≤ 3k glowing voxels. A rejected build is sent back to the agent once for a fix.
- Script a build: `CUBEWISH_PROMPT="a red mailbox" CUBEWISH_AGENT=codex cargo run`
  (add `CUBEWISH_SELECT_LAST=1` to revise the most recent build instead).

## How it fits together

| Piece | Where | Role |
| --- | --- | --- |
| World rules | `crates/rules/` (`cubewish_rules`) | Everything that decides what the world is, with no rendering, so a server can share it: voxel models, the terrain generator (`terrain.rs`), terrain changes (`sculpt.rs`), build specs (`build.rs`), the change log and replay (`log.rs`), world rules (`settings.rs`), land (`land.rs`), budgets (`budget.rs`), rate limits (`rate.rs`), player keys (`identity.rs`) |
| World streaming | `src/world/mod.rs` | Generates and meshes 32³ chunks around each `ChunkLoader` on the async pool; answers voxel queries for collision |
| Change log | `src/world/ledger.rs` | Loads, migrates, records and saves the log; the player profile; live land and budgets |
| Protocol | `crates/net` | Client/server messages, the game's background connection, and finding games on the local network (`lan.rs`) |
| Server | `crates/server` | Headless server: holds the log, checks, orders and shares changes |
| Hero | `src/player.rs` | Builds the character from model parts, kinematic movement with voxel collision and step-up, walk animation, follow camera |
| Voxel rendering | `src/voxel/` | Materials, mesher with baked AO, `.vox.ron` asset loader, picking |
| Scene | `src/scene.rs` | Sky, sun, fog, post-processing |
| Builder | `src/agent/` | egui chat, CLI runner, drafts and placed builds, area selection, land previews and the land overlay (`landscape.rs`), chat commands (`commands.rs`), applying what the server sends (`online.rs`), other players (`presence.rs`), the history panel (`history.rs`), worlds on the local network (`lan.rs`) (desktop only) |

## Voxel models (`assets/**/*.vox.ron`)

Characters and props are data. A model declares `surfaces` (roughness, metallic, emissive,
alpha), a `palette` of named colors that reference a surface, and an ordered list of `ops`:

- `Shape(geom, fill, noise?, mask?)` with `Box`, `Ellipsoid`, `Capsule` or `Cylinder` geometry.
  `noise` distorts the boundary; `mask` only overwrites the listed materials (`"air"` too).
- `Island(...)`, `Scatter(...)`, `Settle(...)`, `Spill(...)` for terrain-like generation.

`parts` cut a model into named pieces that rotate around a `joint` and can hang off a
`parent`; that is how the hero (`assets/characters/hero.vox.ron`) gets animatable limbs.
On native, saving a model while the game runs reloads it live.

`assets/models/sakura_island.vox.ron` is the earlier showcase model, kept as a format example;
spawn it with `VoxelModelSource(asset_server.load("models/sakura_island.vox.ron"))`.

To render a still: `CUBEWISH_SHOT=shot.png CUBEWISH_YAW=0.2 CUBEWISH_PITCH=0.4 cargo run`.
