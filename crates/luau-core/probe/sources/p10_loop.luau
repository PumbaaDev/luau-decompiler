local function lp_num(n, s) for i = 1, n do s = s + i end return s end
local function lp_ipairs(t, s) for i in ipairs(t) do s = s + i end return s end
local function lp_pairs(t, s) for k in pairs(t) do s = s + 1 end return s end
local function lp_next(t, s) for k in next, t do s = s + 1 end return s end
local function lp_gen(t, s) for a in t.iter, t, nil do s = s + 1 end return s end
return { lp_num, lp_ipairs, lp_pairs, lp_next, lp_gen }
