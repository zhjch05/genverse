# Games, worlds, and portals between them

Status: 2026-11-08. Built: games with many worlds, locally and hosted; game-level admins,
saves and packages; items that travel with players between a game's worlds; portals
within a game and to other games. *Later* marks what isn't built.

## Goal

A player keeps many places: a home, a Sokoban room, a fork of last week's campaign. A
**game** is what they host, join and invite people to; it holds **worlds**, the places
inside it, each with its own ground. They move between worlds through portals: voxel art
an agent builds on request. Everything keeps the rule so far: each log is the truth.

## Game and world

| | Game | World |
|---|---|---|
| What it is | What you host or join: one address, one set of players | One place in a game, with its own ground |
| Owns | Players and roles (admins), which worlds exist, game saves, the game's packages | Ground (seed, flat or natural), voxel edits, builds, land, which packages run here, game pieces and timers, what the players in it carry |
| Log | `game.json`: worlds made and removed, admins, saves, packages | `world.jsonl`, as before |

Other games split it the same way: Roblox experiences hold places, a Minecraft server
holds dimensions, a CK3 campaign is saved as one thing.

- **One log per world plus a game log**, not one big log: worlds stay independent, so a
  busy world never slows another's loading or joining, and a server could later run
  each on its own thread or machine.
- **Admins are the game's.** A world's log takes its admins from the game when it loads.
- **Single player is an unhosted game.** Hosting it shares all its worlds at once.

On disk, `saves/games/<game>/game.jsonl` and `saves/games/<game>/worlds/<world>/`
(`world.jsonl`, `snapshot.json`). The first run moves the worlds from before into a game
called `local` (Home becomes its world `home`); nothing is deleted. A server's data
directory is one game the same way (a `world.jsonl` from before becomes its world
`main`). `CUBEWISH_GAME` and `CUBEWISH_WORLD` pick where to start.

## The game log

```rust
GameAction::CreateWorld { name, seed, flat }
GameAction::RemoveWorld { name }      // moved to saves/trash, never deleted
GameAction::SetAdmins { admins }
GameAction::Save { name, worlds }     // world name → its last change: a save of everything
GameAction::LoadSave { name, signed } // the admin's signed Load for each world
GameAction::AddPackage { package }    // a package for the whole game, or a new version
GameAction::RemovePackage { name }
```

Saving the game bookmarks every world at once, like a strategy game's save, and each
world keeps its state as of it. Loading it is a `Load` in each world's log carrying that
state, as loading a world's save is; the game is saved first ("before loading …"), so
loading that brings it back. On a server the admin signs each world's load (without the
state, which the server fills in), so every world change stays signed by whoever made it;
the server checks the signatures and makes them all.

## Game packages

Game rules are installed once for the whole game; each world turns on the ones it plays.

- Installing a package in a world (asking the agent for a game) also adds it to the
  game. Other worlds turn it on from the Rules tab.
- A new version of a package reaches every world that runs it: loaded worlds on a server
  at once, the others when they're next loaded (`GameLog::sync`, recorded in the world's
  log by the world). Taking a package out of the game turns it off everywhere.
- A world running a package from before games had packages gives it to the game.

## Items travel

What a player carries follows them between the worlds of a game. Leaving a world
records `Action::Depart { player, to, items }` there; arriving records `Action::Arrive`
in the new world before its welcome, so each world's log stays the truth and the items
are always in exactly one world. Only the world (a server; offline, an admin's game)
records these. Items keep their package's name; a world where that package is off
shows them by name. Going to another game or a server doesn't carry anything: items
belong to their game.

## Entering a world

One path for every way in (the window, `/world`, a portal, joining or leaving a
server): the world left behind is saved, the new one loads, the ground resets to its
seed, and the hero lands at its spawn, or by a portal leading back, behind the loading
screen. On a server, entering a world asks the server for it; the server sends that
world's log and snapshot, and from then on only that world's changes and the people in
it.

## Hosting a game

One server, one game, one address. Each player is in one world at a time:

- `ToServer::Enter { world }` moves them; the server answers with that world's
  `Welcome` (log, snapshot) and the game (its worlds and admins).
- Proposals apply to the world the player is in. Commits go to the players in that
  world; presence too. Game changes (a new world) go to everyone.
- Admins make worlds on the server from the window or `/newworld`.
- Timers run in every world, near the players in it.

## Portals

```rust
enum PortalTo {
    World { name },          // a world in this game: works for everyone in it
    Game { game, world },    // another local game (single player)
    Server { addr },         // another hosted game
}
```

- **In the log** (`Action::SetPortal { build, to }`), so links replay, undo and travel to
  other players; moving or revising the build keeps its link, removing it removes it.
- **Who:** whoever may change the build.
- **Making one:** ask the agent ("a stone arch that's a portal to sokoban"); it's told
  this game's worlds. Or link any build from the window, or `/portal <world>`.
- **Using one:** walk up to it; a card says where it leads; E goes.

## The Game window (O, or the globe button)

- The game: its name, local or hosted (address, who's on), and Back.
- **Worlds:** this game's worlds (ground, changes, last played), Enter, New (Flat or
  Natural), Fork from a save, Move to trash.
- **Saves:** game saves (every world) and this world's saves: Save, Load (admins),
  Fork.
- **Rules:** the game's packages, which are on here: Turn on, Turn off, Take out of
  the game (admins).
- **Portals:** this world's portals: Go, Unlink, link the selected build.
- **Games:** your local games (Enter, New) and servers (Join; recent ones remembered);
  Host this game; Leave.

## Later

- Each world on its own thread or machine; renaming worlds and games; thumbnails.
- Moving between games with items (a trade between games' logs).
