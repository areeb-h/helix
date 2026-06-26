import pandas as pd

print(pd.read_csv("data/big.csv").groupby("group")["value"].mean())
