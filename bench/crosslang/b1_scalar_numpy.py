import numpy as np

x = np.arange(10_000_000, dtype=np.int64)
print(int(((x * x) % 1000).sum()))
