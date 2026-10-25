local function change(self, ev)
	local cur = { self }
	local nxt = {}
	local seen = {}
	repeat
		local done = true
		for _, v in cur do
			for k2 in v.deps do
				if k2.state == "valid" then
					done = false
					table.insert(nxt, k2)
					table.insert(seen, k2)
					continue
				end
				if k2.state == "busy" then
					return ev("loop")
				end
			end
		end
		cur, nxt = nxt, cur
		table.clear(nxt)
	until done
	return seen
end
return change
