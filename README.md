# Cubelark

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

Cubelark is AI-native: you don't stack 25 cm voxels by hand, you say what the land should
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

Script one: `CUBELARK_AREA=14 CUBELARK_PROMPT="a small pond" cargo run` selects a 14 m square
in front of the hero and sends the message (add `CUBELARK_KEEP=1` to keep the preview).

There's no hand editing of single voxels: land only changes through these described edits.
The edits are a sparse layer on top of the world seed (`src/world/mod.rs`): chunks apply them
whenever they generate, and changed chunks (plus neighbors sharing the border) remesh in the
background. Agents' `get_build_site` sees the reshaped terrain too.

## An open world, kept as a log

Cubelark is heading for shared, open worlds: anyone can build and reshape land with their own
agent, nothing is lost, and every change says who made it. Everything below works the same in
single player and on a server.

The world is the seed plus an ordered log of changes (`saves/world.json`): builds placed,
moved, revised and removed, terrain changes, rule changes, and undos. Each change records who
made it, which agent did if one did, a hash of the request, and when. A selected build shows
"made by you with Claude Code, 2 h ago".

- **Players are keys.** `saves/profile.json` holds your name and an ed25519 secret key; your
  player id is the public key, and every change is signed with it, so any server can check who
  made a change without accounts. `/verify` checks every signature. Keep that file private.
- **Replay is the truth.** Loading replays the log; terrain changes compile against the ground
  the earlier changes left, so the same log always gives the same voxels.
  `saves/world_edits.json` is only a cache of that, used while its hash matches the log.
- **Undo never deletes history:** it appends a revert, and you can only undo your own changes.
  Admins (the world's first player) can roll back a player, or restore the world to an earlier
  change; those are undoable too. When the log changes in a way a simple undo can't express,
  the game rebuilds from it, regenerating only what differs.
- **Each world sets its own rules** (`cubelark_rules::settings`), kept in the log so replay
  always applies the rules of the time: land mode, spawn, limits on one build or land change,
  budgets, and rate limits.
  - **Land:** open by default, so anyone changes anything outside the protected spawn (24 m
    around the start). A world can switch to **parcels** (`/land parcels`): 8 × 8 m plots that
    changes claim for their author, which only owners change. `P` shows the land around you.
  - **Budgets:** per parcel 12 builds, 40k visible build faces, 150k build voxels and 4k glowing
    voxels; per player 60 builds, 1.5M build voxels and 2M voxels of terrain changes.
  - **Rate limits:** off unless a world sets them (`/rate 10 200000`: 10 changes a minute, 200k
    voxels per 10 minutes). Admins are exempt.
- Ownership, budgets and rates aren't saved anywhere: they're derived from the log, and replay
  enforces them as of each change, so a log can't hold anything its authors weren't allowed to do.
- Saves from before the log, and profiles from before keys, are upgraded once (old files kept as
  `.bak`).

Chat commands, until the history panel exists: `/history [n]`, `/revert <id>`, `/rules`,
`/verify`, `/host`, `/join <host[:port]>`, `/leave`, and for admins `/rollback <player>`, `/restore <id>`, `/land open|parcels`,
`/rate <changes per minute> [voxels per 10 minutes]` or `/rate off`. Try them from someone else's
side with `CUBELARK_AS=Bo cargo run`, a non-admin guest for one run; script them with
`CUBELARK_PROMPT="/rate 5; /history"`.

## Playing together

Two ways to host:

- **From the game:** **Host** in the panel (or `/host`) opens your world to others on port 7878.
  The server runs inside your game on your world's own save, so what you build together stays
  in your world. Others join with your address; it closes when you quit.
- **A dedicated server**, for a world that's always up:

  ```sh
  cargo run --release -p cubelark_server -- --port 7878 --data server-data
  ```

  It's a small program with no graphics (only the rules and protocol crates), so it runs on any
  machine with a terminal. Its whole world is `server-data/world.json`; back that file up.

To join, type `host` or `host:port` next to **Join** in the panel, or use `/join host`, or
start with `CUBELARK_SERVER=host cargo run`. **Leave** (`/leave`) goes back to your local world.
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
- Not yet: seeing other players (milestone 3), history browsing beyond `/history`, chunk
  checks, snapshots for long-lived worlds (milestone 4), and encryption (TLS).

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
  | Anywhere | `T`/`Enter` talk to the agent · `G` select ground to reshape · `P` show parcels · click a build to select · middle-click to copy it · `Ctrl/Cmd+Z` undo |
  | Selecting ground | drag across the ground · right-drag look · `G`/`Esc` stop |
  | Area selected | `T` describe the change · `G` select again · `Esc` clear |
  | Land preview | `Enter` keep · `T` ask for changes · `Esc` put the ground back |
  | Selected build | `M` or double-click move · `Delete`/`Backspace` delete · `T` change it · `Esc`/right-click deselect |
  | Draft following the cursor | click place · `Q`/`E` turn · `R`/`F` raise/lower · `Esc`/right-click cancel |
  | Pinned revision | `Enter` keep · `M` pick it up · `T` ask for more · `Esc` cancel |

  Undo steps back through placing, deleting, moving, and revising builds, and kept land changes.

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
- Script a build: `CUBELARK_PROMPT="a red mailbox" CUBELARK_AGENT=codex cargo run`
  (add `CUBELARK_SELECT_LAST=1` to revise the most recent build instead).

## How it fits together

| Piece | Where | Role |
| --- | --- | --- |
| World rules | `crates/rules/` (`cubelark_rules`) | Everything that decides what the world is, with no rendering, so a server can share it: voxel models, the terrain generator (`terrain.rs`), terrain changes (`sculpt.rs`), build specs (`build.rs`), the change log and replay (`log.rs`), world rules (`settings.rs`), land (`land.rs`), budgets (`budget.rs`), rate limits (`rate.rs`), player keys (`identity.rs`) |
| World streaming | `src/world/mod.rs` | Generates and meshes 32³ chunks around each `ChunkLoader` on the async pool; answers voxel queries for collision |
| Change log | `src/world/ledger.rs` | Loads, migrates, records and saves the log; the player profile; live land and budgets |
| Protocol | `crates/net` | Client/server messages and the game's background connection |
| Server | `crates/server` | Headless server: holds the log, checks, orders and shares changes |
| Hero | `src/player.rs` | Builds the character from model parts, kinematic movement with voxel collision and step-up, walk animation, follow camera |
| Voxel rendering | `src/voxel/` | Materials, mesher with baked AO, `.vox.ron` asset loader, picking |
| Scene | `src/scene.rs` | Sky, sun, fog, post-processing |
| Builder | `src/agent/` | egui chat, CLI runner, drafts and placed builds, area selection, land previews and the land overlay (`landscape.rs`), chat commands (`commands.rs`), applying what the server sends (`online.rs`) (desktop only) |

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

To render a still: `CUBELARK_SHOT=shot.png CUBELARK_YAW=0.2 CUBELARK_PITCH=0.4 cargo run`.
