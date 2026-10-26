local filters = {}
local function clean(msg, text, use)
	local changed = false
	if not use then
		repeat
			changed = false
			for _, entry in pairs(filters) do
				for _, word in pairs(entry.Words) do
					local at = string.find(string.lower(text), word)
					if at then
						changed = true
						text = string.sub(text, 1, at - 1) .. entry.Result .. string.sub(text, at + #word)
					end
				end
			end
		until not changed
	end
	if text == msg.Text then
		return
	end
	return text
end
return clean
