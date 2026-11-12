# Genverse

An AI-native voxel sandbox on Bevy 0.20. You walk a chibi hero through an endless HD voxel
world and change it by talking to a coding agent (Claude Code or Codex CLI): describe a build,
reshape the land, or ask for a game, and the agent writes it. Worlds are a signed,
append-only log of changes. They can be shared over the network, undone, saved and forked.

![preview](docs/preview.png)

*A volcano and a mushroom lamp, both built by the agent from a one-line prompt.*

```sh
cargo run                             # or: bevy run
GENVERSE_WORLD=sokoban cargo run      # start in (or create) a named world
```

Needs `claude` and/or `codex` on your `PATH`, already logged in, for anything the agent does.

## Contents

- [Controls](#controls)
- [Talking to the agent](#talking-to-the-agent)
- [Builds](#builds)
- [Reshaping the land](#reshaping-the-land)
- [Games](#games)
- [Worlds, history and saves](#worlds-history-and-saves)
- [Playing together](#playing-together)
- [Scripting a run](#scripting-a-run)
- [How it fits together](#how-it-fits-together)
- [Development](#development)

## Controls

`WASD` moves, `Shift` runs, `Space` jumps, drag to look, scroll to zoom. A hint bar at the bottom always shows the keys that apply right now
(`F1` hides it).

| Context | Keys |
| --- | --- |
| Anywhere | `Esc` game menu (closes open windows first) · `T`/`Enter` talk to the agent · `G` select ground to reshape · click a build to select it · middle-click copies it · `P` show the spawn area (where only admins build) · `H` history · `O` worlds · `Tab` hide the builder panel · `Ctrl/Cmd+Z` undo · `Ctrl/Cmd+Shift+Z` redo |
| Draft following the cursor | click to place · `Q`/`E` turn · `R`/`F` raise/lower · `Esc`/right-click cancel |
| Selected build | `M` or double-click move · `Delete`/`Backspace` delete · `T` change it · `Esc`/right-click deselect |
| Selecting ground | drag across the ground · right-drag look · `G`/`Esc` stop |
| Area selected | `T` describe the change · `G` select again · `Esc` clear |
| Land preview | `Enter` keep · `T` ask for changes · `Esc` put the ground back |
| Games | `1`–`9` pick what's in hand · `I` inventory · right-click put it down or use it · `E` act · `F` hit |
| Inside a board (region with keys) | `WASD`/arrows move a tile · `R` reset · `E` action · `Esc` leave |

## Talking to the agent

Everything that changes the world goes through one chat panel. Pick **Claude Code** or
**Codex CLI** at the top, then press `T` and describe what you want. What the message does
depends on what's selected:

- **nothing selected**: a new build, a game, or a change to the world's game logic
- **a build selected**: a revision of that build
- **an area of ground selected**: a land change

**The world is a workspace.** Before each request the game writes the world out as a folder
and runs the agent in it, like a coding agent in a repository:

| File | Holds |
| --- | --- |
| `WORLD.md` | overview: places, features, packages, players, where you stand |
| `places.jsonl` | the semantic layer: every build, land change, feature and named place, with what it is about |
| `builds/` | each build's name, prompt, position, size and voxel spec |
| `pieces.jsonl` | every game piece with its components |
| `packages/<name>/` | each game package: manifest, defs, `scripts/*.luau`, `models/*.json` |
| `players.json`, `history.md` | stats and items, recent changes and the requests behind them |
| `docs/` | the scripting reference, one file per topic, plus `changes.md` for API changes |

The agent reads, globs, greps and edits files in that folder and nowhere else. Packages it
edits are read back and installed as the next version. It also gets live tools, served by the
game over MCP on `127.0.0.1` with a per-run token:

- **Looking:** `search` (finds places, builds and pieces by meaning and distance), `query`
  (read-only Lua against the live world), `sample_terrain`, `get_build_site`,
  `list_nearby_builds`, `get_build_spec`
- **Checking its work:** `check_build`, `test_game` (plays a game in a throwaway copy of the
  world), and `look` (renders a screenshot so it sees what it made)

It gets nothing else: no shell, no other MCP servers, no user config. Its answer must match
`crates/rules/src/build_spec.schema.json`, and the game checks it before anything changes.

## Builds

The agent answers a build request with a small JSON spec of shapes. The game checks it,
turns it into voxels, and shows it in front of you as a draft. Drafts stick to the surface
under the cursor, Minecraft-style. A red outline means it can't go there: it overlaps you, or
the land or its budget doesn't allow it, and the panel says which. Placed builds are solid.

Every build passes the limits a server would enforce (`crates/rules/src/build.rs`):

- up to 64³ voxels at 12.5 cm, 200 shapes and 24 colors
- up to 120k voxels, 60k visible faces and 3k glowing voxels

A rejected build goes back to the agent once for a fix. While a build is selected, messages
revise it in place: `Enter` keeps the change, `Esc` restores it.

## Reshaping the land

Land isn't edited voxel by voxel; you say what it should become. Press `G` and drag across
the ground. A click selects a 6 m square; areas go up to 48 m a side. Then press `T` and
describe it: "a pond with sandy banks", "a winding stone path to the hill".

The agent answers with a few terrain operations in the area's frame:

- `raise`, `lower`, `flatten`, `smooth` and `terrace`
- `water` and `paint`
- `scatter` (trees, bushes, rocks, flowers, grass)
- `path` and `clear`

Each can take a rectangle or ellipse with a soft edge. `crates/rules/src/sculpt.rs` turns them
into voxels the same way every time. You get a live preview: `Enter` keeps it, `Esc` puts the
ground back exactly, and further messages revise it. Kept changes are stored as operations,
not voxels, as a sparse layer over the world seed. Changed chunks remesh in the background.

## Games

Genverse has a small game engine that you program by asking. In a new world, press `T` and
say "make a basic Sokoban game", then keep going: "wolves in the hills", "a merchant who
buys pelts", "torches scare wolves". Each request changes the world's game logic a little.
The world is the game.

**What the agent sends** is a *package*: definitions, voxel models and Luau scripts. Each
script file is a feature, and every file's handlers run. The agent sends only what changed,
and removing a file takes its pieces with it. Before answering, it plays the change with
`test_game`, which reports what each step did and shows a map, and it fixes what fails. A
version that doesn't pass the checks is never installed. Script errors players hit show up
in the next request, so "fix it" is enough. Only admins change a world's games; everyone
plays them.

**Game logic** runs in a sandboxed Luau VM for each event. It has no files, OS or clock, a
seeded random number generator, and limits on steps, memory and changes. Its changes go into
the log, so games replay, undo and sync like everything else. The engine gives building
blocks, not genres; shops, crops, spawners and quests are a few lines of Lua.

| Area | Scripts can |
| --- | --- |
| Pieces | `spawn`, `move`, `set`, `find`, `near`, `ground(x, z)`; look, label, scale, tint, glow; `look.solid` pieces block walking |
| Time | `after`, `every`, `on_timer` (nothing runs between timers), `set_time` for the time of day |
| Movement | `walk` and `chase` with path-finding around builds, `fly` in straight lines, pawns that carry heroes (boats, lifts) |
| Players | `on_join`, `on_leave`, `on_enter` regions, `on_catch`, `on_hit`, `teleport`, stats (HP, gold), `name(player)`, `body` (speed, jump, gravity, air jumps), `launch`, `appearance` |
| Items | a hotbar and inventory; `on_place`, `on_use` |
| UI | HUD and panels (dialogue, menus, shops) as UI trees of text, values, bars, items, buttons, rows and columns |
| Presentation | `effect` (particles), `sound` (synthesized from numbers, no files), `sky` (time of day, fog, sun, rain, snow) |
| The world | place real builds and reshape land, charged to whoever installed the game; stop players building |
| Input and packages | `on_message` from player scripts, `on_chat` for unknown `/commands`, `emit`/`on_signal` between packages, `remember`/`recall` |

**Player scripts** (`*.player.luau`) run every frame on each player's machine for what has to
feel immediate. They set hero velocity, pose and camera, and use `send()` to reach the game
logic. Nothing they do goes in the log.

**Packages record their scripting API version.** When the engine changes, removed functions
fail with a message that names their replacement. The agent's folder flags old packages and
lists the changes.

`/packages` lists a world's games. Admins can `/uninstall <name>`; its pieces stay, asleep.
More: [docs/game-logic.md](docs/game-logic.md), [docs/foundation.md](docs/foundation.md),
and [docs/testing-games.md](docs/testing-games.md).

## Worlds, history and saves

A world is a seed plus an ordered log of changes (`world.jsonl`, append-only). The log holds
builds, land changes, game installs and moves, rule changes, undos, saves and loads. Each
change records:

- who made it, and which agent did if one did
- a hash of the request
- when it was made

Hover a build or reshaped ground to see who changed it last.

- **Players are keys.** `saves/profile.json` holds your name and an ed25519 secret key. Your
  player id is the public key, and every change is signed with it, so servers need no
  accounts. `/verify` checks every signature. Keep that file private.
- **The log only moves forward.** Undo is a change that puts back what an earlier change
  replaced, refused if something has changed there since. You undo your own changes; admins
  can undo anyone's. The latest 100 changes can be undone. Snapshots hold the whole world, so
  loading never replays the full log.
- **Saves** work like a strategy game's: `/save <name>` (anyone), `/saves`, and an admin's
  `/load <name>` puts the world back exactly, game state included, for everyone. Nothing is
  deleted. `/fork <save> <world>` starts a new world from a save.
- **Rules per world** (`/rules`), kept in the log:
  - **Land:** anyone who can edit changes any of it, except the spawn area around the start (`P` outlines it). Land
    someone is working on (their agent changing it, a preview open, a build draft) waits
    for them: others' changes there are refused until they keep it or put it back.
  - **Budgets:** builds, voxels and glow per parcel and per player.
  - **Rate limits:** off by default, set with `/rate 10 200000` (changes a minute, voxels per
    10 minutes). Admins are exempt.
  - **Pause:** `/pause` (or **Pause world for everyone** in the `Esc` menu) stops a world's
    games for everyone in it: timers wait, and game actions (use, hit, board keys) are
    refused until an admin resumes. Building and talking go on. Playing alone, the `Esc`
    menu pauses the games by itself, without recording anything; online, it only stops
    your hero.

**History** (`H`) lists every change, newest first, filterable by player, with Undo and Load.

**Games and worlds** (`O`): a *game* is what you host or join, and it holds *worlds*, each
with its own ground and log. Your local game lives in `saves/games/local/`. The window lists
worlds (admins create flat or natural ones), saves, portals, and your other games and
servers. Ask the agent for "a stone arch, a portal to sokoban" and it builds a portal; press
`E` at it. Portals lead to another world, another game (`game/world`) or a server
(`host:port`). Items travel between a game's worlds. Packages belong to the game: install
once, then turn them on per world in the Rules tab. Design: [docs/worlds.md](docs/worlds.md).

Chat commands: `/history [n]`, `/undo <id>`, `/rules`, `/pause`, `/verify`, `/save`, `/saves`, `/load`,
`/fork`, `/land`, `/rate`, `/worlds`, `/world`, `/newworld`, `/games`, `/game`, `/newgame`,
`/savegame`, `/loadgame`, `/portal`, `/packages`, `/uninstall`, `/host`, `/join`, `/play`,
`/leave`.

## Playing together

- **From the game:** **Host** in the panel (`/host`) opens your game on port 7878, running
  on your own saves. **Stop hosting** (or `/leave`) closes it and takes you back to your
  local world; so does quitting.
- **On the same network nobody types an address.** Hosts announce themselves over UDP on
  port 7877, and every game lists what it hears with a **Join** button. **Play together**
  (`/play`) joins a world it finds, or hosts yours if there is none. Guest Wi-Fi and VPNs can
  block this; joining by address still works.
- **By address:** `/join host[:port]`, the panel's Join field, or
  `GENVERSE_SERVER=host cargo run`. `/leave` goes back to your local world.
- **A dedicated server** with no graphics, for a world that's always up:

  ```sh
  cargo run --release -p genverse_server -- --port 7878 --data server-data   # add --flat for a flat first world
  ```

  Back up `server-data/` (`game.json` and `worlds/<name>/world.jsonl`).

The server orders changes, like Figma's multiplayer. Players propose signed changes, and the
server checks each one against the world's rules, appends it and broadcasts it. Refused
changes are taken back with the server's reason. Agents run on each player's machine, and the
server sees only finished specs and the request's hash. The first player in a new world is
its admin.

You see each other live: other players' heroes with name tags, their selections, the drafts
they're working on, previews of their land changes, and "Claude Code is designing…". If the
connection drops, the game reconnects by itself. Chunk fingerprints catch floating-point
drift between machines.

Messages are signed but not encrypted yet, so host on a local or private network (Tailscale,
WireGuard).

## Scripting a run

Environment variables drive the game without a mouse, which is handy for tests, evals and
screenshots:

| Variable | Does |
| --- | --- |
| `GENVERSE_WORLD`, `GENVERSE_GAME` | which world and game to start in (created if new) |
| `GENVERSE_PROMPT` | sends a message (or `;`-separated chat commands) at start |
| `GENVERSE_AGENT=codex` | uses Codex instead of Claude Code |
| `GENVERSE_AREA=14`, `GENVERSE_KEEP=1` | selects an area in front of the hero for a land change, and keeps the preview |
| `GENVERSE_SELECT_LAST=1` | revises the most recent build |
| `GENVERSE_PLAY="go 15 0; wait 4; hit"` | plays steps once the agent is done |
| `GENVERSE_AS=Bo` | plays as a non-admin guest for one run |
| `GENVERSE_SERVER=host` | joins a server |
| `GENVERSE_MENU=1` | opens the game menu (Esc) at start |
| `GENVERSE_SHOT=out.png` (+ `_YAW`, `_PITCH`, `_DIST`, `_AFTER`) | renders one still and exits |
| `GENVERSE_QUALITY=low\|medium\|high\|ultra` | graphics quality |

The preview above:

```sh
GENVERSE_WORLD=home GENVERSE_YAW=1.0 GENVERSE_PITCH=0.28 GENVERSE_DIST=18 GENVERSE_SHOT=shot.png cargo run
```

## How it fits together

| Piece | Where | Role |
| --- | --- | --- |
| World rules | `crates/rules` (`genverse_rules`) | Everything that decides what the world is, with no rendering, so the server shares it: voxel models, terrain (`terrain.rs`), land changes (`sculpt.rs`), builds (`build.rs`), the log and replay (`log.rs`), undo, settings, land, budgets, rate limits, keys (`identity.rs`), the semantic layer (`semantic.rs`) |
| Game logic | `crates/rules/src/logic` | Packages, entities, the Luau sandbox (`script.rs`), timers, walking and solids, UI trees, presentation, player-frame scripts, and `test_game` (`trial.rs`) |
| Protocol | `crates/net` | Client/server messages, the background connection, LAN discovery |
| Server | `crates/server` | Headless server: holds the log, checks, orders and shares changes |
| World streaming | `src/world` | Generates and meshes 32³ chunks on the async pool, with LOD; the change log on disk (`ledger.rs`) |
| Hero | `src/player.rs` | Character from model parts, movement with voxel collision, animation, follow camera |
| Rendering | `src/voxel`, `src/scene.rs`, `src/graphics.rs` | Materials, mesher with baked AO, `.vox.ron` loader, sky, sun, fog, post-processing |
| Agent and UI | `src/agent` | Chat panel, CLI runner (`runner.rs`), workspace folder (`workspace.rs`), MCP tools (`mcp.rs`, `tools.rs`), builds, land previews, games, HUD, inventory, player scripts, history, worlds, online play and presence |

The look: 25 cm terrain voxels and 5 cm character voxels, with baked per-vertex AO,
per-voxel color jitter, PBR surfaces, soft cascaded shadows, SSAO, TAA, bloom, distance fog
and a physical sky.

### Voxel models (`assets/**/*.vox.ron`)

Characters and props are data. A model declares `surfaces` (roughness, metallic, emissive,
alpha), a `palette` of named colors, and an ordered list of `ops`:

- `Shape(geom, fill, noise?, mask?)` with `Box`, `Ellipsoid`, `Capsule` or `Cylinder`
- `Voxels(at, legend, layers)` for painted voxels: each layer is a z slice drawn as text
  rows, top row first, one character per voxel (`.` leaves the cell alone); the hero's
  face is painted this way
- `Island`, `Scatter`, `Settle` and `Spill` for terrain-like generation

`parts` cut a model into named pieces that rotate around a `joint` under a `parent`; that's
how the hero (`assets/characters/hero.vox.ron`) gets limbs. Saving a model while the game
runs reloads it live.

## Development

```sh
cargo test --workspace
cargo clippy --workspace --all-targets
python3 evals/run.py <saves folder> <out folder>   # real agent requests, see evals/README.md
```

- [docs/testing-games.md](docs/testing-games.md) covers what the tests check and how to test
  by hand, alone and with two players.
- [evals/](evals/README.md) runs freestyle player requests against the real agent, to find
  what the engine can't do yet.

The web build (`bevy run web`, WebGPU) is on hold while multiplayer and scripting are built.
It compiles, but the Luau engine doesn't build for the browser yet, so game scripts don't run
there.
