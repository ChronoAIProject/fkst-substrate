local M = {}

M.spec = {
  consumes = { "work" },
  timeout = "30s",
}

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
  assert(id:match("^[%w._%-]+$"), "unsafe work id: " .. tostring(id))
  local request_path = "requests/" .. id .. ".md"
  log.info("work started: " .. id)
  log.info("request summary: " .. request_summary(request_path))
  log.info("work completed: " .. id)
end

return M
