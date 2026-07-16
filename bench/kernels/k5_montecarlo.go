// K5 — Monte Carlo pi, Go. The xorshift64 reference stream (uint64, native
// unsigned shifts). Anchor = raw hit count.
//   go build k5_montecarlo.go
// The float64(...) conversions around x*x and y*y are portability insurance,
// and the claim was measured, not assumed. The Go spec lets an implementation
// fuse `x*x + y*y` into a single FMA (it does on arm64/ppc64/s390x); an explicit
// conversion to float64 forces the intermediate to round, pinning the arithmetic
// to plain IEEE-754 on every GOARCH. On the amd64 box this was verified on, Go
// emits no FMA either way (0 FMADD in the disassembly with and without the
// conversions) and both builds print the same anchor -- so here they cost
// nothing and change nothing. They exist so the anchor still matches C/Rust/
// Helix if this suite is ever run on arm64.
package main

import (
	"fmt"
	"os"
	"strconv"
)

func main() {
	var n int64 = 100000000
	if len(os.Args) > 1 {
		v, err := strconv.ParseInt(os.Args[1], 10, 64)
		if err != nil {
			fmt.Fprintln(os.Stderr, "N must be a non-negative integer")
			os.Exit(1)
		}
		n = v
	}

	var s uint64 = 88172645463325252
	var hits int64
	for i := int64(0); i < n; i++ {
		s ^= s << 13
		s ^= s >> 7
		s ^= s << 17
		x := float64(s>>11) / 9007199254740992.0 // 2^53 — exact
		s ^= s << 13
		s ^= s >> 7
		s ^= s << 17
		y := float64(s>>11) / 9007199254740992.0
		if float64(x*x)+float64(y*y) < 1.0 {
			hits++
		}
	}
	fmt.Println(hits)
}
