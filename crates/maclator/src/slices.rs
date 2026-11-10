//! Shared, bounded Mach-O slice discovery for loading and child-process routing.
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};

pub const CPU_TYPE_X86_64: u32 = 0x0100_0007;
pub const CPU_TYPE_ARM64: u32 = 0x0100_000c;
const MH_MAGIC_64: u32 = 0xfeed_facf;

#[derive(Clone, Copy, Debug)]
pub struct Slice {
    pub cpu: u32,
    pub subtype: u32,
    pub offset: u64,
    pub size: u64,
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

pub fn read_slices(file: &mut File) -> io::Result<Vec<Slice>> {
    let len = file.metadata()?.len();
    parse(file, len)
}

fn parse<R: Read + Seek>(file: &mut R, len: u64) -> io::Result<Vec<Slice>> {
    file.seek(SeekFrom::Start(0))?;
    let mut head = [0u8; 8];
    file.read_exact(&mut head)?;
    let magic = u32::from_be_bytes(head[..4].try_into().unwrap());
    let (wide, be) = match magic {
        0xcafe_babe => (false, true),
        0xbeba_feca => (false, false),
        0xcafe_babf => (true, true),
        0xbfba_feca => (true, false),
        _ => {
            if u32::from_le_bytes(head[..4].try_into().unwrap()) != MH_MAGIC_64 || len < 32 {
                return Err(invalid("not a supported 64-bit Mach-O"));
            }
            let mut rest = [0u8; 24];
            file.read_exact(&mut rest)?;
            return Ok(vec![Slice {
                cpu: u32::from_le_bytes(head[4..8].try_into().unwrap()),
                subtype: u32::from_le_bytes(rest[..4].try_into().unwrap()),
                offset: 0, size: len,
            }]);
        }
    };
    let u32at = |b: &[u8], p: usize| {
        let a = b[p..p + 4].try_into().unwrap();
        if be { u32::from_be_bytes(a) } else { u32::from_le_bytes(a) }
    };
    let u64at = |b: &[u8], p: usize| {
        let a = b[p..p + 8].try_into().unwrap();
        if be { u64::from_be_bytes(a) } else { u64::from_le_bytes(a) }
    };
    let count = u32at(&head, 4) as usize;
    let stride = if wide { 32 } else { 20 };
    // This limit bounds allocation and work on corrupt files. Real fat files
    // have only a handful of architectures; the full legal table is scanned.
    if count == 0 || count > 65536 { return Err(invalid("invalid fat architecture count")); }
    let end = 8 + count as u64 * stride as u64;
    if end > len { return Err(invalid("truncated fat architecture table")); }
    let mut table = vec![0u8; count * stride];
    file.read_exact(&mut table)?;
    let mut slices = Vec::with_capacity(count);
    for entry in table.chunks_exact(stride) {
        let (offset, size, alignment) = if wide {
            (u64at(entry, 8), u64at(entry, 16), u32at(entry, 24))
        } else {
            (u32at(entry, 8) as u64, u32at(entry, 12) as u64, u32at(entry, 16))
        };
        if size < 4 || offset < end || alignment >= 64
            || offset & ((1u64 << alignment) - 1) != 0
            || offset.checked_add(size).filter(|&v| v <= len).is_none() {
            return Err(invalid("fat slice outside file or improperly aligned"));
        }
        slices.push(Slice { cpu: u32at(entry, 0), subtype: u32at(entry, 4), offset, size });
    }
    let mut ranges: Vec<_> = slices.iter().map(|s| (s.offset, s.offset + s.size)).collect();
    ranges.sort_unstable();
    if ranges.windows(2).any(|w| w[0].1 > w[1].0) {
        return Err(invalid("overlapping fat slices"));
    }
    Ok(slices)
}

pub fn arm64_only(path: &str) -> bool {
    let Ok(mut f) = File::open(path) else { return false };
    let Ok(s) = read_slices(&mut f) else { return false };
    s.iter().any(|a| a.cpu == CPU_TYPE_ARM64)
        && !s.iter().any(|a| a.cpu == CPU_TYPE_X86_64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn fat(wide: bool, be: bool, cpus: &[u32]) -> Vec<u8> {
        let stride = if wide { 32 } else { 20 };
        let mut b = vec![0; 8 + stride * cpus.len() + 32 * cpus.len()];
        let put32 = |b: &mut [u8], p, v: u32| b[p..p+4].copy_from_slice(
            &if be { v.to_be_bytes() } else { v.to_le_bytes() });
        put32(&mut b, 0, if wide { 0xcafe_babf } else { 0xcafe_babe });
        put32(&mut b, 4, cpus.len() as u32);
        for (i, &cpu) in cpus.iter().enumerate() {
            let p = 8 + i * stride;
            let offset = 8 + stride * cpus.len() + 32 * i;
            put32(&mut b, p, cpu);
            if wide {
                for (pos, v) in [(p+8, offset as u64), (p+16, 32u64)] {
                    b[pos..pos+8].copy_from_slice(&if be { v.to_be_bytes() } else { v.to_le_bytes() });
                }
            } else {
                put32(&mut b, p+8, offset as u32);
                put32(&mut b, p+12, 32);
            }
        }
        b
    }

    #[test]
    fn full_tables_both_widths_and_byte_orders() {
        let mut cpus = vec![CPU_TYPE_ARM64; 260];
        cpus.push(CPU_TYPE_X86_64);
        for wide in [false, true] { for be in [false, true] {
            let b = fat(wide, be, &cpus);
            let s = parse(&mut Cursor::new(&b), b.len() as u64).unwrap();
            assert_eq!(s.len(), cpus.len());
            assert_eq!(s.last().unwrap().cpu, CPU_TYPE_X86_64);
        }}
    }

    #[test]
    fn corrupt_tables_return_errors() {
        for wide in [false, true] { for be in [false, true] {
            let mut b = fat(wide, be, &[CPU_TYPE_ARM64]);
            let len = b.len() as u64;
            assert!(parse(&mut Cursor::new(&b), len-1).is_err());
            b[4..8].fill(0xff);
            assert!(parse(&mut Cursor::new(&b), len).is_err());
            for n in 0..8 { assert!(parse(&mut Cursor::new(&b[..n]), n as u64).is_err()); }
        }}
        let mut b = fat(true, true, &[CPU_TYPE_ARM64]);
        b[16..24].copy_from_slice(&u64::MAX.to_be_bytes());
        let len = b.len() as u64;
        assert!(parse(&mut Cursor::new(&b), len).is_err());
        let mut b = fat(false, true, &[CPU_TYPE_ARM64, CPU_TYPE_X86_64]);
        let first = b[16..20].to_vec();
        b[36..40].copy_from_slice(&first);
        let len = b.len() as u64;
        assert!(parse(&mut Cursor::new(&b), len).is_err());
    }
}
