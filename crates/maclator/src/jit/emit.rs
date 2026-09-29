//! AArch64 -> x86-64 block translator.
//!
//! Conventions for generated code:
//!  * `r15` holds `&mut Cpu`; guest registers live in the Cpu struct.
//!  * `rsp` is 16-byte aligned, so helpers can be `call`ed directly.
//!  * Generated code never embeds absolute host addresses: helpers and the
//!    exit trampoline are reached through `[r15 + off]`. Code is therefore
//!    position independent, which lets the AOT cache store it on disk.
//!  * Instructions the translator does not handle natively are executed by
//!    calling the reference interpreter (`HELPER_INTERP`) for that single
//!    instruction; all guest state is in memory, so this is always safe.

use iced_x86::code_asm::*;
use maclator_core::cpu::off;
use maclator_core::interp::simd::vfp_expand_imm;
use std::collections::HashMap;

pub const HELPER_INTERP: usize = 0;
pub const HELPER_COMMPAGE: usize = 1;

pub const EXIT_CONTINUE: u64 = 0;
pub const EXIT_SVC: u64 = 1;
pub const EXIT_PENDING: u64 = 2;

pub const MAX_BLOCK_INSNS: usize = 96;

/// How a block leaves to a direct successor that isn't part of this unit.
#[derive(Clone, Copy, PartialEq)]
pub enum StubKind {
    /// Exit through a patchable slot (JIT): `jmp [slot]`, slot initially
    /// points to the exit continuation.
    Patchable,
    /// Plain exit to the dispatcher (AOT blobs).
    Plain,
}

pub struct Stub {
    pub target: u64,
    /// Label of the continuation code the slot initially points to.
    pub cont: CodeLabel,
    /// Label of the 8-byte chaining slot (Patchable only).
    pub slot: CodeLabel,
}

/// Emission context shared by all blocks assembled into one unit.
pub struct Unit {
    pub a: CodeAssembler,
    /// Guest pc -> label of a block inside this unit.
    pub labels: HashMap<u64, CodeLabel>,
    pub stubs: Vec<Stub>,
    pub stub_kind: StubKind,
    /// Verification mode: every block exits after one pass (no self loops).
    pub no_internal_jumps: bool,
    /// Padding inserted before the slot table so slots are 8-byte aligned.
    pub slot_padding: usize,
    /// Label of the first slot (to check alignment after assembly).
    pub first_slot: Option<CodeLabel>,
    /// Emit commpage redirection checks on memory accesses.
    pub commpage_checks: bool,
    pub exit_label: CodeLabel,
    /// Stubs emitted by `finish` (slot init needs their continuation labels).
    pub finished_stubs: Vec<Stub>,
    pub insns_translated: u64,
    pub insns_fallback: u64,
}

impl Unit {
    pub fn new(stub_kind: StubKind, commpage_checks: bool) -> Unit {
        let mut a = CodeAssembler::new(64).unwrap();
        let exit_label = a.create_label();
        Unit {
            a,
            labels: HashMap::new(),
            stubs: Vec::new(),
            stub_kind,
            slot_padding: 0,
            no_internal_jumps: false,
            first_slot: None,
            commpage_checks,
            exit_label,
            finished_stubs: Vec::new(),
            insns_translated: 0,
            insns_fallback: 0,
        }
    }

    /// Emit the shared "jump to dispatcher" tail. Call once after all blocks.
    pub fn finish(&mut self) {
        let a = &mut self.a;
        a.set_label(&mut self.exit_label).unwrap();
        a.jmp(qword_ptr(r15 + off::JIT_EXIT)).unwrap();
        let stubs = std::mem::take(&mut self.stubs);
        for mut s in stubs.into_iter() {
            let a = &mut self.a;
            a.set_label(&mut s.cont).unwrap();
            a.mov(rax, s.target).unwrap();
            a.mov(qword_ptr(r15 + off::PC), rax).unwrap();
            if self.stub_kind == StubKind::Patchable {
                a.lea(rax, qword_ptr(s.slot)).unwrap();
                a.mov(qword_ptr(r15 + off::EXIT_DATA), rax).unwrap();
            } else {
                a.mov(qword_ptr(r15 + off::EXIT_DATA), 0).unwrap();
            }
            a.mov(qword_ptr(r15 + off::EXIT_REASON), EXIT_CONTINUE as i32).unwrap();
            a.jmp(qword_ptr(r15 + off::JIT_EXIT)).unwrap();
            self.finished_stubs.push(s);
        }
        if self.stub_kind == StubKind::Patchable && !self.finished_stubs.is_empty() {
            let a = &mut self.a;
            if self.slot_padding > 0 {
                a.db(&vec![0xccu8; self.slot_padding]).unwrap();
            }
            for (i, s) in self.finished_stubs.iter_mut().enumerate() {
                a.set_label(&mut s.slot).unwrap();
                if i == 0 {
                    self.first_slot = Some(s.slot);
                }
                a.dq(&[0]).unwrap();
            }
        }
    }
}

// ---------- tiny helpers over the assembler ----------

type R64 = AsmRegister64;
type R32 = AsmRegister32;

fn r32_of(r: R64) -> R32 {
    match r {
        x if x == rax => eax,
        x if x == rcx => ecx,
        x if x == rdx => edx,
        x if x == rsi => esi,
        x if x == rdi => edi,
        x if x == r8 => r8d,
        x if x == r9 => r9d,
        x if x == r10 => r10d,
        x if x == r11 => r11d,
        _ => panic!("no 32-bit alias"),
    }
}

fn r8_of(r: R64) -> AsmRegister8 {
    match r {
        x if x == rax => al,
        x if x == rcx => cl,
        x if x == rdx => dl,
        x if x == rsi => sil,
        x if x == rdi => dil,
        x if x == r8 => r8b,
        x if x == r9 => r9b,
        x if x == r10 => r10b,
        x if x == r11 => r11b,
        _ => panic!("no 8-bit alias"),
    }
}

fn r16_of(r: R64) -> AsmRegister16 {
    match r {
        x if x == rax => ax,
        x if x == rcx => cx,
        x if x == rdx => dx,
        x if x == rsi => si,
        x if x == rdi => di,
        x if x == r8 => r8w,
        x if x == r9 => r9w,
        x if x == r10 => r10w,
        x if x == r11 => r11w,
        _ => panic!("no 16-bit alias"),
    }
}

fn xoff(n: u32) -> i32 {
    (off::X + n as usize * 8) as i32
}
fn voff(n: u32) -> i32 {
    (off::V + n as usize * 16) as i32
}

#[inline]
fn bits(insn: u32, hi: u32, lo: u32) -> u32 {
    (insn >> lo) & ((1u32 << (hi - lo + 1)) - 1)
}
#[inline]
fn bit(insn: u32, b: u32) -> u32 {
    (insn >> b) & 1
}
fn sext(v: u64, n: u32) -> i64 {
    ((v << (64 - n)) as i64) >> (64 - n)
}

struct E<'a> {
    u: &'a mut Unit,
    pc: u64,
}

macro_rules! a {
    ($e:expr, $m:ident ( $($arg:expr),* )) => { $e.u.a.$m($($arg),*).unwrap() };
}

impl<'a> E<'a> {
    /// Load Xn (31 = XZR or SP) into `r`.
    fn ldx(&mut self, r: R64, n: u32, use_sp: bool) {
        if n == 31 && !use_sp {
            a!(self, xor(r32_of(r), r32_of(r)));
        } else {
            a!(self, mov(r, qword_ptr(r15 + xoff(n))));
        }
    }
    /// Load Wn zero-extended.
    fn ldw(&mut self, r: R64, n: u32, use_sp: bool) {
        if n == 31 && !use_sp {
            a!(self, xor(r32_of(r), r32_of(r)));
        } else {
            a!(self, mov(r32_of(r), dword_ptr(r15 + xoff(n))));
        }
    }
    fn ld(&mut self, r: R64, n: u32, use_sp: bool, sf: bool) {
        if sf {
            self.ldx(r, n, use_sp)
        } else {
            self.ldw(r, n, use_sp)
        }
    }
    /// Store `r` to Xn (31 = discard, unless `sp`).
    fn stx(&mut self, n: u32, r: R64, use_sp: bool) {
        if n == 31 && !use_sp {
            return;
        }
        a!(self, mov(qword_ptr(r15 + xoff(n)), r));
    }
    fn mov_imm(&mut self, r: R64, v: u64) {
        if v == 0 {
            a!(self, xor(r32_of(r), r32_of(r)));
        } else if v <= u32::MAX as u64 {
            a!(self, mov(r32_of(r), v as u32));
        } else {
            a!(self, mov(r, v));
        }
    }
    fn st_imm(&mut self, n: u32, v: u64) {
        if n == 31 {
            return;
        }
        if v as i64 >= i32::MIN as i64 && v as i64 <= i32::MAX as i64 {
            a!(self, mov(qword_ptr(r15 + xoff(n)), v as i64 as i32));
        } else {
            // r11 is reserved as the scratch for this (callers may hold rax).
            self.mov_imm(r11, v);
            a!(self, mov(qword_ptr(r15 + xoff(n)), r11));
        }
    }

    /// Store NZCV from the x86 flags of a preceding add/sub/logic op.
    fn flags_add(&mut self) {
        a!(self, sets(byte_ptr(r15 + off::NF)));
        a!(self, sete(byte_ptr(r15 + off::ZF)));
        a!(self, setb(byte_ptr(r15 + off::CF)));
        a!(self, seto(byte_ptr(r15 + off::VF)));
    }
    fn flags_sub(&mut self) {
        a!(self, sets(byte_ptr(r15 + off::NF)));
        a!(self, sete(byte_ptr(r15 + off::ZF)));
        a!(self, setae(byte_ptr(r15 + off::CF)));
        a!(self, seto(byte_ptr(r15 + off::VF)));
    }
    fn flags_logic(&mut self) {
        a!(self, sets(byte_ptr(r15 + off::NF)));
        a!(self, sete(byte_ptr(r15 + off::ZF)));
        a!(self, mov(byte_ptr(r15 + off::CF), 0));
        a!(self, mov(byte_ptr(r15 + off::VF), 0));
    }

    /// Evaluate condition `cond` into `al` (0/1).
    fn cond_to_al(&mut self, cond: u32) {
        let base = cond >> 1;
        match base {
            0 => a!(self, movzx(eax, byte_ptr(r15 + off::ZF))),
            1 => a!(self, movzx(eax, byte_ptr(r15 + off::CF))),
            2 => a!(self, movzx(eax, byte_ptr(r15 + off::NF))),
            3 => a!(self, movzx(eax, byte_ptr(r15 + off::VF))),
            4 => {
                a!(self, movzx(eax, byte_ptr(r15 + off::ZF)));
                a!(self, xor(eax, 1));
                a!(self, and(al, byte_ptr(r15 + off::CF)));
            }
            5 => {
                a!(self, movzx(eax, byte_ptr(r15 + off::NF)));
                a!(self, xor(al, byte_ptr(r15 + off::VF)));
                a!(self, xor(eax, 1));
            }
            6 => {
                a!(self, movzx(eax, byte_ptr(r15 + off::NF)));
                a!(self, xor(al, byte_ptr(r15 + off::VF)));
                a!(self, or(al, byte_ptr(r15 + off::ZF)));
                a!(self, xor(eax, 1));
            }
            _ => a!(self, mov(eax, 1)),
        }
        if cond & 1 != 0 && cond != 0xf {
            a!(self, xor(eax, 1));
        }
    }

    /// Call the interpreter for one instruction; exit if it reports anything.
    fn fallback(&mut self, insn: u32) {
        self.u.insns_fallback += 1;
        a!(self, mov(rdi, r15));
        a!(self, mov(esi, insn));
        self.mov_imm(rdx, self.pc);
        a!(self, call(qword_ptr(r15 + off::helper(HELPER_INTERP))));
        a!(self, test(eax, eax));
        let exit = self.u.exit_label;
        a!(self, jnz(exit));
    }

    /// Fallback for a control-flow instruction: interpreter sets pc; return
    /// to the dispatcher.
    fn fallback_terminator(&mut self, insn: u32) {
        self.fallback(insn);
        let exit = self.u.exit_label;
        a!(self, mov(qword_ptr(r15 + off::EXIT_REASON), EXIT_CONTINUE as i32));
        a!(self, mov(qword_ptr(r15 + off::EXIT_DATA), 0));
        a!(self, jmp(exit));
    }

    /// Jump to a direct successor: inside the unit, or through a stub.
    fn goto(&mut self, target: u64) {
        if !self.u.no_internal_jumps {
            if let Some(l) = self.u.labels.get(&target).copied() {
                a!(self, jmp(l));
                return;
            }
        }
        let cont = self.u.a.create_label();
        let slot = self.u.a.create_label();
        if self.u.stub_kind == StubKind::Patchable {
            // jmp [slot]; the slot initially points at `cont`.
            a!(self, jmp(qword_ptr(slot)));
        } else {
            a!(self, jmp(cont));
        }
        self.u.stubs.push(Stub { target, cont, slot });
    }

    /// Guest effective address in rax; applies commpage redirection if needed.
    fn commpage_fix(&mut self) {
        if !self.u.commpage_checks {
            return;
        }
        let mut skip = self.u.a.create_label();
        a!(self, mov(rcx, rax));
        a!(self, shr(rcx, 16));
        a!(self, cmp(ecx, 0xF_FFFF));
        a!(self, jne(skip));
        a!(self, push(r8));
        a!(self, push(r9));
        a!(self, mov(rdi, rax));
        a!(self, call(qword_ptr(r15 + off::helper(HELPER_COMMPAGE))));
        a!(self, pop(r9));
        a!(self, pop(r8));
        self.u.a.set_label(&mut skip).unwrap();
        a!(self, nop());
    }
}

/// Debugging aid: MACLATOR_JIT_DISABLE=dpimm,branch,ldst,dpreg routes those
/// instruction classes to the interpreter.
fn disabled_classes() -> u32 {
    use std::sync::OnceLock;
    static D: OnceLock<u32> = OnceLock::new();
    *D.get_or_init(|| {
        let v = std::env::var("MACLATOR_JIT_DISABLE").unwrap_or_default();
        let mut m = 0;
        for p in v.split(',') {
            m |= match p {
                "dpimm" => 1,
                "branch" => 2,
                "ldst" => 4,
                "dpreg" => 8,
                "all" => 15,
                "b" => 16,
                "bcond" => 32,
                "cbz" => 64,
                "tbz" => 128,
                "br" => 256,
                _ => 0,
            };
        }
        m
    })
}

/// Is this instruction a block terminator (changes control flow)?
pub fn is_terminator(insn: u32) -> bool {
    (insn & 0x7C00_0000 == 0x1400_0000)          // B, BL
        || (insn & 0xFF00_0000 == 0x5400_0000)   // B.cond / BC.cond
        || (insn & 0x7E00_0000 == 0x3400_0000)   // CBZ/CBNZ
        || (insn & 0x7E00_0000 == 0x3600_0000)   // TBZ/TBNZ
        || (insn & 0xFE00_0000 == 0xD600_0000)   // BR/BLR/RET(+PAC)
        || (insn & 0xFF00_0000 == 0xD400_0000)   // exceptions (SVC, BRK...)
        || (insn & 0xFFF8_F0E0 == 0xD508_7020)   // IC IVAU (runtime must see it)
}

/// Translate one basic block starting at `pc` into `u`. The caller must have
/// created and registered the label for `pc` if it wants intra-unit jumps.
/// Returns the guest address just past the block.
pub fn emit_block(u: &mut Unit, pc: u64, read: &dyn Fn(u64) -> Option<u32>) -> u64 {
    let mut label = *u.labels.get(&pc).expect("block label");
    u.a.set_label(&mut label).unwrap();
    // set_label updates the label; keep the updated value for label_ip().
    u.labels.insert(pc, label);
    let mut cur = pc;
    let mut count = 0;
    loop {
        let Some(insn) = read(cur) else {
            // Unreadable: let the dispatcher fault properly.
            let mut e = E { u, pc: cur };
            e.st_pc_and_exit(cur);
            return cur;
        };
        let mut e = E { u, pc: cur };
        let term = is_terminator(insn);
        e.u.insns_translated += 1;
        if !e.translate(insn) {
            if term {
                e.fallback_terminator(insn);
            } else {
                e.fallback(insn);
            }
        }
        cur += 4;
        count += 1;
        if term {
            return cur;
        }
        if count >= MAX_BLOCK_INSNS {
            let mut e = E { u, pc: cur };
            e.goto(cur);
            return cur;
        }
        // Stop before running into another block of this unit: jump to it.
        if u.labels.contains_key(&cur) {
            let mut e = E { u, pc: cur };
            e.goto(cur);
            return cur;
        }
    }
}

impl<'a> E<'a> {
    fn st_pc_and_exit(&mut self, pc: u64) {
        self.mov_imm(rax, pc);
        a!(self, mov(qword_ptr(r15 + off::PC), rax));
        a!(self, mov(qword_ptr(r15 + off::EXIT_REASON), EXIT_CONTINUE as i32));
        a!(self, mov(qword_ptr(r15 + off::EXIT_DATA), 0));
        let exit = self.u.exit_label;
        a!(self, jmp(exit));
    }

    /// Returns false if the instruction must go to the interpreter.
    fn translate(&mut self, insn: u32) -> bool {
        let op0 = (insn >> 25) & 0xf;
        let dis = disabled_classes();
        let class = match op0 {
            0b1000 | 0b1001 => 1,
            0b1010 | 0b1011 => 2,
            0b0100 | 0b0110 | 0b1100 | 0b1110 => 4,
            0b0101 | 0b1101 => 8,
            _ => 0,
        };
        if dis & class != 0 {
            return false;
        }
        match op0 {
            0b1000 | 0b1001 => self.dp_imm(insn),
            0b1010 | 0b1011 => self.branch_sys(insn),
            0b0100 | 0b0110 | 0b1100 | 0b1110 => self.ldst(insn),
            0b0101 | 0b1101 => self.dp_reg(insn),
            _ => false,
        }
    }

    // ---------------- data processing (immediate) ----------------

    fn dp_imm(&mut self, insn: u32) -> bool {
        let sf = bit(insn, 31) != 0;
        let rd = bits(insn, 4, 0);
        let rn = bits(insn, 9, 5);
        match bits(insn, 25, 23) {
            0b000 | 0b001 => {
                let immlo = bits(insn, 30, 29) as u64;
                let immhi = bits(insn, 23, 5) as u64;
                let imm = sext((immhi << 2) | immlo, 21) as u64;
                let v = if bit(insn, 31) != 0 {
                    (self.pc & !0xfff).wrapping_add(imm << 12)
                } else {
                    self.pc.wrapping_add(imm)
                };
                self.st_imm(rd, v);
                true
            }
            0b010 => {
                let op = bit(insn, 30);
                let s = bit(insn, 29) != 0;
                let mut imm = bits(insn, 21, 10);
                if bit(insn, 22) != 0 {
                    imm <<= 12;
                }
                self.ld(rax, rn, true, sf);
                if sf {
                    if op == 0 { a!(self, add(rax, imm as i32)) } else { a!(self, sub(rax, imm as i32)) }
                } else if op == 0 {
                    a!(self, add(eax, imm as i32))
                } else {
                    a!(self, sub(eax, imm as i32))
                }
                if s {
                    if op == 0 { self.flags_add() } else { self.flags_sub() }
                }
                self.stx(rd, rax, !s);
                true
            }
            0b100 => {
                let opc = bits(insn, 30, 29);
                let n = bit(insn, 22);
                if !sf && n != 0 {
                    return false;
                }
                let Some(imm) = decode_logic_imm(n, bits(insn, 15, 10), bits(insn, 21, 16), sf) else { return false };
                self.ld(rax, rn, false, sf);
                self.mov_imm(rcx, imm);
                if sf {
                    match opc {
                        0 | 3 => a!(self, and(rax, rcx)),
                        1 => a!(self, or(rax, rcx)),
                        _ => a!(self, xor(rax, rcx)),
                    }
                } else {
                    match opc {
                        0 | 3 => a!(self, and(eax, ecx)),
                        1 => a!(self, or(eax, ecx)),
                        _ => a!(self, xor(eax, ecx)),
                    }
                }
                if opc == 3 {
                    self.flags_logic();
                    self.stx(rd, rax, false);
                } else {
                    self.stx(rd, rax, true);
                }
                true
            }
            0b101 => {
                let opc = bits(insn, 30, 29);
                let hw = bits(insn, 22, 21);
                if !sf && hw > 1 {
                    return false;
                }
                let imm = (bits(insn, 20, 5) as u64) << (hw * 16);
                match opc {
                    0 => {
                        let v = if sf { !imm } else { (!imm) & 0xffff_ffff };
                        self.st_imm(rd, v);
                    }
                    2 => self.st_imm(rd, imm),
                    3 => {
                        if rd == 31 {
                            return true;
                        }
                        self.ldx(rax, rd, false);
                        let mask = !(0xffffu64 << (hw * 16));
                        self.mov_imm(rcx, mask);
                        a!(self, and(rax, rcx));
                        if imm != 0 {
                            self.mov_imm(rcx, imm);
                            a!(self, or(rax, rcx));
                        }
                        if !sf {
                            a!(self, mov(eax, eax));
                        }
                        self.stx(rd, rax, false);
                    }
                    _ => return false,
                }
                true
            }
            0b110 => self.bitfield(insn),
            0b111 => {
                // EXTR
                if bit(insn, 21) != 0 || bits(insn, 30, 29) != 0 || (sf as u32) != bit(insn, 22) {
                    return false;
                }
                let rm = bits(insn, 20, 16);
                let lsb = bits(insn, 15, 10);
                if !sf && lsb >= 32 {
                    return false;
                }
                self.ld(rax, rn, false, sf);
                self.ld(rcx, rm, false, sf);
                if lsb != 0 {
                    if sf {
                        a!(self, shrd(rcx, rax, lsb));
                    } else {
                        a!(self, shrd(ecx, eax, lsb));
                    }
                }
                self.stx(rd, rcx, false);
                true
            }
            _ => false,
        }
    }

    fn bitfield(&mut self, insn: u32) -> bool {
        let sf = bit(insn, 31) != 0;
        let opc = bits(insn, 30, 29);
        let n = bit(insn, 22);
        if (sf as u32) != n || opc == 3 || opc == 1 {
            return false; // BFM handled by the interpreter
        }
        let rd = bits(insn, 4, 0);
        let rn = bits(insn, 9, 5);
        let immr = bits(insn, 21, 16);
        let imms = bits(insn, 15, 10);
        let dsz = if sf { 64 } else { 32 };
        if immr >= dsz || imms >= dsz {
            return false;
        }
        self.ld(rax, rn, false, sf);
        if sf {
            if opc == 2 {
                // UBFM
                if imms >= immr {
                    // UBFX / LSR
                    let width = imms - immr + 1;
                    if immr != 0 {
                        a!(self, shr(rax, immr));
                    }
                    if width < 64 {
                        if width == 32 {
                            a!(self, mov(eax, eax));
                        } else {
                            self.mov_imm(rcx, (1u64 << width) - 1);
                            a!(self, and(rax, rcx));
                        }
                    }
                } else {
                    // UBFIZ / LSL
                    let width = imms + 1;
                    let sh = 64 - immr;
                    a!(self, shl(rax, 64 - width));
                    a!(self, shr(rax, 64 - width - sh));
                }
            } else {
                // SBFM
                if imms >= immr {
                    let l = 63 - imms;
                    if l != 0 {
                        a!(self, shl(rax, l));
                    }
                    let r = l + immr;
                    if r != 0 {
                        a!(self, sar(rax, r));
                    }
                } else {
                    let l = 63 - imms;
                    a!(self, shl(rax, l));
                    a!(self, sar(rax, l));
                    a!(self, shl(rax, 64 - immr));
                }
            }
        } else if opc == 2 {
            if imms >= immr {
                let width = imms - immr + 1;
                if immr != 0 {
                    a!(self, shr(eax, immr));
                }
                if width < 32 {
                    a!(self, and(eax, ((1u64 << width) - 1) as i32));
                }
            } else {
                let width = imms + 1;
                let sh = 32 - immr;
                a!(self, shl(eax, 32 - width));
                a!(self, shr(eax, 32 - width - sh));
            }
        } else if imms >= immr {
            let l = 31 - imms;
            if l != 0 {
                a!(self, shl(eax, l));
            }
            let r = l + immr;
            if r != 0 {
                a!(self, sar(eax, r));
            }
        } else {
            let l = 31 - imms;
            a!(self, shl(eax, l));
            a!(self, sar(eax, l));
            a!(self, shl(eax, 32 - immr));
        }
        self.stx(rd, rax, false);
        true
    }

    // ---------------- data processing (register) ----------------

    /// Apply shift `ty` by `amt` to `r` (datasize from sf).
    fn shift(&mut self, r: R64, ty: u32, amt: u32, sf: bool) {
        if amt == 0 {
            return;
        }
        if sf {
            match ty {
                0 => a!(self, shl(r, amt)),
                1 => a!(self, shr(r, amt)),
                2 => a!(self, sar(r, amt)),
                _ => a!(self, ror(r, amt)),
            }
        } else {
            let r = r32_of(r);
            match ty {
                0 => a!(self, shl(r, amt)),
                1 => a!(self, shr(r, amt)),
                2 => a!(self, sar(r, amt)),
                _ => a!(self, ror(r, amt)),
            }
        }
    }

    /// Extend `r` in place according to `option`, then shift left by `sh`.
    fn extend(&mut self, r: R64, option: u32, sh: u32) {
        let r32 = r32_of(r);
        match option {
            0 => a!(self, movzx(r32, r8_of(r))),
            1 => a!(self, movzx(r32, r16_of(r))),
            2 => a!(self, mov(r32, r32)),
            3 | 7 => {}
            4 => a!(self, movsx(r, r8_of(r))),
            5 => a!(self, movsx(r, r16_of(r))),
            _ => a!(self, movsxd(r, r32)),
        }
        if sh != 0 {
            a!(self, shl(r, sh));
        }
    }

    fn dp_reg(&mut self, insn: u32) -> bool {
        let sf = bit(insn, 31) != 0;
        let rd = bits(insn, 4, 0);
        let rn = bits(insn, 9, 5);
        let rm = bits(insn, 20, 16);
        if insn & 0x1F00_0000 == 0x0A00_0000 {
            // logical shifted register
            let opc = bits(insn, 30, 29);
            let ty = bits(insn, 23, 22);
            let nflag = bit(insn, 21) != 0;
            let amt = bits(insn, 15, 10);
            if !sf && amt >= 32 {
                return false;
            }
            // MOV (register) fast path: ORR Rd, ZR, Rm
            if opc == 1 && rn == 31 && amt == 0 && !nflag {
                self.ld(rax, rm, false, sf);
                self.stx(rd, rax, false);
                return true;
            }
            self.ld(rcx, rm, false, sf);
            self.shift(rcx, ty, amt, sf);
            if nflag {
                if sf { a!(self, not(rcx)) } else { a!(self, not(ecx)) }
            }
            self.ld(rax, rn, false, sf);
            if sf {
                match opc {
                    0 | 3 => a!(self, and(rax, rcx)),
                    1 => a!(self, or(rax, rcx)),
                    _ => a!(self, xor(rax, rcx)),
                }
            } else {
                match opc {
                    0 | 3 => a!(self, and(eax, ecx)),
                    1 => a!(self, or(eax, ecx)),
                    _ => a!(self, xor(eax, ecx)),
                }
            }
            if opc == 3 {
                self.flags_logic();
            }
            self.stx(rd, rax, false);
            return true;
        }
        if insn & 0x1F20_0000 == 0x0B00_0000 {
            // add/sub shifted register
            let op = bit(insn, 30);
            let s = bit(insn, 29) != 0;
            let ty = bits(insn, 23, 22);
            let amt = bits(insn, 15, 10);
            if ty == 3 || (!sf && amt >= 32) {
                return false;
            }
            self.ld(rcx, rm, false, sf);
            self.shift(rcx, ty, amt, sf);
            self.ld(rax, rn, false, sf);
            self.addsub(op, sf, s);
            self.stx(rd, rax, false);
            return true;
        }
        if insn & 0x1F20_0000 == 0x0B20_0000 {
            // add/sub extended register
            let op = bit(insn, 30);
            let s = bit(insn, 29) != 0;
            let option = bits(insn, 15, 13);
            let imm3 = bits(insn, 12, 10);
            if imm3 > 4 || bits(insn, 23, 22) != 0 {
                return false;
            }
            self.ldx(rcx, rm, false);
            self.extend(rcx, option, imm3);
            self.ld(rax, rn, true, sf);
            self.addsub(op, sf, s);
            self.stx(rd, rax, !s);
            return true;
        }
        if insn & 0x1FE0_FC00 == 0x1A00_0000 {
            // ADC / ADCS / SBC / SBCS
            let op = bit(insn, 30);
            let s = bit(insn, 29) != 0;
            self.ld(rax, rn, false, sf);
            self.ld(rcx, rm, false, sf);
            if op == 0 {
                // CF := C
                a!(self, bt(dword_ptr(r15 + (off::NF as i32 & !3)), ((off::CF & 3) * 8) as u32));
                if sf { a!(self, adc(rax, rcx)) } else { a!(self, adc(eax, ecx)) }
                if s {
                    self.flags_add();
                }
            } else {
                // CF := !C (borrow)
                a!(self, cmp(byte_ptr(r15 + off::CF), 1));
                if sf { a!(self, sbb(rax, rcx)) } else { a!(self, sbb(eax, ecx)) }
                if s {
                    self.flags_sub();
                }
            }
            self.stx(rd, rax, false);
            return true;
        }
        if insn & 0x1FE0_0410 == 0x1A40_0000 && bit(insn, 29) == 1 {
            // CCMN / CCMP (register or immediate)
            let op = bit(insn, 30);
            let cond = bits(insn, 15, 12);
            let nzcv = bits(insn, 3, 0);
            let imm = bit(insn, 11) != 0;
            let mut l_else = self.u.a.create_label();
            let mut l_end = self.u.a.create_label();
            self.cond_to_al(cond);
            a!(self, test(al, al));
            a!(self, jz(l_else));
            self.ld(rax, rn, false, sf);
            if imm {
                self.mov_imm(rcx, rm as u64);
            } else {
                self.ld(rcx, rm, false, sf);
            }
            if op == 0 {
                if sf { a!(self, add(rax, rcx)) } else { a!(self, add(eax, ecx)) }
                self.flags_add();
            } else {
                if sf { a!(self, cmp(rax, rcx)) } else { a!(self, cmp(eax, ecx)) }
                self.flags_sub();
            }
            a!(self, jmp(l_end));
            self.u.a.set_label(&mut l_else).unwrap();
            a!(self, mov(byte_ptr(r15 + off::NF), ((nzcv >> 3) & 1) as u32));
            a!(self, mov(byte_ptr(r15 + off::ZF), ((nzcv >> 2) & 1) as u32));
            a!(self, mov(byte_ptr(r15 + off::CF), ((nzcv >> 1) & 1) as u32));
            a!(self, mov(byte_ptr(r15 + off::VF), (nzcv & 1) as u32));
            self.u.a.set_label(&mut l_end).unwrap();
            a!(self, nop());
            return true;
        }
        if insn & 0x3FE0_0800 == 0x1A80_0000 {
            // CSEL / CSINC / CSINV / CSNEG
            let op = bit(insn, 30);
            let o2 = bit(insn, 10);
            let cond = bits(insn, 15, 12);
            self.cond_to_al(cond);
            self.ld(rcx, rn, false, sf);
            self.ld(rdx, rm, false, sf);
            match (op, o2) {
                (0, 0) => {}
                (0, _) => if sf { a!(self, inc(rdx)) } else { a!(self, inc(edx)) },
                (_, 0) => if sf { a!(self, not(rdx)) } else { a!(self, not(edx)) },
                _ => if sf { a!(self, neg(rdx)) } else { a!(self, neg(edx)) },
            }
            a!(self, test(al, al));
            if sf { a!(self, cmovz(rcx, rdx)) } else { a!(self, cmovz(ecx, edx)) }
            self.stx(rd, rcx, false);
            return true;
        }
        if insn & 0x5FE0_0000 == 0x1AC0_0000 {
            return self.dp2(insn, sf, rd, rn, rm);
        }
        if insn & 0x5FE0_0000 == 0x5AC0_0000 {
            return self.dp1(insn, sf, rd, rn);
        }
        if insn & 0x1F00_0000 == 0x1B00_0000 {
            // 3-source
            let op31 = bits(insn, 23, 21);
            let o0 = bit(insn, 15);
            let ra = bits(insn, 14, 10);
            if bits(insn, 30, 29) != 0 {
                return false;
            }
            match (op31, o0) {
                (0, _) => {
                    self.ld(rax, rn, false, sf);
                    self.ld(rcx, rm, false, sf);
                    if sf { a!(self, imul_2(rax, rcx)) } else { a!(self, imul_2(eax, ecx)) }
                    if ra != 31 {
                        self.ld(rcx, ra, false, sf);
                        if o0 == 0 {
                            if sf { a!(self, add(rax, rcx)) } else { a!(self, add(eax, ecx)) }
                        } else if sf {
                            a!(self, sub(rcx, rax));
                            a!(self, mov(rax, rcx));
                        } else {
                            a!(self, sub(ecx, eax));
                            a!(self, mov(eax, ecx));
                        }
                    } else if o0 == 1 {
                        if sf { a!(self, neg(rax)) } else { a!(self, neg(eax)) }
                    }
                    self.stx(rd, rax, false);
                    true
                }
                (1, _) | (5, _) if sf => {
                    // SMADDL/SMSUBL/UMADDL/UMSUBL
                    self.ldw(rax, rn, false);
                    self.ldw(rcx, rm, false);
                    if op31 == 1 {
                        a!(self, movsxd(rax, eax));
                        a!(self, movsxd(rcx, ecx));
                    }
                    a!(self, imul_2(rax, rcx));
                    if ra != 31 || o0 == 1 {
                        self.ldx(rcx, ra, false);
                        if o0 == 0 {
                            a!(self, add(rax, rcx));
                        } else {
                            a!(self, sub(rcx, rax));
                            a!(self, mov(rax, rcx));
                        }
                    }
                    self.stx(rd, rax, false);
                    true
                }
                (2, 0) | (6, 0) if sf => {
                    self.ldx(rax, rn, false);
                    self.ldx(rcx, rm, false);
                    if op31 == 2 { a!(self, imul(rcx)) } else { a!(self, mul(rcx)) }
                    self.stx(rd, rdx, false);
                    true
                }
                _ => false,
            }
        } else {
            false
        }
    }

    /// rax = rax op rcx with optional flags.
    fn addsub(&mut self, op: u32, sf: bool, s: bool) {
        if sf {
            if op == 0 { a!(self, add(rax, rcx)) } else { a!(self, sub(rax, rcx)) }
        } else if op == 0 {
            a!(self, add(eax, ecx))
        } else {
            a!(self, sub(eax, ecx))
        }
        if s {
            if op == 0 { self.flags_add() } else { self.flags_sub() }
        }
    }

    fn dp2(&mut self, insn: u32, sf: bool, rd: u32, rn: u32, rm: u32) -> bool {
        let opcode = bits(insn, 15, 10);
        match opcode {
            0b000010 | 0b000011 => {
                // UDIV / SDIV (x86 faults on /0 and INT_MIN/-1; ARM does not)
                let signed = opcode == 3;
                let mut l_zero = self.u.a.create_label();
                let mut l_neg = self.u.a.create_label();
                let mut l_end = self.u.a.create_label();
                self.ld(rax, rn, false, sf);
                self.ld(rcx, rm, false, sf);
                if sf { a!(self, test(rcx, rcx)) } else { a!(self, test(ecx, ecx)) }
                a!(self, jz(l_zero));
                if signed {
                    if sf { a!(self, cmp(rcx, -1)) } else { a!(self, cmp(ecx, -1)) }
                    a!(self, je(l_neg));
                    if sf {
                        a!(self, cqo());
                        a!(self, idiv(rcx));
                    } else {
                        a!(self, cdq());
                        a!(self, idiv(ecx));
                    }
                } else {
                    a!(self, xor(edx, edx));
                    if sf { a!(self, div(rcx)) } else { a!(self, div(ecx)) }
                }
                a!(self, jmp(l_end));
                self.u.a.set_label(&mut l_neg).unwrap();
                if sf { a!(self, neg(rax)) } else { a!(self, neg(eax)) }
                a!(self, jmp(l_end));
                self.u.a.set_label(&mut l_zero).unwrap();
                a!(self, xor(eax, eax));
                self.u.a.set_label(&mut l_end).unwrap();
                if !sf {
                    a!(self, mov(eax, eax));
                } else {
                    a!(self, nop());
                }
                self.stx(rd, rax, false);
                true
            }
            0b001000..=0b001011 => {
                self.ld(rax, rn, false, sf);
                self.ld(rcx, rm, false, sf);
                if sf {
                    match opcode & 3 {
                        0 => a!(self, shl(rax, cl)),
                        1 => a!(self, shr(rax, cl)),
                        2 => a!(self, sar(rax, cl)),
                        _ => a!(self, ror(rax, cl)),
                    }
                } else {
                    match opcode & 3 {
                        0 => a!(self, shl(eax, cl)),
                        1 => a!(self, shr(eax, cl)),
                        2 => a!(self, sar(eax, cl)),
                        _ => a!(self, ror(eax, cl)),
                    }
                }
                self.stx(rd, rax, false);
                true
            }
            _ => false,
        }
    }

    fn dp1(&mut self, insn: u32, sf: bool, rd: u32, rn: u32) -> bool {
        let opcode2 = bits(insn, 20, 16);
        let opcode = bits(insn, 15, 10);
        if bit(insn, 29) != 0 {
            return false;
        }
        if opcode2 == 1 && sf {
            // Pointer authentication: sign/auth are the identity.
            return match opcode {
                0..=15 => true,
                16 => {
                    // XPACI: clear PAC bits 63:56,54:47 (bit 55 clear => user pointer)
                    self.ldx(rax, rd, false);
                    let mut l = self.u.a.create_label();
                    a!(self, bt(rax, 55));
                    a!(self, jc(l));
                    self.mov_imm(rcx, 0x0000_7FFF_FFFF_FFFF);
                    a!(self, and(rax, rcx));
                    self.u.a.set_label(&mut l).unwrap();
                    a!(self, nop());
                    self.stx(rd, rax, false);
                    true
                }
                17 => {
                    self.ldx(rax, rd, false);
                    let mut l = self.u.a.create_label();
                    a!(self, bt(rax, 55));
                    a!(self, jc(l));
                    self.mov_imm(rcx, !0x007F_8000_0000_0000u64);
                    a!(self, and(rax, rcx));
                    self.u.a.set_label(&mut l).unwrap();
                    a!(self, nop());
                    self.stx(rd, rax, false);
                    true
                }
                _ => false,
            };
        }
        if opcode2 != 0 {
            return false;
        }
        match opcode {
            0b000010 if !sf => {
                self.ldw(rax, rn, false);
                a!(self, bswap(eax));
                self.stx(rd, rax, false);
                true
            }
            0b000011 if sf => {
                self.ldx(rax, rn, false);
                a!(self, bswap(rax));
                self.stx(rd, rax, false);
                true
            }
            0b000100 => {
                // CLZ via BSR
                self.ld(rax, rn, false, sf);
                let mut l_zero = self.u.a.create_label();
                let mut l_end = self.u.a.create_label();
                if sf {
                    a!(self, bsr(rcx, rax));
                    a!(self, jz(l_zero));
                    a!(self, mov(eax, 63));
                    a!(self, sub(eax, ecx));
                    a!(self, jmp(l_end));
                    self.u.a.set_label(&mut l_zero).unwrap();
                    a!(self, mov(eax, 64));
                } else {
                    a!(self, bsr(ecx, eax));
                    a!(self, jz(l_zero));
                    a!(self, mov(eax, 31));
                    a!(self, sub(eax, ecx));
                    a!(self, jmp(l_end));
                    self.u.a.set_label(&mut l_zero).unwrap();
                    a!(self, mov(eax, 32));
                }
                self.u.a.set_label(&mut l_end).unwrap();
                a!(self, nop());
                self.stx(rd, rax, false);
                true
            }
            _ => false,
        }
    }

    // ---------------- branches & system ----------------

    fn branch_sys(&mut self, insn: u32) -> bool {
        let dis = disabled_classes();
        if (dis & 16 != 0 && insn & 0x7C00_0000 == 0x1400_0000)
            || (dis & 32 != 0 && insn & 0xFF00_0000 == 0x5400_0000)
            || (dis & 64 != 0 && insn & 0x7E00_0000 == 0x3400_0000)
            || (dis & 128 != 0 && insn & 0x7E00_0000 == 0x3600_0000)
            || (dis & 256 != 0 && insn & 0xFE00_0000 == 0xD600_0000)
        {
            return false;
        }
        let pc = self.pc;
        let next = pc.wrapping_add(4);
        if insn & 0x7C00_0000 == 0x1400_0000 {
            let target = pc.wrapping_add((sext((insn & 0x03ff_ffff) as u64, 26) << 2) as u64);
            if insn & 0x8000_0000 != 0 {
                self.st_imm(30, next);
            }
            self.goto(target);
            return true;
        }
        if insn & 0xFF00_0000 == 0x5400_0000 {
            let target = pc.wrapping_add((sext(bits(insn, 23, 5) as u64, 19) << 2) as u64);
            let cond = bits(insn, 3, 0);
            if cond >= 14 {
                self.goto(target);
                return true;
            }
            self.cond_to_al(cond);
            self.cond_branch(target, next);
            return true;
        }
        if insn & 0x7E00_0000 == 0x3400_0000 {
            let sf = bit(insn, 31) != 0;
            let target = pc.wrapping_add((sext(bits(insn, 23, 5) as u64, 19) << 2) as u64);
            let rt = bits(insn, 4, 0);
            let nz = bit(insn, 24) != 0;
            if rt == 31 {
                a!(self, xor(eax, eax));
            } else if sf {
                a!(self, cmp(qword_ptr(r15 + xoff(rt)), 0));
            } else {
                a!(self, cmp(dword_ptr(r15 + xoff(rt)), 0));
            }
            if nz { a!(self, setne(al)) } else { a!(self, sete(al)) }
            self.cond_branch(target, next);
            return true;
        }
        if insn & 0x7E00_0000 == 0x3600_0000 {
            let b = (bit(insn, 31) << 5) | bits(insn, 23, 19);
            let target = pc.wrapping_add((sext(bits(insn, 18, 5) as u64, 14) << 2) as u64);
            let rt = bits(insn, 4, 0);
            let nz = bit(insn, 24) != 0;
            self.ldx(rax, rt, false);
            a!(self, bt(rax, b));
            if nz { a!(self, setc(al)) } else { a!(self, setnc(al)) }
            self.cond_branch(target, next);
            return true;
        }
        if insn & 0xFE00_0000 == 0xD600_0000 {
            let opc = bits(insn, 24, 21);
            let op2 = bits(insn, 20, 16);
            let op3 = bits(insn, 15, 10);
            let rn = bits(insn, 9, 5);
            if op2 != 0x1f {
                return false;
            }
            let reg = match opc {
                0 | 1 if op3 == 0 || op3 == 2 || op3 == 3 => rn,
                2 => if op3 == 0 { rn } else { 30 },
                8 | 9 => rn,
                _ => return false,
            };
            self.ldx(rax, reg, false);
            if opc & 1 != 0 {
                self.st_imm(30, next);
            }
            self.indirect(opc == 2);
            return true;
        }
        if insn & 0xFFE0_001F == 0xD400_0001 {
            // SVC
            let imm = bits(insn, 20, 5);
            self.mov_imm(rax, next);
            a!(self, mov(qword_ptr(r15 + off::PC), rax));
            a!(self, mov(qword_ptr(r15 + off::EXIT_REASON), EXIT_SVC as i32));
            a!(self, mov(qword_ptr(r15 + off::EXIT_DATA), imm as i32));
            let exit = self.u.exit_label;
            a!(self, jmp(exit));
            return true;
        }
        // hints (NOP, BTI, PAC*SP, AUT*SP, YIELD...)
        if insn & 0xFFFF_F01F == 0xD503_201F {
            let crm_op2 = bits(insn, 11, 5);
            if crm_op2 == 7 {
                return false; // XPACLRI -> interpreter
            }
            if crm_op2 >= 0x08 && crm_op2 <= 0x0f {
                // PACIA1716 etc: identity
                return true;
            }
            return true;
        }
        // barriers
        if insn & 0xFFFF_F01F == 0xD503_301F {
            let op2 = bits(insn, 7, 5);
            let crm = bits(insn, 11, 8);
            match op2 {
                4 | 5 => {
                    if crm & 3 == 3 || crm & 3 == 0 {
                        a!(self, mfence());
                    }
                    return true;
                }
                6 | 7 => return true, // ISB, SB
                _ => return false,
            }
        }
        match insn & 0xFFFF_FFE0 {
            0xD53B_D060 => {
                // MRS TPIDRRO_EL0
                a!(self, mov(rax, qword_ptr(r15 + off::TPIDRRO)));
                self.stx(insn & 31, rax, false);
                return true;
            }
            0xD53B_D040 => {
                a!(self, mov(rax, qword_ptr(r15 + off::TPIDR)));
                self.stx(insn & 31, rax, false);
                return true;
            }
            0xD51B_D040 => {
                self.ldx(rax, insn & 31, false);
                a!(self, mov(qword_ptr(r15 + off::TPIDR), rax));
                return true;
            }
            _ => {}
        }
        false
    }

    /// al holds the condition; emit taken/not-taken successors.
    fn cond_branch(&mut self, taken: u64, not_taken: u64) {
        let mut l_nt = self.u.a.create_label();
        a!(self, test(al, al));
        a!(self, jz(l_nt));
        self.goto(taken);
        self.u.a.set_label(&mut l_nt).unwrap();
        self.goto(not_taken);
    }

    /// Indirect branch to the guest address in rax via the per-thread
    /// indirect-branch table; misses go to the dispatcher.
    fn indirect(&mut self, _is_ret: bool) {
        let mut l_miss = self.u.a.create_label();
        a!(self, mov(rcx, rax));
        a!(self, shr(ecx, 2));
        a!(self, and(ecx, (crate::jit::IBTC_SIZE - 1) as i32));
        a!(self, shl(ecx, 4));
        a!(self, add(rcx, qword_ptr(r15 + off::IBTC)));
        a!(self, cmp(rax, qword_ptr(rcx)));
        a!(self, jne(l_miss));
        a!(self, jmp(qword_ptr(rcx + 8)));
        self.u.a.set_label(&mut l_miss).unwrap();
        a!(self, mov(qword_ptr(r15 + off::PC), rax));
        a!(self, mov(qword_ptr(r15 + off::EXIT_REASON), EXIT_CONTINUE as i32));
        a!(self, mov(qword_ptr(r15 + off::EXIT_DATA), 0));
        let exit = self.u.exit_label;
        a!(self, jmp(exit));
    }

    // ---------------- loads and stores ----------------

    fn ldst(&mut self, insn: u32) -> bool {
        let rt = bits(insn, 4, 0);
        let rn = bits(insn, 9, 5);
        let v = bit(insn, 26) != 0;
        // load literal
        if insn & 0x3B00_0000 == 0x1800_0000 {
            let opc = bits(insn, 31, 30);
            let addr = self.pc.wrapping_add((sext(bits(insn, 23, 5) as u64, 19) << 2) as u64);
            self.mov_imm(rax, addr);
            return if v {
                if opc == 3 {
                    return false;
                }
                self.access(rt, true, [2, 3, 4][opc as usize], 1, true)
            } else {
                match opc {
                    0 => self.access(rt, false, 2, 1, true),
                    1 => self.access(rt, false, 3, 1, true),
                    2 => self.access(rt, false, 18, 1, true),
                    _ => true, // PRFM (literal)
                }
            };
        }
        // pairs
        if insn & 0x3A00_0000 == 0x2800_0000 {
            return self.pair(insn);
        }
        if insn & 0x3B00_0000 == 0x3900_0000 {
            // unsigned immediate
            let size = bits(insn, 31, 30);
            let opc = bits(insn, 23, 22);
            let scale = if v && opc & 2 != 0 { 4 } else { size };
            let imm = (bits(insn, 21, 10) as u64) << scale;
            self.ldx(rax, rn, true);
            if imm != 0 {
                a!(self, add(rax, imm as i32));
            }
            return self.access_sized(rt, v, size, opc);
        }
        if insn & 0x3B20_0000 == 0x3800_0000 {
            let size = bits(insn, 31, 30);
            let opc = bits(insn, 23, 22);
            let imm = sext(bits(insn, 20, 12) as u64, 9);
            let mode = bits(insn, 11, 10);
            self.ldx(rax, rn, true);
            match mode {
                0 | 2 => {
                    if imm != 0 {
                        a!(self, add(rax, imm as i32));
                    }
                    self.access_sized(rt, v, size, opc)
                }
                1 => {
                    // post-index: access at base, then base += imm
                    a!(self, mov(r8, rax));
                    if !self.access_sized(rt, v, size, opc) {
                        return false;
                    }
                    a!(self, add(r8, imm as i32));
                    a!(self, mov(qword_ptr(r15 + xoff(rn)), r8));
                    true
                }
                _ => {
                    a!(self, add(rax, imm as i32));
                    a!(self, mov(r8, rax));
                    if !self.access_sized(rt, v, size, opc) {
                        return false;
                    }
                    a!(self, mov(qword_ptr(r15 + xoff(rn)), r8));
                    true
                }
            }
        } else if insn & 0x3B20_0C00 == 0x3820_0800 {
            // register offset
            let size = bits(insn, 31, 30);
            let opc = bits(insn, 23, 22);
            let rm = bits(insn, 20, 16);
            let option = bits(insn, 15, 13);
            let s = bit(insn, 12);
            if option & 2 == 0 {
                return false;
            }
            let scale = if v && opc & 2 != 0 { 4 } else { size };
            self.ldx(rcx, rm, false);
            self.extend(rcx, option, if s != 0 { scale } else { 0 });
            self.ldx(rax, rn, true);
            a!(self, add(rax, rcx));
            self.access_sized(rt, v, size, opc)
        } else {
            false
        }
    }

    /// Access with LDR/STR (size, opc) semantics at address rax.
    /// Preserves r8 (used for writeback).
    fn access_sized(&mut self, rt: u32, v: bool, size: u32, opc: u32) -> bool {
        if v {
            let q = opc & 2 != 0;
            if q && size != 0 {
                return false;
            }
            let kind = if q { 4 } else { size };
            return self.access(rt, true, kind, opc & 1, false);
        }
        // kind: 0..3 = zero-extend size, 5..6 = sign-extend to 64 (b,h), 7 = sign-extend w, 8.. = sign-extend to 32
        match opc {
            0 => self.access(rt, false, size, 0, false),
            1 => self.access(rt, false, size, 1, false),
            2 => {
                if size == 3 {
                    return true; // PRFM
                }
                self.access(rt, false, 16 + size, 1, false)
            }
            _ => {
                if size >= 2 {
                    return false;
                }
                self.access(rt, false, 32 + size, 1, false)
            }
        }
    }

    /// Core memory access at host address rax.
    /// kind: 0-3 = 1/2/4/8 bytes; 4 = 16 bytes (SIMD only); 16+s = sign-extend
    /// to 64; 32+s = sign-extend to 32. `load`: 1 = load, 0 = store.
    fn access(&mut self, rt: u32, v: bool, kind: u32, load: u32, literal: bool) -> bool {
        let _ = literal;
        self.commpage_fix();
        let load = load != 0;
        if v {
            match (kind, load) {
                (4, true) => {
                    a!(self, movdqu(xmm0, xmmword_ptr(rax)));
                    a!(self, movdqu(xmmword_ptr(r15 + voff(rt)), xmm0));
                }
                (4, false) => {
                    a!(self, movdqu(xmm0, xmmword_ptr(r15 + voff(rt))));
                    a!(self, movdqu(xmmword_ptr(rax), xmm0));
                }
                (k, true) if k < 4 => {
                    match k {
                        0 => a!(self, movzx(ecx, byte_ptr(rax))),
                        1 => a!(self, movzx(ecx, word_ptr(rax))),
                        2 => a!(self, mov(ecx, dword_ptr(rax))),
                        _ => a!(self, mov(rcx, qword_ptr(rax))),
                    }
                    a!(self, mov(qword_ptr(r15 + voff(rt)), rcx));
                    a!(self, mov(qword_ptr(r15 + voff(rt) + 8), 0));
                }
                (k, false) if k < 4 => match k {
                    0 => {
                        a!(self, mov(cl, byte_ptr(r15 + voff(rt))));
                        a!(self, mov(byte_ptr(rax), cl));
                    }
                    1 => {
                        a!(self, mov(cx, word_ptr(r15 + voff(rt))));
                        a!(self, mov(word_ptr(rax), cx));
                    }
                    2 => {
                        a!(self, mov(ecx, dword_ptr(r15 + voff(rt))));
                        a!(self, mov(dword_ptr(rax), ecx));
                    }
                    _ => {
                        a!(self, mov(rcx, qword_ptr(r15 + voff(rt))));
                        a!(self, mov(qword_ptr(rax), rcx));
                    }
                },
                _ => return false,
            }
            return true;
        }
        if load {
            match kind {
                0 => a!(self, movzx(ecx, byte_ptr(rax))),
                1 => a!(self, movzx(ecx, word_ptr(rax))),
                2 => a!(self, mov(ecx, dword_ptr(rax))),
                3 => a!(self, mov(rcx, qword_ptr(rax))),
                16 => a!(self, movsx(rcx, byte_ptr(rax))),
                17 => a!(self, movsx(rcx, word_ptr(rax))),
                18 => a!(self, movsxd(rcx, dword_ptr(rax))),
                32 => a!(self, movsx(ecx, byte_ptr(rax))),
                33 => a!(self, movsx(ecx, word_ptr(rax))),
                _ => return false,
            }
            self.stx(rt, rcx, false);
        } else {
            self.ldx(rcx, rt, false);
            match kind {
                0 => a!(self, mov(byte_ptr(rax), cl)),
                1 => a!(self, mov(word_ptr(rax), cx)),
                2 => a!(self, mov(dword_ptr(rax), ecx)),
                3 => a!(self, mov(qword_ptr(rax), rcx)),
                _ => return false,
            }
        }
        true
    }

    fn pair(&mut self, insn: u32) -> bool {
        let opc = bits(insn, 31, 30);
        let v = bit(insn, 26) != 0;
        let idx = bits(insn, 24, 23);
        let l = bit(insn, 22) != 0;
        let rt = bits(insn, 4, 0);
        let rt2 = bits(insn, 14, 10);
        let rn = bits(insn, 9, 5);
        let (scale, kind) = if v {
            if opc == 3 {
                return false;
            }
            (2 + opc, if opc == 2 { 4 } else { 2 + opc })
        } else {
            match opc {
                0 => (2, 2),
                1 if l => (2, 18),
                2 => (3, 3),
                _ => return false,
            }
        };
        let off = (sext(bits(insn, 21, 15) as u64, 7) << scale) as i32;
        let nb = 1i32 << scale;
        self.ldx(rax, rn, true);
        if idx == 3 || idx == 2 || idx == 0 {
            if off != 0 {
                a!(self, add(rax, off));
            }
        }
        // r8 keeps the writeback value
        if idx == 1 {
            a!(self, lea(r8, qword_ptr(rax + off)));
        } else {
            a!(self, mov(r8, rax));
        }
        a!(self, mov(r9, rax));
        if l && !v && rt == rt2 {
            return false;
        }
        if !self.access(rt, v, kind, l as u32, false) {
            return false;
        }
        a!(self, lea(rax, qword_ptr(r9 + nb)));
        if !self.access(rt2, v, kind, l as u32, false) {
            return false;
        }
        if idx == 1 || idx == 3 {
            a!(self, mov(qword_ptr(r15 + xoff(rn)), r8));
        }
        true
    }
}

/// Logical immediate decoding (returns None for reserved encodings).
fn decode_logic_imm(n: u32, imms: u32, immr: u32, sf: bool) -> Option<u64> {
    let combined = (n << 6) | (!imms & 0x3f);
    if combined == 0 {
        return None;
    }
    let len = 31 - combined.leading_zeros();
    if len < 1 {
        return None;
    }
    let esize = 1u32 << len;
    let dsz = if sf { 64 } else { 32 };
    if esize > dsz {
        return None;
    }
    let levels = esize - 1;
    let s = imms & levels;
    let r = immr & levels;
    if s == levels {
        return None;
    }
    let ones = |n: u32| if n >= 64 { u64::MAX } else { (1u64 << n) - 1 };
    let welem = ones(s + 1);
    let emask = ones(esize);
    let rot = if r == 0 { welem } else { ((welem >> r) | (welem << (esize - r))) & emask };
    let mut v = 0u64;
    let mut i = 0;
    while i < dsz {
        v |= rot << i;
        i += esize;
    }
    Some(if sf { v } else { v & 0xffff_ffff })
}

#[allow(dead_code)]
fn _keep(_: u32) -> u64 {
    vfp_expand_imm(0)
}
