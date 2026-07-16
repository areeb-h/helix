// K5 — Monte Carlo pi, C. The xorshift64 reference stream (uint64, native
// unsigned shifts). Anchor = raw hit count.
//   gcc -O3 -march=native -ffp-contract=off k5_montecarlo.c -o k5_montecarlo
//
// On -ffp-contract=off: this is insurance, NOT a fudge, and the distinction was
// measured rather than assumed. Without the flag gcc really does fuse
// `x*x + y*y` into one `vfmadd132sd` (checked in the disassembly: 1 FMA with
// contraction on, 0 with it off), which skips the rounding of x*x that the other
// languages perform. But the fused and unfused builds print the SAME anchor at
// every N tested up to 1e8 -- flipping `< 1.0` needs a sample within ~1 ulp of
// the unit circle, i.e. ~2^-52 per draw, so it essentially never fires. The flag
// stays because it makes cross-language agreement structural instead of lucky,
// and keeps it that way if anyone later changes N or the seed. It is not what
// makes the anchors match today.
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>

int main(int argc, char **argv) {
    long long n = 100000000LL;
    if (argc > 1) n = atoll(argv[1]);

    uint64_t s = 88172645463325252ULL;
    long long hits = 0;
    for (long long i = 0; i < n; i++) {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        double x = (double)(s >> 11) / 9007199254740992.0; // 2^53 — exact
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        double y = (double)(s >> 11) / 9007199254740992.0;
        if (x * x + y * y < 1.0) hits++;
    }
    printf("%lld\n", hits);
    return 0;
}
