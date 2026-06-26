# S5 — fused polynomial pipeline, NumPy (materializes the intermediate arrays).
import numpy as np

N = 50000000
x = np.arange(N, dtype=np.int64)
p = ((x % 97) * 31 + (x % 13) * 7 + 3) % 1000
print(int(p[p % 2 == 0].sum()))
