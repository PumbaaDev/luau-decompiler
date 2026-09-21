local function mf_count(limit, acc) for step = 1, limit do acc = acc + step end return acc end
local function mf_seq(box, acc) for idx in ipairs(box) do acc = acc + idx end return acc end
local function mf_map(box, acc) for key in pairs(box) do acc = acc + 1 end return acc end
local function mf_raw(box, acc) for key in next, box do acc = acc + 1 end return acc end
local function mf_custom(box, acc) for one in box.walk, box, nil do acc = acc + 1 end return acc end
local function mf_bind()
	local total = 0
	return function() total = total + 1 return total end
end
local function mf_each(box)
	for idx = 1, 3 do
		local held = idx
		box[idx] = function() return held end
	end
	return box
end
local function mf_free() return function() return 0 end end
local function mf_apply(fn, one) return fn(one) end
local function mf_send(obj, one) return obj:mirrorMethod(one) end
local function mf_spread(...) local one, two = ... return one, two end
local function mf_collect(...) return { ... } end
local function mf_relay(fn, ...) return fn(...) end
local function mf_abs(one) return math.abs(one) end
local function mf_max(one, two) return math.max(one, two) end
local function mf_slice(text) return string.sub(text, 3) end
local function mf_clamp(one, two, three) return math.clamp(one, two, three) end
local function mf_widest(...) return math.max(...) end
local function mf_store(v) MirrorGlobalSlot = v end
local function mf_load() return MirrorGlobalSlot end
return {
	mf_count, mf_seq, mf_map, mf_raw, mf_custom, mf_bind, mf_each, mf_free,
	mf_apply, mf_send, mf_spread, mf_collect, mf_relay,
	mf_abs, mf_max, mf_slice, mf_clamp, mf_widest, mf_store, mf_load,
}
