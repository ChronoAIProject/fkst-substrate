local M = {}

M.spec = {
  consumes = { "tick" },
  timeout = "30s",
}

function pipeline(event)
  log.info("event received: " .. tostring(event.type))
end

return M
