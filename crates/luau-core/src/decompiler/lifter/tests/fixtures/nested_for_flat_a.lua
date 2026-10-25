local function GetHumanoid(p)
	while true do
		local h = p:FindFirstChild("Humanoid")
		if h then
			if h:IsA("Humanoid") then
				return h
			end
			for _, v in pairs(p:GetChildren()) do
				if v.Name == "Humanoid" and v:IsA("Humanoid") then
					return v
				end
			end
		end
		if p.Parent and p.Parent:IsDescendantOf(workspace) then
			p = p.Parent
		else
			return nil
		end
	end
end
return GetHumanoid
