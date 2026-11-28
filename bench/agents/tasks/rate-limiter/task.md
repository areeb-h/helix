A token-bucket rate limiter

Write `limiter.helix`, a module other Helix files import, exporting two functions:

- `new(capacity, refill_ms)` — a limiter whose bucket holds at most `capacity` tokens and
  starts full. One token is added every `refill_ms` milliseconds.
- `allow(limiter, now_ms)` — a request arrives at time `now_ms` (milliseconds; never
  earlier than the previous call's). Returns a pair `(limiter, ok)`: the limiter to use for
  the next request, and `true` if the request is allowed. An allowed request uses one token;
  a refused one uses none.

Time is measured from the first call to `allow` (the bucket is full at that moment).
Refilling is in whole tokens: time that has not yet earned a whole token is not lost, it
counts toward the next one; while the bucket is full, time earns nothing.

For example, with `new(2, 1000)`: requests at 0, 0 and 0 give true, true, false; one more
at 999 gives false; one at 1000 gives true.
