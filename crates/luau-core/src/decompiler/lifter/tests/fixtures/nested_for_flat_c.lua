local function g(p)
	local t = { Friends = {} }
	local ok, pages = pcall(function()
		return p:GetFriendsAsync(1)
	end)
	if ok then
		while true do
			for _, v in ipairs(pages:GetCurrentPage()) do
				table.insert(t.Friends, { Username = v.Username, DisplayName = v.DisplayName, UserId = v.Id })
			end
			if pages.IsFinished then
				break
			end
			pages:AdvanceToNextPageAsync()
		end
	end
	return t
end
return g
