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
        // arm64 top-byte-ignore: drop bits 63:56 (flags are dead across memory accesses).
        a!(self, shl(rax, 8));
        a!(self, shr(rax, 8));
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
                "simd" => 512,
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
            0b0111 | 0b1111 => 512,
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
            0b0111 | 0b1111 => self.simd_fp(insn),
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
        if (sf as u32) != n || opc == 3 {
            return false;
        }
        let rd = bits(insn, 4, 0);
        let rn = bits(insn, 9, 5);
        let immr = bits(insn, 21, 16);
        let imms = bits(insn, 15, 10);
        let dsz = if sf { 64 } else { 32 };
        if immr >= dsz || imms >= dsz {
            return false;
        }
        if opc == 1 {
            // BFM (BFI / BFXIL / BFC): dst = (dst & ~tmask) | (((dst & ~wmask) | (ROR(src, immr) & wmask)) & tmask)
            let Some((wmask, tmask)) = maclator_core::interp::decode_bit_masks(n, imms, immr, false, dsz) else {
                return false;
            };
            self.ld(rax, rn, false, sf);
            if immr != 0 {
                if sf {
                    a!(self, ror(rax, immr as i32));
                } else {
                    a!(self, ror(eax, immr as i32));
                }
            }
            self.ld(rcx, rd, false, sf);
            self.mov_imm(rdx, wmask);
            a!(self, and(rax, rdx));
            self.mov_imm(rdx, !wmask);
            a!(self, and(rdx, rcx));
            a!(self, or(rax, rdx));
            self.mov_imm(rdx, tmask);
            a!(self, and(rax, rdx));
            self.mov_imm(rdx, !tmask);
            a!(self, and(rdx, rcx));
            a!(self, or(rax, rdx));
            if !sf {
                a!(self, mov(eax, eax));
            }
            self.stx(rd, rax, false);
            return true;
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
            0b000000 => {
                // RBIT: byte swap, then swap nibbles, bit pairs and bits.
                self.ld(rax, rn, false, sf);
                if sf {
                    a!(self, bswap(rax));
                } else {
                    a!(self, bswap(eax));
                }
                let width_mask: u64 = if sf { u64::MAX } else { 0xffff_ffff };
                for (sh, m) in [(4, 0x0f0f_0f0f_0f0f_0f0fu64), (2, 0x3333_3333_3333_3333), (1, 0x5555_5555_5555_5555)] {
                    self.mov_rr(rcx, rax);
                    a!(self, shr(rax, sh));
                    self.mov_imm(rdx, m & width_mask);
                    a!(self, and(rax, rdx));
                    a!(self, and(rcx, rdx));
                    a!(self, shl(rcx, sh));
                    a!(self, or(rax, rcx));
                }
                self.stx(rd, rax, false);
                true
            }
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

    /// Acquire/release loads and stores and LSE atomics. x86's TSO ordering already gives
    /// acquire/release semantics for plain accesses, and LOCK-prefixed ops are full barriers.
    fn try_atomics(&mut self, insn: u32) -> Option<bool> {
        let rt = bits(insn, 4, 0);
        let rn = bits(insn, 9, 5);
        let rs = bits(insn, 20, 16);
        let size = bits(insn, 31, 30);
        // LDAR/LDLAR/STLR/STLLR
        if insn & 0x3FBF_7C00 == 0x089F_7C00 {
            let load = bit(insn, 22) != 0;
            self.ldx(rax, rn, true);
            return Some(self.access(rt, false, size, load as u32, false));
        }
        // LDAPR (RCpc)
        if insn & 0x3FFF_FC00 == 0x38BF_C000 {
            self.ldx(rax, rn, true);
            return Some(self.access(rt, false, size, 1, false));
        }
        // LDAPUR/STLUR family (RCpc unscaled)
        if insn & 0x3F20_0C00 == 0x1900_0000 {
            let off = sext(bits(insn, 20, 12) as u64, 9) as i64;
            self.ldx(rax, rn, true);
            if off != 0 {
                self.mov_imm(rcx, off as u64);
                a!(self, add(rax, rcx));
            }
            let opc = bits(insn, 23, 22);
            return Some(self.access_sized(rt, false, size, opc));
        }
        // CAS/CASA/CASL/CASAL
        if insn & 0x3FA0_7C00 == 0x08A0_7C00 {
            self.ldx(rax, rn, true);
            self.commpage_fix();
            a!(self, mov(rsi, rax));
            self.ldx(rax, rs, false);
            self.ldx(rdx, rt, false);
            match size {
                0 => { self.u.a.lock().cmpxchg(byte_ptr(rsi), dl).unwrap(); a!(self, movzx(eax, al)); }
                1 => { self.u.a.lock().cmpxchg(word_ptr(rsi), dx).unwrap(); a!(self, movzx(eax, ax)); }
                2 => { self.u.a.lock().cmpxchg(dword_ptr(rsi), edx).unwrap(); a!(self, mov(eax, eax)); }
                _ => { self.u.a.lock().cmpxchg(qword_ptr(rsi), rdx).unwrap(); }
            }
            self.stx(rs, rax, false);
            return Some(true);
        }
        // LD<op> / SWP (LSE): size 111000 A R 1 Rs o3 opc 00 Rn Rt
        if insn & 0x3F20_0C00 == 0x3820_0000 {
            let o3 = bit(insn, 15);
            let opc = bits(insn, 14, 12);
            let simple = (o3 == 0 && opc <= 3) || (o3 == 1 && opc == 0);
            if !simple {
                return None;
            }
            self.ldx(rax, rn, true);
            self.commpage_fix();
            a!(self, mov(rsi, rax));
            self.ldx(rdx, rs, false);
            if o3 == 1 {
                // SWP
                match size {
                    0 => { a!(self, xchg(byte_ptr(rsi), dl)); a!(self, movzx(edx, dl)); }
                    1 => { a!(self, xchg(word_ptr(rsi), dx)); a!(self, movzx(edx, dx)); }
                    2 => { a!(self, xchg(dword_ptr(rsi), edx)); a!(self, mov(edx, edx)); }
                    _ => { a!(self, xchg(qword_ptr(rsi), rdx)); }
                }
                self.stx(rt, rdx, false);
                return Some(true);
            }
            if opc == 0 {
                // LDADD
                match size {
                    0 => { self.u.a.lock().xadd(byte_ptr(rsi), dl).unwrap(); a!(self, movzx(edx, dl)); }
                    1 => { self.u.a.lock().xadd(word_ptr(rsi), dx).unwrap(); a!(self, movzx(edx, dx)); }
                    2 => { self.u.a.lock().xadd(dword_ptr(rsi), edx).unwrap(); a!(self, mov(edx, edx)); }
                    _ => { self.u.a.lock().xadd(qword_ptr(rsi), rdx).unwrap(); }
                }
                self.stx(rt, rdx, false);
                return Some(true);
            }
            // LDCLR / LDEOR / LDSET: compare-exchange loop
            self.mov_rr(rcx, rdx);
            if opc == 1 {
                a!(self, not(rcx));
            }
            match size {
                0 => a!(self, movzx(eax, byte_ptr(rsi))),
                1 => a!(self, movzx(eax, word_ptr(rsi))),
                2 => a!(self, mov(eax, dword_ptr(rsi))),
                _ => a!(self, mov(rax, qword_ptr(rsi))),
            }
            let mut retry = self.u.a.create_label();
            self.u.a.set_label(&mut retry).unwrap();
            self.mov_rr(rdx, rax);
            match opc {
                1 => a!(self, and(rdx, rcx)),
                2 => a!(self, xor(rdx, rcx)),
                _ => a!(self, or(rdx, rcx)),
            }
            match size {
                0 => self.u.a.lock().cmpxchg(byte_ptr(rsi), dl).unwrap(),
                1 => self.u.a.lock().cmpxchg(word_ptr(rsi), dx).unwrap(),
                2 => self.u.a.lock().cmpxchg(dword_ptr(rsi), edx).unwrap(),
                _ => self.u.a.lock().cmpxchg(qword_ptr(rsi), rdx).unwrap(),
            }
            a!(self, jne(retry));
            match size {
                0 => a!(self, movzx(eax, al)),
                1 => a!(self, movzx(eax, ax)),
                2 => a!(self, mov(eax, eax)),
                _ => {}
            }
            self.stx(rt, rax, false);
            return Some(true);
        }
        None
    }

    fn mov_rr(&mut self, d: R64, s: R64) {
        a!(self, mov(d, s));
    }

    fn ldst(&mut self, insn: u32) -> bool {
        let rt = bits(insn, 4, 0);
        let rn = bits(insn, 9, 5);
        let v = bit(insn, 26) != 0;
        if !v {
            if let Some(r) = self.try_atomics(insn) {
                return r;
            }
        }
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


// ---------------- Advanced SIMD and scalar floating point ----------------
//
// Only the forms that dominate real workloads (string/blit loops, colour and geometry maths)
// are translated; everything else falls back to the interpreter. Guest vector registers live in
// memory at `voff(n)`; xmm0-xmm3 are scratch.

impl<'a> E<'a> {
    fn vld(&mut self, x: AsmRegisterXmm, n: u32, q: bool) {
        if q {
            a!(self, movdqu(x, xmmword_ptr(r15 + voff(n))));
        } else {
            a!(self, movq(x, qword_ptr(r15 + voff(n))));
        }
    }

    /// Store xmm0 to Vd (clearing the upper half for 64-bit operations).
    fn vfin(&mut self, d: u32, q: bool) {
        if !q {
            a!(self, movq(xmm0, xmm0));
        }
        a!(self, movdqu(xmmword_ptr(r15 + voff(d)), xmm0));
    }

    /// Store the 64-bit value in rax to Vd (upper half zero unless `dup_hi`).
    fn vst_rax(&mut self, d: u32, dup_hi: bool) {
        a!(self, mov(qword_ptr(r15 + voff(d)), rax));
        if dup_hi {
            a!(self, mov(qword_ptr(r15 + voff(d) + 8), rax));
        } else {
            a!(self, mov(qword_ptr(r15 + voff(d) + 8), 0));
        }
    }

    fn elem_size(imm5: u32) -> Option<(u32, u32)> {
        // (log2 size, index)
        if imm5 & 1 != 0 {
            Some((0, imm5 >> 1))
        } else if imm5 & 2 != 0 {
            Some((1, imm5 >> 2))
        } else if imm5 & 4 != 0 {
            Some((2, imm5 >> 3))
        } else if imm5 & 8 != 0 {
            Some((3, imm5 >> 4))
        } else {
            None
        }
    }

    /// Load element `idx` of Vn zero-extended into rax.
    fn velem_ld(&mut self, n: u32, size: u32, idx: u32) {
        let o = voff(n) + (idx << size) as i32;
        match size {
            0 => a!(self, movzx(eax, byte_ptr(r15 + o))),
            1 => a!(self, movzx(eax, word_ptr(r15 + o))),
            2 => a!(self, mov(eax, dword_ptr(r15 + o))),
            _ => a!(self, mov(rax, qword_ptr(r15 + o))),
        }
    }

    /// Replicate the low `size` element of rax across 64 bits.
    fn replicate_rax(&mut self, size: u32) {
        match size {
            0 => {
                a!(self, movzx(eax, al));
                self.mov_imm(rcx, 0x0101_0101_0101_0101);
                a!(self, imul_2(rax, rcx));
            }
            1 => {
                a!(self, movzx(eax, ax));
                self.mov_imm(rcx, 0x0001_0001_0001_0001);
                a!(self, imul_2(rax, rcx));
            }
            2 => {
                a!(self, mov(eax, eax));
                self.mov_imm(rcx, 0x0000_0001_0000_0001);
                a!(self, imul_2(rax, rcx));
            }
            _ => {}
        }
    }

    /// Replace the x86 "real indefinite" NaN produced by invalid operations with the ARM default NaN.
    fn fix_default_nan(&mut self, double: bool) {
        let mut skip = self.u.a.create_label();
        if double {
            a!(self, movq(rax, xmm0));
            self.mov_imm(rcx, 0xFFF8_0000_0000_0000);
            a!(self, cmp(rax, rcx));
            a!(self, jne(skip));
            self.mov_imm(rax, 0x7FF8_0000_0000_0000);
            a!(self, movq(xmm0, rax));
        } else {
            a!(self, movd(eax, xmm0));
            a!(self, cmp(eax, 0xFFC0_0000u32 as i32));
            a!(self, jne(skip));
            a!(self, mov(eax, 0x7FC0_0000));
            a!(self, movd(xmm0, eax));
        }
        self.u.a.set_label(&mut skip).unwrap();
        a!(self, nop());
    }

    fn simd_fp(&mut self, insn: u32) -> bool {
        if self.simd_more(insn) {
            return true;
        }
        let rd = bits(insn, 4, 0);
        let rn = bits(insn, 9, 5);
        let rm = bits(insn, 20, 16);
        let q = bit(insn, 30) != 0;

        // ---- Advanced SIMD three same: logic, add/sub, compares ----
        if insn & 0x9F20_0400 == 0x0E20_0400 {
            let u = bit(insn, 29) != 0;
            let size = bits(insn, 23, 22);
            let opcode = bits(insn, 15, 11);
            match opcode {
                0b00011 => {
                    self.vld(xmm0, rn, q);
                    self.vld(xmm1, rm, q);
                    match (u, size) {
                        (false, 0) => a!(self, pand(xmm0, xmm1)),
                        (false, 1) => {
                            a!(self, pandn(xmm1, xmm0));
                            a!(self, movdqa(xmm0, xmm1));
                        }
                        (false, 2) => a!(self, por(xmm0, xmm1)),
                        (false, _) => {
                            a!(self, pcmpeqd(xmm2, xmm2));
                            a!(self, pxor(xmm1, xmm2));
                            a!(self, por(xmm0, xmm1));
                        }
                        (true, 0) => a!(self, pxor(xmm0, xmm1)),
                        (true, 1) => {
                            // BSL: (n & d) | (m & ~d)
                            self.vld(xmm2, rd, q);
                            a!(self, pand(xmm0, xmm2));
                            a!(self, pandn(xmm2, xmm1));
                            a!(self, por(xmm0, xmm2));
                        }
                        (true, 2) => {
                            // BIT: (d & ~m) | (n & m)
                            self.vld(xmm2, rd, q);
                            a!(self, pand(xmm0, xmm1));
                            a!(self, pandn(xmm1, xmm2));
                            a!(self, por(xmm0, xmm1));
                        }
                        (true, _) => {
                            // BIF: (d & m) | (n & ~m)
                            self.vld(xmm2, rd, q);
                            a!(self, pand(xmm2, xmm1));
                            a!(self, pandn(xmm1, xmm0));
                            a!(self, por(xmm1, xmm2));
                            a!(self, movdqa(xmm0, xmm1));
                        }
                    }
                    self.vfin(rd, q);
                    return true;
                }
                0b10000 => {
                    self.vld(xmm0, rn, q);
                    self.vld(xmm1, rm, q);
                    match (u, size) {
                        (false, 0) => a!(self, paddb(xmm0, xmm1)),
                        (false, 1) => a!(self, paddw(xmm0, xmm1)),
                        (false, 2) => a!(self, paddd(xmm0, xmm1)),
                        (false, _) => a!(self, paddq(xmm0, xmm1)),
                        (true, 0) => a!(self, psubb(xmm0, xmm1)),
                        (true, 1) => a!(self, psubw(xmm0, xmm1)),
                        (true, 2) => a!(self, psubd(xmm0, xmm1)),
                        (true, _) => a!(self, psubq(xmm0, xmm1)),
                    }
                    self.vfin(rd, q);
                    return true;
                }
                0b10001 if u && size < 3 => {
                    self.vld(xmm0, rn, q);
                    self.vld(xmm1, rm, q);
                    match size {
                        0 => a!(self, pcmpeqb(xmm0, xmm1)),
                        1 => a!(self, pcmpeqw(xmm0, xmm1)),
                        _ => a!(self, pcmpeqd(xmm0, xmm1)),
                    }
                    self.vfin(rd, q);
                    return true;
                }
                0b00110 | 0b00111 if size < 3 => {
                    // CMGT/CMGE (signed), CMHI/CMHS (unsigned); the "or equal" forms are ~(m > n).
                    let or_equal = opcode == 0b00111;
                    self.vld(xmm0, rn, q);
                    self.vld(xmm1, rm, q);
                    if or_equal {
                        a!(self, movdqa(xmm2, xmm0));
                        a!(self, movdqa(xmm0, xmm1));
                        a!(self, movdqa(xmm1, xmm2));
                    }
                    // Now compute xmm0 > xmm1.
                    if !u {
                        match size {
                            0 => a!(self, pcmpgtb(xmm0, xmm1)),
                            1 => a!(self, pcmpgtw(xmm0, xmm1)),
                            _ => a!(self, pcmpgtd(xmm0, xmm1)),
                        }
                    } else if size == 0 {
                        a!(self, movdqa(xmm2, xmm0));
                        a!(self, pmaxub(xmm2, xmm1));
                        a!(self, pcmpeqb(xmm2, xmm1)); // xmm0 <= xmm1
                        a!(self, pcmpeqd(xmm3, xmm3));
                        a!(self, pxor(xmm2, xmm3));
                        a!(self, movdqa(xmm0, xmm2));
                    } else {
                        a!(self, pcmpeqd(xmm2, xmm2));
                        if size == 1 {
                            a!(self, psllw(xmm2, 15));
                        } else {
                            a!(self, pslld(xmm2, 31));
                        }
                        a!(self, pxor(xmm0, xmm2));
                        a!(self, pxor(xmm1, xmm2));
                        if size == 1 {
                            a!(self, pcmpgtw(xmm0, xmm1));
                        } else {
                            a!(self, pcmpgtd(xmm0, xmm1));
                        }
                    }
                    if or_equal {
                        a!(self, pcmpeqd(xmm2, xmm2));
                        a!(self, pxor(xmm0, xmm2));
                    }
                    self.vfin(rd, q);
                    return true;
                }
                _ => return false,
            }
        }

        // ---- Advanced SIMD permute: ZIP/UZP (128-bit) ----
        if insn & 0xBF20_8C00 == 0x0E00_0800 && q {
            let size = bits(insn, 23, 22);
            let opc = bits(insn, 14, 12);
            let ok = matches!(opc, 0b001 | 0b101 | 0b011 | 0b111);
            if !ok {
                return false;
            }
            self.vld(xmm0, rn, true);
            self.vld(xmm1, rm, true);
            match opc {
                0b011 => match size {
                    0 => a!(self, punpcklbw(xmm0, xmm1)),
                    1 => a!(self, punpcklwd(xmm0, xmm1)),
                    2 => a!(self, punpckldq(xmm0, xmm1)),
                    _ => a!(self, punpcklqdq(xmm0, xmm1)),
                },
                0b111 => match size {
                    0 => a!(self, punpckhbw(xmm0, xmm1)),
                    1 => a!(self, punpckhwd(xmm0, xmm1)),
                    2 => a!(self, punpckhdq(xmm0, xmm1)),
                    _ => a!(self, punpckhqdq(xmm0, xmm1)),
                },
                0b001 => match size {
                    0 => {
                        a!(self, pcmpeqd(xmm2, xmm2));
                        a!(self, psrlw(xmm2, 8));
                        a!(self, pand(xmm0, xmm2));
                        a!(self, pand(xmm1, xmm2));
                        a!(self, packuswb(xmm0, xmm1));
                    }
                    1 => {
                        a!(self, pslld(xmm0, 16));
                        a!(self, psrad(xmm0, 16));
                        a!(self, pslld(xmm1, 16));
                        a!(self, psrad(xmm1, 16));
                        a!(self, packssdw(xmm0, xmm1));
                    }
                    2 => a!(self, shufps(xmm0, xmm1, 0x88)),
                    _ => a!(self, punpcklqdq(xmm0, xmm1)),
                },
                _ => match size {
                    0 => {
                        a!(self, psrlw(xmm0, 8));
                        a!(self, psrlw(xmm1, 8));
                        a!(self, packuswb(xmm0, xmm1));
                    }
                    1 => {
                        a!(self, psrad(xmm0, 16));
                        a!(self, psrad(xmm1, 16));
                        a!(self, packssdw(xmm0, xmm1));
                    }
                    2 => a!(self, shufps(xmm0, xmm1, 0xDD)),
                    _ => a!(self, punpckhqdq(xmm0, xmm1)),
                },
            }
            self.vfin(rd, true);
            return true;
        }

        // ---- MOVI / MVNI (modified immediate, plain forms) ----
        if insn & 0x9FF8_0C00 == 0x0F00_0400 {
            let op = bit(insn, 29);
            let cmode = bits(insn, 15, 12);
            let plain = cmode & 0b1001 == 0 || cmode & 0b1101 == 0b1000 || cmode & 0b1110 == 0b1100 || cmode == 0b1110;
            if !plain {
                return false;
            }
            let imm8 = (bits(insn, 18, 16) << 5) | bits(insn, 9, 5);
            let mut imm = maclator_core::interp::simd::adv_simd_expand_imm(op, cmode, imm8);
            if op == 1 && cmode < 0b1110 {
                imm = !imm;
            }
            self.mov_imm(rax, imm);
            self.vst_rax(rd, q);
            return true;
        }

        // ---- DUP (element / general), UMOV, SMOV, INS ----
        if insn & 0xBFE0_FC00 == 0x0E00_0400 {
            // DUP (element)
            let Some((size, idx)) = Self::elem_size(bits(insn, 20, 16)) else { return false };
            if size == 3 && !q {
                return false;
            }
            self.velem_ld(rn, size, idx);
            self.replicate_rax(size);
            self.vst_rax(rd, q);
            return true;
        }
        if insn & 0xBFE0_FC00 == 0x0E00_0C00 {
            // DUP (general)
            let Some((size, _)) = Self::elem_size(bits(insn, 20, 16)) else { return false };
            if size == 3 && !q {
                return false;
            }
            self.ldx(rax, rn, false);
            self.replicate_rax(size);
            self.vst_rax(rd, q);
            return true;
        }
        if insn & 0xFFE0_FC00 == 0x5E00_0400 {
            // DUP (scalar, element)
            let Some((size, idx)) = Self::elem_size(bits(insn, 20, 16)) else { return false };
            self.velem_ld(rn, size, idx);
            self.vst_rax(rd, false);
            return true;
        }
        if insn & 0xBFE0_FC00 == 0x0E00_3C00 {
            // UMOV
            let Some((size, idx)) = Self::elem_size(bits(insn, 20, 16)) else { return false };
            if (size == 3) != q {
                return false;
            }
            self.velem_ld(rn, size, idx);
            self.stx(rd, rax, false);
            return true;
        }
        if insn & 0xBFE0_FC00 == 0x0E00_2C00 {
            // SMOV
            let Some((size, idx)) = Self::elem_size(bits(insn, 20, 16)) else { return false };
            if size == 3 || (size == 2 && !q) {
                return false;
            }
            let o = voff(rn) + (idx << size) as i32;
            match (size, q) {
                (0, false) => { a!(self, movsx(eax, byte_ptr(r15 + o))); }
                (0, true) => { a!(self, movsx(rax, byte_ptr(r15 + o))); }
                (1, false) => { a!(self, movsx(eax, word_ptr(r15 + o))); }
                (1, true) => { a!(self, movsx(rax, word_ptr(r15 + o))); }
                _ => { a!(self, movsxd(rax, dword_ptr(r15 + o))); }
            }
            self.stx(rd, rax, false);
            return true;
        }
        if insn & 0xFFE0_FC00 == 0x4E00_1C00 {
            // INS (general)
            let Some((size, idx)) = Self::elem_size(bits(insn, 20, 16)) else { return false };
            self.ldx(rax, rn, false);
            let o = voff(rd) + (idx << size) as i32;
            match size {
                0 => a!(self, mov(byte_ptr(r15 + o), al)),
                1 => a!(self, mov(word_ptr(r15 + o), ax)),
                2 => a!(self, mov(dword_ptr(r15 + o), eax)),
                _ => a!(self, mov(qword_ptr(r15 + o), rax)),
            }
            return true;
        }
        if insn & 0xFFE0_8400 == 0x6E00_0400 {
            // INS (element)
            let Some((size, didx)) = Self::elem_size(bits(insn, 20, 16)) else { return false };
            let sidx = bits(insn, 14, 11) >> size;
            self.velem_ld(rn, size, sidx);
            let o = voff(rd) + (didx << size) as i32;
            match size {
                0 => a!(self, mov(byte_ptr(r15 + o), al)),
                1 => a!(self, mov(word_ptr(r15 + o), ax)),
                2 => a!(self, mov(dword_ptr(r15 + o), eax)),
                _ => a!(self, mov(qword_ptr(r15 + o), rax)),
            }
            return true;
        }

        // ---- Across lanes: UMINV / UMAXV on bytes ----
        if insn & 0xBF3E_0C00 == 0x0E30_0800 && bits(insn, 23, 22) == 0 && bit(insn, 29) == 1 {
            let opc = bits(insn, 16, 12);
            if opc != 0b11010 && opc != 0b01010 {
                return false;
            }
            self.vld(xmm0, rn, q);
            let shifts: &[i32] = if q { &[8, 4, 2, 1] } else { &[4, 2, 1] };
            for &sh in shifts {
                a!(self, movdqa(xmm1, xmm0));
                a!(self, psrldq(xmm1, sh));
                if opc == 0b11010 {
                    a!(self, pminub(xmm0, xmm1));
                } else {
                    a!(self, pmaxub(xmm0, xmm1));
                }
            }
            a!(self, movd(eax, xmm0));
            a!(self, movzx(eax, al));
            self.vst_rax(rd, false);
            return true;
        }

        // ---- LD1 / ST1 (multiple structures, 1-4 registers, optional post-index) ----
        if insn & 0xBFFF_0000 == 0x0C00_0000 || insn & 0xBFA0_0000 == 0x0C80_0000 {
            let post = bit(insn, 23) != 0;
            let load = bit(insn, 22) != 0;
            let nregs = match bits(insn, 15, 12) {
                0b0111 => 1,
                0b1010 => 2,
                0b0110 => 3,
                0b0010 => 4,
                _ => return false,
            };
            if !post && bits(insn, 20, 16) != 0 {
                return false;
            }
            let chunk = if q { 16 } else { 8 };
            self.ldx(rax, rn, true);
            self.commpage_fix();
            for i in 0..nregs {
                let reg = (rd + i) & 31;
                let disp = (i * chunk) as i32;
                if load {
                    if q {
                        a!(self, movdqu(xmm0, xmmword_ptr(rax + disp)));
                    } else {
                        a!(self, movq(xmm0, qword_ptr(rax + disp)));
                    }
                    a!(self, movdqu(xmmword_ptr(r15 + voff(reg)), xmm0));
                } else {
                    a!(self, movdqu(xmm0, xmmword_ptr(r15 + voff(reg))));
                    if q {
                        a!(self, movdqu(xmmword_ptr(rax + disp), xmm0));
                    } else {
                        a!(self, movq(qword_ptr(rax + disp), xmm0));
                    }
                }
            }
            if post {
                self.ldx(rcx, rn, true);
                if rm == 31 {
                    a!(self, add(rcx, (nregs * chunk) as i32));
                } else {
                    self.ldx(rdx, rm, false);
                    a!(self, add(rcx, rdx));
                }
                self.stx(rn, rcx, true);
            }
            return true;
        }

        // ---- scalar floating point ----
        let ftype = bits(insn, 23, 22);
        // FMOV to/from general registers
        if insn & 0x7F20_FC00 == 0x1E26_0000 || insn & 0x7F20_FC00 == 0x1E27_0000 {
            let sf = bit(insn, 31) != 0;
            let to_fp = bit(insn, 16) != 0;
            let ok = (sf && ftype == 1) || (!sf && ftype == 0);
            if !ok || bits(insn, 20, 19) != 0 {
                return false;
            }
            if to_fp {
                self.ld(rax, rn, false, sf);
                self.vst_rax(rd, false);
            } else {
                if sf {
                    a!(self, mov(rax, qword_ptr(r15 + voff(rn))));
                } else {
                    a!(self, mov(eax, dword_ptr(r15 + voff(rn))));
                }
                self.stx(rd, rax, false);
            }
            return true;
        }
        // FMOV (register), FABS, FNEG
        if insn & 0xFF3E_7C00 == 0x1E20_4000 && ftype <= 1 {
            let opc = bits(insn, 16, 15);
            if opc > 2 {
                return false;
            }
            let double = ftype == 1;
            if double {
                a!(self, mov(rax, qword_ptr(r15 + voff(rn))));
                match opc {
                    1 => a!(self, btr(rax, 63)),
                    2 => a!(self, btc(rax, 63)),
                    _ => {}
                }
            } else {
                a!(self, mov(eax, dword_ptr(r15 + voff(rn))));
                match opc {
                    1 => a!(self, btr(eax, 31)),
                    2 => a!(self, btc(eax, 31)),
                    _ => {}
                }
            }
            self.vst_rax(rd, false);
            return true;
        }
        // FMOV (immediate)
        if insn & 0xFF20_1FE0 == 0x1E20_1000 && ftype <= 1 {
            let imm8 = bits(insn, 20, 13);
            let d = vfp_expand_imm(imm8);
            let bitsv = if ftype == 1 { d } else { (f64::from_bits(d) as f32).to_bits() as u64 };
            self.mov_imm(rax, bitsv);
            self.vst_rax(rd, false);
            return true;
        }
        // FADD/FSUB/FMUL/FDIV
        if insn & 0xFF20_0C00 == 0x1E20_0800 && ftype <= 1 {
            let opcode = bits(insn, 15, 12);
            if opcode > 3 {
                return false;
            }
            let double = ftype == 1;
            if double {
                a!(self, movsd_2(xmm0, qword_ptr(r15 + voff(rn))));
                match opcode {
                    0 => a!(self, mulsd(xmm0, qword_ptr(r15 + voff(rm)))),
                    1 => a!(self, divsd(xmm0, qword_ptr(r15 + voff(rm)))),
                    2 => a!(self, addsd(xmm0, qword_ptr(r15 + voff(rm)))),
                    _ => a!(self, subsd(xmm0, qword_ptr(r15 + voff(rm)))),
                }
            } else {
                a!(self, movss(xmm0, dword_ptr(r15 + voff(rn))));
                match opcode {
                    0 => a!(self, mulss(xmm0, dword_ptr(r15 + voff(rm)))),
                    1 => a!(self, divss(xmm0, dword_ptr(r15 + voff(rm)))),
                    2 => a!(self, addss(xmm0, dword_ptr(r15 + voff(rm)))),
                    _ => a!(self, subss(xmm0, dword_ptr(r15 + voff(rm)))),
                }
            }
            self.fix_default_nan(double);
            a!(self, movdqu(xmmword_ptr(r15 + voff(rd)), xmm0));
            return true;
        }
        // FSQRT
        if insn & 0xFF3F_FC00 == 0x1E21_C000 && ftype <= 1 {
            let double = ftype == 1;
            if double {
                a!(self, movsd_2(xmm0, qword_ptr(r15 + voff(rn))));
                a!(self, sqrtsd(xmm0, xmm0));
            } else {
                a!(self, movss(xmm0, dword_ptr(r15 + voff(rn))));
                a!(self, sqrtss(xmm0, xmm0));
            }
            self.fix_default_nan(double);
            a!(self, movdqu(xmmword_ptr(r15 + voff(rd)), xmm0));
            return true;
        }
        // SCVTF / UCVTF (scalar, integer source)
        if insn & 0x7F3F_FC00 == 0x1E22_0000 || insn & 0x7F3F_FC00 == 0x1E23_0000 {
            if ftype > 1 {
                return false;
            }
            let sf = bit(insn, 31) != 0;
            let unsigned = bit(insn, 16) != 0;
            let double = ftype == 1;
            self.ld(rax, rn, false, sf);
            if unsigned && sf {
                let mut neg = self.u.a.create_label();
                let mut done = self.u.a.create_label();
                a!(self, test(rax, rax));
                a!(self, js(neg));
                if double { a!(self, cvtsi2sd(xmm0, rax)); } else { a!(self, cvtsi2ss(xmm0, rax)); }
                a!(self, jmp(done));
                self.u.a.set_label(&mut neg).unwrap();
                a!(self, mov(rcx, rax));
                a!(self, shr(rax, 1));
                a!(self, and(ecx, 1));
                a!(self, or(rax, rcx));
                if double {
                    a!(self, cvtsi2sd(xmm0, rax));
                    a!(self, addsd(xmm0, xmm0));
                } else {
                    a!(self, cvtsi2ss(xmm0, rax));
                    a!(self, addss(xmm0, xmm0));
                }
                self.u.a.set_label(&mut done).unwrap();
                a!(self, nop());
            } else {
                // 32-bit unsigned sources were zero-extended, so a 64-bit signed convert is exact.
                let wide = sf || unsigned;
                if double {
                    if wide { a!(self, cvtsi2sd(xmm0, rax)); } else { a!(self, cvtsi2sd(xmm0, eax)); }
                } else if wide {
                    a!(self, cvtsi2ss(xmm0, rax));
                } else {
                    a!(self, cvtsi2ss(xmm0, eax));
                }
            }
            // The convert leaves the upper lanes of xmm0 undefined; keep only the scalar.
            if double {
                a!(self, movq(xmm0, xmm0));
            } else {
                a!(self, movd(eax, xmm0));
                a!(self, movd(xmm0, eax));
            }
            a!(self, movdqu(xmmword_ptr(r15 + voff(rd)), xmm0));
            return true;
        }
        // FCMP / FCMPE (register and zero forms)
        if insn & 0xFF20_FC07 == 0x1E20_2000 && ftype <= 1 {
            let with_zero = bit(insn, 3) != 0;
            let double = ftype == 1;
            if double {
                a!(self, movsd_2(xmm0, qword_ptr(r15 + voff(rn))));
                if with_zero {
                    a!(self, xorpd(xmm1, xmm1));
                } else {
                    a!(self, movsd_2(xmm1, qword_ptr(r15 + voff(rm))));
                }
                a!(self, ucomisd(xmm0, xmm1));
            } else {
                a!(self, movss(xmm0, dword_ptr(r15 + voff(rn))));
                if with_zero {
                    a!(self, xorps(xmm1, xmm1));
                } else {
                    a!(self, movss(xmm1, dword_ptr(r15 + voff(rm))));
                }
                a!(self, ucomiss(xmm0, xmm1));
            }
            let mut un = self.u.a.create_label();
            let mut lt = self.u.a.create_label();
            let mut eq = self.u.a.create_label();
            let mut end = self.u.a.create_label();
            a!(self, jp(un));
            a!(self, jb(lt));
            a!(self, je(eq));
            // greater: N=0 Z=0 C=1 V=0
            self.set_nzcv_const(0, 0, 1, 0);
            a!(self, jmp(end));
            self.u.a.set_label(&mut lt).unwrap();
            self.set_nzcv_const(1, 0, 0, 0);
            a!(self, jmp(end));
            self.u.a.set_label(&mut eq).unwrap();
            self.set_nzcv_const(0, 1, 1, 0);
            a!(self, jmp(end));
            self.u.a.set_label(&mut un).unwrap();
            self.set_nzcv_const(0, 0, 1, 1);
            self.u.a.set_label(&mut end).unwrap();
            a!(self, nop());
            return true;
        }
        // FCSEL
        if insn & 0xFF20_0C00 == 0x1E20_0C00 && ftype <= 1 {
            let cond = bits(insn, 15, 12);
            let double = ftype == 1;
            self.cond_to_al(cond);
            let mut use_m = self.u.a.create_label();
            let mut end = self.u.a.create_label();
            a!(self, test(al, al));
            a!(self, jz(use_m));
            if double { a!(self, mov(rax, qword_ptr(r15 + voff(rn)))); } else { a!(self, mov(eax, dword_ptr(r15 + voff(rn)))); }
            a!(self, jmp(end));
            self.u.a.set_label(&mut use_m).unwrap();
            if double { a!(self, mov(rax, qword_ptr(r15 + voff(rm)))); } else { a!(self, mov(eax, dword_ptr(r15 + voff(rm)))); }
            self.u.a.set_label(&mut end).unwrap();
            a!(self, nop());
            self.vst_rax(rd, false);
            return true;
        }
        false
    }

    fn set_nzcv_const(&mut self, n: u32, z: u32, c: u32, v: u32) {
        a!(self, mov(byte_ptr(r15 + off::NF), n));
        a!(self, mov(byte_ptr(r15 + off::ZF), z));
        a!(self, mov(byte_ptr(r15 + off::CF), c));
        a!(self, mov(byte_ptr(r15 + off::VF), v));
    }
}


// ---- second batch: Skia/pixel-pipeline NEON ----

impl<'a> E<'a> {
    /// Shift each element of xmm0 right (logical) by `n` for element size `size` (0=b..3=d).
    fn psrl_elem(&mut self, x: AsmRegisterXmm, size: u32, n: u32) {
        match size {
            0 => {
                a!(self, psrlw(x, n as i32));
                self.mov_imm(rax, 0x0101_0101_0101_0101u64 * (0xffu64 >> n));
                a!(self, movq(xmm3, rax));
                a!(self, punpcklqdq(xmm3, xmm3));
                a!(self, pand(x, xmm3));
            }
            1 => a!(self, psrlw(x, n as i32)),
            2 => a!(self, psrld(x, n as i32)),
            _ => a!(self, psrlq(x, n as i32)),
        }
    }

    fn psll_elem(&mut self, x: AsmRegisterXmm, size: u32, n: u32) {
        match size {
            0 => {
                a!(self, psllw(x, n as i32));
                self.mov_imm(rax, 0x0101_0101_0101_0101u64 * ((0xffu64 << n) & 0xff));
                a!(self, movq(xmm3, rax));
                a!(self, punpcklqdq(xmm3, xmm3));
                a!(self, pand(x, xmm3));
            }
            1 => a!(self, psllw(x, n as i32)),
            2 => a!(self, pslld(x, n as i32)),
            _ => a!(self, psllq(x, n as i32)),
        }
    }

    fn padd_elem(&mut self, x: AsmRegisterXmm, y: AsmRegisterXmm, size: u32) {
        match size {
            0 => a!(self, paddb(x, y)),
            1 => a!(self, paddw(x, y)),
            2 => a!(self, paddd(x, y)),
            _ => a!(self, paddq(x, y)),
        }
    }

    /// Load a 64-bit constant into all lanes of `x`.
    fn vconst(&mut self, x: AsmRegisterXmm, v: u64) {
        self.mov_imm(rax, v);
        a!(self, movq(x, rax));
        a!(self, punpcklqdq(x, x));
    }

    fn simd_more(&mut self, insn: u32) -> bool {
        let rd = bits(insn, 4, 0);
        let rn = bits(insn, 9, 5);
        let rm = bits(insn, 20, 16);
        let q = bit(insn, 30) != 0;
        let u = bit(insn, 29) != 0;
        let size = bits(insn, 23, 22);

        // ---- three-same integer: saturating add/sub, min/max, mul/mla/mls ----
        if insn & 0x9F20_0400 == 0x0E20_0400 {
            let opcode = bits(insn, 15, 11);
            match opcode {
                0b00001 | 0b00101 if size < 2 => {
                    self.vld(xmm0, rn, q);
                    self.vld(xmm1, rm, q);
                    match (opcode, u, size) {
                        (0b00001, true, 0) => a!(self, paddusb(xmm0, xmm1)),
                        (0b00001, true, _) => a!(self, paddusw(xmm0, xmm1)),
                        (0b00001, false, 0) => a!(self, paddsb(xmm0, xmm1)),
                        (0b00001, false, _) => a!(self, paddsw(xmm0, xmm1)),
                        (_, true, 0) => a!(self, psubusb(xmm0, xmm1)),
                        (_, true, _) => a!(self, psubusw(xmm0, xmm1)),
                        (_, false, 0) => a!(self, psubsb(xmm0, xmm1)),
                        (_, false, _) => a!(self, psubsw(xmm0, xmm1)),
                    }
                    self.vfin(rd, q);
                    return true;
                }
                0b01100 | 0b01101 if size < 3 => {
                    let is_min = opcode == 0b01101;
                    self.vld(xmm0, rn, q);
                    self.vld(xmm1, rm, q);
                    match (is_min, u, size) {
                        (true, true, 0) => a!(self, pminub(xmm0, xmm1)),
                        (true, true, 1) => a!(self, pminuw(xmm0, xmm1)),
                        (true, true, _) => a!(self, pminud(xmm0, xmm1)),
                        (true, false, 0) => a!(self, pminsb(xmm0, xmm1)),
                        (true, false, 1) => a!(self, pminsw(xmm0, xmm1)),
                        (true, false, _) => a!(self, pminsd(xmm0, xmm1)),
                        (false, true, 0) => a!(self, pmaxub(xmm0, xmm1)),
                        (false, true, 1) => a!(self, pmaxuw(xmm0, xmm1)),
                        (false, true, _) => a!(self, pmaxud(xmm0, xmm1)),
                        (false, false, 0) => a!(self, pmaxsb(xmm0, xmm1)),
                        (false, false, 1) => a!(self, pmaxsw(xmm0, xmm1)),
                        (false, false, _) => a!(self, pmaxsd(xmm0, xmm1)),
                    }
                    self.vfin(rd, q);
                    return true;
                }
                0b10010 | 0b10011 if size == 1 || size == 2 => {
                    // MLA/MLS (opcode 10010), MUL (10011, U=0)
                    if opcode == 0b10011 && u {
                        return false;
                    }
                    self.vld(xmm0, rn, q);
                    self.vld(xmm1, rm, q);
                    if size == 1 {
                        a!(self, pmullw(xmm0, xmm1));
                    } else {
                        a!(self, pmulld(xmm0, xmm1));
                    }
                    if opcode == 0b10010 {
                        self.vld(xmm2, rd, q);
                        if u {
                            // MLS: d - n*m
                            if size == 1 { a!(self, psubw(xmm2, xmm0)); } else { a!(self, psubd(xmm2, xmm0)); }
                        } else if size == 1 {
                            a!(self, paddw(xmm2, xmm0));
                        } else {
                            a!(self, paddd(xmm2, xmm0));
                        }
                        a!(self, movdqa(xmm0, xmm2));
                    }
                    self.vfin(rd, q);
                    return true;
                }
                _ => {}
            }
        }

        // ---- vector floating-point three-same ----
        if insn & 0x9F20_0400 == 0x0E20_0400 {
            let a_bit = bit(insn, 23);
            let sz = bit(insn, 22);
            let opcode = bits(insn, 15, 11);
            let double = sz == 1;
            if double && !q {
                return false;
            }
            let op = match (u, a_bit, opcode) {
                (false, 0, 0b11010) => 1, // FADD
                (false, 1, 0b11010) => 2, // FSUB
                (true, 0, 0b11011) => 3,  // FMUL
                (true, 0, 0b11111) => 4,  // FDIV
                (false, 0, 0b11100) => 5, // FCMEQ
                (true, 0, 0b11100) => 6,  // FCMGE
                (true, 1, 0b11100) => 7,  // FCMGT
                _ => 0,
            };
            if op == 0 {
                return false;
            }
            self.vld(xmm0, rn, q);
            self.vld(xmm1, rm, q);
            match (op, double) {
                (1, false) => a!(self, addps(xmm0, xmm1)),
                (1, true) => a!(self, addpd(xmm0, xmm1)),
                (2, false) => a!(self, subps(xmm0, xmm1)),
                (2, true) => a!(self, subpd(xmm0, xmm1)),
                (3, false) => a!(self, mulps(xmm0, xmm1)),
                (3, true) => a!(self, mulpd(xmm0, xmm1)),
                (4, false) => a!(self, divps(xmm0, xmm1)),
                (4, true) => a!(self, divpd(xmm0, xmm1)),
                (5, false) => a!(self, cmpps(xmm0, xmm1, 0)),
                (5, true) => a!(self, cmppd(xmm0, xmm1, 0)),
                // n >= m  <=>  m <= n : cmpleps(m, n)
                (6, false) => { a!(self, cmpps(xmm1, xmm0, 2)); a!(self, movdqa(xmm0, xmm1)); }
                (6, true) => { a!(self, cmppd(xmm1, xmm0, 2)); a!(self, movdqa(xmm0, xmm1)); }
                (7, false) => { a!(self, cmpps(xmm1, xmm0, 1)); a!(self, movdqa(xmm0, xmm1)); }
                (_, _) => { a!(self, cmppd(xmm1, xmm0, 1)); a!(self, movdqa(xmm0, xmm1)); }
            }
            if op <= 4 {
                // x86's "real indefinite" NaN has the sign bit set; ARM's default NaN does not.
                if double {
                    self.vconst(xmm2, 0xFFF8_0000_0000_0000);
                    a!(self, movdqa(xmm3, xmm0));
                    a!(self, pcmpeqq(xmm3, xmm2));
                    self.vconst(xmm4, 0x7FF8_0000_0000_0000);
                } else {
                    self.vconst(xmm2, 0xFFC0_0000_FFC0_0000);
                    a!(self, movdqa(xmm3, xmm0));
                    a!(self, pcmpeqd(xmm3, xmm2));
                    self.vconst(xmm4, 0x7FC0_0000_7FC0_0000);
                }
                a!(self, movdqa(xmm5, xmm3));
                a!(self, pandn(xmm5, xmm0));
                a!(self, pand(xmm3, xmm4));
                a!(self, por(xmm5, xmm3));
                a!(self, movdqa(xmm0, xmm5));
            }
            self.vfin(rd, q);
            return true;
        }

        // ---- MVN / NOT (two-reg misc) ----
        if insn & 0xBFFF_FC00 == 0x2E20_5800 {
            self.vld(xmm0, rn, q);
            a!(self, pcmpeqd(xmm1, xmm1));
            a!(self, pxor(xmm0, xmm1));
            self.vfin(rd, q);
            return true;
        }

        // ---- SCVTF (vector, integer) ----
        if insn & 0xBFBF_FC00 == 0x0E21_D800 {
            let double = bit(insn, 22) != 0;
            if double {
                return false;
            }
            self.vld(xmm0, rn, q);
            a!(self, cvtdq2ps(xmm0, xmm0));
            self.vfin(rd, q);
            return true;
        }

        // ---- shifts by immediate: USHR/SSHR/URSHR/SHL ----
        if insn & 0xBF80_0400 == 0x0F00_0400 {
            let immh = bits(insn, 22, 19);
            let immb = bits(insn, 18, 16);
            let opcode = bits(insn, 15, 11);
            if immh == 0 {
                return false;
            }
            let sz = if immh & 8 != 0 { 3 } else if immh & 4 != 0 { 2 } else if immh & 2 != 0 { 1 } else { 0 };
            if sz == 3 && !q {
                return false;
            }
            let esize = 8u32 << sz;
            let immv = (immh << 3) | immb;
            match opcode {
                0b00000 | 0b00100 => {
                    // USHR/SSHR (00000), URSHR/SRSHR (00100)
                    let n = 2 * esize - immv;
                    if n == 0 || n > esize {
                        return false;
                    }
                    let rounding = opcode == 0b00100;
                    if !u && (sz == 0 || sz == 3) {
                        return false; // no arithmetic byte/qword shifts in SSE2
                    }
                    if rounding && !u {
                        return false;
                    }
                    self.vld(xmm0, rn, q);
                    if !rounding {
                        if u {
                            if n == esize {
                                a!(self, pxor(xmm0, xmm0));
                            } else {
                                self.psrl_elem(xmm0, sz, n);
                            }
                        } else if sz == 1 {
                            a!(self, psraw(xmm0, n.min(15) as i32));
                        } else {
                            a!(self, psrad(xmm0, n.min(31) as i32));
                        }
                    } else {
                        // URSHR: t = x >> (n-1); r = (t >> 1) + (t & 1)
                        if n > 1 {
                            self.psrl_elem(xmm0, sz, n - 1);
                        }
                        a!(self, movdqa(xmm1, xmm0));
                        self.psrl_elem(xmm0, sz, 1);
                        self.vconst(xmm2, match sz { 0 => 0x0101_0101_0101_0101, 1 => 0x0001_0001_0001_0001, 2 => 0x0000_0001_0000_0001, _ => 1 });
                        a!(self, pand(xmm1, xmm2));
                        self.padd_elem(xmm0, xmm1, sz);
                    }
                    self.vfin(rd, q);
                    return true;
                }
                0b01010 if !u => {
                    // SHL
                    let n = immv - esize;
                    self.vld(xmm0, rn, q);
                    if n != 0 {
                        self.psll_elem(xmm0, sz, n);
                    }
                    self.vfin(rd, q);
                    return true;
                }
                _ => {}
            }
        }

        // ---- USHLL/SSHLL (and the 2 forms): widen + shift ----
        if insn & 0xBF80_FC00 == 0x0F00_A400 {
            let immh = bits(insn, 22, 19);
            let immb = bits(insn, 18, 16);
            if immh == 0 || immh & 8 != 0 {
                return false;
            }
            let sz = if immh & 4 != 0 { 2 } else if immh & 2 != 0 { 1 } else { 0 };
            let shift = ((immh << 3) | immb) - (8u32 << sz);
            if q {
                // upper half of the source
                a!(self, movdqu(xmm0, xmmword_ptr(r15 + voff(rn))));
                a!(self, psrldq(xmm0, 8));
            } else {
                a!(self, movq(xmm0, qword_ptr(r15 + voff(rn))));
            }
            match (u, sz) {
                (true, 0) => a!(self, pmovzxbw(xmm0, xmm0)),
                (true, 1) => a!(self, pmovzxwd(xmm0, xmm0)),
                (true, _) => a!(self, pmovzxdq(xmm0, xmm0)),
                (false, 0) => a!(self, pmovsxbw(xmm0, xmm0)),
                (false, 1) => a!(self, pmovsxwd(xmm0, xmm0)),
                (false, _) => a!(self, pmovsxdq(xmm0, xmm0)),
            }
            if shift != 0 {
                self.psll_elem(xmm0, sz + 1, shift);
            }
            a!(self, movdqu(xmmword_ptr(r15 + voff(rd)), xmm0));
            return true;
        }

        // ---- UMULL/SMULL (+2): bytes->halves, halves->words ----
        if insn & 0xBF20_FC00 == 0x0E20_C000 && size < 2 {
            let upper = q;
            let src = |e: &mut Self, x: AsmRegisterXmm, n: u32| {
                if upper {
                    a!(e, movdqu(x, xmmword_ptr(r15 + voff(n))));
                    a!(e, psrldq(x, 8));
                } else {
                    a!(e, movq(x, qword_ptr(r15 + voff(n))));
                }
            };
            src(self, xmm0, rn);
            src(self, xmm1, rm);
            if size == 0 {
                if u {
                    a!(self, pmovzxbw(xmm0, xmm0));
                    a!(self, pmovzxbw(xmm1, xmm1));
                } else {
                    a!(self, pmovsxbw(xmm0, xmm0));
                    a!(self, pmovsxbw(xmm1, xmm1));
                }
                a!(self, pmullw(xmm0, xmm1));
            } else {
                a!(self, movdqa(xmm2, xmm0));
                a!(self, pmullw(xmm0, xmm1));
                if u {
                    a!(self, pmulhuw(xmm2, xmm1));
                } else {
                    a!(self, pmulhw(xmm2, xmm1));
                }
                a!(self, punpcklwd(xmm0, xmm2));
            }
            a!(self, movdqu(xmmword_ptr(r15 + voff(rd)), xmm0));
            return true;
        }

        // ---- ADDHN/RADDHN (halves->bytes, words->halves, dwords->words), lower/upper "2" forms ----
        // The sum wraps at the source element width; the result is its high half (+ rounding).
        if insn & 0xBF20_FC00 == 0x0E20_4000 && size < 3 {
            let round = u;
            self.vld(xmm0, rn, true);
            self.vld(xmm1, rm, true);
            match size {
                0 => {
                    a!(self, paddw(xmm0, xmm1));
                    if round {
                        self.vconst(xmm2, 0x0080_0080_0080_0080);
                        a!(self, paddw(xmm0, xmm2));
                    }
                    a!(self, psrlw(xmm0, 8));
                    a!(self, packuswb(xmm0, xmm0));
                }
                1 => {
                    a!(self, paddd(xmm0, xmm1));
                    if round {
                        self.vconst(xmm2, 0x0000_8000_0000_8000);
                        a!(self, paddd(xmm0, xmm2));
                    }
                    a!(self, psrld(xmm0, 16));
                    a!(self, packusdw(xmm0, xmm0));
                }
                _ => {
                    a!(self, paddq(xmm0, xmm1));
                    if round {
                        self.vconst(xmm2, 0x8000_0000);
                        a!(self, paddq(xmm0, xmm2));
                    }
                    a!(self, psrlq(xmm0, 32));
                    a!(self, pshufd(xmm0, xmm0, 0x08));
                }
            }
            if q {
                a!(self, movq(xmm0, xmm0));
                a!(self, pslldq(xmm0, 8));
                a!(self, movq(xmm1, qword_ptr(r15 + voff(rd))));
                a!(self, por(xmm0, xmm1));
            } else {
                a!(self, movq(xmm0, xmm0));
            }
            a!(self, movdqu(xmmword_ptr(r15 + voff(rd)), xmm0));
            return true;
        }

        // ---- XTN / UQXTN (+2) ----
        if insn & 0xBF3F_FC00 == 0x0E21_2800 || insn & 0xBF3F_FC00 == 0x2E21_4800 {
            if size > 2 {
                return false;
            }
            let saturating = bit(insn, 29) != 0;
            self.vld(xmm0, rn, true);
            match size {
                0 => {
                    if saturating {
                        self.vconst(xmm1, 0x00ff_00ff_00ff_00ff);
                        a!(self, pminuw(xmm0, xmm1));
                    } else {
                        self.vconst(xmm1, 0x00ff_00ff_00ff_00ff);
                        a!(self, pand(xmm0, xmm1));
                    }
                    a!(self, packuswb(xmm0, xmm0));
                }
                1 => {
                    if saturating {
                        self.vconst(xmm1, 0x0000_ffff_0000_ffff);
                        a!(self, pminud(xmm0, xmm1));
                    } else {
                        self.vconst(xmm1, 0x0000_ffff_0000_ffff);
                        a!(self, pand(xmm0, xmm1));
                    }
                    a!(self, packusdw(xmm0, xmm0));
                }
                _ => {
                    if saturating {
                        return false;
                    }
                    a!(self, pshufd(xmm0, xmm0, 0x08));
                }
            }
            if q {
                a!(self, movq(xmm0, xmm0));
                a!(self, pslldq(xmm0, 8));
                a!(self, movq(xmm1, qword_ptr(r15 + voff(rd))));
                a!(self, por(xmm0, xmm1));
            } else {
                a!(self, movq(xmm0, xmm0));
            }
            a!(self, movdqu(xmmword_ptr(r15 + voff(rd)), xmm0));
            return true;
        }

        // ---- FMOV (vector immediate) ----
        if insn & 0x9FF8_0C00 == 0x0F00_0400 && bits(insn, 15, 12) == 0b1111 && bit(insn, 11) == 0 {
            let op = bit(insn, 29);
            if op == 1 && !q {
                return false;
            }
            let imm8 = (bits(insn, 18, 16) << 5) | bits(insn, 9, 5);
            let imm = maclator_core::interp::simd::adv_simd_expand_imm(op, 0b1111, imm8);
            self.mov_imm(rax, imm);
            self.vst_rax(rd, q);
            return true;
        }

        // ---- ORR / BIC (vector immediate) ----
        if insn & 0x9FF8_0400 == 0x0F00_0400 && bit(insn, 11) == 0 {
            let cmode = bits(insn, 15, 12);
            let op = bit(insn, 29);
            if !(cmode & 0b1001 == 0b0001 || cmode & 0b1101 == 0b1001) {
                return false;
            }
            let imm8 = (bits(insn, 18, 16) << 5) | bits(insn, 9, 5);
            let imm = maclator_core::interp::simd::adv_simd_expand_imm(op, cmode, imm8);
            self.vld(xmm0, rd, q);
            self.vconst(xmm1, imm);
            if op == 0 {
                a!(self, por(xmm0, xmm1));
            } else {
                a!(self, pandn(xmm1, xmm0));
                a!(self, movdqa(xmm0, xmm1));
            }
            self.vfin(rd, q);
            return true;
        }

        // ---- LD1/ST1 (single structure, one lane) and LD1R ----
        if insn & 0xBF9F_0000 == 0x0D00_0000 || insn & 0xBF80_0000 == 0x0D80_0000 {
            let post = bit(insn, 23) != 0;
            let load = bit(insn, 22) != 0;
            let r_bit = bit(insn, 21);
            let opcode = bits(insn, 15, 13);
            let s_bit = bit(insn, 12);
            let sz = bits(insn, 11, 10);
            if r_bit != 0 {
                return false;
            }
            if !post && bits(insn, 20, 16) != 0 {
                return false;
            }
            enum K { Lane(u32, u32), Rep(u32) }
            let kind = match opcode {
                0b000 => K::Lane(0, (q as u32) << 3 | s_bit << 2 | sz),
                0b010 if sz & 1 == 0 => K::Lane(1, (q as u32) << 2 | s_bit << 1 | (sz >> 1)),
                0b100 if sz == 0 => K::Lane(2, (q as u32) << 1 | s_bit),
                0b100 if sz == 1 && s_bit == 0 => K::Lane(3, q as u32),
                0b110 if load && s_bit == 0 => K::Rep(sz),
                _ => return false,
            };
            let esize_log = match kind { K::Lane(l, _) => l, K::Rep(l) => l };
            self.ldx(rax, rn, true);
            self.commpage_fix();
            match kind {
                K::Lane(l, idx) => {
                    let o = voff(rd) + (idx << l) as i32;
                    if load {
                        match l {
                            0 => { a!(self, movzx(ecx, byte_ptr(rax))); a!(self, mov(byte_ptr(r15 + o), cl)); }
                            1 => { a!(self, movzx(ecx, word_ptr(rax))); a!(self, mov(word_ptr(r15 + o), cx)); }
                            2 => { a!(self, mov(ecx, dword_ptr(rax))); a!(self, mov(dword_ptr(r15 + o), ecx)); }
                            _ => { a!(self, mov(rcx, qword_ptr(rax))); a!(self, mov(qword_ptr(r15 + o), rcx)); }
                        }
                    } else {
                        match l {
                            0 => { a!(self, movzx(ecx, byte_ptr(r15 + o))); a!(self, mov(byte_ptr(rax), cl)); }
                            1 => { a!(self, movzx(ecx, word_ptr(r15 + o))); a!(self, mov(word_ptr(rax), cx)); }
                            2 => { a!(self, mov(ecx, dword_ptr(r15 + o))); a!(self, mov(dword_ptr(rax), ecx)); }
                            _ => { a!(self, mov(rcx, qword_ptr(r15 + o))); a!(self, mov(qword_ptr(rax), rcx)); }
                        }
                    }
                }
                K::Rep(l) => {
                    match l {
                        0 => a!(self, movzx(eax, byte_ptr(rax))),
                        1 => a!(self, movzx(eax, word_ptr(rax))),
                        2 => a!(self, mov(eax, dword_ptr(rax))),
                        _ => a!(self, mov(rax, qword_ptr(rax))),
                    }
                    self.replicate_rax(l);
                    self.vst_rax(rd, q);
                }
            }
            if post {
                self.ldx(rcx, rn, true);
                if rm == 31 {
                    a!(self, add(rcx, (1i32 << esize_log)));
                } else {
                    self.ldx(rdx, rm, false);
                    a!(self, add(rcx, rdx));
                }
                self.stx(rn, rcx, true);
            }
            return true;
        }

        // ---- LD4 / ST4 (multiple structures), 128-bit, b/h/s elements ----
        if q && (insn & 0xBFFF_F000 == 0x0C00_0000 || insn & 0xBFA0_F000 == 0x0C80_0000) {
            let post = bit(insn, 23) != 0;
            let load = bit(insn, 22) != 0;
            if !post && bits(insn, 20, 16) != 0 {
                return false;
            }
            if size == 3 {
                return false;
            }
            self.ldx(rax, rn, true);
            self.commpage_fix();
            let regs = [rd & 31, (rd + 1) & 31, (rd + 2) & 31, (rd + 3) & 31];
            if load {
                for i in 0..4 {
                    a!(self, movdqu(XMM_TMP[i], xmmword_ptr(rax + (16 * i) as i32)));
                }
                // xmm0..xmm3 = memory quarters a,b,c,d
                match size {
                    0 | 1 => {
                        // group each register's bytes/halves by channel: 4 groups of 4 bytes
                        let mask: u128 = if size == 0 {
                            u128::from_le_bytes([0, 4, 8, 12, 1, 5, 9, 13, 2, 6, 10, 14, 3, 7, 11, 15])
                        } else {
                            u128::from_le_bytes([0, 1, 8, 9, 2, 3, 10, 11, 4, 5, 12, 13, 6, 7, 14, 15])
                        };
                        self.mov_imm(rdx, mask as u64);
                        a!(self, movq(xmm4, rdx));
                        self.mov_imm(rdx, (mask >> 64) as u64);
                        a!(self, movq(xmm5, rdx));
                        a!(self, punpcklqdq(xmm4, xmm5));
                        for i in 0..4 {
                            a!(self, pshufb(XMM_TMP[i], xmm4));
                        }
                    }
                    _ => {}
                }
                // dword 4x4 transpose: xmm0..3 -> channel vectors
                a!(self, movdqa(xmm4, xmm0));
                a!(self, punpckldq(xmm4, xmm1)); // s0
                a!(self, movdqa(xmm5, xmm2));
                a!(self, punpckldq(xmm5, xmm3)); // s1
                a!(self, punpckhdq(xmm0, xmm1)); // s2
                a!(self, punpckhdq(xmm2, xmm3)); // s3
                a!(self, movdqa(xmm1, xmm4));
                a!(self, punpcklqdq(xmm4, xmm5)); // v0
                a!(self, punpckhqdq(xmm1, xmm5)); // v1
                a!(self, movdqa(xmm3, xmm0));
                a!(self, punpcklqdq(xmm0, xmm2)); // v2
                a!(self, punpckhqdq(xmm3, xmm2)); // v3
                a!(self, movdqu(xmmword_ptr(r15 + voff(regs[0])), xmm4));
                a!(self, movdqu(xmmword_ptr(r15 + voff(regs[1])), xmm1));
                a!(self, movdqu(xmmword_ptr(r15 + voff(regs[2])), xmm0));
                a!(self, movdqu(xmmword_ptr(r15 + voff(regs[3])), xmm3));
            } else {
                for i in 0..4 {
                    a!(self, movdqu(XMM_TMP[i], xmmword_ptr(r15 + voff(regs[i]))));
                }
                // xmm0..3 = channels 0..3
                match size {
                    0 => {
                        a!(self, movdqa(xmm4, xmm0));
                        a!(self, punpcklbw(xmm4, xmm1)); // t0
                        a!(self, movdqa(xmm5, xmm2));
                        a!(self, punpcklbw(xmm5, xmm3)); // t1
                        a!(self, punpckhbw(xmm0, xmm1)); // t2
                        a!(self, punpckhbw(xmm2, xmm3)); // t3
                        a!(self, movdqa(xmm1, xmm4));
                        a!(self, punpcklwd(xmm4, xmm5)); // a
                        a!(self, punpckhwd(xmm1, xmm5)); // b
                        a!(self, movdqa(xmm3, xmm0));
                        a!(self, punpcklwd(xmm0, xmm2)); // c
                        a!(self, punpckhwd(xmm3, xmm2)); // d
                    }
                    1 => {
                        a!(self, movdqa(xmm4, xmm0));
                        a!(self, punpcklwd(xmm4, xmm1));
                        a!(self, movdqa(xmm5, xmm2));
                        a!(self, punpcklwd(xmm5, xmm3));
                        a!(self, punpckhwd(xmm0, xmm1));
                        a!(self, punpckhwd(xmm2, xmm3));
                        a!(self, movdqa(xmm1, xmm4));
                        a!(self, punpckldq(xmm4, xmm5));
                        a!(self, punpckhdq(xmm1, xmm5));
                        a!(self, movdqa(xmm3, xmm0));
                        a!(self, punpckldq(xmm0, xmm2));
                        a!(self, punpckhdq(xmm3, xmm2));
                    }
                    _ => {
                        a!(self, movdqa(xmm4, xmm0));
                        a!(self, punpckldq(xmm4, xmm1));
                        a!(self, movdqa(xmm5, xmm2));
                        a!(self, punpckldq(xmm5, xmm3));
                        a!(self, punpckhdq(xmm0, xmm1));
                        a!(self, punpckhdq(xmm2, xmm3));
                        a!(self, movdqa(xmm1, xmm4));
                        a!(self, punpcklqdq(xmm4, xmm5));
                        a!(self, punpckhqdq(xmm1, xmm5));
                        a!(self, movdqa(xmm3, xmm0));
                        a!(self, punpcklqdq(xmm0, xmm2));
                        a!(self, punpckhqdq(xmm3, xmm2));
                    }
                }
                a!(self, movdqu(xmmword_ptr(rax), xmm4));
                a!(self, movdqu(xmmword_ptr(rax + 16), xmm1));
                a!(self, movdqu(xmmword_ptr(rax + 32), xmm0));
                a!(self, movdqu(xmmword_ptr(rax + 48), xmm3));
            }
            if post {
                self.ldx(rcx, rn, true);
                if rm == 31 {
                    a!(self, add(rcx, 64i32));
                } else {
                    self.ldx(rdx, rm, false);
                    a!(self, add(rcx, rdx));
                }
                self.stx(rn, rcx, true);
            }
            return true;
        }

        false
    }
}

const XMM_TMP: [AsmRegisterXmm; 4] = [xmm0, xmm1, xmm2, xmm3];
