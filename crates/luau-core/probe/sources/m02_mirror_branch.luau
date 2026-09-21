local function mb_below(p, q, box) if p < q then box.flag = 0 end return box end
local function mb_notbelow(p, q, box) if not (p < q) then box.flag = 0 end return box end
local function mb_atmost(p, q, box) if p <= q then box.flag = 0 end return box end
local function mb_notatmost(p, q, box) if not (p <= q) then box.flag = 0 end return box end
local function mb_same(p, q, box) if p == q then box.flag = 0 end return box end
local function mb_differ(p, q, box) if p ~= q then box.flag = 0 end return box end
local function mb_truthy(p, box) if p then box.flag = 0 end return box end
local function mb_falsy(p, box) if not p then box.flag = 0 end return box end
local function mb_isnil(p, box) if p == nil then box.flag = 0 end return box end
local function mb_istrue(p, box) if p == true then box.flag = 0 end return box end
local function mb_isnum(p, box) if p == 19 then box.flag = 0 end return box end
local function mb_isstr(p, box) if p == "mirror comparison literal" then box.flag = 0 end return box end
local function mb_repeat(p, box) repeat box.flag = box.flag + 1 until box.flag > p return box end
local function mb_split(p, box)
	if p then box.a = 0 else box.a = 1 end
	box.b = 2
	box.c = 3
	box.d = 4
	return box
end
return {
	mb_below, mb_notbelow, mb_atmost, mb_notatmost, mb_same, mb_differ,
	mb_truthy, mb_falsy, mb_isnil, mb_istrue, mb_isnum, mb_isstr,
	mb_repeat, mb_split,
}
