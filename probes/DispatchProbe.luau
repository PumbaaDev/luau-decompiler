--!nonstrict
-- Reproduces `rc_0274 Packet_Types` proto #135: a range-dispatch chain where
-- every arm ends in `return` and small `while` loops sit between the tests.
-- Each arm carries a distinct marker, so a missing marker names the dropped arm.
local P = {}
local size, len = 8, 0

function P.writeNumber(n)
	if n % 1 == 0 then
		if n >= 0 then
			if n < 256 then
				while size < len + 2 do size = size * 2 end
				return "MARKER_U8"
			elseif n < 65536 then
				while size < len + 3 do size = size * 2 end
				return "MARKER_U16"
			elseif n < 16777216 then
				while size < len + 4 do size = size * 2 end
				return "MARKER_U24"
			else
				while size < len + 5 do size = size * 2 end
				return "MARKER_U32"
			end
		else
			if n >= -256 then
				while size < len + 2 do size = size * 2 end
				return "MARKER_S8"
			elseif n >= -65536 then
				while size < len + 3 do size = size * 2 end
				return "MARKER_S16"
			else
				while size < len + 5 do size = size * 2 end
				return "MARKER_S32"
			end
		end
	else
		if n >= -340282346638528859811704183484516925440 then
			while size < len + 5 do size = size * 2 end
			return "MARKER_F32"
		else
			while size < len + 9 do size = size * 2 end
			return "MARKER_F64"
		end
	end
end

return P
