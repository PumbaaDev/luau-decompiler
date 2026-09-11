local function ar_add(a, b) return a + b end
local function ar_sub(a, b) return a - b end
local function ar_mul(a, b) return a * b end
local function ar_div(a, b) return a / b end
local function ar_mod(a, b) return a % b end
local function ar_pow(a, b) return a ^ b end
local function ar_idiv(a, b) return a // b end
return { ar_add, ar_sub, ar_mul, ar_div, ar_mod, ar_pow, ar_idiv }
