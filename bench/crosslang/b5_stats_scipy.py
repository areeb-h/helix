import numpy as np
from scipy import stats

x = np.arange(1_000_000, dtype=np.float64)
y = x * 2 + (x % 7)
print(stats.pearsonr(x, y).statistic)
print(stats.linregress(x, y))
