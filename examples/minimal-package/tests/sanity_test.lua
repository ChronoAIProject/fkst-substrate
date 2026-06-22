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
  test_restricted_lua_load_returns_plain_data = function()
    local result = restricted_lua_load({
      source = [[
        return {
          name = "demo",
          enabled = true,
          items = { "alpha", "beta" },
        }
      ]],
    })

    t.eq(result.name, "demo")
    t.eq(result.enabled, true)
    t.eq(result.items[2], "beta")
  end,
  test_restricted_lua_load_blocks_ambient_capabilities = function()
    local result = restricted_lua_load({
      source = [[
        return {
          require = require,
          load = load,
          global = _G,
          debug = debug,
          package = package,
          rawget = rawget,
          getmetatable = getmetatable,
          io = io,
          os = os,
          coroutine = coroutine,
          string_dump = string and string.dump,
        }
      ]],
    })

    t.is_nil(result.require)
    t.is_nil(result.load)
    t.is_nil(result.global)
    t.is_nil(result.debug)
    t.is_nil(result.package)
    t.is_nil(result.rawget)
    t.is_nil(result.getmetatable)
    t.is_nil(result.io)
    t.is_nil(result.os)
    t.is_nil(result.coroutine)
    t.is_nil(result.string_dump)
    t.raises(function()
      restricted_lua_load({ source = [[ return ("").dump ]] })
    end)
  end,
  test_restricted_lua_load_uses_explicit_bindings = function()
    local result = restricted_lua_load({
      source = [[ return { label = label, value = plus_one(41) } ]],
      bindings = {
        label = "granted",
        plus_one = function(value)
          return value + 1
        end,
      },
    })

    t.eq(result.label, "granted")
    t.eq(result.value, 42)
  end,
  test_restricted_lua_load_rejects_bytecode_by_default = function()
    local dumped = ("").dump(function()
      return 42
    end)

    t.raises(function()
      restricted_lua_load({ source = dumped })
    end)
  end,
}
