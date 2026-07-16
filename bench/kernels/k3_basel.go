// K3 — Basel series, Go. go build k3_basel.go
//
// Naive left-to-right f64 summation. The Go spec permits fusing `x*y + z` into an FMA,
// but this loop has no such pattern (the multiply x*x feeds a division, not an add), so
// no fusion is possible and the result is bit-identical to C. Go never reassociates a
// float accumulation, so the loop stays serial.
package main

import (
	"fmt"
	"os"
	"strconv"
)

func main() {
	var n int64 = 100000000
	if len(os.Args) > 1 {
		if v, err := strconv.ParseInt(os.Args[1], 10, 64); err == nil {
			n = v
		}
	}
	s := 0.0
	for k := int64(1); k <= n; k++ {
		x := float64(k)
		s += 1.0 / (x * x)
	}
	fmt.Printf("%.17f\n", s)
}
