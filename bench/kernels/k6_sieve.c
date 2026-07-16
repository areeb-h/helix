// K6 — count the primes below N (default 10,000,000). Anchor 664579 = pi(10^7).
// Sieve of Eratosthenes over a byte array. Integer-only: no -ffp-contract needed.
//   gcc -O3 -march=native k6_sieve.c -o k6_sieve
// N comes from argv so the compiler cannot precompute the answer.
//
// FAIRNESS — the Helix column does not compile a sieve at all: it calls a native
// `primes()` builtin (see k6_sieve.helix). Helix's 0.020 s ties this column's
// 0.017 s because both are native Eratosthenes, NOT because Helix compiles this
// loop well. The pure-Helix version of this kernel is k6_sieve_trial.helix, at 97 s.
#include <stdio.h>
#include <stdlib.h>

int main(int argc, char **argv) {
    long n = (argc > 1) ? atol(argv[1]) : 10000000L;
    if (n <= 2) { printf("0\n"); return 0; }

    // composite[p] marks p as composite. Counts primes p with 2 <= p < n.
    unsigned char *composite = calloc((size_t)n, 1);
    if (!composite) { fprintf(stderr, "alloc failed\n"); return 1; }

    long count = 0;
    for (long p = 2; p < n; p++) {
        if (!composite[p]) {
            count++;
            // p*p <= ~1e14 for n <= 1e7: fits in long. When p*p >= n this is a no-op.
            for (long m = p * p; m < n; m += p) composite[m] = 1;
        }
    }

    printf("%ld\n", count);
    free(composite);
    return 0;
}
