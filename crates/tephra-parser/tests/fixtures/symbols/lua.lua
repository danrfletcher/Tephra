-- module
local M = {}
local VERSION = 1
count = 0

local function helper()
end

function M.run()
end

function M:method()
end

function global_fn()
  local function inner() end
  local scratch = 1
end

local handler = function() end

return M
