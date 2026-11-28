Fit a line to data

`points.csv` has two numeric columns, `x` and `y`. Write `fit.helix` so that
`helix run fit.helix` fits the least-squares line y = slope·x + intercept and prints
exactly three lines:

    slope: 2.0312
    intercept: -0.5000
    r2: 0.9876

each value with exactly four decimals, where `r2` is the coefficient of determination
(1 − residual sum of squares / total sum of squares).

It must keep working when `points.csv` is replaced by another file with the same columns.
