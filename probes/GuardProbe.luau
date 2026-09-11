--!nonstrict
-- The shapes the dispatch fix must NOT damage. Modelled on the files that
-- regressed when the branch case ran ungated: `lv_0910 VRCamera` (a chain of
-- argument guards), `lv_0475 Equip_Accessory` (a table built in a loop),
-- `lv_0928 ClickToMoveController` (calls after a guard).
local P = {}

-- lv_0910 shape: a chain of argument guards, each nested.
function P.guards(mode, head, flag)
	if mode == "offtrack" then
		if not flag then
			if head then
				return "MARKER_G_offtrack_head"
			end
			return "MARKER_G_offtrack_nohead"
		end
		return "MARKER_G_offtrack_flag"
	end
	if mode == "force" then
		return "MARKER_G_force"
	end
	return "MARKER_G_default"
end

-- An ordinary early-return guard clause: nothing after it belongs to a sibling.
function P.earlyReturn(x)
	if not x then
		return "MARKER_E_nil"
	end
	local y = x * 2
	return "MARKER_E_ok" .. tostring(y)
end

-- lv_0475 shape: a table built inside a loop and read afterwards.
function P.tableInLoop(items)
	local out = {}
	for i, v in pairs(items) do
		out[i] = v
	end
	if #out > 0 then
		return "MARKER_T_filled"
	end
	return "MARKER_T_empty"
end

-- Short-circuit selection: must stay a value join, not become control flow.
function P.shortCircuit(t)
	local n = t and t.n or -1
	if n < 0 then
		return "MARKER_SC_default"
	end
	return "MARKER_SC_value"
end

-- A loop with a conditional break, then calls after it.
function P.loopBreak(list, limit)
	local total = 0
	for _, v in pairs(list) do
		if v > limit then
			break
		end
		total = total + v
	end
	if total == 0 then
		return "MARKER_L_zero"
	end
	return "MARKER_L_total"
end

return P
