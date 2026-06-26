// S5 — fused polynomial pipeline, Go. go run s5_fused.go
package main

import "fmt"

func poly(x int64) int64 {
	return ((x%97)*31 + (x%13)*7 + 3) % 1000
}

func main() {
	var n int64 = 50_000_000
	var total int64
	for x := int64(0); x < n; x++ {
		p := poly(x)
		if p%2 == 0 {
			total += p
		}
	}
	fmt.Println(total)
}
