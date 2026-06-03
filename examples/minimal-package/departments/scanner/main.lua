local M = {}

M.spec = {
  consumes = { "reconcile_tick", "request_changed" },
  produces = { "work" },
  timeout = "30s",
}

local function trim(value)
  return (value:gsub("^%s+", ""):gsub("%s+$", ""))
end

local function request_id(path)
  local name = path:match("([^/]+)$") or path
  return (name:gsub("%.md$", ""))
end

local function done_path(id)
  return "state/done/" .. id .. ".txt"
end

local function list_requests()
  local out = exec_sync("find requests -maxdepth 1 -type f -name '*.md' | sort")
  if out.exit_code ~= 0 then
    log.warn("request scan failed: " .. trim(out.stderr))
    return {}
  end

  local requests = {}
  for path in out.stdout:gmatch("[^\r\n]+") do
    requests[#requests + 1] = path
  end
  return requests
end

function pipeline(event)
  log.info("scanner reconciling after " .. tostring(event.type))
  for _, path in ipairs(list_requests()) do
    local id = request_id(path)
    if not file.exists(done_path(id)) then
      raise("work", { id = id, request_path = path })
    end
  end
end

return M
