local prompt = require("departments.codex_demo.prompt")

function pipeline(event)
  assert(prompt.build("x") == "Summarize: x", "build failed")
  assert(prompt.parse("hello\n") == "hello", "parse failed")
  log.info("codex_demo unit tests passed")
end
