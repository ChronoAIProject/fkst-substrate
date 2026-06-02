local M = {}

M.spec = {
  consumes = { "work" },
}

local function runtime_root()
  local out = exec_sync("printf %s \"$FKST_RUNTIME_ROOT\"")
  assert(out.exit_code == 0, "failed to read FKST_RUNTIME_ROOT")
  assert(out.stdout ~= "", "FKST_RUNTIME_ROOT is required")
  return out.stdout
end

local function write_witness(name, content)
  local root = runtime_root()
  exec_sync("mkdir -p " .. root .. "/pipeline")
  file.write(root .. "/pipeline/" .. name, content)
end

function pipeline(event)
  write_witness("consumer-b-witness.txt", "consumer_b saw " .. tostring(event.type) .. "\n")
end

return M
