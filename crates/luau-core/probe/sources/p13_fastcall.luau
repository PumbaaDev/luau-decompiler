local function fc_one(a) return math.abs(a) end
local function fc_two(a, b) return math.max(a, b) end
local function fc_twok(s) return string.sub(s, 2) end
local function fc_three(a, b, c) return math.clamp(a, b, c) end
local function fc_multi(...) return math.max(...) end
return { fc_one, fc_two, fc_twok, fc_three, fc_multi }
