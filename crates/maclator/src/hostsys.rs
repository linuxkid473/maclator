//! Raw x86-64 Darwin system call / Mach trap invocation.
//!
//! Guest syscalls are passed straight through to the host kernel wherever the
//! semantics are architecture independent. Darwin x86-64 conventions:
//! rax = class | number (BSD 0x2000000, Mach 0x1000000, machdep 0x3000000),
//! args in rdi, rsi, rdx, r10, r8, r9 and then on the user stack at rsp+8.
//! Errors are reported with the carry flag set and errno in rax.

pub const CLASS_MACH: u64 = 0x100_0000;
pub const CLASS_UNIX: u64 = 0x200_0000;

#[derive(Debug, Clone, Copy)]
pub struct SysResult {
    pub rax: u64,
    pub rdx: u64,
    pub carry: bool,
}

#[cfg(target_arch = "x86_64")]
std::arch::global_asm!(
    ".globl _maclator_host_syscall",
    ".p2align 4",
    "_maclator_host_syscall:",
    // rdi = number (with class), rsi = args[8], rdx = out[3]
    "push rbp",
    "mov rbp, rsp",
    "push rbx",
    "push r12",
    "mov r12, rdx",
    "mov rbx, rsi",
    "mov r11, rdi",
    "and rsp, -16",
    "push qword ptr [rbx + 56]",
    "push qword ptr [rbx + 48]",
    "push 0",
    "mov rax, r11",
    "mov rdi, [rbx]",
    "mov rsi, [rbx + 8]",
    "mov rdx, [rbx + 16]",
    "mov r10, [rbx + 24]",
    "mov r8, [rbx + 32]",
    "mov r9, [rbx + 40]",
    "syscall",
    "setc cl",
    "movzx ecx, cl",
    "mov [r12], rax",
    "mov [r12 + 8], rdx",
    "mov [r12 + 16], rcx",
    "lea rsp, [rbp - 16]",
    "pop r12",
    "pop rbx",
    "pop rbp",
    "ret",
);

#[cfg(target_arch = "x86_64")]
extern "C" {
    fn maclator_host_syscall(nr: u64, args: *const u64, out: *mut u64);
}

#[cfg(target_arch = "x86_64")]
#[inline]
pub fn raw(nr: u64, args: &[u64; 8]) -> SysResult {
    let mut out = [0u64; 3];
    unsafe { maclator_host_syscall(nr, args.as_ptr(), out.as_mut_ptr()) };
    SysResult { rax: out[0], rdx: out[1], carry: out[2] != 0 }
}

#[cfg(not(target_arch = "x86_64"))]
pub fn raw(_nr: u64, _args: &[u64; 8]) -> SysResult {
    unimplemented!("host syscalls require an x86-64 host")
}

#[inline]
pub fn unix(num: u64, args: &[u64; 8]) -> SysResult {
    raw(CLASS_UNIX | num, args)
}

#[inline]
pub fn mach(num: u64, args: &[u64; 8]) -> SysResult {
    raw(CLASS_MACH | num, args)
}
