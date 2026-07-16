// K4 — all-pairs Manhattan distance sum, C.
//   gcc -O3 -march=native k4_allpairs.c -o k4_allpairs
// Pure i64 arithmetic — no floats, so no -ffp-contract concern.
// N comes from argv[1] (default 15000) so the compiler cannot precompute the sum.
//
// VECTORIZATION (machine-specific — re-check it on your box, do not quote it as
// a fixed property of C). With -march=native on this box (AMD Ryzen 7 7700X,
// Zen 4, avx512f/bw/dq/vl), gcc 13.3 vectorizes the inner loop to AVX-512:
//   vpsubq (%rax),%zmm4,%zmm0 / vpabsq %zmm0,%zmm0 / vpaddq %zmm0,%zmm1,%zmm1
// i.e. 8 i64 lanes per instruction, with pts[i] broadcast into %zmm4 (the hoist
// is free — hand-hoisting it measures -1.5%, noise). Narrower ymm/xmm copies of
// the body follow as the loop tail. On a machine without AVX-512 gcc emits the
// AVX2 4-lane form instead and this reference gets slower — the Helix-vs-C ratio
// quoted in k4_allpairs.helix (~7x) is therefore a property of THIS box.
// Verify with:  objdump -d k4_allpairs | sed -n '/<main>:/,/^$/p' | grep vpabsq
#include <stdio.h>
#include <stdlib.h>

int main(int argc, char **argv) {
    long long n = (argc > 1) ? atoll(argv[1]) : 15000;
    long long *pts = malloc((size_t)n * sizeof(long long));
    if (!pts) return 1;
    for (long long i = 0; i < n; i++) pts[i] = (i * 2654435761LL) % 1000;

    long long total = 0;
    for (long long i = 0; i < n; i++) {
        for (long long j = i + 1; j < n; j++) {
            long long d = pts[i] - pts[j];
            total += (d < 0) ? -d : d;
        }
    }
    printf("%lld\n", total);
    free(pts);
    return 0;
}
