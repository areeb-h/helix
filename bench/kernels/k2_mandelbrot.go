// K2 — escape-time Mandelbrot, 1200x1200 grid, max 100 iterations, over
// x in [-2.1, 0.6] and y in [-1.2, 1.2]. Anchor = sum of every pixel's iteration count.
//
//   go build -o k2_mandelbrot k2_mandelbrot.go
//
// The Go spec PERMITS fusing `2.0*zr*zi + ci` into an FMA. Under the default GOAMD64=v1
// gc emits none (FMA3 is not in that baseline). Built with GOAMD64=v3 it emits 7 FMAs in
// this file — and still prints the same anchor at every grid tested, as does contracted
// gcc. So the FMA hazard discussed at length in k2_mandelbrot.c is real in principle but
// does not bite this kernel on amd64 either way. See that file for the measurements.
package main

import (
	"fmt"
	"os"
	"strconv"
)

func main() {
	var g int64 = 1200
	if len(os.Args) > 1 {
		if v, err := strconv.ParseInt(os.Args[1], 10, 64); err == nil {
			g = v
		}
	}
	var total int64
	for y := int64(0); y < g; y++ {
		ci := -1.2 + float64(y)*2.4/float64(g)
		for x := int64(0); x < g; x++ {
			cr := -2.1 + float64(x)*2.7/float64(g)
			zr, zi := 0.0, 0.0
			var i int64
			for i < 100 && zr*zr+zi*zi <= 4.0 {
				t := zr*zr - zi*zi + cr
				zi = 2.0*zr*zi + ci
				zr = t
				i++
			}
			total += i
		}
	}
	fmt.Println(total)
}
