local function kk_add(a) return a + 3 end
local function kk_sub(a) return a - 3 end
local function kk_mul(a) return a * 3 end
local function kk_div(a) return a / 3 end
local function kk_mod(a) return a % 3 end
local function kk_pow(a) return a ^ 3 end
local function kk_idiv(a) return a // 3 end
local function rk_sub(a) return 3 - a end
local function rk_div(a) return 3 / a end
return { kk_add, kk_sub, kk_mul, kk_div, kk_mod, kk_pow, kk_idiv, rk_sub, rk_div }
