// K8 — dense f64 matmul, naive ijk triple loop (C, std only).
//   gcc -O3 -march=native k8_matmul.c -o k8_matmul
//
//   A[i][j] = ((i*j) % 100) * 0.5      B[i][j] = ((i+j) % 100) * 0.25
//   anchor  = C[0][0] + C[n-1][n-1] + C[n/2][n/2]   printed as %.6f
//
// No -ffp-contract=off here, deliberately: every A entry is a multiple of 1/2 and
// every B entry a multiple of 1/4, so every product is an exact multiple of 1/8 and
// every partial sum is a multiple of 1/8 bounded by n*49.5*24.75 (5018112 eighths =
// 23 bits at n=512) — comfortably inside f64's 53-bit mantissa. Every intermediate
// is therefore EXACT, and fma(a,b,c) and a*b+c round identically when both are
// exact. FMA contraction cannot move the anchor. (Verified: -ffp-contract=off and
// -ffp-contract=fast produce the same string.)
//
// This is the reference the naive Helix/Rust/Go/CPython files match. It is NOT the
// right comparison for k8_matmul.helix's tensor path, which calls a blocked GEMM;
// naive-C vs BLAS-Helix measures "who calls a real GEMM", not code generation.
#include <stdio.h>
#include <stdlib.h>

int main(int argc, char **argv) {
    long n = (argc > 1) ? strtol(argv[1], NULL, 10) : 512;
    if (n <= 0) { fprintf(stderr, "n must be positive\n"); return 1; }

    double *a = malloc((size_t)n * (size_t)n * sizeof(double));
    double *b = malloc((size_t)n * (size_t)n * sizeof(double));
    double *c = malloc((size_t)n * (size_t)n * sizeof(double));
    if (!a || !b || !c) { fprintf(stderr, "out of memory\n"); return 1; }

    for (long i = 0; i < n; i++) {
        for (long j = 0; j < n; j++) {
            a[i * n + j] = (double)((i * j) % 100) * 0.5;
            b[i * n + j] = (double)((i + j) % 100) * 0.25;
        }
    }

    // naive ijk: inner loop strides B by n (cache-hostile). Every sibling does the
    // same thing in the same order.
    for (long i = 0; i < n; i++) {
        for (long j = 0; j < n; j++) {
            double s = 0.0;
            for (long k = 0; k < n; k++) {
                s += a[i * n + k] * b[k * n + j];
            }
            c[i * n + j] = s;
        }
    }

    double anchor = c[0] + c[(n - 1) * n + (n - 1)] + c[(n / 2) * n + (n / 2)];
    printf("%.6f\n", anchor);

    free(a); free(b); free(c);
    return 0;
}
