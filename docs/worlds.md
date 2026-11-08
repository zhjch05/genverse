# Many worlds, and portals between them

Status: 2026-11-08. Built as described, except where marked *later*.

## Goal

A player keeps many worlds: a home world, a Sokoban world, a fork of last week's
campaign. They move between them from a Worlds window, and through **portals**: voxel
art in the world (a stone arch, a glowing door) that an agent builds on request, which
takes whoever walks through it to another world. All of it keeps the rules so far:
each world is its own log, and the log is the truth.

## What a world is

- A folder: `saves/` for **Home** (the world from before named worlds), and
  `saves/worlds/<name>/` for the rest. Each holds `world.jsonl` (the log) and
  `snapshot.json` (a cache).
- Its ground: a seed and whether it's flat. New worlds pick: **Flat** (empty, for games
  and building) or **Natural** (a fresh landscape, a new seed).
- A server is a world too, at an address. Joining one is entering a world that lives
  elsewhere.

## Entering a world

One path for every way in (the Worlds window, `/world`, a portal, joining or leaving a
server):

1. The world you leave is saved: its log is already on disk, and its snapshot is
   written now, so coming back loads fast.
2. The new world's log and snapshot load ([`ledger`](../src/world/ledger.rs)).
3. The voxel world resets to the new ground: every chunk goes, and the terrain changes
   to the new seed and flatness. Chunks stream in around the hero as at startup.
   (Before this, a server with different ground couldn't be joined.)
4. Builds, game pieces and land follow the new log. Games you were playing end.
5. The hero goes to the new world's spawn, or, arriving through a portal, next to a
   portal there that leads back. The loading screen covers it until the ground is in.

You can't leave a world you're hosting (others are in it); it closes when you quit.

## Portals

A portal is a placed build linked to a world:

```rust
Action::SetPortal { build: u64, to: Option<PortalTo> } // None unlinks it
enum PortalTo { World { name }, Server { addr } }
```

- **In the log** like everything else, so they replay, undo and travel to other
  players. A separate action rather than a field on the build: moving or revising the
  build keeps its link, and removing the build removes it.
- **Who:** whoever may change the build (its land's owner, or anyone on open land).
- **Making one:** ask the agent ("a stone arch that's a portal to sokoban"). Its answer
  can carry `portal: {to}` beside the build; it gets the list of your worlds to pick
  from, and builds an opening to walk through. Or select any build and link it from the
  Worlds window.
- **Using one:** walk up to it. A label shows where it leads ("→ sokoban"); E goes.
  The game's own E (a game piece in reach) comes first.
- **In multiplayer:** a portal to a server address works for everyone. A portal to a
  local world name works for players who have a world of that name; others see "that
  world is on someone else's computer". *Later:* one server hosting many worlds, so a
  shared world's portals lead to worlds everyone shares.

## The Worlds window (O, or the globe button)

Four tabs:

- **Worlds:** every local world as a row: name, ground (flat or natural), how many
  changes, when last played. The current one is marked. Enter, Fork (from one of its
  saves), and Move to trash (to `saves/trash/`, never deleted outright; confirm first).
  At the top: a new world (name, Flat or Natural, Create and enter), and Back to the
  world you came from.
- **Saves:** this world's saves, newest first, with who and when, marked if later than
  the world as it is. Save now (name), Load (admins), Fork into a new world (name).
- **Portals:** this world's portals: build, where it leads, how far. Go (travel there
  now), Unlink. Link the selected build to a world (pick one) or a server (address).
- **Servers:** join an address (recent ones remembered), host this world, leave.

Chat commands do the same: `/worlds`, `/world <name>`, `/newworld <name> [flat|natural]`,
`/portal <world|host:port>` (links the selected build), `/save`, `/saves`, `/load`,
`/fork`.

## Later

- One server, many worlds, with portals among them and a shared world list.
- Renaming worlds, world thumbnails, and per-world settings in the window.
- Carrying items between worlds (each world's games keep their own inventories now).
