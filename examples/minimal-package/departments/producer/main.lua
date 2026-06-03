local M = {}

M.spec = {
  consumes = { "tick" },
  produces = { "example_event" },
  stall_window = "30s",
}

function pipeline(event)
  log.info("producer received source event on queue: " .. tostring(event.queue))
  raise("example_event", {
    from = "producer",
    note = "opaque example payload",
    source_queue = event.queue,
    source_raiser = (event.payload and event.payload.raiser) or "",
  })
end

return M
