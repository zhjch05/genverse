# Game logic made by agents

Status: 2026-11-08. Built: the kernel and log (phase 1), data packages (phase 2), and
server-side Luau scripts with permissions and limits (phase 3), plus engine components
for games (`look`, `label`, `zone`, `pawn`). Pieces are drawn only as their
package's voxel models, built like builds, and agents test packages in a throwaway
world (`trial`, the `test_game` tool) before answering. Changed from this proposal: a game's rules apply
to the whole world, and only admins change them; an intent goes only to the package that
owns its item or entity; scripts change the world through a checked batch as they run,
rather than returning a list of intents. Not built yet: per-tick events,
and the game master agent.

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
| UI described as data: dialogue, menus, HUD widgets, shops | all of the above |

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
`timer`, `tick` (rate limited), `turn` (in turn-based modes).

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
7. **Rollback.** Admins can roll back a whole package the way they roll back a player.
   A player can undo their own installs and upgrades.
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
