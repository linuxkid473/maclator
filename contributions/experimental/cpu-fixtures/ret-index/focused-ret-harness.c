/* Parent-run only. Executes actual unmodified JIT dump code in a native
 * process with fake Cpu/IBTC. Does not execute a guest PC as a host address.
 * Prepared without compilation or execution by source/design agent. */
#include <assert.h>
#include <errno.h>
#include <inttypes.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>

#if !defined(__x86_64__)
#error This harness requires x86_64
#endif
#ifndef MAP_ANON
#define MAP_ANON MAP_ANONYMOUS
#endif

typedef struct __attribute__((aligned(64))) Cpu {
    uint64_t x[32], pc;
    uint8_t nf, zf, cf, vf;
    uint32_t pad0;
    uint64_t v[32][2];
    uint64_t fpcr, fpsr, tpidr_el0, tpidrro_el0;
    uint64_t excl_addr, excl_val, excl_val2, excl_size;
    uint64_t exit_reason, exit_data, jit_exit, ibtc, thread, budget;
    uint64_t pad1[2], helpers[32];
} Cpu;
_Static_assert(offsetof(Cpu, pc) == 256, "Cpu.pc offset");
_Static_assert(offsetof(Cpu, nf) == 264, "Cpu.nzcv offset");
_Static_assert(offsetof(Cpu, v) == 272, "Cpu.v offset");
_Static_assert(offsetof(Cpu, exit_reason) == 848, "Cpu.exit_reason offset");
_Static_assert(offsetof(Cpu, exit_data) == 856, "Cpu.exit_data offset");
_Static_assert(offsetof(Cpu, jit_exit) == 864, "Cpu.jit_exit offset");
_Static_assert(offsetof(Cpu, ibtc) == 872, "Cpu.ibtc offset");
_Static_assert(offsetof(Cpu, budget) == 888, "Cpu.budget offset");
_Static_assert(offsetof(Cpu, helpers) == 912, "Cpu.helpers offset");
_Static_assert(sizeof(Cpu) == 1216, "Cpu repr(C) aligned size");

extern int probe_jit_enter(Cpu *, void *, int);
extern void probe_jit_hit(void), probe_jit_miss(void);
#ifdef __APPLE__
#define S "_"
#else
#define S ""
#endif
/* Jump (not call) into the JIT body. Both exit trampolines jump to this
 * invocation's restoration tail. The real ABI return address stays below
 * all six saved callee-saved registers. RDX argument is copied before the
 * JIT uses it as scratch. The memory-budget mode does not rewrite budget. */
__asm__(
    ".text\n.intel_syntax noprefix\n"
    ".p2align 4\n.globl " S "probe_jit_enter\n"
    S "probe_jit_enter:\n"
    "push rbp\npush rbx\npush r12\npush r13\npush r14\npush r15\n"
    "sub rsp, 8\nmov dword ptr [rsp], edx\nmov r15, rdi\n"
    "mov r14, qword ptr [r15 + 888]\njmp rsi\n"
    ".globl " S "probe_jit_hit\n" S "probe_jit_hit:\n"
    "mov eax, 1\njmp " S "probe_jit_return\n"
    ".globl " S "probe_jit_miss\n" S "probe_jit_miss:\n"
    "xor eax, eax\njmp " S "probe_jit_return\n"
    S "probe_jit_return:\n"
    "cmp dword ptr [rsp], 0\nje " S "probe_jit_memory\n"
    "mov qword ptr [r15 + 888], r14\n"
    S "probe_jit_memory:\n"
    "add rsp, 8\npop r15\npop r14\npop r13\npop r12\npop rbx\npop rbp\nret\n"
    ".att_syntax prefix\n"
);

typedef struct Entry { uint64_t tag, host; } Entry;
_Static_assert(sizeof(Entry) == 16, "IBTC pair size");
static uint64_t calls, checksum;
static void require(int yes, const char *what) {
    if (!yes) { fprintf(stderr, "FAIL %s at call=%" PRIu64 "\n", what, calls); exit(2); }
}

/* stage 0: matching full tag -> owned hit trampoline.
 * stage 1: different tag with exactly the same index -> owned miss trampoline.
 * stage 2: normal empty-cache [1,0] sentinel, except target 1 is excluded.
 * stage 3: clear/replace then reinsert -> hit after state transition.
 * Only the addressed entry can affect this body; no host pointer is obtained
 * from runtime memory or from an invalid guest target. */
static void one(void *body, Entry *table, uint32_t word, uint64_t guest_pc,
                uint64_t supplied_target, unsigned nzcv, unsigned stage,
                int register_budget, uint64_t budget) {
    Cpu c, want;
    memset(&c, 0, sizeof(c));
    for (unsigned n=0;n<32;n++) {
        c.x[n] = UINT64_C(0x91ab23cd45ef6700) ^ (UINT64_C(0x1000100010001)*n);
        c.v[n][0] = UINT64_C(0x2233445566778899) ^ n;
        c.v[n][1] = UINT64_C(0xfedcba9876543210) ^ n;
    }
    c.pc = UINT64_C(0x3344556677889900);
    c.nf=(nzcv>>3)&1; c.zf=(nzcv>>2)&1; c.cf=(nzcv>>1)&1; c.vf=nzcv&1;
    c.fpcr=0x9f00000; c.fpsr=0x1f; c.tpidr_el0=0x1234567; c.tpidrro_el0=0x7654321;
    c.excl_addr=0xabcdef; c.excl_val=0x238976ab; c.excl_val2=0xab769832; c.excl_size=8;
    unsigned opc=(word>>21)&15, op3=(word>>10)&63, rn=(word>>5)&31;
    unsigned reg=opc==2 && op3!=0 ? 30 : rn;
    c.x[reg]=supplied_target;
    uint64_t target=reg==31 ? 0 : c.x[reg];
    c.exit_reason=UINT64_C(0xaabbccdd11223344);
    c.exit_data=UINT64_C(0x1122334455667788);
    c.jit_exit=(uint64_t)(uintptr_t)probe_jit_miss;
    c.ibtc=(uint64_t)(uintptr_t)table;
    c.budget=budget;
    memcpy(&want, &c, sizeof(c));
    size_t index=(size_t)((target>>2)&32767);
    int expect_hit=(stage==0 || stage==3) && budget>0;
    if (stage==0) { table[index].tag=target; table[index].host=(uint64_t)(uintptr_t)probe_jit_hit; }
    if (stage==1) { table[index].tag=target^UINT64_C(0x20000); table[index].host=(uint64_t)(uintptr_t)probe_jit_hit; }
    if (stage==2) {
        require(target!=1, "inherited empty sentinel target 1 excluded");
        table[index].tag=1; table[index].host=0;
    }
    if (stage==3) {
        table[index].tag=target^UINT64_C(0x20000); table[index].host=0;
        table[index].tag=target; table[index].host=(uint64_t)(uintptr_t)probe_jit_hit;
    }
    int got=probe_jit_enter(&c, body, register_budget);
    calls++;
    require(got==expect_hit, "hit/miss result");
    want.budget=budget-1; /* the real one-guest-instruction budget SUB */
    if (budget==0) {
        want.pc=guest_pc; want.exit_reason=0; want.exit_data=0;
    } else {
        if (opc&1) want.x[30]=guest_pc+4; /* existing BLR write-through */
        if (!expect_hit) { want.pc=target; want.exit_reason=0; want.exit_data=0; }
    }
    if (memcmp(&want,&c,sizeof(c))!=0) {
        const unsigned char *a=(const unsigned char *)&want, *b=(const unsigned char *)&c;
        for (size_t n=0;n<sizeof(c);n++) if(a[n]!=b[n]) {
            fprintf(stderr,"byte mismatch %zu want=%02x got=%02x word=%08x target=%016" PRIx64 " stage=%u budget=%" PRIu64 "\n",n,a[n],b[n],word,target,stage,budget); break;
        }
        require(0,"complete Cpu (including architectural state and runtime plumbing)");
    }
    checksum = checksum*UINT64_C(0x100000001b3) ^ target ^ c.pc ^ c.budget ^ (uint64_t)got;
}

int main(int argc,char **argv) {
    require(argc==5,"usage: harness full-code.bin word-hex guest-pc-hex register-budget-0or1");
    uint32_t word=(uint32_t)strtoul(argv[2],NULL,16);
    uint64_t guest_pc=strtoull(argv[3],NULL,16);
    int register_budget=atoi(argv[4]);
    require(register_budget==0 || register_budget==1,"budget mode");
    FILE *f=fopen(argv[1],"rb"); require(f!=NULL,"open code");
    require(fseek(f,0,SEEK_END)==0,"seek code"); long len=ftell(f);
    require(len>0 && len<=512,"bounded full code length"); rewind(f);
    long pagesize=sysconf(_SC_PAGESIZE); require(pagesize>0,"page size");
    void *body=mmap(NULL,(size_t)pagesize,PROT_READ|PROT_WRITE,MAP_PRIVATE|MAP_ANON,-1,0);
    require(body!=MAP_FAILED,"mmap own body");
    memset(body,0xcc,(size_t)pagesize);
    require(fread(body,1,(size_t)len,f)==(size_t)len,"read complete code"); fclose(f);
    require(mprotect(body,(size_t)pagesize,PROT_READ|PROT_EXEC)==0,"protect own code");
    Entry *table=calloc(32768,sizeof(*table)); require(table!=NULL,"allocate fake IBTC");
    for (size_t n=0;n<32768;n++) table[n].tag=1;
    const uint64_t high[]={0,UINT64_C(1)<<17,UINT64_C(1)<<31,UINT64_C(1)<<32,
        UINT64_C(1)<<33,UINT64_C(1)<<63,UINT64_C(0xab00000000000000),UINT64_C(0xfffffffffffe0000)};
    const uint64_t low[]={0,1,2,3,4,5,7,8,0x3ffc,0x3fff,0x7ffc,0x7fff,
        0xfffc,0xffff,0x10000,0x10003,0x1fff8,0x1fffc,0x1fffd,0x1ffff};
    for(size_t h=0;h<sizeof(high)/sizeof(high[0]);h++)
      for(size_t l=0;l<sizeof(low)/sizeof(low[0]);l++)
        for(unsigned nzcv=0;nzcv<16;nzcv++)
          for(unsigned stage=0;stage<4;stage++) {
            uint64_t target=high[h]|low[l];
            unsigned opc=(word>>21)&15,op3=(word>>10)&63,rn=(word>>5)&31;
            unsigned reg=opc==2&&op3!=0?30:rn;
            uint64_t effective=reg==31?0:target;
            if(stage==2 && effective==1) continue;
            one(body,table,word,guest_pc,target,nzcv,stage,register_budget,1000000);
          }
    /* Exact entry-budget suspension before the target read/BLR write. */
    for(unsigned nzcv=0;nzcv<16;nzcv++)
        one(body,table,word,guest_pc,UINT64_C(0xab00000012345679),nzcv,0,register_budget,0);
    free(table); require(munmap(body,(size_t)pagesize)==0,"unmap own body");
    printf("{\"status\":\"PASS\",\"word\":\"%08x\",\"calls\":%" PRIu64 ",\"checksum\":\"%016" PRIx64 "\",\"cpu_size\":%zu,\"budget_mode\":\"%s\",\"scope\":\"actual dumped body/fake Cpu+IBTC; no runtime invalidation or guest execution\"}\n",word,calls,checksum,sizeof(Cpu),register_budget?"register":"memory");
    return 0;
}
