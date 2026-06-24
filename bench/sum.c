#include <stdio.h>
int main(void) {
    long s = 0;
    for (long i = 0; i < 10000000; i++) s = (s + i) % 1000000007;
    printf("%ld\n", s);
    return 0;
}
