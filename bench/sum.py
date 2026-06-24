s = 0
for i in range(10000000):
    s = (s + i) % 1000000007
print(s)
