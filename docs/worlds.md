# Games, worlds, and portals between them

Status: 2026-11-08. Built: games with many worlds, locally and hosted; game-level admins
and saves; portals within a game and to other games. *Later* marks what isn't built.

## Goal

A player keeps many places: a home, a Sokoban room, a fork of last week's campaign. A
**game** is what they host, join and invite people to; it holds **worlds**, the places
inside it, each with its own ground. They move between worlds through portals: voxel art
an agent builds on request. Everything keeps the rule so far: each log is the truth.

## Game and world

| | Game | World |
|---|---|---|
| What it is | What you host or join: one address, one set of players | One place in a game, with its own ground |
| Owns | Players and roles (admins), which worlds exist, game saves | Ground (seed, flat or natural), voxel edits, builds, land, game pieces and timers |
| Log | `game.jsonl`: worlds made and removed, admins, saves | `world.jsonl`, as before |

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
```

Saving the game bookmarks every world at once, like a strategy game's save. Loading it
is a `Restore` in each world's log (each one undoable), as loading a world's save is.

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
- **Portals:** this world's portals: Go, Unlink, link the selected build.
- **Games:** your local games (Enter, New) and servers (Join; recent ones remembered);
  Host this game; Leave.

## Later

- Inventories that follow a player between worlds (game-level items, handed over in
  the game log), and game packages installed once per game with each world choosing
  which run. Today both stay per world.
- Each world on its own thread or machine; renaming worlds and games; thumbnails.
