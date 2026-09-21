local function br_lt(a, b, s) if a < b then s.n = 1 end return s end
local function br_nlt(a, b, s) if not (a < b) then s.n = 1 end return s end
local function br_le(a, b, s) if a <= b then s.n = 1 end return s end
local function br_nle(a, b, s) if not (a <= b) then s.n = 1 end return s end
local function br_eq(a, b, s) if a == b then s.n = 1 end return s end
local function br_neq(a, b, s) if a ~= b then s.n = 1 end return s end
local function br_if(a, s) if a then s.n = 1 end return s end
local function br_ifnot(a, s) if not a then s.n = 1 end return s end
local function br_while(a, s) while s.n < a do s.n = s.n + 1 end return s end
local function br_jump(a, s)
	if a then s.x = 1 else s.x = 2 end
	s.p = 1
	s.q = 2
	s.r = 3
	return s
end
return { br_lt, br_nlt, br_le, br_nle, br_eq, br_neq, br_if, br_ifnot, br_while, br_jump }
