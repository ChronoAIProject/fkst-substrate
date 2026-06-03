local M = {}

M.spec = {
  consumes = { "tick" },
  timeout = "30s",
}

function pipeline(event)
  log.info("event received on queue: " .. tostring(event.queue))
end

return M
