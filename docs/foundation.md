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

Status: all six steps are built. Still to do:
- step 3: `ground(x, z)` for game scripts, players' names, and setting the time of day
  for the game logic (the sky can already hold a time of day as players see it);
- step 5: scripts declaring the API version they were written for;
- player scripts and effects on the web build.

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
