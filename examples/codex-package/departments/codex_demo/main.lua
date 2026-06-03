local M = {}
local prompt = require("departments.codex_demo.prompt")

M.spec = {
  consumes = { "codex_request" },
  produces = { "codex_result" },
  stall_window = "5m",
}

function pipeline(event)
  local request = (event.payload or {}).input or ""
  local result = spawn_codex_sync({
    prompt = prompt.build(request),
    stall_window = 300,
  })
  log.info(string.format(
    "codex exit_code=%s log=%s",
    tostring(result.exit_code),
    tostring(result.log_path)))
  raise("codex_result", {
    exit_code = result.exit_code,
    summary = prompt.parse(result.stdout),
  })
end

return M
