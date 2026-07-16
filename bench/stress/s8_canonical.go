// S8 — canonical k-mer spectrum, Go (std only). go run s8_canonical.go
package main

import (
	"bufio"
	"fmt"
	"os"
)

const K = 10

func revcomp(code uint64, k int) uint64 {
	var rc uint64
	for i := 0; i < k; i++ {
		rc = (rc << 2) | (3 - (code & 3))
		code >>= 2
	}
	return rc
}

func main() {
	f, _ := os.Open("data/genome.fa")
	defer f.Close()
	seq := make([]byte, 0, 1<<20)
	sc := bufio.NewScanner(f)
	sc.Buffer(make([]byte, 1<<20), 1<<20)
	for sc.Scan() {
		line := sc.Bytes()
		if len(line) > 0 && line[0] == '>' {
			continue
		}
		seq = append(seq, line...)
	}

	var mask uint64 = (1 << (2 * K)) - 1
	var code uint64
	counts := make(map[uint64]uint32)
	for i, b := range seq {
		var two uint64
		switch b {
		case 'A':
			two = 0
		case 'C':
			two = 1
		case 'G':
			two = 2
		case 'T':
			two = 3
		default:
			continue
		}
		code = ((code << 2) | two) & mask
		if i+1 >= K {
			rc := revcomp(code, K)
			canon := code
			if rc < canon {
				canon = rc
			}
			counts[canon]++
		}
	}

	var total uint64
	var max uint32
	for _, c := range counts {
		total += uint64(c)
		if c > max {
			max = c
		}
	}
	fmt.Printf("distinct=%d\n", len(counts))
	fmt.Printf("total=%d\n", total)
	fmt.Printf("max=%d\n", max)
}
