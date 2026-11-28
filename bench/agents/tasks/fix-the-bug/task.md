Fix two reported bugs in a statistics module

`stats.helix` is a small module that other code imports. Two bugs have been reported:

1. `median([1, 2, 3, 4])` gives `3`. The median of an even number of values is the mean of
   the two middle ones, so this should be `2.5`.
2. `mean([1, 2])` gives `1`. It should be `1.5`.

Fix both. Do not change what the module's other functions do, and do not rename or remove
anything it exports. `stats_test.helix` has the existing tests; keep them passing, and add
tests for these two bugs.
