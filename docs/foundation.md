# The foundation: anything by prompting

The goal is a world the agent can do anything with, just from what players ask for.
The engine provides building blocks and things only an engine can do. Everything else
is the agent's code and data. Fairness and enforcement (cheating, server checks) come
later.

Two rules decide what goes in the engine:

- **Programmable, not configurable.** A fixed behavior with a few settings is a wall
  the agent hits sooner or later. Where behavior differs from game to game (movement,
  animation, input, effects), scripts decide it, and the engine runs it.
- **The agent works like a coding agent.** It reads the world the way it reads a
  repository: lists, reads and searches files, runs queries, and edits scripts in
  place. Everything placed in the world keeps a record of what it means, so later
  requests understand earlier ones.

Each step ends by rerunning the request set in [`evals/`](../evals/README.md).

Status: steps one to six are built, and seven as far as the agent's tools go (ops, building and land anywhere). They include `ground(x, z)`, `name(player)`,
`set_time(t)`, and packages recording the scripting API they were written for. One
thing is left: game scripts and player scripts don't run in the web build yet. The Lua
engine used here (mlua with Luau) doesn't build for the browser, so that needs a Lua
that does. It's a project of its own.

## 1. The world as a workspace

Today the agent runs with no tools except a few MCP calls, and gets the world as a
paragraph in its prompt. It answers with one JSON blob, and must resend whole script
files to change one line.

- **A folder per world**, written before each request and used as the agent's working
  directory:
  - `WORLD.md`: an overview: places, features, packages, players, where the player
    stands.
  - `places.jsonl`: the semantic layer (below).
  - `builds/`: each build's name, prompt, position, size and meaning (its voxel spec
    beside it).
  - `pieces.jsonl`: every game piece: id, package, position, components.
  - `packages/<name>/`: each package's manifest, defs, `scripts/*.luau` and
    `models/*.json`.
  - `players.json`: stats and items.
  - `history.md`: recent changes and the requests behind them.
  - `docs/`: the scripting reference, one file per topic, so the prompt only says
    where to look.
- **File tools:** Read, Glob, Grep, Edit and Write, inside that folder only. The agent
  changes a game by editing files under `packages/`. When it finishes, changed packages
  are read back from the folder and installed as the next version. `test_game` tries
  the files as they are on disk.
- **Live tools:**
  - `query`: runs read-only Lua against the live world (`find`, `get`, `near`,
    `ground`, …) and returns what the code returns. This plays the part of a shell.
  - `search`: geo-aware search (below).
  - `sample_terrain`: heights and surfaces.
  - The existing build-site tools.
- **Semantic layer:** everything made by a request keeps what it means. That covers
  builds, land changes, game features and named places. Each record has:
  - a name
  - a one-line "about"
  - tags
  - the request it came from
  - who made it, and when
  - where it is (a point or an area)

  The agent writes the about and tags as part of its answer. It can also name places
  ("the camp", "wolf field") without building anything. Records are kept in the
  world's log, so they replay, sync and undo with the world.
- **Geo search:** `search(text, near, radius)` ranks places, builds, pieces and features
  by how well they match the text and how close they are. Results come with ids,
  positions and why they matched. For example, "where are the wolves", "anything
  about bread near me" or "the merchant's hut".

## 2. Player scripts

These are scripts that run on each player's machine every frame. Nothing they do goes
in the log: they handle what has to feel immediate. The world's game logic stays in
the log.

- **Movement:** `hero.luau` gets the input (keys held, where the camera points), the
  hero's state and the `body` the game logic set. It decides velocity and gravity: fly,
  float, swim, glide, dash. The engine keeps collision. Today's controller becomes the
  default for packages that don't include one.
- **Animation:** per-frame offsets and turns for pieces (bob, spin, swing, slide)
  without changing their place in the log.
- **Camera:** first person, top-down, shake, a pan.
- **Effects and sound when they happen**, using the data from step 4.

## 3. Messages and queries

- **Messages from players:** `send(name, data)` from a player script reaches
  `on_message(ev)` in the game logic, so any key or mouse button can drive a game. The
  game logic decides what happens. Chat lines reach `on_chat(ev)` (`/home`).
- **Seeing the world from game logic:**
  - `near(pos, radius, component)`
  - `ground(x, z)`
  - `players()` with names and positions
  - setting the time of day

## 4. Presentation as data

- **Look:** `scale`, `tint`, `hidden`, glow.
- **Particle effects:** a spec saying what to emit, its color, speed and how long it
  lasts. `effect(pos, spec)`.
- **Sounds:** a spec for a small sound generator, numbers instead of sound files.
  `sound(pos, spec)`.
- **Sky, fog, light and weather**, set by the game logic.
- **The hero's appearance:** a model or costume a game puts on a player.

## 5. Packages together, and engine changes

- **Packages talk:** `emit(name, data)` and `on_signal(ev)`, plus a small store of
  values per package.
- **Engine changes don't quietly break worlds:**
  - A removed function fails with a clear error naming its replacement.
  - The player sees it.
  - The agent gets it with the next request, so a plain "fix it" works.
  - Scripts declare the API version they were written for.

## 6. The agent checks its work

- **Docs on demand**, in the workspace's `docs/` (step 1). The prompt says where they
  are and stays short.
- **A fuller `test_game`:**
  - right-clicking an item onto a piece
  - skipping ahead in time ("wait until night")
  - sending messages and chat
  - running player scripts
- **Screenshots:** `look(at, from)` renders the world and returns the image, so the
  agent sees what it made.

## 7. What a player can do, the agent can do

The agent used to change the world only through its answer (one build, land in a
selected area, a portal's target) and by editing package files. Everything else the
game can do sat behind chat commands and the Game window, out of its reach: asked for a
new world, it could only tell the player where the button was.

- **Ops** (`src/agent/ops.rs`): each thing a player can do to the game and its worlds is
  one entry: a name, its arguments, who may do it, and the code. Chat commands, the Game
  window and the history panel all go through it, and the agent gets every op as a tool.
  A new op reaches all of them at once.
- **As the player:** an op changes the world through the ledger, signed by the player,
  with the agent and the request kept on the change. Land, admin and rate rules hold.
- **Asked first:** what can't be undone or takes the player online (removing a world,
  loading a save, hosting, joining, leaving, taking a package out of the game) shows a
  card in the chat, and happens on Allow.
- **At the end of the turn:** going to another world, hosting and joining wait until the
  agent answers, so the world it's working in doesn't change under it.
- **The folder follows:** `GAME.md` shows the game (worlds, saves, packages, whether the
  player is an admin), and the view is rewritten after each op, leaving the packages
  the agent is editing as they are.
- **No gaps by accident:** a new kind of change in the log doesn't compile until
  `ops.rs` says which op makes it, or why none does.

- **Building and land, anywhere:** `place_build` puts an object where the agent says
  (any number in one request, on the ground there, facing the player unless told
  otherwise), `change_build` moves, turns or redesigns one by id, `remove_build` takes
  one away, and `change_land` reshapes an area the agent chooses (`sample_terrain` reads
  any rectangle of the world first). They happen at once and undo takes them back. The
  previews stay for what the player points at: one new object with no place named is
  still a draft they place, and a selected area still gets a land preview.

- **Games reach other worlds too:** `travel(player, world)` sends a player to another
  world of the game with what they carry, and `portal(build, to)` links a build the
  game placed. Scripts don't list the game's worlds: replaying a world's log must not
  depend on anything outside it, so the agent writes the names in (`GAME.md` has them).

- **A request can cross worlds:** a trip may carry `then`, the agent's note on what is
  left to do where it leads. When the player has arrived, the agent is started again
  there with that world's folder, the note and the conversation, up to three worlds
  per request. "A dungeon world, a portal to it here, and one back from there" is one
  request.

Next: claiming and releasing parcels as ops.
