--!nonstrict
-- The shape the small probes could NOT catch.
--
-- `DispatchProbe` and `GuardProbe` are a few hundred bytes each, and a change
-- that made both perfect (2/9 -> 9/9 and 6/13 -> 13/13) simultaneously
-- collapsed `rc_0043_GlovesShop` from 1,588 lines to 73, calls 230 -> 40 - the
-- whole module body gone. Small probes cannot see that.
--
-- This mirrors the shape that collapsed: a module with top-level constant
-- tables, several functions closing over them, and branches with loops inside.
local P = {}

local NAMES = { "Ui-Product", "Ui-Pass", "Ui-Pack", "Ui-Gift" }
local PALETTE = {
	cosmic = { 0, 0.11, 0.05, 0.3 },
	solar = { 0.9, 0.4, 0.1, 1.0 },
}
local cache = {}
local order = {}

function P.build(kind, count)
	local out = {}
	for i = 1, count do
		local name = NAMES[(i % #NAMES) + 1]
		if PALETTE[kind] then
			out[i] = name .. "_" .. kind
		else
			out[i] = name
		end
	end
	cache[kind] = out
	return "MARKER_M_build"
end

function P.lookup(kind)
	if cache[kind] then
		return "MARKER_M_hit"
	end
	local n = 0
	for _ in pairs(cache) do
		n = n + 1
	end
	if n > 8 then
		while n > 8 do
			n = n - 1
		end
		return "MARKER_M_evicted"
	end
	return "MARKER_M_miss"
end

function P.order(kind, weight)
	if weight <= 0 then
		return "MARKER_M_skip"
	end
	if PALETTE[kind] then
		order[#order + 1] = kind
		while #order > 4 do
			table.remove(order, 1)
		end
		return "MARKER_M_ordered"
	end
	return "MARKER_M_unknown"
end

function P.reset()
	cache = {}
	order = {}
	return "MARKER_M_reset"
end

return P
