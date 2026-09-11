-- Slightly richer fixture with distinctive strings + identifiers we can grep
-- the protected output for.
local KillAuraEnabled = false
local FlyMaxSpeed = 200
local secretToken = "X-Secret-AB7C9D"

local function ToggleKillAura(value)
    KillAuraEnabled = value
    print("KillAura now:", KillAuraEnabled, "token:", secretToken)
end

local function ComputeDamage(base, multiplier)
    return base * multiplier + FlyMaxSpeed
end

ToggleKillAura(true)
print(ComputeDamage(10, 3.5))
