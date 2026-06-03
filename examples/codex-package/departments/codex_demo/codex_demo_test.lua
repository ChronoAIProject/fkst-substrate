local prompt = require("departments.codex_demo.prompt")
local t = fkst.test

return {
  test_build = function()
    t.eq(prompt.build("x"), "Summarize: x")
  end,
  test_parse = function()
    t.eq(prompt.parse("hello\n"), "hello")
  end,
}
