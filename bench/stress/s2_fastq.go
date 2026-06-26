// S2 — FASTQ GC + Phred, Go (std only). go run s2_fastq.go
package main

import (
	"bufio"
	"fmt"
	"math"
	"os"
)

func main() {
	f, _ := os.Open("data/reads.fq")
	defer f.Close()
	sc := bufio.NewScanner(f)
	sc.Buffer(make([]byte, 1<<20), 1<<20)

	var lines []string
	for sc.Scan() {
		lines = append(lines, sc.Text())
	}

	var n, bases, hiq uint64
	var gcsum, qsum float64
	for i := 0; i+3 < len(lines); i += 4 {
		seq := lines[i+1]
		qual := lines[i+3]
		l := float64(len(seq))
		n++
		bases += uint64(len(seq))
		gc := 0.0
		for j := 0; j < len(seq); j++ {
			if seq[j] == 'G' || seq[j] == 'C' {
				gc++
			}
		}
		gcsum += gc / l
		qs := 0.0
		for j := 0; j < len(qual); j++ {
			qs += float64(qual[j] - 33)
		}
		mq := qs / l
		qsum += mq
		if mq >= 30.0 {
			hiq++
		}
	}

	fmt.Printf("reads=%d\n", n)
	fmt.Printf("bases=%d\n", bases)
	fmt.Printf("gc4=%d\n", int64(math.Floor(gcsum/float64(n)*10000+0.5)))
	fmt.Printf("q4=%d\n", int64(math.Floor(qsum/float64(n)*10000+0.5)))
	fmt.Printf("hiq=%d\n", hiq)
}
