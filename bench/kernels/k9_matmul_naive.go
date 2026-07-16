// K8 — dense f64 matmul, naive ijk triple loop (Go, stdlib only).
//   go build k8_matmul.go        # default n=512
//
//   A[i][j] = ((i*j) % 100) * 0.5      B[i][j] = ((i+j) % 100) * 0.25
//   anchor  = C[0][0] + C[n-1][n-1] + C[n/2][n/2]   printed as %.6f
//
// Same algorithm, same ijk order, same materialized row-major arrays as the C file.
// The Go spec permits fusing x*y+z into an FMA; it cannot move the anchor, because
// every product and partial sum here is an exact multiple of 1/8 well inside f64's
// mantissa, so contraction and accumulation order are both irrelevant to the value.
//
// COMPILER-FLAG ASYMMETRY, disclosed: this box reports `go env GOAMD64` = v1, so the
// default `go build` targets baseline amd64 — no AVX2, no FMA — while the C sibling
// is built -march=native and the Rust one -C target-cpu=native, both of which use the
// AVX2+FMA this CPU advertises. That is not an equal-flags comparison on its face.
// Measured, however, it makes no difference to THIS kernel: rebuilding with
// GOAMD64=v3 gives 0.331s against v1's 0.317s at n=512 (min of 5, idle 6-core box) —
// i.e. no gain, within noise. A naive ijk inner loop is bound by the cache-missing
// strided B[k*n+j] load and the serial dependency on `s`, not by FLOPs, and Go's
// compiler does not vectorize it at either level. So the flag gap is real and worth
// knowing, but it is not what makes the Go number what it is. (Go 1.26.1.)
//
// Single-goroutine on purpose: the C/Rust/Helix-naive siblings are single-threaded,
// so parallelising here would be comparing different things.
//
// One harness note: any timing of this file should discard a warmup run and report a
// min-of-N, as bench/kernels/final_k8.py does. Measured here, a first run straight
// after `go build` was 0.32s at 105% CPU against a warm min-of-5 of 0.327s — i.e. no
// cold-start penalty was reproducible on this box (ext4, binary in /tmp), across two
// fresh-build trials. Warm up anyway; a cold binary is an I/O measurement, not a
// language measurement.
package main

import (
	"fmt"
	"os"
	"strconv"
)

func main() {
	n := 512
	if len(os.Args) > 1 {
		v, err := strconv.Atoi(os.Args[1])
		if err != nil || v <= 0 {
			fmt.Fprintln(os.Stderr, "n must be a positive integer")
			os.Exit(1)
		}
		n = v
	}

	a := make([]float64, n*n)
	b := make([]float64, n*n)
	c := make([]float64, n*n)

	for i := 0; i < n; i++ {
		for j := 0; j < n; j++ {
			a[i*n+j] = float64((i*j)%100) * 0.5
			b[i*n+j] = float64((i+j)%100) * 0.25
		}
	}

	// naive ijk: inner loop strides B by n (cache-hostile), matching every sibling.
	for i := 0; i < n; i++ {
		for j := 0; j < n; j++ {
			s := 0.0
			for k := 0; k < n; k++ {
				s += a[i*n+k] * b[k*n+j]
			}
			c[i*n+j] = s
		}
	}

	anchor := c[0] + c[(n-1)*n+(n-1)] + c[(n/2)*n+(n/2)]
	fmt.Printf("%.6f\n", anchor)
}
