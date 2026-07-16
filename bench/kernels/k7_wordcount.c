// K7 — word frequency over a generated corpus, C. Hand-rolled open-addressing hash map
// with STRING keys (FNV-1a 64, linear probing, grow at 0.7 load) — the honest counterpart
// to everyone else's built-in map, not a fixed array keyed by the numeric id.
// gcc -O3 -march=native k7_wordcount.c -o k7_wordcount
//
// The corpus is driven by the SAME xorshift64 reference stream as k5 (seed
// 88172645463325252), one draw per word, so the word sequence is bit-identical in all
// six languages. It replaces an earlier `id = (i*2654435761) % 10000` generator that was
// degenerate: 2654435761 % 10000 = 5761 and gcd(5761, 10000) = 1, so i -> 5761*i is a
// BIJECTION on Z/10000 and every one of the 10k words landed within 1 of every other.
// The old anchor was therefore identically ceil(N/10000) — a program that counted nothing
// and printed ceil(N/10000) passed the cross-language gate at every N. Under this stream
// the counts are genuinely uneven (measured at N=5e6: 10000 distinct, max 585, min 418,
// and exactly one word attains the max), and the anchor is not a closed form of N.
//
// WHAT THE ANCHOR STILL DOES NOT PIN: the spelling of the words. A max count is invariant
// under any injective relabeling, so this gate constrains the STREAM and the COUNTING but
// not the string content. Verified, not assumed: sed'ing this file's "w%ld" to
// "XYZZY_%ld_JUNK" still prints 585, and so does stripping the prefix to a bare "%ld" —
// which is 4-char keys instead of 5, i.e. strictly less string work for the same anchor.
// Nothing here exploits that (all six mint "w" + the id), but a future edit could, and the
// gate would not catch it. Closing it needs a different anchor (e.g. also printing the
// winning word, tie-broken lexicographically); that is a deliberate open item, not an
// oversight.
//
// `(s >> 11) & 9007199254740991` — the mask is a NO-OP here and in Rust/Go, because
// s>>11 on a uint64 is already 53 bits. It is written out anyway because it is load-
// bearing in Helix, whose ints are signed and whose `>>` sign-extends; keeping the
// expression identical in all six files is what makes the streams comparable by eye.
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define DEFAULT_N 5000000L
#define MODULUS 10000L

typedef struct {
    char **keys;
    long *counts;
    size_t cap; // always a power of two
    size_t len;
} Table;

static size_t hash_str(const char *s) {
    size_t h = 1469598103934665603UL; // FNV-1a 64 offset basis
    while (*s) {
        h ^= (unsigned char)*s++;
        h *= 1099511628211UL;
    }
    return h;
}

static void table_init(Table *t, size_t cap) {
    t->cap = cap;
    t->len = 0;
    t->keys = calloc(cap, sizeof(char *));
    t->counts = calloc(cap, sizeof(long));
}

static void table_grow(Table *t) {
    size_t ncap = t->cap * 2, mask = ncap - 1;
    char **nk = calloc(ncap, sizeof(char *));
    long *nc = calloc(ncap, sizeof(long));
    for (size_t j = 0; j < t->cap; j++) {
        if (!t->keys[j]) continue;
        size_t i = hash_str(t->keys[j]) & mask;
        while (nk[i]) i = (i + 1) & mask;
        nk[i] = t->keys[j]; // move the pointer; no re-strdup
        nc[i] = t->counts[j];
    }
    free(t->keys);
    free(t->counts);
    t->keys = nk;
    t->counts = nc;
    t->cap = ncap;
}

static void table_bump(Table *t, const char *key) {
    size_t mask = t->cap - 1;
    size_t i = hash_str(key) & mask;
    while (t->keys[i]) {
        if (strcmp(t->keys[i], key) == 0) {
            t->counts[i]++;
            return;
        }
        i = (i + 1) & mask;
    }
    t->keys[i] = strdup(key);
    t->counts[i] = 1;
    t->len++;
    if (t->len * 10 >= t->cap * 7) table_grow(t);
}

int main(int argc, char **argv) {
    long n = (argc > 1) ? strtol(argv[1], NULL, 10) : DEFAULT_N;

    Table t;
    table_init(&t, 1024);

    uint64_t s = 88172645463325252ULL;
    char buf[32];
    for (long i = 0; i < n; i++) {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        long id = (long)((s >> 11) & 9007199254740991UL) % MODULUS;
        snprintf(buf, sizeof buf, "w%ld", id);
        table_bump(&t, buf);
    }

    // N=0 leaves the table empty and prints 0 — the same empty-input answer every
    // other language here gives.
    long max = 0;
    for (size_t j = 0; j < t.cap; j++) {
        if (t.keys[j] && t.counts[j] > max) max = t.counts[j];
    }
    printf("%ld\n", max);
    return 0;
}
