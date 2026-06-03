local M = {}

M.spec = {
  consumes = { "example_event" },
  stall_window = "30s",
}

function pipeline(event)
  local p = event.payload or {}
  log.info(string.format(
    "consumer received Event{queue=%s, payload={from=%s, note=%s, source_queue=%s, source_raiser=%s}, ts=%s}",
    tostring(event.queue), tostring(p.from), tostring(p.note),
    tostring(p.source_queue), tostring(p.source_raiser), tostring(event.ts)))
end

return M
