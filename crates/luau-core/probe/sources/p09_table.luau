local function tb_new(x) local t = {} t[x] = x return t end
local function tb_dup() return { alpha = 1, beta = 2 } end
local function tb_list(a, b, c) return { a, b, c } end
local function tb_ks(t) t.field = t.other return t end
local function tb_get(t, k) return t[k] end
local function tb_num(t) t[3] = t[2] return t end
return { tb_new, tb_dup, tb_list, tb_ks, tb_get, tb_num }
