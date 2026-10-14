/*
 * Source-only tagged-memory differential fixture. No fault tests.
 * Build ARM with -march=armv8.2-a+lse; x86 uses the exact C semantic model.
 * Every case visits all 256 top-byte tags and all 16 NZCV patterns, coupled
 * as (tag + case index) & 15, not their full Cartesian product.
 * ARM subjects are single real instructions. Fixed-register file-scope asm prevents C from
 * canonicalizing tagged pointers or consuming an undeclared scratch GPR.
 */
#define _POSIX_C_SOURCE 200112L
#include <inttypes.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

enum kind { GLOAD, GSIGNED, GSTORE, CAS, ADD, CLR, EOR, SET, SWAP,
            VLOAD, VSTORE, PLOAD, PSTORE, PSIGNED, QPLOAD, QPSTORE,
            MLOAD, MSTORE, LLOAD, LSTORE, REPLICATE, FOURLOAD, FOURSTORE };
struct snapshot {
    uint64_t gpr[32]; /* x0..x30, SP */
    uint64_t nzcv;
    uint64_t pad;
    _Alignas(16) uint8_t vec[64]; /* q0..q3 */
};
struct request {
    uint64_t base, a, b, flags;
    struct snapshot *before, *after;
    _Alignas(16) uint8_t vec[64];
};
_Static_assert(offsetof(struct request, vec) == 48, "request asm layout");
_Static_assert(offsetof(struct snapshot, vec) == 272, "snapshot asm layout");
_Static_assert(sizeof(struct snapshot) == 336, "snapshot size");

/* name, real ARM subject, kind, access bytes, target offset, writeback,
 * lane, register count, vector bytes, operand mode (0=constant, 1=CAS hit,
 * 2=CAS miss, 3=64-byte register post-index).
 * The two FOUR sizes are the separate 64-bit and 128-bit LD4 callsites.
 */
#define CASES(X) \
 X(ldrb, "ldrb w2, [x1]", GLOAD,1,0,0,0,1,0,0) \
 X(ldrh, "ldrh w2, [x1]", GLOAD,2,0,0,0,1,0,0) \
 X(ldrw, "ldr w2, [x1]", GLOAD,4,0,0,0,1,0,0) \
 X(ldrx, "ldr x2, [x1]", GLOAD,8,0,0,0,1,0,0) \
 X(ldrsb, "ldrsb x2, [x1]", GSIGNED,1,0,0,0,1,0,0) \
 X(ldrsh, "ldrsh x2, [x1]", GSIGNED,2,0,0,0,1,0,0) \
 X(ldrsw, "ldrsw x2, [x1]", GSIGNED,4,0,0,0,1,0,0) \
 X(strb, "strb w2, [x1]", GSTORE,1,0,0,0,1,0,0) \
 X(strh, "strh w2, [x1]", GSTORE,2,0,0,0,1,0,0) \
 X(strw, "str w2, [x1]", GSTORE,4,0,0,0,1,0,0) \
 X(strx, "str x2, [x1]", GSTORE,8,0,0,0,1,0,0) \
 X(ldr_pre, "ldr x2, [x1, #8]!", GLOAD,8,8,8,0,1,0,0) \
 X(ldr_post, "ldr x2, [x1], #8", GLOAD,8,0,8,0,1,0,0) \
 X(str_pre, "str x2, [x1, #-8]!", GSTORE,8,-8,-8,0,1,0,0) \
 X(str_post, "str x2, [x1], #-8", GSTORE,8,0,-8,0,1,0,0) \
 X(ldarb, "ldarb w2, [x1]", GLOAD,1,0,0,0,1,0,0) \
 X(ldarh, "ldarh w2, [x1]", GLOAD,2,0,0,0,1,0,0) \
 X(ldarw, "ldar w2, [x1]", GLOAD,4,0,0,0,1,0,0) \
 X(ldarx, "ldar x2, [x1]", GLOAD,8,0,0,0,1,0,0) \
 X(stlrb, "stlrb w2, [x1]", GSTORE,1,0,0,0,1,0,0) \
 X(stlrh, "stlrh w2, [x1]", GSTORE,2,0,0,0,1,0,0) \
 X(stlrw, "stlr w2, [x1]", GSTORE,4,0,0,0,1,0,0) \
 X(stlrx, "stlr x2, [x1]", GSTORE,8,0,0,0,1,0,0) \
 X(casb_hit, "casb w2, w3, [x1]", CAS,1,0,0,0,1,0,1) \
 X(cash_hit, "cash w2, w3, [x1]", CAS,2,0,0,0,1,0,1) \
 X(casw_hit, "cas w2, w3, [x1]", CAS,4,0,0,0,1,0,1) \
 X(casx_hit, "cas x2, x3, [x1]", CAS,8,0,0,0,1,0,1) \
 X(casb_miss, "casb w2, w3, [x1]", CAS,1,0,0,0,1,0,2) \
 X(cash_miss, "cash w2, w3, [x1]", CAS,2,0,0,0,1,0,2) \
 X(casw_miss, "cas w2, w3, [x1]", CAS,4,0,0,0,1,0,2) \
 X(casx_miss, "cas x2, x3, [x1]", CAS,8,0,0,0,1,0,2) \
 X(casa_hit, "casa x2, x3, [x1]", CAS,8,0,0,0,1,0,1) \
 X(casl_hit, "casl x2, x3, [x1]", CAS,8,0,0,0,1,0,1) \
 X(casal_hit, "casal x2, x3, [x1]", CAS,8,0,0,0,1,0,1) \
 X(ldaddb, "ldaddb w2, w3, [x1]", ADD,1,0,0,0,1,0,0) \
 X(ldaddh, "ldaddh w2, w3, [x1]", ADD,2,0,0,0,1,0,0) \
 X(ldaddw, "ldadd w2, w3, [x1]", ADD,4,0,0,0,1,0,0) \
 X(ldaddx, "ldadd x2, x3, [x1]", ADD,8,0,0,0,1,0,0) \
 X(ldclrw, "ldclr w2, w3, [x1]", CLR,4,0,0,0,1,0,0) \
 X(ldclrx, "ldclr x2, x3, [x1]", CLR,8,0,0,0,1,0,0) \
 X(ldeorw, "ldeor w2, w3, [x1]", EOR,4,0,0,0,1,0,0) \
 X(ldeorx, "ldeor x2, x3, [x1]", EOR,8,0,0,0,1,0,0) \
 X(ldsetw, "ldset w2, w3, [x1]", SET,4,0,0,0,1,0,0) \
 X(ldsetx, "ldset x2, x3, [x1]", SET,8,0,0,0,1,0,0) \
 X(swpb, "swpb w2, w3, [x1]", SWAP,1,0,0,0,1,0,0) \
 X(swph, "swph w2, w3, [x1]", SWAP,2,0,0,0,1,0,0) \
 X(swpw, "swp w2, w3, [x1]", SWAP,4,0,0,0,1,0,0) \
 X(swpx, "swp x2, x3, [x1]", SWAP,8,0,0,0,1,0,0) \
 X(ldrb_vec, "ldr b0, [x1]", VLOAD,1,0,0,0,1,16,0) \
 X(ldrh_vec, "ldr h0, [x1]", VLOAD,2,0,0,0,1,16,0) \
 X(ldrs_vec, "ldr s0, [x1]", VLOAD,4,0,0,0,1,16,0) \
 X(ldrd_vec, "ldr d0, [x1]", VLOAD,8,0,0,0,1,16,0) \
 X(ldrq_vec, "ldr q0, [x1]", VLOAD,16,0,0,0,1,16,0) \
 X(strb_vec, "str b0, [x1]", VSTORE,1,0,0,0,1,16,0) \
 X(strh_vec, "str h0, [x1]", VSTORE,2,0,0,0,1,16,0) \
 X(strs_vec, "str s0, [x1]", VSTORE,4,0,0,0,1,16,0) \
 X(strd_vec, "str d0, [x1]", VSTORE,8,0,0,0,1,16,0) \
 X(strq_vec, "str q0, [x1]", VSTORE,16,0,0,0,1,16,0) \
 X(ldpq_aligned, "ldp x2, x3, [x1]", PLOAD,8,0,0,0,2,0,0) \
 X(stpq_aligned, "stp x2, x3, [x1]", PSTORE,8,0,0,0,2,0,0) \
 X(ldpw, "ldp w2, w3, [x1]", PLOAD,4,0,0,0,2,0,0) \
 X(stpw, "stp w2, w3, [x1]", PSTORE,4,0,0,0,2,0,0) \
 X(ldpsw, "ldpsw x2, x3, [x1]", PSIGNED,4,0,0,0,2,0,0) \
 X(ldpq_unaligned, "ldp x2, x3, [x1, #8]", PLOAD,8,8,0,0,2,0,0) \
 X(stpq_unaligned, "stp x2, x3, [x1, #8]", PSTORE,8,8,0,0,2,0,0) \
 X(ldpq_post, "ldp x2, x3, [x1], #16", PLOAD,8,0,16,0,2,0,0) \
 X(stpq_pre, "stp x2, x3, [x1, #-16]!", PSTORE,8,-16,-16,0,2,0,0) \
 X(ldp_vecq, "ldp q0, q1, [x1], #32", QPLOAD,16,0,32,0,2,16,0) \
 X(stp_vecq, "stp q0, q1, [x1, #-32]!", QPSTORE,16,-32,-32,0,2,16,0) \
 X(ld1_q1, "ld1 {v0.16b}, [x1], #16", MLOAD,16,0,16,0,1,16,0) \
 X(ld1_q2, "ld1 {v0.16b, v1.16b}, [x1], #32", MLOAD,16,0,32,0,2,16,0) \
 X(ld1_q3, "ld1 {v0.16b, v1.16b, v2.16b}, [x1], #48", MLOAD,16,0,48,0,3,16,0) \
 X(ld1_q4, "ld1 {v0.16b, v1.16b, v2.16b, v3.16b}, [x1], #64", MLOAD,16,0,64,0,4,16,0) \
 X(st1_q4, "st1 {v0.16b, v1.16b, v2.16b, v3.16b}, [x1], #64", MSTORE,16,0,64,0,4,16,0) \
 X(ld1_d1, "ld1 {v0.8b}, [x1], #8", MLOAD,8,0,8,0,1,8,0) \
 X(ld1_d4, "ld1 {v0.8b, v1.8b, v2.8b, v3.8b}, [x1], #32", MLOAD,8,0,32,0,4,8,0) \
 X(st1_d4, "st1 {v0.8b, v1.8b, v2.8b, v3.8b}, [x1], #32", MSTORE,8,0,32,0,4,8,0) \
 X(ld1_qreg, "ld1 {v0.16b, v1.16b}, [x1], x2", MLOAD,16,0,64,0,2,16,3) \
 X(st1_qreg, "st1 {v0.16b, v1.16b}, [x1], x2", MSTORE,16,0,64,0,2,16,3) \
 X(lane_b_load, "ld1 {v0.b}[15], [x1], #1", LLOAD,1,0,1,15,1,16,0) \
 X(lane_h_load, "ld1 {v0.h}[7], [x1], #2", LLOAD,2,0,2,7,1,16,0) \
 X(lane_s_load, "ld1 {v0.s}[3], [x1], #4", LLOAD,4,0,4,3,1,16,0) \
 X(lane_d_load, "ld1 {v0.d}[1], [x1], #8", LLOAD,8,0,8,1,1,16,0) \
 X(lane_b_store, "st1 {v0.b}[15], [x1], #1", LSTORE,1,0,1,15,1,16,0) \
 X(lane_h_store, "st1 {v0.h}[7], [x1], #2", LSTORE,2,0,2,7,1,16,0) \
 X(lane_s_store, "st1 {v0.s}[3], [x1], #4", LSTORE,4,0,4,3,1,16,0) \
 X(lane_d_store, "st1 {v0.d}[1], [x1], #8", LSTORE,8,0,8,1,1,16,0) \
 X(rep_b, "ld1r {v0.16b}, [x1], #1", REPLICATE,1,0,1,0,1,16,0) \
 X(rep_h, "ld1r {v0.8h}, [x1], #2", REPLICATE,2,0,2,0,1,16,0) \
 X(rep_s, "ld1r {v0.4s}, [x1], #4", REPLICATE,4,0,4,0,1,16,0) \
 X(rep_d, "ld1r {v0.2d}, [x1], #8", REPLICATE,8,0,8,0,1,16,0) \
 X(ld4_db, "ld4 {v0.8b, v1.8b, v2.8b, v3.8b}, [x1], #32", FOURLOAD,1,0,32,0,4,8,0) \
 X(ld4_dh, "ld4 {v0.4h, v1.4h, v2.4h, v3.4h}, [x1], #32", FOURLOAD,2,0,32,0,4,8,0) \
 X(ld4_ds, "ld4 {v0.2s, v1.2s, v2.2s, v3.2s}, [x1], #32", FOURLOAD,4,0,32,0,4,8,0) \
 X(st4_db, "st4 {v0.8b, v1.8b, v2.8b, v3.8b}, [x1], #32", FOURSTORE,1,0,32,0,4,8,0) \
 X(st4_dh, "st4 {v0.4h, v1.4h, v2.4h, v3.4h}, [x1], #32", FOURSTORE,2,0,32,0,4,8,0) \
 X(st4_ds, "st4 {v0.2s, v1.2s, v2.2s, v3.2s}, [x1], #32", FOURSTORE,4,0,32,0,4,8,0) \
 X(ld4_qb, "ld4 {v0.16b, v1.16b, v2.16b, v3.16b}, [x1], #64", FOURLOAD,1,0,64,0,4,16,0) \
 X(ld4_qh, "ld4 {v0.8h, v1.8h, v2.8h, v3.8h}, [x1], #64", FOURLOAD,2,0,64,0,4,16,0) \
 X(ld4_qs, "ld4 {v0.4s, v1.4s, v2.4s, v3.4s}, [x1], #64", FOURLOAD,4,0,64,0,4,16,0) \
 X(st4_qb, "st4 {v0.16b, v1.16b, v2.16b, v3.16b}, [x1], #64", FOURSTORE,1,0,64,0,4,16,0) \
 X(st4_qh, "st4 {v0.8h, v1.8h, v2.8h, v3.8h}, [x1], #64", FOURSTORE,2,0,64,0,4,16,0) \
 X(st4_qs, "st4 {v0.4s, v1.4s, v2.4s, v3.4s}, [x1], #64", FOURSTORE,4,0,64,0,4,16,0)

#if defined(__aarch64__) || defined(__arm64__)
#if defined(__APPLE__)
#define SYMBOL(name) "_tm_" #name
#else
#define SYMBOL(name) "tm_" #name
#endif
/* All callee-saved GPRs touched by setup are preserved by the ABI prologue.
 * x18 (platform), x29 (frame) and x30 (return) are observed but not assigned.
 * x16 is restored to its sentinel after the before snapshot uses it for
 * SP/NZCV capture. After capture may clobber x16 only after its GPR was saved.
 */
#define SAVE(pointer) \
 "stp x0, x1, [" pointer ", #0]\n" \
 "stp x2, x3, [" pointer ", #16]\n" \
 "stp x4, x5, [" pointer ", #32]\n" \
 "stp x6, x7, [" pointer ", #48]\n" \
 "stp x8, x9, [" pointer ", #64]\n" \
 "stp x10, x11, [" pointer ", #80]\n" \
 "stp x12, x13, [" pointer ", #96]\n" \
 "stp x14, x15, [" pointer ", #112]\n" \
 "stp x16, x17, [" pointer ", #128]\n" \
 "stp x18, x19, [" pointer ", #144]\n" \
 "stp x20, x21, [" pointer ", #160]\n" \
 "stp x22, x23, [" pointer ", #176]\n" \
 "stp x24, x25, [" pointer ", #192]\n" \
 "stp x26, x27, [" pointer ", #208]\n" \
 "stp x28, x29, [" pointer ", #224]\n" \
 "str x30, [" pointer ", #240]\n" \
 "mov x16, sp\nstr x16, [" pointer ", #248]\n" \
 "mrs x16, nzcv\nstr x16, [" pointer ", #256]\n" \
 "stp q0, q1, [" pointer ", #272]\n" \
 "stp q2, q3, [" pointer ", #304]\n"
#define DECLARE(name, instruction, kind, width, offset, wb, lane, nr, vb, operand) \
 extern void tm_##name(struct request *); \
 __asm__(".text\n.p2align 2\n.globl " SYMBOL(name) "\n" SYMBOL(name) ":\n" \
 "sub sp, sp, #96\n" \
 "stp x19, x20, [sp, #0]\nstp x21, x22, [sp, #16]\n" \
 "stp x23, x24, [sp, #32]\nstp x25, x26, [sp, #48]\n" \
 "stp x27, x28, [sp, #64]\nstp x29, x30, [sp, #80]\n" \
 "ldr x1, [x0]\nldp x2, x3, [x0, #8]\nldr x6, [x0, #24]\n" \
 "ldp x4, x5, [x0, #32]\nldp q0, q1, [x0, #48]\nldp q2, q3, [x0, #80]\n" \
 "msr nzcv, x6\n" \
 "mov x6, #0x606\nmov x7, #0x707\nmov x8, #0x808\nmov x9, #0x909\n" \
 "mov x10, #0xa0a\nmov x11, #0xb0b\nmov x12, #0xc0c\nmov x13, #0xd0d\n" \
 "mov x14, #0xe0e\nmov x15, #0xf0f\nmov x16, #0x1010\nmov x17, #0x1111\n" \
 "mov x19, #0x1313\nmov x20, #0x1414\nmov x21, #0x1515\nmov x22, #0x1616\n" \
 "mov x23, #0x1717\nmov x24, #0x1818\nmov x25, #0x1919\nmov x26, #0x1a1a\n" \
 "mov x27, #0x1b1b\nmov x28, #0x1c1c\n" \
 SAVE("x4") "mov x16, #0x1010\n" instruction "\n" SAVE("x5") \
 "ldp x19, x20, [sp, #0]\nldp x21, x22, [sp, #16]\n" \
 "ldp x23, x24, [sp, #32]\nldp x25, x26, [sp, #48]\n" \
 "ldp x27, x28, [sp, #64]\nldp x29, x30, [sp, #80]\n" \
 "add sp, sp, #96\nret\n");
CASES(DECLARE)
#define FUNCTION(name) tm_##name
#else
#define FUNCTION(name) NULL
#endif

struct test_case {
    const char *name;
    void (*run)(struct request *);
    enum kind kind;
    unsigned width;
    int offset, wb;
    unsigned lane, nregs, vector_bytes, operand;
};
#define ITEM(name, instruction, kind, width, offset, wb, lane, nr, vb, operand) \
 {#name, FUNCTION(name), kind, width, offset, wb, lane, nr, vb, operand},
static const struct test_case cases[] = { CASES(ITEM) };

static uint64_t read_word(const uint8_t *p, unsigned width) {
    uint64_t value = 0;
    for (unsigned i=0; i<width; ++i) value |= (uint64_t)p[i] << (i*8);
    return value;
}
static void write_word(uint8_t *p, unsigned width, uint64_t value) {
    for (unsigned i=0; i<width; ++i) p[i] = (uint8_t)(value >> (i*8));
}
static uint64_t mask(unsigned width) {
    return width == 8 ? UINT64_MAX : (UINT64_C(1) << (width*8))-1;
}
static uint64_t sign_extend(uint64_t value, unsigned width) {
    uint64_t sign = UINT64_C(1) << (width*8-1);
    return (value ^ sign) - sign;
}
static void model(const struct test_case *c, const struct request *r,
                  uint8_t memory[256], struct snapshot *after) {
    uint8_t *p = memory + 96 + c->offset;
    uint64_t old, value;
    after->gpr[1] = r->base + (uint64_t)(int64_t)c->wb;
    switch (c->kind) {
    case GLOAD: after->gpr[2]=read_word(p,c->width); break;
    case GSIGNED: after->gpr[2]=sign_extend(read_word(p,c->width),c->width); break;
    case GSTORE: write_word(p,c->width,r->a); break;
    case CAS:
        old=read_word(p,c->width);
        if ((r->a & mask(c->width))==old) write_word(p,c->width,r->b);
        after->gpr[2]=old;
        break;
    case ADD: case CLR: case EOR: case SET: case SWAP:
        old=read_word(p,c->width);
        value=c->kind==ADD ? old+r->a : c->kind==CLR ? old & ~r->a :
              c->kind==EOR ? old^r->a : c->kind==SET ? old|r->a : r->a;
        write_word(p,c->width,value); after->gpr[3]=old;
        break;
    case VLOAD:
        memset(after->vec,0,16); memcpy(after->vec,p,c->width); break;
    case VSTORE: memcpy(p,r->vec,c->width); break;
    case PLOAD: case PSIGNED:
        for (unsigned j=0;j<2;++j) {
            value=read_word(p+j*c->width,c->width);
            after->gpr[2+j]=c->kind==PSIGNED ? sign_extend(value,c->width) : value;
        }
        break;
    case PSTORE: write_word(p,c->width,r->a); write_word(p+c->width,c->width,r->b); break;
    case QPLOAD: case MLOAD:
        for (unsigned j=0;j<c->nregs;++j) {
            memset(after->vec+j*16,0,16);
            memcpy(after->vec+j*16,p+j*c->width,c->width);
        }
        break;
    case QPSTORE: case MSTORE:
        for (unsigned j=0;j<c->nregs;++j) memcpy(p+j*c->width,r->vec+j*16,c->width);
        break;
    case LLOAD: memcpy(after->vec+c->lane*c->width,p,c->width); break;
    case LSTORE: memcpy(p,r->vec+c->lane*c->width,c->width); break;
    case REPLICATE:
        for (unsigned j=0;j<16;j+=c->width) memcpy(after->vec+j,p,c->width);
        break;
    case FOURLOAD:
        memset(after->vec,0,64);
        for (unsigned j=0;j<c->vector_bytes/c->width;++j)
            for (unsigned k=0;k<4;++k)
                memcpy(after->vec+k*16+j*c->width,p+(j*4+k)*c->width,c->width);
        break;
    case FOURSTORE:
        for (unsigned j=0;j<c->vector_bytes/c->width;++j)
            for (unsigned k=0;k<4;++k)
                memcpy(p+(j*4+k)*c->width,r->vec+k*16+j*c->width,c->width);
        break;
    }
}
static void reference_before(const struct request *r, struct snapshot *s) {
    memset(s,0,sizeof(*s));
    for (unsigned j=0;j<32;++j) s->gpr[j]=(uint64_t)j*0x101;
    s->gpr[0]=(uintptr_t)r; s->gpr[1]=r->base; s->gpr[2]=r->a; s->gpr[3]=r->b;
    s->gpr[4]=(uintptr_t)r->before; s->gpr[5]=(uintptr_t)r->after;
    s->nzcv=r->flags; memcpy(s->vec,r->vec,64);
}
static uint64_t hash_bytes(uint64_t hash, const void *data, size_t n) {
    const uint8_t *p=data;
    for (size_t j=0;j<n;++j) { hash^=p[j]; hash*=UINT64_C(1099511628211); }
    return hash;
}
static int bad(const char *name, unsigned tag, const char *field, unsigned index,
               uint64_t actual, uint64_t expected) {
    fprintf(stderr,"FAIL case=%s tag=%u field=%s index=%u actual=%016" PRIx64
                   " expected=%016" PRIx64 "\n",name,tag,field,index,actual,expected);
    return 1;
}
int main(void) {
    uint8_t *arena=NULL;
    if (posix_memalign((void **)&arena,64,256)!=0) return 2;
    if (((uintptr_t)arena >> 56)!=0) { free(arena); return 3; }
    uint64_t checksum=UINT64_C(1469598103934665603);
    unsigned completed=0;
    for (unsigned ci=0;ci<sizeof(cases)/sizeof(cases[0]);++ci) {
        const struct test_case *c=&cases[ci];
        for (unsigned tag=0;tag<256;++tag) {
            uint8_t expected_memory[256];
            struct snapshot before, after, expected;
            struct request r;
            memset(&r,0,sizeof(r)); memset(&before,0,sizeof(before)); memset(&after,0,sizeof(after));
            /* Tags vary the data as well: every signed load sees both signs. */
            for (unsigned j=0;j<256;++j) arena[j]=(uint8_t)(j*37+0x91+tag);
            memcpy(expected_memory,arena,256);
            r.base=(uint64_t)(uintptr_t)(arena+96) | ((uint64_t)tag<<56);
            r.a=UINT64_C(0x8f765432fedcba91); r.b=UINT64_C(0xbadc0ffe87654321);
            if (c->operand==1 || c->operand==2) {
                r.a=read_word(arena+96+c->offset,c->width);
                if (c->operand==2) r.a^=1;
            } else if (c->operand==3) r.a=64;
            r.flags=(uint64_t)((tag+ci)&15)<<28;
            r.before=&before; r.after=&after;
            for (unsigned j=0;j<64;++j) r.vec[j]=(uint8_t)(0xe3+j*11);
#if defined(__aarch64__) || defined(__arm64__)
            c->run(&r);
#else
            reference_before(&r,&before);
            after=before;
            model(c,&r,arena,&after);
#endif
            /* Input snapshot is independently checked, not merely trusted.
             * Dynamic ABI values x18/x29/x30/SP are compared before/after.
             */
            struct snapshot input;
            reference_before(&r,&input);
            for (unsigned j=0;j<32;++j) {
                if (j==18 || j==29 || j==30 || j==31) continue;
                if (before.gpr[j]!=input.gpr[j]) {
                    free(arena); return bad(c->name,tag,"before-gpr",j,before.gpr[j],input.gpr[j]);
                }
            }
            if (before.nzcv!=r.flags) { free(arena); return bad(c->name,tag,"before-nzcv",0,before.nzcv,r.flags); }
            if (memcmp(before.vec,r.vec,64)!=0) { free(arena); return bad(c->name,tag,"before-vector",0,1,0); }
            expected=before;
            model(c,&r,expected_memory,&expected);
            for (unsigned j=0;j<32;++j) {
                if (after.gpr[j]!=expected.gpr[j]) {
                    free(arena); return bad(c->name,tag,"after-gpr",j,after.gpr[j],expected.gpr[j]);
                }
            }
            if (after.nzcv!=r.flags) { free(arena); return bad(c->name,tag,"after-nzcv",0,after.nzcv,r.flags); }
            for (unsigned j=0;j<64;++j) if (after.vec[j]!=expected.vec[j]) {
                free(arena); return bad(c->name,tag,"after-vector-byte",j,after.vec[j],expected.vec[j]);
            }
            for (unsigned j=0;j<256;++j) if (arena[j]!=expected_memory[j]) {
                uint8_t value=arena[j]; free(arena); return bad(c->name,tag,"arena-byte",j,value,expected_memory[j]);
            }
            uint64_t delta=after.gpr[1]-before.gpr[1];
            if ((after.gpr[1]>>56)!=tag) { free(arena); return bad(c->name,tag,"writeback-tag",0,after.gpr[1]>>56,tag); }
            checksum=hash_bytes(checksum,&ci,sizeof(ci));
            checksum=hash_bytes(checksum,&tag,sizeof(tag));
            checksum=hash_bytes(checksum,&delta,sizeof(delta));
            checksum=hash_bytes(checksum,&after.gpr[2],sizeof(uint64_t)*2);
            checksum=hash_bytes(checksum,&after.nzcv,sizeof(after.nzcv));
            checksum=hash_bytes(checksum,after.vec,64);
            checksum=hash_bytes(checksum,arena,256);
            ++completed;
        }
    }
    printf("PASS tagged-memory cases=%zu tags=256 nzcv-patterns-per-case=16 "
           "executions=%u checksum=%016" PRIx64 "\n",
           sizeof(cases)/sizeof(cases[0]),completed,checksum);
    free(arena);
    return 0;
}
