//! Minimal Mach-O parsing: fat/thin headers, segments, entry points and
//! function starts (used by the AOT translator to discover code).

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};

pub const MH_MAGIC_64: u32 = 0xfeed_facf;
pub use crate::slices::CPU_TYPE_ARM64;
pub const CPU_SUBTYPE_ARM64E: u32 = 2;

pub const LC_SEGMENT_64: u32 = 0x19;
pub const LC_UNIXTHREAD: u32 = 0x5;
pub const LC_MAIN: u32 = 0x8000_0028;
pub const LC_LOAD_DYLINKER: u32 = 0xe;
pub const LC_FUNCTION_STARTS: u32 = 0x26;
pub const LC_SYMTAB: u32 = 0x2;
pub const LC_UUID: u32 = 0x1b;

pub const MH_EXECUTE: u32 = 2;
pub const MH_DYLINKER: u32 = 7;

#[derive(Debug, Clone)]
pub struct Segment {
    pub name: String,
    pub vmaddr: u64,
    pub vmsize: u64,
    pub fileoff: u64,
    pub filesize: u64,
    pub maxprot: u32,
    pub initprot: u32,
}

#[derive(Debug, Clone)]
pub struct MachO {
    /// File offset of this (thin) Mach-O slice within the file.
    pub slice_offset: u64,
    pub slice_size: u64,
    pub cputype: u32,
    pub cpusubtype: u32,
    pub filetype: u32,
    pub segments: Vec<Segment>,
    /// LC_MAIN entry offset (from __TEXT start) if present.
    pub main_entryoff: Option<u64>,
    /// LC_UNIXTHREAD pc if present (unslid).
    pub thread_pc: Option<u64>,
    pub dylinker: Option<String>,
    /// (dataoff, datasize) of LC_FUNCTION_STARTS
    pub function_starts: Option<(u32, u32)>,
    pub uuid: [u8; 16],
    /// Raw load command bytes (header + commands), for re-parsing.
    pub header_bytes: Vec<u8>,
}

fn rd32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
fn rd64(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}
fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
fn cstr(b: &[u8]) -> String {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..end]).into_owned()
}

impl MachO {
    /// Open `path` and select the arm64 (preferring arm64e when `prefer_e`)
    /// slice.
    pub fn open_arm64(path: &str, prefer_e: bool) -> io::Result<MachO> {
        let mut f = File::open(path)?;
        let slices = crate::slices::read_slices(&mut f)?;
        let slice = slices.iter().filter(|s| s.cpu == CPU_TYPE_ARM64)
            .min_by_key(|s| ((s.subtype & 0xff) == CPU_SUBTYPE_ARM64E) != prefer_e)
            .ok_or_else(|| invalid("no arm64 slice"))?;
        let (off, size) = (slice.offset, slice.size);
        if size < 32 { return Err(invalid("truncated Mach-O header")); }
        f.seek(SeekFrom::Start(off))?;
        let mut mh = vec![0u8; 32];
        f.read_exact(&mut mh)?;
        if rd32(&mh, 0) != MH_MAGIC_64 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, format!("{path}: not a 64-bit Mach-O")));
        }
        if rd32(&mh, 4) != CPU_TYPE_ARM64 { return Err(invalid("slice CPU does not match header")); }
        let sizeofcmds = rd32(&mh, 20) as usize;
        if sizeofcmds as u64 > size - 32 || sizeofcmds > 64 * 1024 * 1024 {
            return Err(invalid("load commands outside slice or unreasonably large"));
        }
        let mut all = vec![0u8; 32 + sizeofcmds];
        f.seek(SeekFrom::Start(off))?;
        f.read_exact(&mut all)?;
        let mut m = MachO::parse_header(&all)?;
        for s in &m.segments {
            if s.filesize > s.vmsize || s.fileoff.checked_add(s.filesize).filter(|&v| v <= size).is_none() {
                return Err(invalid("segment file range outside slice"));
            }
        }
        if let Some((offset, length)) = m.function_starts {
            if offset as u64 + length as u64 > size { return Err(invalid("function starts outside slice")); }
        }
        m.slice_offset = off;
        m.slice_size = size;
        Ok(m)
    }

    /// Parse from bytes holding the mach header and load commands.
    pub fn parse_header(all: &[u8]) -> io::Result<MachO> {
        if all.len() < 32 || rd32(all, 0) != MH_MAGIC_64 {
            return Err(invalid("invalid or truncated Mach-O header"));
        }
        let sizeofcmds = rd32(all, 20) as usize;
        if sizeofcmds > all.len() - 32 || rd32(all, 16) as usize > sizeofcmds / 8 {
            return Err(invalid("truncated load commands or invalid command count"));
        }
        let end = 32 + sizeofcmds;
        let cputype = rd32(all, 4);
        let cpusubtype = rd32(all, 8);
        let filetype = rd32(all, 12);
        let ncmds = rd32(all, 16);
        let mut m = MachO {
            slice_offset: 0,
            slice_size: 0,
            cputype,
            cpusubtype,
            filetype,
            segments: vec![],
            main_entryoff: None,
            thread_pc: None,
            dylinker: None,
            function_starts: None,
            uuid: [0; 16],
            header_bytes: all.to_vec(),
        };
        let mut p = 32usize;
        for _ in 0..ncmds {
            if p > end || end - p < 8 { return Err(invalid("truncated load command")); }
            let cmd = rd32(all, p);
            let cmdsize = rd32(all, p + 4) as usize;
            if cmdsize < 8 || cmdsize & 7 != 0 || cmdsize > end - p {
                return Err(invalid("invalid load command size"));
            }
            let minimum = match cmd {
                LC_SEGMENT_64 => 72, LC_MAIN => 24, LC_UNIXTHREAD => 16,
                LC_LOAD_DYLINKER => 12, LC_FUNCTION_STARTS => 16, LC_UUID => 24,
                _ => 8,
            };
            if cmdsize < minimum { return Err(invalid("short load command")); }
            match cmd {
                LC_SEGMENT_64 => {
                    if rd32(all, p + 64) as u64 * 80 + 72 > cmdsize as u64
                        || rd64(all, p + 24).checked_add(rd64(all, p + 32)).is_none() {
                        return Err(invalid("invalid segment or section range"));
                    }
                    m.segments.push(Segment {
                        name: cstr(&all[p + 8..p + 24]),
                        vmaddr: rd64(all, p + 24),
                        vmsize: rd64(all, p + 32),
                        fileoff: rd64(all, p + 40),
                        filesize: rd64(all, p + 48),
                        maxprot: rd32(all, p + 56),
                        initprot: rd32(all, p + 60),
                    });
                }
                LC_MAIN => m.main_entryoff = Some(rd64(all, p + 8)),
                LC_UNIXTHREAD => {
                    // flavor, count, then arm_thread_state64: x0..x28, fp, lr, sp, pc, cpsr
                    let flavor = rd32(all, p + 8);
                    if flavor == 6 {
                        if rd32(all, p + 12) < 68 || cmdsize < 16 + 68 * 4 {
                            return Err(invalid("truncated ARM thread state"));
                        }
                        m.thread_pc = Some(rd64(all, p + 16 + 32 * 8));
                    }
                }
                LC_LOAD_DYLINKER => {
                    let off = rd32(all, p + 8) as usize;
                    if off < 12 || off >= cmdsize || !all[p + off..p + cmdsize].contains(&0) {
                        return Err(invalid("invalid dylinker name offset or terminator"));
                    }
                    m.dylinker = Some(cstr(&all[p + off..p + cmdsize]));
                }
                LC_FUNCTION_STARTS => m.function_starts = Some((rd32(all, p + 8), rd32(all, p + 12))),
                LC_UUID => m.uuid.copy_from_slice(&all[p + 8..p + 24]),
                _ => {}
            }
            p += cmdsize;
        }
        if p != end { return Err(invalid("load command count/size mismatch")); }
        Ok(m)
    }

    pub fn text_vmaddr(&self) -> u64 {
        self.segments.iter().find(|s| s.name == "__TEXT").map(|s| s.vmaddr).unwrap_or(0)
    }

    /// Lowest and highest (exclusive) vm address of all segments except __PAGEZERO.
    pub fn vm_range(&self) -> (u64, u64) {
        let mut lo = u64::MAX;
        let mut hi = 0;
        for s in &self.segments {
            if s.name == "__PAGEZERO" {
                continue;
            }
            lo = lo.min(s.vmaddr);
            hi = hi.max(s.vmaddr + s.vmsize);
        }
        (lo, hi)
    }
}

/// Decode LC_FUNCTION_STARTS ULEB128 deltas into absolute (unslid) addresses.
pub fn decode_function_starts(data: &[u8], text_start: u64) -> Vec<u64> {
    let mut out = Vec::new();
    let mut addr = text_start;
    let mut i = 0;
    while i < data.len() {
        let mut v: u64 = 0;
        let mut shift = 0;
        loop {
            let b = data[i];
            i += 1;
            if shift >= 64 || (shift == 63 && b & 0x7f > 1) { return out; }
            v |= ((b & 0x7f) as u64) << shift;
            shift += 7;
            if b & 0x80 == 0 {
                break;
            }
            if i >= data.len() { return out; }
        }
        if v == 0 {
            break;
        }
        let Some(next) = addr.checked_add(v) else { return out };
        addr = next;
        out.push(addr);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    fn header(command: &[u8]) -> Vec<u8> {
        let mut b = vec![0; 32];
        b[..4].copy_from_slice(&MH_MAGIC_64.to_le_bytes());
        b[4..8].copy_from_slice(&CPU_TYPE_ARM64.to_le_bytes());
        b[16..20].copy_from_slice(&(if command.is_empty() { 0u32 } else { 1 }).to_le_bytes());
        b[20..24].copy_from_slice(&(command.len() as u32).to_le_bytes());
        b.extend_from_slice(command);
        b
    }
    #[test]
    fn truncated_headers_and_commands_do_not_panic() {
        let mut c = vec![0; 24];
        c[..4].copy_from_slice(&LC_MAIN.to_le_bytes());
        c[4..8].copy_from_slice(&24u32.to_le_bytes());
        let good = header(&c);
        assert_eq!(MachO::parse_header(&good).unwrap().main_entryoff, Some(0));
        for n in 0..good.len() { assert!(MachO::parse_header(&good[..n]).is_err()); }
        for size in [0u32, 4, 8, 16, 25, u32::MAX] {
            let mut b = good.clone();
            b[36..40].copy_from_slice(&size.to_le_bytes());
            assert!(MachO::parse_header(&b).is_err());
        }
    }
    #[test]
    fn dylinker_and_segment_internal_ranges_checked() {
        let mut c = vec![0; 16];
        c[..4].copy_from_slice(&LC_LOAD_DYLINKER.to_le_bytes());
        c[4..8].copy_from_slice(&16u32.to_le_bytes());
        for offset in [0u32, 8, 16, u32::MAX] {
            c[8..12].copy_from_slice(&offset.to_le_bytes());
            assert!(MachO::parse_header(&header(&c)).is_err());
        }
        c[8..12].copy_from_slice(&12u32.to_le_bytes());
        assert!(MachO::parse_header(&header(&c)).is_ok());
        c[12..16].fill(b'a');
        assert!(MachO::parse_header(&header(&c)).is_err());
        let mut c = vec![0; 72];
        c[..4].copy_from_slice(&LC_SEGMENT_64.to_le_bytes());
        c[4..8].copy_from_slice(&72u32.to_le_bytes());
        c[64..68].copy_from_slice(&1u32.to_le_bytes());
        assert!(MachO::parse_header(&header(&c)).is_err());
    }
    #[test]
    fn function_start_overflow_and_truncation_stop_cleanly() {
        assert_eq!(decode_function_starts(&[1, 2, 0], 100), vec![101, 103]);
        assert_eq!(decode_function_starts(&[1, 0x80], 100), vec![101]);
        assert!(decode_function_starts(&[1], u64::MAX).is_empty());
        assert!(decode_function_starts(&[0xff; 12], 0).is_empty());
    }
}
