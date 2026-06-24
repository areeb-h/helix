package main
import "fmt"
func main() {
	s := 0
	for i := 0; i < 10000000; i++ {
		s = (s + i) % 1000000007
	}
	fmt.Println(s)
}
