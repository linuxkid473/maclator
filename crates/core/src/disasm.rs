//! Human-readable disassembly for diagnostics.

use yaxpeax_arch::{Decoder, U8Reader};
use yaxpeax_arm::armv8::a64::InstDecoder;

pub fn disasm(insn: u32) -> String {
    let bytes = insn.to_le_bytes();
    let mut reader = U8Reader::new(&bytes);
    match InstDecoder::default().decode(&mut reader) {
        Ok(i) => i.to_string(),
        Err(e) => format!("<{e}>"),
    }
}
