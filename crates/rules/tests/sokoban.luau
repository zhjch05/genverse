-- Sokoban, as an agent might write it: a test that the script API is enough for a real
-- game. Not shipped; worlds get their games from their admins' agents.

local LEVEL = {
  "########",
  "#      #",
  "# @$ . #",
  "#  $   #",
  "#  .   #",
  "########",
}
local W, H = #LEVEL[1], #LEVEL

local function at(board, col, row)
  local o = board.c.board.origin
  return { x = o.x + (col - 1) - (W - 1) / 2, y = o.y, z = o.z + (row - 1) - (H - 1) / 2 }
end

local function cell(col, row)
  return string.sub(LEVEL[row], col, col)
end

local function box_at(board_id, col, row)
  for _, b in ipairs(find("box")) do
    if b.c.box.board == board_id and b.c.box.col == col and b.c.box.row == row then
      return b
    end
  end
  return nil
end

local function pawn_of(board_id, player)
  for _, p in ipairs(find("piece")) do
    if p.c.piece.board == board_id and p.c.pawn == player then
      return p
    end
  end
  return nil
end

function on_place(ev)
  local board_id = spawn(ev.pos, {
    board = { origin = ev.pos },
    zone = { size = { W, H } },
    label = "Sokoban",
    look = { model = "floor" },
  })
  local board = get(board_id)
  for row = 1, H do
    for col = 1, W do
      local c = cell(col, row)
      local p = at(board, col, row)
      if c == "#" then
        spawn(p, { wall = true, look = { model = "wall" } })
      elseif c == "." then
        spawn(p, { target = true, look = { model = "target" } })
      elseif c == "$" then
        spawn(p, { box = { board = board_id, col = col, row = row, start = { col, row } }, look = { model = "box" } })
      end
    end
  end
end

local STEP = { north = { 0, -1 }, south = { 0, 1 }, east = { 1, 0 }, west = { -1, 0 } }

local function solved(board_id)
  for _, b in ipairs(find("box")) do
    if b.c.box.board == board_id and cell(b.c.box.col, b.c.box.row) ~= "." then
      return false
    end
  end
  return true
end

function on_key(ev)
  local board = get(ev.entity)
  if not board or not board.c.board then return end
  local pawn = pawn_of(ev.entity, ev.player)
  if ev.key == "enter" then
    if pawn then return end
    for row = 1, H do
      for col = 1, W do
        if cell(col, row) == "@" then
          spawn(at(board, col, row), { piece = { board = ev.entity, col = col, row = row }, pawn = ev.player })
        end
      end
    end
  elseif ev.key == "leave" then
    if pawn then despawn(pawn.id) end
  elseif ev.key == "reset" then
    for _, b in ipairs(find("box")) do
      if b.c.box.board == ev.entity then
        local box = b.c.box
        box.col, box.row = box.start[1], box.start[2]
        set(b.id, "box", box)
        move(b.id, at(board, box.col, box.row))
      end
    end
    set(ev.entity, "label", "Sokoban")
  elseif STEP[ev.key] and pawn then
    local d = STEP[ev.key]
    local col, row = pawn.c.piece.col + d[1], pawn.c.piece.row + d[2]
    if cell(col, row) == "#" then return end
    local box = box_at(ev.entity, col, row)
    if box then
      local bc, br = col + d[1], row + d[2]
      if cell(bc, br) == "#" or box_at(ev.entity, bc, br) then return end
      local b = box.c.box
      b.col, b.row = bc, br
      set(box.id, "box", b)
      move(box.id, at(board, bc, br))
    end
    local piece = pawn.c.piece
    piece.col, piece.row = col, row
    set(pawn.id, "piece", piece)
    move(pawn.id, at(board, col, row))
    if solved(ev.entity) then
      set(ev.entity, "label", "Sokoban · solved!")
      notify(ev.player, "Solved!")
    end
  end
end
