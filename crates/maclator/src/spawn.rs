//! execve / posix_spawn. Native (x86-64 or universal) programs run directly on
//! the host; arm64-only programs are re-launched through Maclator.

use crate::hostsys::{self, SysResult};

pub fn exec_like(n: u64, args: &[u64; 8]) -> SysResult {
    hostsys::unix(n, args)
}
