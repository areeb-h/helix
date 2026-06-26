// S3 — VCF analysis, Go (std only). go run s3_vcf.go
package main

import (
	"bufio"
	"fmt"
	"os"
	"strconv"
	"strings"
)

func main() {
	f, _ := os.Open("data/variants.vcf")
	defer f.Close()
	sc := bufio.NewScanner(f)
	sc.Buffer(make([]byte, 1<<20), 1<<20)

	var total, keep uint64
	counts := make(map[string]uint64)
	for sc.Scan() {
		line := sc.Text()
		if strings.HasPrefix(line, "#") {
			continue
		}
		total++
		c := strings.Split(line, "\t")
		qual, _ := strconv.ParseFloat(c[5], 64)
		if qual > 50.0 {
			keep++
			for _, kv := range strings.Split(c[7], ";") {
				if strings.HasPrefix(kv, "GENE=") {
					counts[kv[5:]]++
				}
			}
		}
	}

	fmt.Printf("total=%d\n", total)
	fmt.Printf("pass=%d\n", keep)
	for _, g := range []string{"BRCA1", "BRCA2", "EGFR", "KRAS", "TP53"} {
		fmt.Printf("%s=%d\n", g, counts[g])
	}
}
