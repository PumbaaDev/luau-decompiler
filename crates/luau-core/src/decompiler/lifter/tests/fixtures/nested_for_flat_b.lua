local function walk(self, target, list, flag)
	local best = 0
	if flag then
		local bi = 1
		local bd = math.huge
		local pos = self.hrp.Position
		for i, p in ipairs(list) do
			local d = (pos - p.Position).magnitude
			if (pos - p.Position).magnitude < bd then
				bd = d
				bi = i
			else
				break
			end
		end
		best = bi
	end
	print(best, target)
end
return walk
