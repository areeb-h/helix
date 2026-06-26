# S4 — pattern matching, NumPy vectorized (np.select mirrors the arm order).
import numpy as np

N = 20000000
m = np.arange(N) % 12
w = np.select(
    [m == 0, (m >= 1) & (m <= 3), m == 11, m > 7],
    [7, 2, 5, 3],
    default=1,
)
print(int(w.sum()))
