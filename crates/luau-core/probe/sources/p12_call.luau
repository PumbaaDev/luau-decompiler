local function ca_call(f, a) return f(a) end
local function ca_method(o, a) return o:probeMethod(a) end
local function ca_vararg(...) local a, b = ... return a, b end
local function ca_pack(...) return { ... } end
local function ca_fwd(f, ...) return f(...) end
return { ca_call, ca_method, ca_vararg, ca_pack, ca_fwd }
