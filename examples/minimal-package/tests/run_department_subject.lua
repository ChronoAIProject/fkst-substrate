function pipeline(event)
  raise("example_event", {
    from = "run_department_subject",
    source_queue = event.queue,
    source_raiser = event.payload and event.payload.raiser or "",
  })
end
