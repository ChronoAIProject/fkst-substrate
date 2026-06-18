local t = fkst.test

return {
  test_run_department_captures_raises = function()
    local result = t.run_department("departments/producer/main.lua", {
      queue = "tick",
      payload = { raiser = "integration-test" },
      ts = 123,
    })

    t.eq(result.exit_code, 0)
    t.eq(result.raises[1].queue, "example_event")
    t.eq(result.raises[1].payload.from, "producer")
    t.eq(result.raises[1].payload.source_queue, "tick")
    t.eq(result.raises[1].payload.source_raiser, "integration-test")
    t.is_nil(result.raises[2])
  end,
}
