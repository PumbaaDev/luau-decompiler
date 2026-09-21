local function un_not(x) return not x end
local function un_minus(x) return -x end
local function un_len(x) return #x end
return { un_not, un_minus, un_len }
