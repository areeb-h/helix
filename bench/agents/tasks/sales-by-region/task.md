Total sales per region from a CSV

`sales.csv` has one row per sale, with columns `date`, `region`, `product` and `amount`
(the amount in dollars; refunds are negative amounts).

Write `report.helix` so that `helix run report.helix` prints one line per region:

    North: 1234.50

- the region's name, a colon and a space, then the total of its amounts with exactly two
  decimals;
- the region with the largest total first; regions with equal totals in alphabetical order;
- nothing else on stdout.

It must keep working when `sales.csv` is replaced by next month's file, which has the same
columns but different regions and rows.
