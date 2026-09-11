local function mi_plus(p, q) return q + p end
local function mi_minus(p, q) return q - p end
local function mi_times(p, q) return q * p end
local function mi_over(p, q) return q / p end
local function mi_rem(p, q) return q % p end
local function mi_raise(p, q) return q ^ p end
local function mi_floordiv(p, q) return q // p end
local function mi_plusk(p) return p + 11 end
local function mi_minusk(p) return p - 11 end
local function mi_timesk(p) return p * 11 end
local function mi_overk(p) return p / 11 end
local function mi_remk(p) return p % 11 end
local function mi_raisek(p) return p ^ 11 end
local function mi_floork(p) return p // 11 end
local function mi_rminus(p) return 11 - p end
local function mi_rover(p) return 11 / p end
local function mi_negate(p) return -p end
local function mi_invert(p) return not p end
local function mi_count(p) return #p end
local function mi_join(p, q) return p .. q .. p end
local function mi_yes() local v = true return v end
local function mi_no() local v = false return v end
local function mi_none() local v = nil return v end
local function mi_small() local v = 12 return v end
local function mi_text() local v = "mirror constant string" return v end
return {
	mi_plus, mi_minus, mi_times, mi_over, mi_rem, mi_raise, mi_floordiv,
	mi_plusk, mi_minusk, mi_timesk, mi_overk, mi_remk, mi_raisek, mi_floork,
	mi_rminus, mi_rover, mi_negate, mi_invert, mi_count, mi_join,
	mi_yes, mi_no, mi_none, mi_small, mi_text,
}
