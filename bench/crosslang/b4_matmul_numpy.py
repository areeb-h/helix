import numpy as np

n = 1024
a = np.ones((n, n))
print((a @ a).sum())
