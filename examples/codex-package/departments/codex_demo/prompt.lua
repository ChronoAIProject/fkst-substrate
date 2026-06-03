local M = {}

function M.build(request)
  return "Summarize: " .. tostring(request)
end

function M.parse(stdout)
  return (tostring(stdout):gsub("%s+$", ""))
end

return M
