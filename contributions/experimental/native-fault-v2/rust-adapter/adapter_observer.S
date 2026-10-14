.text
.p2align 4
.globl _mcl_ra_read_flags
_mcl_ra_read_flags:
    pushfq
    popq %rax
    cld // Diagnostic return protects later C/Rust code; saved RAX retains DF.
    ret

#if MCL_RA_OBSERVE
// The sixth argument is r9. Only caller-saved r10 is used AFTER the nested call;
// all six argument registers reach the original frozen C implementation intact.
// RSP entry is 8 mod16: reserve 8 to align the nested C call. First instruction
// after that call captures flags before any C/errno/TLS/Rust instruction. Then
// CLD protects the Rust caller even if the recorded native return DF is wrong.
.p2align 4
.globl _mcl_nf_call
_mcl_nf_call:
    subq $8, %rsp
    call _mcl_ra_impl_call
    pushfq
    popq %r10
    movq %r10, _mcl_ra_c_flags(%rip)
    movl %eax, _mcl_ra_c_status(%rip)
    incl _mcl_ra_c_calls(%rip)
    cld
    addq $8, %rsp
    ret
#endif
