// Owned leaf primitives. No stack adjustments, calls or callee-saved clobbers.
// r9 holds the descriptor at every exact faultable site. RCX/RSI/RDI provide
// the architecturally precise remaining cursor after a REP page fault.
.text
.p2align 4
.globl _mcl_nf_copy_leaf
_mcl_nf_copy_leaf:
    movq %rdi, %r9
    movq 40(%r9), %r10
    // Foreign-assembly atomic ABI: aligned 64-bit release MOV under x86 TSO.
    // C statically checks the cell width/alignment and descriptor offset.
    movq %rsp, (%r10)
    movq %rsi, %rdi
    movq %rdx, %rsi
    testl %r8d, %r8d
    jz Lcopy_forward
    leaq -1(%rdi,%rcx), %rdi
    leaq -1(%rsi,%rcx), %rsi
    std
.globl _mcl_nf_copy_backward_site
_mcl_nf_copy_backward_site:
    rep movsb
    cld
    testq %rcx, %rcx
    jnz Lincomplete_return
    xorl %eax, %eax
    ret
Lcopy_forward:
    cld
.globl _mcl_nf_copy_forward_site
_mcl_nf_copy_forward_site:
    rep movsb
    cld
    testq %rcx, %rcx
    jnz Lincomplete_return
    xorl %eax, %eax
    ret

.p2align 4
.globl _mcl_nf_fill_leaf
_mcl_nf_fill_leaf:
    movq %rdi, %r9
    movq 40(%r9), %r10
    movq %rsp, (%r10)
    movq %rsi, %rdi
    movl %edx, %eax
    cld
.globl _mcl_nf_fill_site
_mcl_nf_fill_site:
    rep stosb
    cld
    testq %rcx, %rcx
    jnz Lincomplete_return
    xorl %eax, %eax
    ret

// A chained native handler that skips directly past REP must not turn a
// nonzero remaining count into Complete. Arbitrary native PC rewrites or
// longjmp escapes remain unsupported; this catches the immediate skip case.
Lincomplete_return:
    cld
    movl $-3, %eax
    ret

// Host sigreturn redirects RIP here, retaining the leaf's original RSP. Normal
// ret unwinds exactly one C ABI call, never a Rust frame or libc inner frame.
.p2align 4
.globl _mcl_nf_fault_return
_mcl_nf_fault_return:
    cld
    movl $1, %eax
    ret

// Do not assume the kernel's signal-handler entry exports DF=0. Clear it
// before the first C instruction even when a backward REP was interrupted.
.p2align 4
.globl _mcl_nf_signal_entry
_mcl_nf_signal_entry:
    cld
    jmp _mcl_nf_receive
