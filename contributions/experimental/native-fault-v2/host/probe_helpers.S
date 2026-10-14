// Test helpers only. No changes to the v2 fault primitives.
.text
.p2align 4
.globl _mcl_probe_read_flags
_mcl_probe_read_flags:
    pushfq
    popq %rax
    ret

// Seven-argument C ABI wrapper: the final flags pointer is on the stack.
// Save RFLAGS immediately after the v2 call, before any C/__error code.
// Clear DF after recording it so a faulty primitive cannot enter the C probe
// with DF=1; the recorded bit still fails the probe. Preserve callee-saved RBX
// and keep RSP 16-byte aligned before the nested call.
.p2align 4
.globl _mcl_probe_call_and_flags
_mcl_probe_call_and_flags:
    pushq %rbx
    movq 16(%rsp), %rbx
    call _mcl_nf_call
    pushfq
    popq %rdx
    movq %rdx, (%rbx)
    cld
    popq %rbx
    ret

// An actual synchronous write outside every owned REP site, with interrupted
// DF=1. It must default-terminate; no native-PC escape is used.
.p2align 4
.globl _mcl_probe_nonleaf_fault
_mcl_probe_nonleaf_fault:
    std
    movb $0x5a, (%rdi)
    cld
    ret
