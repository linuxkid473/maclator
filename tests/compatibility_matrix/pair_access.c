// ARM LDP/STP addressing and readonly-memory regression for atomic-pair emitters.
#include <stdint.h>
#include <stdio.h>
#include <sys/mman.h>

static int same(uint64_t a, uint64_t b, uint64_t *p) { return a == p[0] && b == p[1]; }
int main(void) {
    uint64_t *page = mmap(NULL, 16384, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANON, -1, 0);
    if (page == MAP_FAILED) return 2;
    for (unsigned i = 0; i < 16; ++i) page[i] = 0x123456789abcd000ULL + i;
    if (mprotect(page, 16384, PROT_READ)) return 3;
    int failures = 0;
    uint64_t a, b, base;
    __asm__ volatile("ldp %0, %1, [%2]" : "=&r"(a), "=&r"(b) : "r"(page) : "memory");
    failures += !same(a, b, page);
    // Source base may alias an output register when there is no writeback.
    __asm__ volatile("mov x0, %2\nldp x0, x1, [x0]\nmov %0, x0\nmov %1, x1"
                     : "=&r"(a), "=&r"(b) : "r"(page) : "x0", "x1", "memory");
    failures += !same(a, b, page);
    __asm__ volatile("ldp %0, %1, [%2, #16]" : "=&r"(a), "=&r"(b) : "r"(page) : "memory");
    failures += !same(a, b, page + 2);
    __asm__ volatile("ldp %0, %1, [%2, #-16]" : "=&r"(a), "=&r"(b) : "r"(page + 2) : "memory");
    failures += !same(a, b, page);
    __asm__ volatile("mov x4, %3\nldp x2, x3, [x4, #16]!\nmov %0, x2\nmov %1, x3\nmov %2, x4"
                     : "=&r"(a), "=&r"(b), "=&r"(base) : "r"(page)
                     : "x2", "x3", "x4", "memory");
    failures += !same(a, b, page + 2) || base != (uint64_t)(page + 2);
    __asm__ volatile("mov x4, %3\nldp x2, x3, [x4], #16\nmov %0, x2\nmov %1, x3\nmov %2, x4"
                     : "=&r"(a), "=&r"(b), "=&r"(base) : "r"(page)
                     : "x2", "x3", "x4", "memory");
    failures += !same(a, b, page) || base != (uint64_t)(page + 2);
    __asm__ volatile("ldp xzr, %0, [%1]" : "=&r"(b) : "r"(page) : "memory");
    failures += b != page[1];
    __asm__ volatile("ldp d0, d1, [%2]\nfmov %0, d0\nfmov %1, d1"
                     : "=&r"(a), "=&r"(b) : "r"(page) : "v0", "v1", "memory");
    failures += !same(a, b, page);
    // Non-16-byte-aligned (but 8-byte-aligned) pair path must still load correctly.
    __asm__ volatile("ldp %0, %1, [%2]" : "=&r"(a), "=&r"(b) : "r"(page + 1) : "memory");
    failures += !same(a, b, page + 1);
    // Last naturally aligned pair wholly inside a read-only mapping.
    __asm__ volatile("ldp %0, %1, [%2]" : "=&r"(a), "=&r"(b) : "r"(page + 2046) : "memory");
    failures += a != 0 || b != 0;
    if (mprotect(page, 16384, PROT_READ | PROT_WRITE)) return 4;
    a = 0x123456789abcdef0ULL; b = 0xfedcba9876543210ULL;
    __asm__ volatile("stp %0, %1, [%2]" : : "r"(a), "r"(b), "r"(page) : "memory");
    failures += !same(a, b, page);
    __asm__ volatile("mov x4, %3\nstp %1, %2, [x4, #16]!\nmov %0, x4"
                     : "=&r"(base) : "r"(a), "r"(b), "r"(page) : "x4", "memory");
    failures += !same(a, b, page + 2) || base != (uint64_t)(page + 2);
    __asm__ volatile("mov x4, %3\nstp %1, %2, [x4], #16\nmov %0, x4"
                     : "=&r"(base) : "r"(a), "r"(b), "r"(page + 4) : "x4", "memory");
    failures += !same(a, b, page + 4) || base != (uint64_t)(page + 6);
    __asm__ volatile("fmov d0, %0\nfmov d1, %1\nstp d0, d1, [%2]"
                     : : "r"(a), "r"(b), "r"(page + 6) : "v0", "v1", "memory");
    failures += !same(a, b, page + 6);
    __asm__ volatile("stp xzr, xzr, [%0]" : : "r"(page + 8) : "memory");
    failures += page[8] != 0 || page[9] != 0;
    munmap(page, 16384);
    printf("PAIR_READONLY_ALIAS_WRITEBACK %s failures=%d\n", failures ? "FAIL" : "PASS", failures);
    return failures ? 1 : 0;
}
