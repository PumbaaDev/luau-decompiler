-- Recursive function + closures.
local function fib(n)
    if n < 2 then return n end
    return fib(n - 1) + fib(n - 2)
end

print(fib(10))

local function counter()
    local n = 0
    return function()
        n = n + 1
        return n
    end
end

local c = counter()
print(c(), c(), c())
