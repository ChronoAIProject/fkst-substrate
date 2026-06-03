local M = {}

M.spec = {
  consumes = { "work" },
  timeout = "30s",
}

local function done_path(id)
  return "state/done/" .. id .. ".txt"
end

local function temp_done_path(id)
  return "state/done/." .. id .. ".tmp"
end

local function ensure_done_dir()
  local out = exec_sync("mkdir -p state/done")
  assert(out.exit_code == 0, "failed to create state/done")
end

local function request_summary(path)
  if not path or path == "" or not file.exists(path) then
    return "missing request"
  end
  local body = file.read(path)
  local first = body:match("[^\r\n]+") or ""
  return first:gsub("^#%s*", "")
end

function pipeline(event)
  local payload = event.payload or {}
  local id = assert(payload.id, "work payload missing id")
  local request_path = payload.request_path
  with_lock("worker-" .. id, function()
    local target = done_path(id)
    if file.exists(target) then
      log.info("work already done: " .. id)
      return
    end

    ensure_done_dir()
    local content = table.concat({
      "id=" .. id,
      "request_path=" .. tostring(request_path),
      "summary=" .. request_summary(request_path),
      "done_at=" .. tostring(now()),
      "",
    }, "\n")
    local temp = temp_done_path(id)
    file.write(temp, content)
    local moved = exec_sync("mv -f " .. temp .. " " .. target)
    assert(moved.exit_code == 0, "failed to move done marker")
    log.info("work completed: " .. id)
  end)
end

return M
