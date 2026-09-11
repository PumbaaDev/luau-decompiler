local function cl_counter()
	local n = 0
	return function() n = n + 1 return n end
end
local function cl_scoped(t)
	for i = 1, 3 do
		local v = i
		t[i] = function() return v end
	end
	return t
end
local function cl_plain()
	return function() return 1 end
end
return { cl_counter, cl_scoped, cl_plain }
