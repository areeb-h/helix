// K3 — Basel series, C. The reference implementation for this kernel's anchor.
//   gcc -O3 -march=native k3_basel.c -o k3_basel
//
// Naive left-to-right f64 summation. NEVER add -ffast-math: it reassociates the
// accumulation and changes the answer (measured at N=1e6: 1.64493306684874074 vs the
// correct 1.64493306684877005). That order-sensitivity is the point of this kernel.
//
// Worth knowing, because "vectorized" sounds alarming here but is not: gcc -O3
// -march=native DOES vectorize part of this loop, and that is fine. Reading the asm,
// it emits `vdivpd` (packed divides — the 1/(x*x) terms computed 4-8 lanes at a time,
// an elementwise op where each lane is independently correctly-rounded, so no
// reassociation) but ZERO `vaddpd` and 15 `vaddsd` — the ADDITIONS stay scalar and
// strictly ordered. gcc may not reassociate a float reduction without -ffast-math, and
// it doesn't. Verified: -O3 -march=native, -O3 -fno-tree-vectorize and -O0 all print
// the same anchor, which also equals the unambiguously-serial CPython reference.
//
// -ffp-contract=off is deliberately NOT used: the only multiply (x*x) feeds a division,
// never an addition, so there is no `a*b+c` pattern for FMA to contract. Verified: the
// anchor is byte-identical under -ffp-contract=off and -ffp-contract=fast.
#include <stdio.h>
#include <stdlib.h>

int main(int argc, char **argv) {
    long n = (argc > 1) ? atol(argv[1]) : 100000000L;
    double s = 0.0;
    for (long k = 1; k <= n; k++) {
        double x = (double)k;
        s += 1.0 / (x * x);
    }
    printf("%.17f\n", s);
    return 0;
}
