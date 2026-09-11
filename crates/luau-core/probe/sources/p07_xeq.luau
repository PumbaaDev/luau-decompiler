local function xq_nil(a, s) if a == nil then s.n = 1 end return s end
local function xq_bool(a, s) if a == true then s.n = 1 end return s end
local function xq_num(a, s) if a == 7 then s.n = 1 end return s end
local function xq_str(a, s) if a == "probe equality string" then s.n = 1 end return s end
return { xq_nil, xq_bool, xq_num, xq_str }
