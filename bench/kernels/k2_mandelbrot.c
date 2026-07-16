// K2 — escape-time Mandelbrot, 1200x1200 grid, max 100 iterations, over
// x in [-2.1, 0.6] and y in [-1.2, 1.2]. Anchor = sum of every pixel's iteration count.
//
// BUILD:
//   gcc -O3 -march=native -ffp-contract=off k2_mandelbrot.c -o k2_mandelbrot
//
// WHY -ffp-contract=off — read this before removing it, and note what it does NOT claim.
//
// With -march=native and contraction at gcc's default (`fast`), gcc really does fuse
// `2.0*zr*zi + ci` into an FMA. That is visible in the disassembly of this exact file:
// the default build emits one `vfmadd132sd` in the inner loop where the -ffp-contract=off
// build emits a `vmulsd`/`vaddsd` pair. (`zr*zr - zi*zi` is left alone by gcc 13.)
// An FMA keeps the product at full width instead of rounding it to double first, so a
// pixel sitting within an ulp of the |z|^2 > 4.0 escape boundary can fall on the other
// side of the test and take a different iteration count — which would change the anchor.
//
// MEASURED, so nobody has to take the above on faith: for THIS kernel's parameters that
// risk does not actually materialise. All seven builds — contracted gcc, -ffp-contract=off
// gcc, Rust, Go GOAMD64=v1, Go GOAMD64=v3 (which emits 7 FMAs), CPython and Helix — print
// the same anchor at every one of 83 grids tested from 1 to 2000, including the default
// 1200 (40715058). The drift is a real hazard of the construct, not an observed fact about
// this grid; do not cite a divergence here that this kernel does not exhibit.
//
// The flag stays anyway, for two honest reasons: (1) it pins C to the same sequence of
// IEEE double roundings as the other four languages, which is the arithmetic-parity rule
// this suite is built on rather than a lucky coincidence to be rediscovered on each new
// gcc/CPU; (2) it is free — best-of-3 at grid 1200 is 0.08s either way, so it costs C
// nothing and hands Helix no advantage. The other four do not contract by default:
// Rust/LLVM defaults to fp-contract=off, Go emits no automatic FMA on amd64 under the
// default GOAMD64=v1, and CPython has no FMA at all.
#include <stdio.h>
#include <stdlib.h>

int main(int argc, char **argv) {
    long g = (argc > 1) ? atol(argv[1]) : 1200;
    long long total = 0;
    for (long y = 0; y < g; y++) {
        double ci = -1.2 + (double)y * 2.4 / (double)g;
        for (long x = 0; x < g; x++) {
            double cr = -2.1 + (double)x * 2.7 / (double)g;
            double zr = 0.0, zi = 0.0;
            long i = 0;
            while (i < 100 && zr * zr + zi * zi <= 4.0) {
                double t = zr * zr - zi * zi + cr;
                zi = 2.0 * zr * zi + ci;
                zr = t;
                i++;
            }
            total += i;
        }
    }
    printf("%lld\n", total);
    return 0;
}
