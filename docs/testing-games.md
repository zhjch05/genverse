# Testing games: stats, HUD, panels, shops, builds

How to check the game engine's player features (stats, HUD widgets, dialogue and menu
panels, shops) and games changing the world (builds, land), from the automated tests
to playing by hand, alone and with two players.

## 1. Automated tests (a minute)

```sh
cargo test --workspace
cargo clippy --workspace --all-targets
```

What covers what:

| Test | Checks |
|---|---|
| `crates/rules/tests/players.rs` `stats_hud_and_panels` | stats with a maximum, HUD for one player and for everyone (bar, text, item counter), a dialogue panel, choices that aren't offered refused, running out of gold refused, closing |
| `players.rs` `shops_need_no_script` | a shop from data alone: E opens it, buying takes the price and gives the item, not enough money refused, the shop stays open |
| `players.rs` `games_build_with_the_installers_budget` | a script places a solid build and digs land; both are charged to whoever installed the game; removing its build; over budget, nothing changes at all (two walls, budget one); snapshots keep games' builds |
| `players.rs` `trials_show_panels_stats_and_builds` | the agent's `test_game` report shows stats, HUD, the open panel, and builds |
| `crates/rules/tests/timers.rs` | timers, including through the same path builds take |
| `crates/rules/tests/features.rs` | a world's logic growing request by request: a change adds files by name, every file's handlers run, an unchanged file isn't set up again, removing a file takes its pieces and timers; migrations (`ev.from`, `players()`), renames keeping pieces and timers; `test_game` on a copy of the world |
| `crates/rules/tests/walking.rs` | walks (one change, `on_arrive`), routines and spawners from data alone, sleeping while nobody is near; `the_world_is_the_game`: `on_start`, `on_join`, regions, chases, bites, teleport to camp, `on_hit`, building locked for players |
| `crates/server/tests/two_players.rs` | everything above through a real server: items and stats travel between worlds, game saves |

## 2. By hand, alone (15 minutes)

### Start here: one world, many requests

The world has one set of game logic that every request changes a little. Don't ask for "a
game": ask for things, one at a time, the way you'd play with a level editor, and keep going.
Start an empty world (made flat the first time):

```sh
CUBEWISH_WORLD=valley cargo run
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

   E on rocks gives coins (shown on screen); E on the merchant opens his shop; a torch costs 2.
   Wolves and health still work.
4. > Wolves keep their distance from anyone carrying a torch.

   With a torch, wolves stop chasing you; without one, they come.

Then keep going, in your own words. Things to try, in any order:

- Tweaks: "wolves are too fast", "bites hurt less at night", "the merchant should say hello".
- Additions: "a campfire that heals you when you stand near it", "a fishing spot by the
  pond", "a scoreboard of wolves driven off with F".
- Rules: "players can't build while this is on", "new players start with a torch".
- Removals: "take the wolves out": they leave, and the rest keeps working.
- Fixes: when something errors, the chat shows it; "fix it" is enough.

What to check every time:

- **No game bar detour.** Things appear where you asked ("next to me", "20 m east"); the game
  bar only shows things you asked to put down yourself.
- **It adds, it doesn't replace.** What earlier requests made is still there and works.
- **Migrations.** Ask for a change to something that exists ("wolves take 2 hits now"): the
  wolves already out get it too. The chat says it saved "before … vN" first; `/load` that
  save puts the world back, data and all.
- **Pieces aren't set up twice** when a feature changes (the agent checks what's there).
- **Taking a feature out** takes its pieces with it.
- **History.** `H` lists each version as a change; quitting and starting again keeps it all.

A scripted run of the same, each prompt sent when the one before is answered:
`CUBEWISH_WORLD=valley CUBEWISH_PROMPT="first request || second request || third" cargo run`.

### An inn: stats, HUD, dialogue, builds

```sh
CUBEWISH_WORLD=inn cargo run
```

### An inn: stats, HUD, dialogue, builds

Press `T` and ask (or paste):

> Make a small inn game. Put down the inn: it builds a little stone house around an
> innkeeper (use real builds for the walls). Players start with 20 gold and 5 of 10 HP,
> shown as an HP bar and a gold counter on screen. Talking to the innkeeper opens a
> dialogue: rent a room for 5 gold (restores HP to full), buy bread for 2 gold (+2 HP),
> or leave. Also put a small shop next to the innkeeper that sells a lantern for 8 gold.
> Test it with test_game, including buying and renting.

Wait for the agent (a few minutes; it plays the game itself with `test_game` first; its
runs are in the terminal log, `test_game (N steps)`). Then check:

1. **Put it down.** Pick "Inn" in the game bar and click the ground a few steps away.
   The house appears as real builds: walk into a wall and it stops you; the history (`H`)
   shows "placed inn:inn".
2. **HUD.** Bottom left: Gold 20 and an HP bar at 5 / 10 (once the game gives you stats,
   on placing or first talking, as the agent wrote it).
3. **Dialogue.** Walk up to the innkeeper ("E to use" over her) and press `E`. A panel:
   the title, the text, three choices, Close.
4. **Choices.** Rent a room: gold 15, HP 10 / 10, and the panel says so. Buy bread
   until you can't: the refusal shows at the top ("not enough gold", in the agent's
   words) and nothing changes.
5. **Close.** Close, or Leave, closes it.
6. **Shop.** E on the lantern stall; buy one; your gold or coins go down, the lantern
   shows in the game bar.
7. **Save and load.** Game moves (placing the inn, buying) are moves, so undo skips them.
   `/save shopping`, buy something, then `/load shopping`: the gold is back. Loading saved
   the world first, so `/load before loading shopping` brings the purchase back.
8. **It's all in the log.** Quit and start again (`CUBEWISH_WORLD=inn cargo run`):
   same gold, HP, HUD, house. A panel left open is still open.
9. **Travel.** `/newworld annex`, then `/world inn` and back: your gold, HP and items
   go with you (`H` in each world shows "left for…" and "arrived from…").

### Games that change the land

> Make a "dungeon gate" fixture: when placed, it digs a 4 by 4 m pit 1 m deep in front of
> it and builds a stone archway over the pit's far side. Pressing E on the gate takes the
> archway down; pressing E again puts it back. Show a line of text on everyone's screen:
> "The gate is open" or "The gate is closed".

Check: the pit is dug (real terrain, you can fall in), the archway is solid, E toggles
it, the text on screen changes. `/history` shows it all as the game's changes. Select
the archway and delete it yourself: allowed if you could delete any build there.

### A shop from data alone

> A town square without scripts: a coin purse that gives 5 coins a day, and a general
> store (a shop) selling 3 turnip seeds for 2 coins, and turnip crops.

Put down the purse and the store, take coins, open the store, buy until you run out.
No script runs: it's all the engine's.

### Walking, routines and spawners

> Make a small village game. A slime pool that keeps up to 3 slimes hopping about nearby at
> any time of day. A baker NPC who walks around her little stall during the day; pressing E
> on her opens a short dialogue where you can buy bread for a coin, and players start with
> 5 coins. Use the engine's spawners and walking, not timers that move things step by step.

Check:

1. **Spawner.** Put the slime pool down: within its `every` seconds a slime appears, then
   more, never more than 3. They walk smoothly between spots (no jumping tile to tile), and
   around water.
2. **NPC.** Put the baker down: she wanders around her stall; E on her still opens her
   dialogue while she walks.
3. **Sleep.** Walk more than 64 m away and come back: the slimes are where they'd be, and
   `H` shows no changes while you were away.
4. **Cheap log.** `H` while they walk: about one change per slime per leg, never one a step.
5. **Replay.** Quit and start again: everyone is where they were, and keeps going.

`crates/rules/tests/walking.rs` covers the rest: a scripted walk is one change and ends
in `on_arrive`; spawners keep to their most and sleep with nobody near; an NPC's routine
takes it to its noon stop.

## 3. Two players (10 minutes)

Two copies of the game on one computer:

```sh
cargo run                     # you: /host (or the Game window's Games tab, Host)
CUBEWISH_AS=Bo CUBEWISH_SERVER=127.0.0.1 cargo run    # Bo, a guest, not an admin
```

Then, in the hosted game:

1. Bo sees the inn's house and can't walk through it.
2. Bo talks to the innkeeper: his own panel, his own gold and HP; yours don't change,
   and your screen doesn't show his panel.
3. Text the game puts on everyone's screen (`"*"`) shows for both.
4. A gate opening (a build coming down) happens for both at once.
5. Bo can't install or change games (only admins), but plays them.

## 4. Scripted runs (repeatable)

The game can play itself; useful after a change, and how the screenshots in the
history were made:

```sh
# Ask for a game, put it down, press E, pick a choice, take a screenshot after 9 minutes.
CUBEWISH_WORLD=inn CUBEWISH_PROMPT="make a small inn game ..." \
  CUBEWISH_PLAY="place; use; choose room; close" \
  CUBEWISH_SHOT=inn.png CUBEWISH_SHOT_AFTER=540 cargo run

# In a world that has it already: start next to the innkeeper, talk, rent.
CUBEWISH_WORLD=inn CUBEWISH_SPAWN=0.6,2.4 CUBEWISH_DIST=2.5 \
  CUBEWISH_PLAY="use; choose room" CUBEWISH_SHOT=after.png CUBEWISH_SHOT_AFTER=16 cargo run
```

`CUBEWISH_PLAY` steps: `place` (the game bar's first fixture), an item name (puts it
down), `use` (E on the nearest piece), a key (`enter`, `north`, …), `choose <id>`,
`close`, `portal`. `CUBEWISH_KEEP=1` places the agent's build without a click. The
choice ids are in the game's script (`/packages`, or `saves/games/local/worlds/inn/`).

## What to look for when something's off

- The agent's own test runs: `test_game (N steps)` in the terminal, with what each step
  did and the tester's stats, HUD and open panel.
- Refusals: shown at the top of the view, and kept in the chat for the agent's next
  request ("fix it" is enough).
- `/history`: every change with who made it; games' changes show the player who acted.
