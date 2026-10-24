local tbl = {}
local function a()
	while task.wait(0.5) do
		tbl.Update()
	end
end
local function b(f)
	while f() do
		tbl.Update()
	end
end
local function c()
	while tbl.Ready:GetAttribute("On") do
		tbl.Update()
	end
end
return { a = a, b = b, c = c, tbl = tbl }
