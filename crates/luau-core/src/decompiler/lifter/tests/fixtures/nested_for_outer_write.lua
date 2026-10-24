local isl = {}
local function pick(self, map, a, b)
	local key = nil
	if map then
		for _, v in isl.get(map, "P", self.Name) do
			if table.find(v.Tags, "Starter") then
				if v.Index.Key == "Pirate" and self.Parent.Name == "Pirates" then
					key = v.Index.Key
					break
				end
				if v.Index.Key == "Marine" and self.Parent.Name == "Marines" then
					key = v.Index.Key
					break
				end
			else
				key = v.Index.Key
			end
		end
	end
	print(key)
end
return pick
