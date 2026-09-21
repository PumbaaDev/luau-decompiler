-- Simple smoke test: arithmetic, locals, conditional, loop, function call.
local sum = 0
for i = 1, 10 do
    sum = sum + i
end
print("sum =", sum)
if sum == 55 then
    print("ok")
else
    print("fail")
end
