// K6 — count the primes below N (default 10,000,000). Anchor 664579 = pi(10^7).
// Sieve of Eratosthenes over a byte array ([]bool is one byte per element).
//   go build k6_sieve.go
// N comes from argv so the compiler cannot precompute the answer.
// No GOAMD64 tuning: measured, it does not move this kernel (v1 0.0225 s, v3
// 0.0230 s, v4 0.0233 s — a strided byte-store loop does not vectorize).
//
// FAIRNESS — the Helix column does not compile a sieve at all: it calls a native
// `primes()` builtin (see k6_sieve.helix). Helix's 0.020 s and this column's
// 0.023 s are both native Eratosthenes, NOT a statement about Helix's codegen.
// The pure-Helix version of this kernel is k6_sieve_trial.helix, at 97 s.
package main

import (
	"fmt"
	"os"
	"strconv"
)

func main() {
	n := int64(10000000)
	if len(os.Args) > 1 {
		if v, err := strconv.ParseInt(os.Args[1], 10, 64); err == nil {
			n = v
		}
	}
	if n <= 2 {
		fmt.Println(0)
		return
	}

	// composite[p] marks p as composite. Counts primes p with 2 <= p < n.
	composite := make([]bool, n)
	var count int64
	for p := int64(2); p < n; p++ {
		if !composite[p] {
			count++
			// When p*p >= n the loop body never runs.
			for m := p * p; m < n; m += p {
				composite[m] = true
			}
		}
	}

	fmt.Println(count)
}
