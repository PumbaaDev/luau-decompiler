local M = {}
function M.a(p, q, r)
	if p then
		if not q then
			return
		end
		print(1)
	else
		print(2)
	end
	print(3)
end
function M.b(s)
	return #s % 3 == 0 and s:sub(2) or s:upper()
end
function M.c(self, f)
	local frame = self.Frame
	if frame ~= nil then
		self.X = 1
		if f then
			frame.P = 2
			return
		end
		frame:T(3)
	end
end
function M.Pop(self,p,q) if p then if type(p)=="number" then p=UDim2.new(p,0,p,0) end else p=UDim2.new(0.7,0,0.7,0) end self.Size=p end
function M.Nest(p) if p then if p.a then print(1) else print(2) end else print(3) end print(4) end
function M.NestNoElse(p) if p then if p.a then print(1) end else print(3) end print(4) end
return M
