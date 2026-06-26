import numpy as np

n = 256
a = np.full((n, n), 1.0)
np.fill_diagonal(a, 100.0)
print(np.linalg.inv(a).sum())
