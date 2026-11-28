Customer totals from an orders export

`orders.json` is an array of orders. Each order has an `id`, a `customer`, a `status`
(`"paid"`, `"shipped"` or `"cancelled"`) and `items`, an array of `{sku, qty, price}`.

Write `totals.helix` so that `helix run totals.helix` writes `totals.json`: a JSON object
with one key per customer who has at least one order that is not cancelled, whose value is
that customer's total spend — the sum of `qty × price` over all items of their
non-cancelled orders — rounded to two decimals. Cancelled orders count for nothing.
Some orders have no items; they total 0.

It must keep working when `orders.json` is replaced by a newer export.
