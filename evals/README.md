# Request evals

Freestyle requests a player might make of a world, each tried on its own with the real
agent, to find what the engine can't do yet. Rerun after adding to the engine or the
prompt, and compare.

```bash
cargo build
python3 evals/run.py <saves folder holding the world> <out folder> [ids...]
```

Each request runs in its own copy of the world (`GENVERSE_EVAL_WORLD`, default
`valley`), four at a time (`GENVERSE_EVAL_JOBS`). `<out>/<id>/summary.txt` has the
agent's reply and its `test_game` runs; `run.log` and `saves/` have the rest.

## Baseline, 2026-11-09

On the valley world: wolves, health, a merchant and shiny rocks. All 26 got an
answer within 3 minutes.

| Outcome | Requests |
| --- | --- |
| Done as asked | dog, chest, chickens, farm, fishing, guard, plate, quest, race, prices, pelt, sheep |
| Done another way, less than asked | bell, banner, boat, door, fireball, home, lift, rain, scoreboard, sparkle, torchlight, trampoline |
| Can't be done | doublejump, potion (it sells, but can't speed you up) |

What was missing, by how many requests ran into it:

- **Changing how players move** (5): speed, jump height, launching, riding.
  doublejump and potion couldn't be done. trampoline teleports the player
  upward instead of launching them. boat and lift move the player in
  hops.
- **Sound and effects** (5): bell and banner show a text message in place of
  a sound. sparkle and fireball use glowing models in place of effects.
  rain spawns 35 cloud pieces.
- **Smooth movement** (3): `move` jumps a piece straight to its new spot, so
  boat, lift and door move in steps of about a second.
- **Testing can't do everything a player can** (4): `test_game` can't
  right-click an item onto a piece (chest, farm) or skip ahead to night
  (prices, rain).
- **Input** (2): no free keys for a game to use (fireball moved to an item),
  and no chat commands (home moved to items).
- **Knowing about players** (1): scoreboard can only show player ids, not
  names.
- **Old worlds break when the engine changes** (3): valley's wolf script calls
  `spawner` and `wander`, which no longer exist. Its wolves, merchant and rocks
  never appear. pelt, sheep and potion noticed this and rewrote them, but
  nothing told the agent or the player.

## After player movement, 2026-11-09

Scripts can now call `body` (speed, jump, gravity, air jumps) and `launch`, use
`fly` for straight-line moves, and carry a hero on a pawn while it walks or flies.
Reran the five movement requests:

- doublejump: done (`body(p, {air_jumps = 1})` for everyone).
- potion: done (`body(player, {speed = 2})`, put back by a timer).
- trampoline: done (`launch(player, {x = 0, y = 18, z = 0})`).
- boat: done (the boat is the pawn and flies across).
- lift: done (the platform is the pawn and flies up). Checked in the game: the hero
  rides up and stands on the deck at the top.
