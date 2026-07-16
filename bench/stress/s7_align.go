// S7 — local pairwise alignment, Go (std only). Score-only Gotoh affine DP,
// replicating src/align.rs local mode. go run s7_align.go
package main

import (
	"bufio"
	"fmt"
	"os"
)

const (
	NEG   = -(1 << 30)
	MATCH = 1
	MIS   = -1
	GEXT  = -1
	FG    = -6 // gap_open + gap_extend
)

func max2(a, b int32) int32 {
	if a > b {
		return a
	}
	return b
}

func localScore(x, y []byte) int32 {
	m := len(y)
	prevMM := make([]int32, m+1)
	prevXX := make([]int32, m+1)
	prevYY := make([]int32, m+1)
	for j := range prevMM {
		prevMM[j], prevXX[j], prevYY[j] = NEG, NEG, NEG
	}
	prevMM[0] = 0
	for j := 1; j <= m; j++ {
		prevYY[j] = max2(prevMM[j-1]+FG, prevYY[j-1]+GEXT)
	}
	var best int32 = 0
	curMM := make([]int32, m+1)
	curXX := make([]int32, m+1)
	curYY := make([]int32, m+1)
	for _, xi := range x {
		curMM[0], curXX[0], curYY[0] = NEG, max2(prevMM[0]+FG, prevXX[0]+GEXT), NEG
		for j := 1; j <= m; j++ {
			var s int32 = MIS
			if xi == y[j-1] {
				s = MATCH
			}
			d := max2(max2(prevMM[j-1], prevXX[j-1]), prevYY[j-1])
			mval := d + s
			if mval < 0 {
				mval = 0
			}
			curMM[j] = mval
			if mval > best {
				best = mval
			}
			curXX[j] = max2(prevMM[j]+FG, prevXX[j]+GEXT)
			curYY[j] = max2(curMM[j-1]+FG, curYY[j-1]+GEXT)
		}
		prevMM, curMM = curMM, prevMM
		prevXX, curXX = curXX, prevXX
		prevYY, curYY = curYY, prevYY
	}
	return best
}

func readFasta(path string) [][]byte {
	f, _ := os.Open(path)
	defer f.Close()
	sc := bufio.NewScanner(f)
	sc.Buffer(make([]byte, 1<<20), 1<<20)
	var seqs [][]byte
	var cur []byte
	for sc.Scan() {
		line := sc.Bytes()
		if len(line) > 0 && line[0] == '>' {
			if len(cur) > 0 {
				seqs = append(seqs, cur)
				cur = nil
			}
		} else {
			cur = append(cur, line...)
		}
	}
	if len(cur) > 0 {
		seqs = append(seqs, cur)
	}
	return seqs
}

func main() {
	reference := readFasta("data/ref.fa")[0]
	var total, hits int64
	var max int32
	for _, q := range readFasta("data/queries.fa") {
		s := localScore(q, reference)
		total += int64(s)
		if s >= 40 {
			hits++
		}
		if s > max {
			max = s
		}
	}
	fmt.Printf("total=%d\n", total)
	fmt.Printf("hits=%d\n", hits)
	fmt.Printf("max=%d\n", max)
}
