// S4 — pattern matching, Go (switch-true mirrors the arm order). go run s4_match.go
package main

import "fmt"

func weight(m int64) int64 {
	switch {
	case m == 0:
		return 7
	case m >= 1 && m <= 3:
		return 2
	case m == 11:
		return 5
	case m > 7:
		return 3
	default:
		return 1
	}
}

func main() {
	var n int64 = 20_000_000
	var s int64
	for k := int64(0); k < n; k++ {
		s += weight(k % 12)
	}
	fmt.Println(s)
}
