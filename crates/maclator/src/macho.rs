//! Minimal Mach-O parsing: fat/thin headers, segments, entry points and
//! function starts (used by the AOT translator to discover code).

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};

pub const MH_MAGIC_64: u32 = 0xfeed_facf;
pub const FAT_MAGIC: u32 = 0xcafe_babe;
pub const FAT_MAGIC_64: u32 = 0xcafe_babf;
pub const CPU_TYPE_ARM64: u32 = 0x0100_000c;
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
fn rdbe32(b: &[u8], o: usize) -> u32 {
    u32::from_be_bytes(b[o..o + 4].try_into().unwrap())
}
fn rdbe64(b: &[u8], o: usize) -> u64 {
    u64::from_be_bytes(b[o..o + 8].try_into().unwrap())
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
        let len = f.metadata()?.len();
        let mut hdr = [0u8; 4096];
        let n = f.read(&mut hdr)?;
        let magic_be = rdbe32(&hdr, 0);
        let (off, size) = if magic_be == FAT_MAGIC || magic_be == FAT_MAGIC_64 {
            let nfat = rdbe32(&hdr, 4) as usize;
            let mut best: Option<(u64, u64, bool)> = None;
            for i in 0..nfat {
                let (cpu, sub, off, size) = if magic_be == FAT_MAGIC {
                    let o = 8 + i * 20;
                    (rdbe32(&hdr, o), rdbe32(&hdr, o + 4), rdbe32(&hdr, o + 8) as u64, rdbe32(&hdr, o + 12) as u64)
                } else {
                    let o = 8 + i * 32;
                    (rdbe32(&hdr, o), rdbe32(&hdr, o + 4), rdbe64(&hdr, o + 8), rdbe64(&hdr, o + 16))
                };
                if cpu == CPU_TYPE_ARM64 {
                    let is_e = (sub & 0xff) == CPU_SUBTYPE_ARM64E;
                    // Avoid special variants like arm64e.x1 unless nothing else.
                    let special = (sub & 0x00ff_ff00) != 0 && is_e && (sub & 0xff00_0000) == 0 && false;
                    let _ = special;
                    match best {
                        None => best = Some((off, size, is_e)),
                        Some((_, _, be)) if be != prefer_e && is_e == prefer_e => best = Some((off, size, is_e)),
                        _ => {}
                    }
                }
            }
            let (o, s, _) = best.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, format!("{path}: no arm64 slice")))?;
            (o, s)
        } else {
            (0, len)
        };
        let _ = n;
        f.seek(SeekFrom::Start(off))?;
        let mut mh = vec![0u8; 32];
        f.read_exact(&mut mh)?;
        if rd32(&mh, 0) != MH_MAGIC_64 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, format!("{path}: not a 64-bit Mach-O")));
        }
        let sizeofcmds = rd32(&mh, 20) as usize;
        let mut all = vec![0u8; 32 + sizeofcmds];
        f.seek(SeekFrom::Start(off))?;
        f.read_exact(&mut all)?;
        let mut m = MachO::parse_header(&all)?;
        m.slice_offset = off;
        m.slice_size = size;
        Ok(m)
    }

    /// Parse from bytes holding the mach header and load commands.
    pub fn parse_header(all: &[u8]) -> io::Result<MachO> {
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
            let cmd = rd32(all, p);
            let cmdsize = rd32(all, p + 4) as usize;
            match cmd {
                LC_SEGMENT_64 => {
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
                        m.thread_pc = Some(rd64(all, p + 16 + 32 * 8));
                    }
                }
                LC_LOAD_DYLINKER => {
                    let off = rd32(all, p + 8) as usize;
                    m.dylinker = Some(cstr(&all[p + off..p + cmdsize]));
                }
                LC_FUNCTION_STARTS => m.function_starts = Some((rd32(all, p + 8), rd32(all, p + 12))),
                LC_UUID => m.uuid.copy_from_slice(&all[p + 8..p + 24]),
                _ => {}
            }
            p += cmdsize;
        }
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
            v |= ((b & 0x7f) as u64) << shift;
            shift += 7;
            if b & 0x80 == 0 || i >= data.len() {
                break;
            }
        }
        if v == 0 {
            break;
        }
        addr += v;
        out.push(addr);
    }
    out
}
