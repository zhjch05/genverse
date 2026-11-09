# Testing game logic

How to check a world's game logic, built up request by request by the agent in Lua, from
the automated tests to playing by hand, alone and with two players. The engine keeps only
building blocks (pieces, timers, walking and chasing, regions, stats, items and the
inventory, HUD and panels, builds and land); everything a game does is its scripts'.

## 1. Automated tests (a minute)

```sh
cargo test --workspace
cargo clippy --workspace --all-targets
```

What covers what:

| Test | Checks |
|---|---|
| `crates/rules/tests/players.rs` `stats_hud_and_panels` | stats with a maximum, HUD for one player and for everyone (bar, text, item counter), a dialogue panel, buttons that aren't offered refused, running out of gold refused, closing |
| `players.rs` `games_build_with_the_installers_budget` | a script places a solid build and digs land; both are charged to whoever installed the game; removing its build; over budget, nothing changes at all (two walls, budget one); snapshots keep games' builds |
| `players.rs` `trials_show_panels_stats_and_builds` | the agent's `test_game` report shows stats, HUD, the open panel, and builds |
| `players.rs` `panels_are_ui_trees` | a Lua shop panel built as a tree (rows of item tiles and buttons), a button choosing, live values |
| `crates/rules/tests/timers.rs` | timers, including through the same path builds take |
| `crates/rules/tests/features.rs` | a world's logic growing request by request: a change adds files by name, every file's handlers run, an unchanged file isn't set up again, removing a file takes its pieces and timers; migrations (`ev.from`, `players()`), renames keeping pieces and timers; `test_game` on a copy of the world |
| `crates/rules/tests/walking.rs` | walks (one change, `on_arrive`); `walks_go_around_solid_pieces`: a `look.solid` fence turns a walk aside (unit tests in `walk.rs` and `solid.rs`: around a wall, onto a low deck, never inside a build); `routines_wandering_and_spawners_are_lua`: a spawner, wandering and a daily routine written in Lua; `the_world_is_the_game`: `on_start`, `on_join`, regions, chases, bites, teleport to camp, `on_hit`, building locked for players |
| `crates/server/tests/two_players.rs` | everything above through a real server: items and stats travel between worlds, game saves |

## 2. By hand, alone (15 minutes)

### Start here: one world, many requests

The world has one set of game logic that every request changes a little. Don't ask for "a
game": ask for things, one at a time, the way you'd play with a level editor, and keep going.
Start an empty world (made flat the first time):

```sh
GENVERSE_WORLD=valley cargo run
```

Press `T` and send these one by one, waiting for each answer (a few minutes: the agent plays
each change before answering). After each, the chat says what changed ("\"valley\" v3 is live:
added merchant.luau, rock; …"), and `/packages` shows the version.

1. > Let a few wolves roam the field about 20 m east of here; they chase anyone who comes close.

   Walk east: wolves wander, and one runs at you when you get close.
2. > Give players 10 health shown on screen. Wolf bites hurt, and at zero you wake up back here with full health.

   The wolves are still there (not set up twice); an HP bar appears; a bite takes HP; at 0
   you're back here.
3. > Add a merchant next to me who sells a torch for 2 coins, and scatter a few shiny rocks around that give a coin when you press E on them.

   E on rocks gives coins: they show in your hotbar. E on the merchant opens his shop (a
   panel the agent wrote); a torch costs 2 and lands in your hotbar.
4. > Wolves keep their distance from anyone carrying a torch.

   With a torch, wolves stop chasing you; without one, they come.
5. > Let me plant a tent: a tent item I can put down, and sleeping in it heals me.

   The tent is in your hotbar: pick its slot (1-9), right-click the ground and it's there.

Then keep going, in your own words. Things to try, in any order:

- Tweaks: "wolves are too fast", "bites hurt less at night", "the merchant should say hello".
- Additions: "a campfire that heals you when you stand near it", "a fishing spot by the
  pond", "a scoreboard of wolves driven off with F", "a dungeon gate that opens with a key".
- Rules: "players can't build while this is on", "new players start with a torch".
- Removals: "take the wolves out": they leave, and the rest keeps working.
- Fixes: when something errors, the chat shows it; "fix it" is enough.

What to check every time:

- **Things appear where you asked** ("next to me", "20 m east"), placed by the world;
  nothing has to be put down first.
- **It adds, it doesn't replace.** What earlier requests made is still there and works.
- **Migrations.** Ask for a change to something that exists ("wolves take 2 hits now"): the
  wolves already out get it too. The chat says it saved "before … vN" first; `/load` that
  save puts the world back, data and all.
- **Pieces aren't set up twice** when a feature changes (the agent checks what's there).
- **Taking a feature out** takes its pieces with it.
- **History.** `H` lists each version as a change; quitting and starting again keeps it all.

A scripted run of the same, each prompt sent when the one before is answered:
`GENVERSE_WORLD=valley GENVERSE_PROMPT="first request || second request || third" cargo run`.

### The inventory

With items from the requests above (coins, a torch, a tent):

1. **Hotbar.** Nine slots at the bottom with the day above; what you carry fills them, a
   colored tile (its icon model's color) with its initials and how many.
2. **In hand.** 1-9 picks a slot; its name shows for a moment.
3. **Put down, use.** Right-click the ground with a tent in hand: the game puts it there
   (`on_place`). Right-click a piece in reach with an item: the game uses it on that piece
   (`on_use`; a key on a gate, if the agent wrote one).
4. **Inventory.** I opens it: 27 slots above the hotbar. Click a stack, click another slot:
   it moves (or swaps). Esc or I closes it.
5. **It stays.** Quit and start again: the slots are where you left them (the layout is
   yours, kept for each world; the counts are the game's).
6. **Running out.** Spend the last coin: its slot empties; get one again: it comes back to
   the first empty slot.

### Games that change the land

> A dungeon gate 10 m north of me: it digs a 4 by 4 m pit in front of it and builds a stone
> archway over the pit's far side. E on the gate takes the archway down; E again puts it
> back. Show "The gate is open" or "The gate is closed" on everyone's screen.

Check: the pit is dug (real terrain, you can fall in), the archway is solid, E toggles
it, the text on screen changes. `/history` shows it all as the game's changes. Select
the archway and delete it yourself: allowed if you could delete any build there.

## 3. Two players (10 minutes)

Two copies of the game on one computer:

```sh
cargo run                     # you: /host (or the Game window's Games tab, Host)
GENVERSE_AS=Bo GENVERSE_SERVER=127.0.0.1 cargo run    # Bo, a guest, not an admin
```

Then, in the hosted game (with the valley's logic, or any):

1. Bo sees the same pieces and builds, and can't walk through builds.
2. Bo talks to the merchant: his own panel, his own coins and HP; yours don't change, and
   your screen doesn't show his panel. His hotbar has his own items.
3. Text the game puts on everyone's screen (`"*"`) shows for both.
4. A wolf chasing Bo runs after Bo, not you; a gate opening happens for both at once.
5. Bo can't install or change games (only admins), but plays them.

## 4. Scripted runs (repeatable)

The game can play itself; useful after a change:

```sh
# Ask for something, then walk east, hit what's in reach, and take a screenshot.
GENVERSE_WORLD=valley GENVERSE_PROMPT="wolves in the field 20 m east of me" \
  GENVERSE_PLAY="wait 3; go 15 0; wait 4; hit" \
  GENVERSE_SHOT=valley.png GENVERSE_SHOT_AFTER=540 cargo run
```

`GENVERSE_PLAY` steps: `go <x> <z>` (puts the hero there), `wait <s>`, `use` (E on the
nearest piece), `hit` (F), `place` (puts down what's in hand) or an item name, a key
(`north`, `east`, …; sent to the region with keys the hero stands in), `choose <id>`, `close`, `portal`. `GENVERSE_KEEP=1` places the
agent's build without a click; `GENVERSE_SHOW_REQUEST=1` logs what the agent is told.

## What to look for when something's off

- The agent's own test runs: `test_game (N steps)` in the terminal, with what each step
  did and the tester's stats, HUD and open panel.
- Refusals: shown at the top of the view, and kept in the chat for the agent's next
  request ("fix it" is enough).
- `/history`: every change with who made it; games' changes show the player who acted.
