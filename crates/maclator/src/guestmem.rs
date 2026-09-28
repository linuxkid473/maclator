//! Fault-tolerant access to guest memory from the runtime (syscall
//! emulation must return EFAULT rather than crash on bad pointers).

extern "C" {
    fn mach_task_self() -> u32;
    fn mach_vm_read_overwrite(task: u32, addr: u64, size: u64, data: u64, outsize: *mut u64) -> i32;
    fn mach_vm_write(task: u32, addr: u64, data: u64, cnt: u32) -> i32;
}

pub fn read(addr: u64, buf: &mut [u8]) -> bool {
    let mut out = 0u64;
    unsafe { mach_vm_read_overwrite(mach_task_self(), addr, buf.len() as u64, buf.as_mut_ptr() as u64, &mut out) == 0 }
}

pub fn write(addr: u64, data: &[u8]) -> bool {
    unsafe { mach_vm_write(mach_task_self(), addr, data.as_ptr() as u64, data.len() as u32) == 0 }
}

pub fn read_u64(addr: u64) -> Option<u64> {
    let mut b = [0u8; 8];
    read(addr, &mut b).then(|| u64::from_le_bytes(b))
}

pub fn write_u64(addr: u64, v: u64) -> bool {
    write(addr, &v.to_le_bytes())
}
