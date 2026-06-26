# S5 — fused polynomial pipeline, pure CPython.
def poly(x):
    return ((x % 97) * 31 + (x % 13) * 7 + 3) % 1000


N = 50000000
total = 0
for x in range(N):
    p = poly(x)
    if p % 2 == 0:
        total += p
print(total)
