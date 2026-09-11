local function gl_set(v) ProbeGlobalSlot = v end
local function gl_get() return ProbeGlobalSlot end
return { gl_set, gl_get }
