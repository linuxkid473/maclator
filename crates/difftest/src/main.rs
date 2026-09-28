//! Differential tester: executes random AArch64 instructions natively (on an
//! Apple Silicon host) and in the Maclator interpreter, and compares the
//! resulting architectural state.
//!
//! Usage: difftest [class] [count] [seed]

#[cfg(target_arch = "aarch64")]
mod native;

#[cfg(target_arch = "aarch64")]
fn main() {
    native::main();
}

#[cfg(not(target_arch = "aarch64"))]
fn main() {
    eprintln!("difftest must run natively on arm64");
}
