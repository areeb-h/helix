// K4 — all-pairs Manhattan distance sum, Go.
//   go build k4_allpairs.go
// N comes from argv[1] (default 15000) so the compiler cannot precompute the sum.
package main

import (
	"fmt"
	"os"
	"strconv"
)

func main() {
	var n int64 = 15000
	if len(os.Args) > 1 {
		v, err := strconv.ParseInt(os.Args[1], 10, 64)
		if err != nil {
			panic(err)
		}
		n = v
	}

	pts := make([]int64, n)
	for i := int64(0); i < n; i++ {
		pts[i] = (i * 2654435761) % 1000
	}

	var total int64
	for i := int64(0); i < n; i++ {
		for j := i + 1; j < n; j++ {
			d := pts[i] - pts[j]
			if d < 0 {
				d = -d
			}
			total += d
		}
	}
	fmt.Println(total)
}
