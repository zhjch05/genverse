# Game logic made by agents

Status: 2026-11-08. Built: the kernel and log (phase 1), data packages (phase 2), and
server-side Luau scripts with permissions and limits (phase 3), plus engine components
for games (`look`, `label`, `region`, `pawn`). Pieces are drawn only as their
package's voxel models, built like builds, and agents test packages in a throwaway
world (`trial`, the `test_game` tool) before answering. Changed from this proposal: a game's rules apply
to the whole world, and only admins change them; an intent goes only to the package that
owns its item or entity; scripts change the world through a checked batch as they run,
rather than returning a list of intents. Timers replace per-tick events (see
[Timers](#timers)). Also built: players' stats, a HUD and panels (dialogue, menus,
shops, with `Intent::Choose`), both UI trees ([`ui`](../crates/rules/src/logic/ui.rs)), and games changing the world's voxels
(`build`, `unbuild`, `terrain`) through the world's own rules, charged to the installer's
land and budget, all or nothing (see [`entity`](../crates/rules/src/logic/entity.rs) and
[testing-games.md](testing-games.md)). Also built: pieces that move on their own (walking
with paths over the ground, and chasing; see [Movement](#movement)),
and the world as the game ([The world is the game](#the-world-is-the-game)): games set the
world up, see players come and go and walk into places, chase and teleport them, and can
keep players from building. Not built yet: migrations, preview, and the game master agent.

## One world, many requests

A world's game logic isn't written once. Players keep asking for things, and each answer
changes it a little:

- **A change, not a package.** The agent answers with what's new or different in the
  world's package: definitions, script files and models replace those of the same name, the
  rest stays, and `remove` takes names out ([`GameSpec::onto`](../crates/rules/src/logic/data.rs)).
  The result installs as the package's next version. `test_game` plays the change on top of
  what the world runs.
- **Script files are features.** Every file's handler for an event runs (in file order), so
  `wolves.luau` and `merchant.luau` both answer `on_join` and `on_interact`, each for its
  own pieces. A piece a file spawns carries `feature = "<file>"`.
- **New versions start only what changed.** `on_start` runs for new and changed files
  (`ev.upgrade` for a new version); unchanged files aren't set up twice. A file taken out
  takes its pieces with it.
- **The agent sees the world as it runs:** every script in full, definitions, and models by
  name and size, plus recent script errors, and where the player stands with the ground
  heights around them (world meters), so "next to me" and "20 m east" are positions its
  scripts can hold: nothing needs putting down first.

### Migrations

A new version may change how a feature keeps things (new fields on its pieces, a stat
renamed). What makes that safe:

- **All or nothing.** If any `on_start` of the new version fails, the install is refused and
  the world keeps the old version.
- **The migration runs where the change lands:** the changed file's `on_start`, with
  `ev.upgrade` and `ev.from` (the version it replaces). It fixes its pieces (`find`) and every
  player's stats and items (`players()`, everyone the world keeps something for, there or not).
- **Tested on the real state.** `test_game` plays the change on a copy of the world as it is:
  its pieces, timers, players' stats and items, its ground, the tester where the player
  stands. The report says what the upgrade starts and takes out, and shows other players'
  stats and items after it.
- **Features own their things.** Pieces and script timers carry the file that made them:
  taking a file out takes both; `rename` ({from, to}) moves them to the new name instead.
- **A way back.** The world is saved ("before <package> v<N>") just before each new version;
  undoing the install puts the old code back, and loading that save puts the data back too.

## The world is the game

A game isn't only something players put down. Installed, it can be the whole world:

- **It sets the world up.** `on_start` runs when the package is installed (and again, with
  `ev.upgrade`, for each new version): the map's pieces, creatures, characters, regions, builds and
  land changes, the spawn point. It's part of the install change, so it replays with it,
  and if it fails the install is refused.
- **It sees players.** Whoever keeps the world (the server; offline, the game) tells the
  [clockwork](../crates/rules/src/logic/clockwork.rs) who's in the world and where, each
  time it runs. The clockwork tells each package what changed: `on_join` and `on_leave`,
  and `on_enter` and `on_exit` for a piece's `region` (a radius or a rectangle, on a piece
  that may be unseen). Positions never go in the log, only what games did about them,
  recorded like timers. What it has seen lives in memory, so after a restart everyone is
  greeted again.
- **It acts on players.** `chase(id, player)` runs a piece after a player (re-aimed every
  2 s, `on_catch` within reach); `teleport(player, pos)` moves a hero (each teleport is
  shown once); `spawn_point(pos)` is where players appear; F attacks the piece in reach
  (`Intent::Hit`, `on_hit`). Health, score and the like are stats. `body(player, {speed,
  jump, gravity, air_jumps})` changes how a hero moves until it's put back, and
  `launch(player, velocity)` throws one (each throw is felt once). A hero rides their pawn
  as it walks, or flies (`fly(id, pos)`: a straight line, for boats and lifts).
- **It owns the world's rules.** `building(false)` puts an unseen piece with
  `world = {building = false}` in the world: while its package is installed, only admins
  build, reshape land or claim parcels.

Agents test it as a player: `test_game` installs the package (running `on_start`), brings
the tester in at the spawn point (`on_join`), and steps include `go <x> <z>` and `hit`.

## Movement

Pieces move through two engine functions, because a script can't do them itself: a path
needs the ground (scripts don't see voxels), a chase needs where players are now, and
players' games have to move the piece smoothly between changes
([`walk`](../crates/rules/src/logic/walk.rs)):

- **A walk is one change.** `walk(id, pos)` asks for it; the engine's own timer finds a
  path over the ground the host knows (around water, no step over 0.6 m, at most 64 m) and
  records it, with when it starts and how fast, in the piece's `walk` component. Players'
  games move the piece along that path as time passes; its position stays where it set off
  until a second timer moves it to the end and calls the package's `on_arrive`. So a walk
  costs two log changes however long it is, and replay never looks for a path.
- **A chase** (`chase(id, player)`) aims a walk at where the player is every 2 s, and calls
  `on_catch` within reach.
- **The rest is Lua.** A day's routine is an `every` timer walking a character to where
  `ev.time` says; wandering is `on_arrive` picking the next spot; a spawner is a timer
  spawning while `find()` says there are too few (`walking.rs` has all three).
- **They sleep like any timer anchored to a piece** while no player is within 64 m.
- **The ground** comes from whoever runs timers: the server from its world (the seed's
  heights with the voxel edits over them), the game offline from its loaded chunks, a
  trial not at all (walks go straight).

Not yet: builds as obstacles (paths cross them), pieces that block each other, and paths
longer than 64 m in one walk.

## Items and the inventory

A package's only definitions are its items: a title, and optionally one of its models as
the icon ([`data`](../crates/rules/src/logic/data.rs)); scripts `give`, `take` and `count`
them. What a player carries is the game's, in the log; how it's laid out in the player's
inventory (a hotbar of 9, 27 more behind it) is the player's own, kept locally per world.
Right-click with an item in hand puts it down (`on_place`) or uses it on the piece in reach
(`on_use`). Definitions of the kinds from before (crop, source, fixture, shop, spawner,
npc) are ignored, so old worlds still load.

## Timers

A game moves on its own through timers, never through a world tick. A script calls
`after(seconds, name, data)` or `every(seconds, name, data)`, and `on_timer(ev)` runs
when it goes off. Timers are part of the game state, and the log keeps them like any
other effect.

- **Cost follows events, not time.** Nothing runs between timers, so an idle world, or
  a thousand installed games that are waiting, cost nothing. The same idea runs
  discrete-event simulations and MMO server timers.
- **The log keeps what changed, not the time that passed.** The server (offline, the
  admin's game) runs the due timers about twice a second, and commits what each
  package's timers did as one `Action::Timers { package, until, effects }` by `world`.
  A repeating timer that changes nothing leaves no trace. A one-time timer always
  records, so it's forgotten on replay.
- **Replay doesn't run timers.** It applies the recorded effects, checked against the
  package's grants again. So timer scripts needn't be deterministic, and upgrading a
  package never changes the past.
- **Catching up.** A repeating timer that was due several times while nobody ran it
  (a server that was down, a single-player world that was closed) goes off once, with
  `ev.missed` saying how many runs it skipped. A game catches up itself (a crop grows
  three stages) instead of replaying every step.
- **Only near players.** `after(s, name, data, near)` and `every(...)` take an entity
  (the game's board): the timer sleeps while no player is within 64 m of it, and
  catches up through `missed` when someone comes. A world full of games costs only for
  the games people are at. The [`Clockwork`](../crates/rules/src/logic/clockwork.rs)
  that runs timers (on the server, offline in the game, and in `test_game`) remembers
  when each sleeping timer last ran; after a restart, sleepers count from the log's
  last timer change.
- **A one-time timer records cancelling itself** when it goes off, so replay forgets it
  too, and it never goes off twice.
- **Limits.** A timer waits at least 1 second. A package may have 256 waiting, and
  runs at most 64 per firing (the rest go off next time). A timer that errors or runs
  too long is rolled back and cancelled, and the others still run. Players can't
  record `Timers` changes; only the world or an admin can.
- **Fast motion stays out of the log.** A hero gliding, animation and later physics run
  live on each machine. Only results that matter to the game (a round ending, a hit)
  should become changes.

## Costs that don't grow with history

- **Saving appends.** The log is `world.jsonl`: a header, then one line per package and
  per change ([`store`](../crates/rules/src/store.rs)). Saving a change writes that
  change; a cut-off last line is skipped on load. Old `world.json` files move over once.
- **Changes apply on top, always.** Offline, the game applies each change to its current
  state, as the server does. Undo is a change that puts back what a change replaced
  (refused if something changed there since), and loading a save swaps in the state kept
  with it, so nothing ever replays the log to change the world.
- **Snapshots hold the whole world.** A snapshot (`snapshot.json`; on the server every 50
  changes, offline every 2 s after voxel changes or 30 s after game changes) holds the
  voxels, builds, land, budgets, installs, the games' state and timers, and what can still
  be undone. Loading, restarting and joining apply only the changes after it, and never
  run the old intents' scripts again. Since the log only grows, a snapshot never goes stale.

Not built: compacting the log itself. Joining still downloads the whole log (players see
it as the history). Since a snapshot is the whole world, the changes before it could be
moved to an archive; nothing needs them to rebuild the world.

## Goal

Today an agent can change what the world *looks like*: builds and terrain. This design
lets an agent also change how the world *plays*: items, crops, creatures, NPCs, quests
and rules. The same open world can then become a survival game, a Stardew-like farm,
an Animal Crossing-like village or a D&D campaign, without anyone writing Rust.

It has to stay safe in multiplayer. A package written by one player's agent must not
crash the server, hurt other players, or break the world's history.

## Principle

Game logic takes the same path as builds today:

1. A person describes what they want in the chat panel.
2. Their local agent turns it into a spec.
3. The engine checks the spec and applies it deterministically.
4. The server puts it in order, records it in the log, and others see it.

Builds are already like this: `BuildSpec` and `TerrainSpec` are checked in
`cubewish_rules`, and `Action::PlaceBuild` / `Action::Terrain` in
[`log.rs`](../crates/rules/src/log.rs) record them. Game logic adds new kinds of spec
and new `Action`s. It does not add a second path.

## Non-goals

- Agents changing the Rust engine itself. The kernel grows only through normal
  development.
- LLM calls inside game logic. Models are slow, cost money, and give different answers
  each time. See [The game master agent](#the-game-master-agent) for how AI still takes
  part at runtime.
- Web clients (deferred for the MVP, as already decided).

## Three layers

### 1. Kernel (Rust, trusted)

General building blocks in `cubewish_rules`. Every genre is put together from them,
and none of them is specific to one genre:

| Building block | Used for |
|---|---|
| Entities with typed components, one store per namespace | creatures, crops, NPCs, chests |
| Inventories and item stacks | survival, farming, shops |
| Stats and resources | hunger, HP, gold, friendship |
| World clock with days and seasons, plus timers | crop growth, daily resets, festivals |
| Regions and interaction targets | farm plots, dungeons, shop counters |
| Spawners | mobs, fish, visitors |
| NPC schedules and paths | villagers walking their day |
| Seeded random numbers | dice, loot, weather |
| UI described as data: dialogue, menus, HUD, shops | all of the above |

Voxel changes still go through build and terrain operations, so budgets, land rules
and rate limits apply to them as they do now.

### 2. Content as data

Most of what agents write. Definitions are checked against a JSON schema, the way
`build_spec.schema.json` checks builds:

- items, tools and recipes
- crops (seed item, growth stages in days, tiles they grow on, what they yield)
- creatures and loot tables
- NPCs (look, schedule, dialogue, gift preferences)
- quests and shops

Data alone covers much of a farming or survival game. It is the first thing to ship.
(Changed since: only items stayed data. Crops, shops, spawners and villagers proved
better written in Lua by the agent for the game at hand, so the engine dropped them.)

### 3. Scripts

For rules that data can't express. The language is **Luau**, run through `mlua`:

- Roblox built it to sandbox code written by its players.
- Models write Lua well.
- It can't reach files, the network or the clock unless the host gives it a function.
- The host can interrupt it after a set amount of work.

Scripts are only event handlers:

```lua
-- farm-pack/scripts/watering.luau
on("interact", function(ev)
  local crop = ev.target:get("farm:crop")
  if crop and ev.item.kind == "farm:watering_can" then
    return { set(ev.target, "farm:crop", { watered = true }) }
  end
end)

on("day_start", function(ev)
  for _, e in query("farm:crop") do
    -- grow watered crops, then dry them out
  end
end)
```

Events: `interact`, `use_item`, `enter_region`, `leave_region`, `day_start`,
`timer`, `tick` (rate limited), `turn` (in turn-based modes). (Built instead: `on_place`,
`on_interact`, `on_use`, `on_key` and `on_timer`; no tick. See [Timers](#timers).)

A script never changes the world directly. It returns **intents** (set a component,
spawn an entity, give an item, open a dialogue, place a build spec). The kernel
checks each intent against the package's permissions and the installer's budgets,
then applies it.

Wasm (wasmtime with fuel metering) is the option for heavier mods later. It isn't the
place to start: agents would need a compiler toolchain, and the host interface is a lot
more work.

## Packages

A package is what an agent produces and what a world installs:

```
farm-pack/
  package.ron        # name, version, permissions, depends, migrations
  data/*.ron         # content definitions
  scripts/*.luau     # event handlers
  tests/*.ron        # scenarios the test fork runs
```

- A package is identified by the hash of its contents and signed by its author.
- Each package owns a namespace (`farm:`). Its components and state keys live there.
  It reads another package's data only through interfaces that package exports, so a
  survival package can't wipe a farm package's crops.
- A **game mode** is a set of packages. A **preset** is a saved game mode with exact
  versions.
- A mode can be on for the whole world or **only in a region**. One parcel can run a
  D&D campaign while the next one is a farm.

New log actions:

```rust
enum Action {
    // ...existing actions...
    /// Installs or upgrades a package, in the whole world or one region.
    InstallPackage { package: PackageRef, scope: Scope },
    UninstallPackage { package: String, scope: Scope },
    /// A player's request to the game: "interact with this", "use that item".
    Intent(PlayerIntent),
}
```

Upgrading a package is `InstallPackage` with a newer version, and rolling it back is
installing the older one again. Package contents are kept in a store keyed by hash, so
the log stays small and replay can still rebuild the world exactly.

`RULES_VERSION` covers the kernel. Package versions cover everything built on top of it.

## Safety in multiplayer

1. **Only the server runs game rules.** Clients send `Intent`s such as
   `Interact { entity }` and `UseItem { slot }`. They never send state. Scripts on
   clients do presentation only (UI, animation, sound).
2. **Scripts see nothing they aren't given.** No I/O, no network, no wall-clock time.
   They get world time and a random generator seeded from the change id and event.
   That makes them deterministic, so replay keeps working.
3. **Hard limits.** Each handler call has a cap on work done and on memory. Each
   package also has a budget per tick. A package that keeps going over is turned off
   automatically and the admin is told.
4. **Permissions in the manifest**, for example:

   ```ron
   permissions: [
       Components("farm:*"),
       Inventory(write: ["farm:*"]),
       Spawn(scope: SelfRegion),
       Voxels(budget: 20000),
   ]
   ```

   A default set is allowed without asking. Anything beyond it needs an admin to
   approve it at install time.
5. **Someone is always accountable.** Every change a script makes counts against the
   installer's budget, land access and rate limits. "Last changed by" reads
   "farm-pack (installed by Alex)".
6. **Proven before it goes live.** See [Lifecycle](#lifecycle).
7. **Rollback.** A player can undo their own installs and upgrades (undo puts the old
   version back). Admins go back further by loading a save.
8. **Moderation.** Text that scripts produce (dialogue, names, signs) goes through the
   per-server moderation setting already planned.

## Lifecycle

Game logic can be changed live from the chat panel. Every edit makes a new package
version and goes through the same checks. How far it can reach depends on what it
touches:

| Change | Example | Path |
|---|---|---|
| Tuning data | "turnips grow in 3 days" | schema check → preview for you → keep; live in your own region or package |
| New mechanic | "add fishing with a timing minigame" | permission check → test fork → ghost preview → keep |
| Server-wide rules, mode switch | "hunger for everyone" | the same checks → admin approval → announced to players |

Steps:

1. **Write.** The agent writes or edits the package and its test scenarios.
2. **Check.** The schema, the permissions, and a lint that allows only known API.
3. **Test fork.** A headless copy of the world runs the package for N simulated days
   with randomised inputs and the package's own scenarios. If there is a migration,
   it runs on a copy of the real state. Overruns, errors and broken invariants stop
   the change here.
4. **Preview.** The author plays with it in their own view first, like a ghost build.
5. **Keep.** It goes into the log as `InstallPackage`.
6. **Approve,** only when it asks for more than the default permissions or reaches the
   whole world.

Rules for live changes:

- **They land between events.** A new version takes effect at the next tick or day
  boundary, never partway through a handler. In turn-based modes it waits until
  between turns.
- **Migrations.** When a version changes how a package stores its data, it ships a
  migration script. The test fork runs it first.
- **Lock setting.** `WorldRules` gains `logic: Open | AdminOnly | Locked`. A
  competitive survival server or a planned campaign can lock its rules for the whole
  game. A sandbox server leaves them open.

## The game master agent

D&D-style play needs improvisation. Scripts don't call a model. Instead:

1. A script raises an `ask_dm` event: "the players are talking to the innkeeper about
   the missing child".
2. A **DM agent** runs on the game master's machine, matching the existing decision that
   agents run locally. It picks up the event whenever it is ready.
3. It answers through the same intent API as everyone else, using a `dm` permission:
   dialogue, a new NPC, a quest, a build spec for a dungeon.

The fixed rules (dice, turn order, damage) stay in scripts. The agent adds the story
and makes things up as it goes.

## Genres from the building blocks

| Genre | Kernel | Mostly data | Scripts |
|---|---|---|---|
| Survival | stats, damage, spawners, day/night | creatures, loot, recipes | hunger tick, death and respawn |
| Stardew-like | clock, timers, NPC schedules, shops | crops, tools, gifts | friendship hearts, festivals |
| Animal Crossing-like | daily reset, catalog, NPCs | furniture, villagers | visitors, the turnip market |
| D&D | turns, seeded dice, character sheets | monsters, spells | encounter rules, plus the DM agent |

When chat keeps asking for something the building blocks can't do, that is the signal
for what to add to the kernel next.

## Phases

1. **Kernel and log.** Entity/component store in `cubewish_rules`, the world clock,
   `Action::Intent`, `InstallPackage` and the package store.
2. **Data-only packages.** Items, crops, creatures, NPCs and shops. No scripts yet.
   (Since retired, except items: scripts do the rest.)
3. **Server-side Luau.** Event handlers, intents, permissions, budgets, turning off
   a package that overruns.
4. **Agent authoring flow.** Chat panel → package, test fork, preview, keep, approval,
   migrations.
5. **Game master agent.** `ask_dm` and the `dm` permission.

## Open questions

- Should client-side prediction run scripts, or should clients wait for the server?
  Waiting is simpler. Turn-based and farming games tolerate the delay well, but fast
  combat may not.
- How are packages shared between servers? A public registry, or only presets
  exported from a world?
- Is per-region scope enough, or do modes also need per-player opt-in (for example
  PvP)?
- What goes in the default permission set?
