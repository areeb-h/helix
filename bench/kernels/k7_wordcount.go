// K7 — word frequency over a generated corpus, Go (std only). go build k7_wordcount.go
// map[string]int64 + `counts[w]++` — the idiom every Go user writes.
//
// Corpus = the k5 xorshift64 reference stream (seed 88172645463325252), one draw per
// word, bit-identical in all six languages. See k7_wordcount.c for why the previous
// `(i*2654435761) % 10000` generator was a non-test. The `& 9007199254740991` is a no-op
// on uint64 (s>>11 is already 53 bits); it is spelled out to match Helix, where it is
// load-bearing.
package main

import (
	"fmt"
	"os"
	"strconv"
)

const (
	defaultN = 5000000
	modulus  = 10000
)

func main() {
	n := int64(defaultN)
	if len(os.Args) > 1 {
		if v, err := strconv.ParseInt(os.Args[1], 10, 64); err == nil {
			n = v
		}
	}

	counts := make(map[string]int64)
	var s uint64 = 88172645463325252
	for i := int64(0); i < n; i++ {
		s ^= s << 13
		s ^= s >> 7
		s ^= s << 17
		id := int64((s>>11)&9007199254740991) % modulus
		w := "w" + strconv.FormatInt(id, 10)
		counts[w]++
	}

	// max stays 0 for an empty corpus (N=0) — same answer as everyone else.
	var max int64
	for _, c := range counts {
		if c > max {
			max = c
		}
	}
	fmt.Println(max)
}
