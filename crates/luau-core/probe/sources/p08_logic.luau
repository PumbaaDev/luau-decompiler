local function lg_and(a, b) return a and b end
local function lg_or(a, b) return a or b end
local function lg_andk(a) return a and 5 end
local function lg_ork(a) return a or 5 end
return { lg_and, lg_or, lg_andk, lg_ork }
