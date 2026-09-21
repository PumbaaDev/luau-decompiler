local function ld_nil() local a = nil return a end
local function ld_true() local a = true return a end
local function ld_false() local a = false return a end
local function ld_num() local a = 7 return a end
local function ld_str() local a = "probe constant string one" return a end
local function ld_move(x) local a = x local b = a return b end
local function cc_three(a, b, c) return a .. b .. c end
return { ld_nil, ld_true, ld_false, ld_num, ld_str, ld_move, cc_three }
