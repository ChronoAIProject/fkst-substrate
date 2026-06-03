local t = fkst.test

return {
  test_sanity = function()
    t.eq(1 + 1, 2)
  end,
  test_raises = function()
    t.raises(function()
      error("expected failure")
    end)
  end,
  test_nil = function()
    t.is_nil(nil)
  end,
  test_json_decode = function()
    -- json.decode parses a JSON string into a Lua value: object fields,
    -- nested objects, and 1-indexed arrays.
    local decoded = json.decode('{"queue":"tick","payload":{"raiser":"tick"},"items":[1,2,3]}')
    t.eq(decoded.queue, "tick")
    t.eq(decoded.payload.raiser, "tick")
    t.eq(decoded.items[2], 2)
  end,
  test_json_decode_invalid_input_raises = function()
    -- malformed JSON raises a Lua error (class `json.decode invalid-json:`).
    t.raises(function()
      json.decode("{bad")
    end)
  end,
}
