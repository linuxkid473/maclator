#include <stdio.h>
#include <stdlib.h>
#include <stdint.h>
#include <string.h>
#include <time.h>
static double now(void) { struct timespec t; clock_gettime(CLOCK_MONOTONIC, &t); return t.tv_sec * 1e3 + t.tv_nsec / 1e6; }
static int fib(int n) { return n < 2 ? n : fib(n - 1) + fib(n - 2); }
static int cmp(const void *a, const void *b) { int x = *(const int *)a, y = *(const int *)b; return (x > y) - (x < y); }
static uint32_t crc_tab[256];
struct node { struct node *next; uint64_t v; };
int main(int argc, char **argv) {
    volatile uint64_t sink = 0;
    double t0, tot = 0;
    #define T(name, ...) t0 = now(); __VA_ARGS__; { double d = now() - t0; tot += d; printf("%-10s %8.0f ms\n", name, d); fflush(stdout); }
    T("fib(36)", sink += fib(32));
    T("qsort", { int n = 1500000; int *a = malloc(n * 4); uint32_t s = 1; for (int i = 0; i < n; i++) { s = s * 1664525 + 1013904223; a[i] = s >> 8; } qsort(a, n, 4, cmp); sink += a[n / 2]; free(a); });
    T("sieve", { int n = 30000000; char *c = calloc(n, 1); int cnt = 0; for (int i = 2; i < n; i++) { if (!c[i]) { cnt++; for (long j = (long)i * i; j < n; j += i) c[j] = 1; } } sink += cnt; free(c); });
    T("crc32", { for (int i = 0; i < 256; i++) { uint32_t c = i; for (int k = 0; k < 8; k++) c = c & 1 ? 0xEDB88320 ^ (c >> 1) : c >> 1; crc_tab[i] = c; } uint8_t *b = malloc(8 << 20); memset(b, 7, 8 << 20); uint32_t c = ~0u; for (int r = 0; r < 10; r++) for (int i = 0; i < (8 << 20); i++) c = crc_tab[(c ^ b[i]) & 255] ^ (c >> 8); sink += c; free(b); });
    T("list", { int n = 200000; struct node *nodes = malloc(n * sizeof *nodes); for (int i = 0; i < n; i++) { nodes[i].v = i; nodes[i].next = &nodes[(i * 7919 + 1) % n]; } uint64_t s = 0; struct node *p = &nodes[0]; for (int i = 0; i < 40000000; i++) { s += p->v; p = p->next; } sink += s; free(nodes); });
    T("matmul", { int n = 300; double *A = malloc(n * n * 8), *B = malloc(n * n * 8), *C = calloc(n * n, 8); for (int i = 0; i < n * n; i++) { A[i] = i % 7; B[i] = i % 5; } for (int i = 0; i < n; i++) for (int j = 0; j < n; j++) { double s = 0; for (int k = 0; k < n; k++) s += A[i * n + k] * B[k * n + j]; C[i * n + j] = s; } sink += (uint64_t)C[n + 3]; });
    T("memops", { char *a = malloc(1 << 20), *b = malloc(1 << 20); memset(a, 1, 1 << 20); for (int i = 0; i < 2500; i++) { memcpy(b, a, 1 << 20); a[i] ^= b[i * 31]; } sink += a[5]; });
    printf("%-10s %8.0f ms\n", "TOTAL", tot);
    return (int)(sink & 0);
}
