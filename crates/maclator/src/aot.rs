//! Ahead-of-time translation cache.
//!
//! Images that Maclator maps at fixed addresses (the main executable and the
//! arm64 shared cache) get a persistent translation file in
//! `~/Library/Caches/Maclator/aot/`. Each file holds position-independent x86
//! code for a set of guest blocks plus an index.
//!
//! Where the blocks come from:
//!  * static discovery: `LC_FUNCTION_STARTS` + following direct branches
//!    (main executable only; the shared cache is too large to translate whole);
//!  * feedback: every block the JIT translated during a run is recorded in a
//!    profile, and the next AOT build includes it.
//!
//! On exit, if new blocks were seen, a forked child rebuilds the AOT files in
//! the background, so the next launch starts with warm translations.

use crate::jit::emit::{self, StubKind, Unit};
use iced_x86::BlockEncoderOptions;
use std::collections::{BTreeSet, HashSet};
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

const MAGIC: &[u8; 8] = b"MCLAOT04";
/// Bump when the translator's output changes.
const TRANSLATOR_VERSION: u32 = 25;

#[derive(Clone)]
struct Image {
    name: String,
    key: String,
    start: u64,
    end: u64,
    /// Static entry points (function starts), if known.
    entries: Vec<u64>,
    loaded: bool,
    /// Whether a cache for this translator version was successfully loaded.
    cached: bool,
}

static IMAGES: Mutex<Vec<Image>> = Mutex::new(Vec::new());
/// Blocks translated by the JIT this run (per image index).
static SEEN: Mutex<Vec<HashSet<u64>>> = Mutex::new(Vec::new());
static ENABLED: AtomicBool = AtomicBool::new(true);
static SAVED: AtomicBool = AtomicBool::new(false);

pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::SeqCst);
}

fn cache_dir() -> std::path::PathBuf {
    if let Ok(d) = std::env::var("MACLATOR_CACHE") {
        return d.into();
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    std::path::Path::new(&home).join("Library/Caches/Maclator/aot")
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// FNV-1a, enough to name cache files.
fn fnv(parts: &[&[u8]]) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for p in parts {
        for &b in *p {
            h ^= b as u64;
            h = h.wrapping_mul(0x100_0000_01b3);
        }
        h ^= 0xff;
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    format!("{h:016x}")
}

fn flags_word() -> u32 {
    crate::jit::commpage_checks() as u32 | ((crate::jit::emit::native_fma() as u32) << 1)
        | ((crate::jit::emit::direct_operands() as u32) << 2)
        | ((crate::jit::emit::loop_cache() as u32) << 3)
        | ((maclator_core::mem::native_pair_atomic() as u32) << 4)
        | ((crate::jit::emit::register_budget() as u32) << 5)
        | ((crate::jit::emit::cold_budget() as u32) << 7)
        | ((crate::jit::emit::csel_chain() as u32) << 8)
        | ((crate::jit::emit::packed_flags() as u32) << 11)
}

/// Register the main executable (mapped at a fixed address).
pub fn register_main(path: &str, uuid: [u8; 16], start: u64, end: u64, entries: Vec<u64>) {
    let key = format!("main-{}-{}", hex(&uuid), fnv(&[path.as_bytes()]));
    add_image(Image { name: path.rsplit('/').next().unwrap_or(path).to_string(), key, start, end, entries, loaded: false, cached: false });
}

/// Register dyld (also mapped at a fixed address).
pub fn register_dyld(path: &str, uuid: [u8; 16], start: u64, end: u64) {
    let key = format!("dyld-{}-{}", hex(&uuid), fnv(&[path.as_bytes()]));
    add_image(Image { name: "dyld".into(), key, start, end, entries: vec![], loaded: false, cached: false });
}

/// Register the shared cache once dyld has had us map it.
pub fn register_cache(uuid: [u8; 16], start: u64, end: u64) {
    let key = format!("dsc-{}", hex(&uuid));
    add_image(Image { name: "dyld_shared_cache".into(), key, start, end, entries: vec![], loaded: false, cached: false });
    ensure_loaded();
}

/// Register the arm64 Mach-O whose header is mapped at `addr` (a dyld-loaded library or
/// helper image) by a mapping `len` bytes long. Keyed by UUID and load address, so it only hits
/// the cache when the library loads at the same place again.
pub fn register_image_at(addr: u64, len: u64) {
    if !ENABLED.load(Ordering::Relaxed) {
        return;
    }
    // The mapping is the guest's: it may be unreadable, shorter than its header claims, or not a
    // Mach-O at all. So copy the header and then the load commands with the fault-safe read, and
    // never read load commands past the mapping.
    let mut head = [0u8; 32];
    if !crate::guestmem::read(addr, &mut head) {
        return;
    }
    let field = |o: usize| u32::from_le_bytes(head[o..o + 4].try_into().unwrap());
    // mach_header_64: magic, cputype, ..., ncmds at +16, sizeofcmds at +20
    if field(0) != 0xfeed_facf || field(4) != 0x0100_000c {
        if std::env::var_os("MACLATOR_STATS").is_some() {
            eprintln!("[maclator] aot: mmap exec at {addr:#x} not an arm64 mach-o ({:#x} {:#x})", field(0), field(4));
        }
        return;
    }
    let ncmds = field(16);
    let mut buf = vec![0u8; (32 + field(20) as usize).min(0x4000).min(len as usize)];
    if !crate::guestmem::read(addr, &mut buf) {
        return;
    }
    let rd = |o: u64| buf.get(o as usize..o as usize + 4).map_or(0, |s| u32::from_le_bytes(s.try_into().unwrap()));
    let rd64 = |o: u64| buf.get(o as usize..o as usize + 8).map_or(0, |s| u64::from_le_bytes(s.try_into().unwrap()));
    let rd16 = |o: u64| -> [u8; 16] { buf.get(o as usize..o as usize + 16).map_or([0; 16], |s| s.try_into().unwrap()) };
    let mut off = 32u64;
    let mut uuid = [0u8; 16];
    let (mut text_vm, mut lo, mut hi) = (u64::MAX, u64::MAX, 0u64);
    for _ in 0..ncmds.min(512) {
        let cmd = rd(off);
        let size = rd(off + 4) as u64;
        // An LC_UUID (24 bytes) or LC_SEGMENT_64 (72) too short for the fields read below is malformed too.
        if size < 8 || off + size > buf.len() as u64 || (cmd == 0x1b && size < 24) || (cmd == 0x19 && size < 72) {
            return;
        }
        if cmd == 0x1b {
            uuid = rd16(off + 8);
        } else if cmd == 0x19 {
            let name = rd16(off + 8);
            let vmaddr = rd64(off + 24);
            let vmsize = rd64(off + 32);
            if name.starts_with(b"__PAGEZERO") {
                off += size;
                continue;
            }
            if name.starts_with(b"__TEXT\0") {
                text_vm = vmaddr;
            }
            lo = lo.min(vmaddr);
            hi = hi.max(vmaddr + vmsize);
        }
        off += size;
    }
    if text_vm == u64::MAX || uuid == [0u8; 16] || hi <= lo {
        return;
    }
    let slide = addr.wrapping_sub(text_vm);
    let (start, end) = (lo.wrapping_add(slide), hi.wrapping_add(slide));
    let key = format!("lib-{}-{:x}", hex(&uuid), start);
    if std::env::var_os("MACLATOR_STATS").is_some() {
        eprintln!("[maclator] aot: registered image {key} [{start:#x}, {end:#x})");
    }
    add_image(Image { name: format!("lib-{}", &hex(&uuid)[..8]), key, start, end, entries: vec![], loaded: false, cached: false });
    ensure_loaded();
}

fn add_image(img: Image) {
    let mut v = IMAGES.lock().unwrap();
    if v.iter().any(|i| i.key == img.key) {
        return;
    }
    v.push(img);
    SEEN.lock().unwrap().push(HashSet::new());
}

thread_local! {
    /// (start, end, image index) of the image this thread last recorded a block in.
    static LAST_IMG: std::cell::Cell<(u64, u64, usize)> = const { std::cell::Cell::new((1, 0, 0)) };
    static PENDING: std::cell::RefCell<Vec<(usize, u64)>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Record that the guest block at `pc` executed (JIT-translated or cold-interpreted), so the
/// next AOT build covers it.
pub fn note_translated(pc: u64) {
    if !ENABLED.load(Ordering::Relaxed) {
        return;
    }
    let (start, end, idx) = LAST_IMG.with(|l| l.get());
    let idx = if pc >= start && pc < end {
        idx
    } else {
        let imgs = IMAGES.lock().unwrap();
        match imgs.iter().position(|im| pc >= im.start && pc < im.end) {
            Some(i) => {
                LAST_IMG.with(|l| l.set((imgs[i].start, imgs[i].end, i)));
                i
            }
            None => return,
        }
    };
    // Batch inserts so the global lock is taken once per 256 blocks.
    let flush = PENDING.with(|p| {
        let mut p = p.borrow_mut();
        p.push((idx, pc));
        if p.len() >= 256 {
            Some(std::mem::take(&mut *p))
        } else {
            None
        }
    });
    if let Some(batch) = flush {
        let mut seen = SEEN.lock().unwrap();
        for (i, pc) in batch {
            seen[i].insert(pc);
        }
    }
}

/// Publish this thread's not-yet-flushed blocks (called before saving the profile).
pub fn flush_pending() {
    let batch = PENDING.with(|p| std::mem::take(&mut *p.borrow_mut()));
    let mut seen = SEEN.lock().unwrap();
    for (i, pc) in batch {
        seen[i].insert(pc);
    }
}

fn file_for(img: &Image, ext: &str) -> std::path::PathBuf {
    cache_dir().join(format!("{}.{}", img.key, ext))
}

/// Load AOT code for any registered image not loaded yet.
pub fn ensure_loaded() {
    if !ENABLED.load(Ordering::Relaxed) {
        return;
    }
    let mut imgs = IMAGES.lock().unwrap();
    for img in imgs.iter_mut() {
        if img.loaded {
            continue;
        }
        img.loaded = true;
        let begun = std::time::Instant::now();
        match load_file(&file_for(img, "aot"), img.start, img.end) {
            Ok(n) if n > 0 => {
                img.cached = true;
                if std::env::var_os("MACLATOR_TRACE").is_some() || std::env::var_os("MACLATOR_STATS").is_some() {
                    eprintln!("[maclator] aot: loaded {n} blocks for {} in {:.1} ms", img.name, begun.elapsed().as_secs_f64() * 1000.0);
                }
            }
            _ => {}
        }
    }
}

const CODE_ALIGN: usize = 0x4000;

fn code_offset(n: usize) -> usize {
    (32 + n * 16 + CODE_ALIGN - 1) & !(CODE_ALIGN - 1)
}

// Bound disk-controlled sizes before allocating memory or publishing executable
// code. The current cache is normally a few MiB to tens of MiB per image.
const MAX_AOT_BLOCKS: usize = 4_000_000;
const MAX_AOT_CODE: usize = 512 << 20;

fn cache_layout(head: &[u8; 32], file_len: u64) -> Option<(usize, usize, usize)> {
    let rd32 = |o| u32::from_le_bytes(head[o..o + 4].try_into().unwrap());
    let rd64 = |o| u64::from_le_bytes(head[o..o + 8].try_into().unwrap());
    if &head[..8] != MAGIC || rd32(8) != TRANSLATOR_VERSION || rd32(12) != flags_word() {
        return None;
    }
    let n = usize::try_from(rd64(16)).ok()?;
    let code_len = usize::try_from(rd64(24)).ok()?;
    if n == 0 || n > MAX_AOT_BLOCKS || code_len == 0 || code_len > MAX_AOT_CODE {
        return None;
    }
    let code_off = code_offset(n);
    if (code_off + code_len) as u64 > file_len {
        return None;
    }
    Some((n, code_len, code_off))
}

fn load_file(path: &std::path::Path, start: u64, end: u64) -> std::io::Result<usize> {
    let mut f = std::fs::File::open(path)?;
    let mut head = [0u8; 32];
    if f.read_exact(&mut head).is_err() {
        return Ok(0);
    }
    let rd64 = |b: &[u8], o: usize| u64::from_le_bytes(b[o..o + 8].try_into().unwrap());
    let Some((n, code_len, code_off)) = cache_layout(&head, f.metadata()?.len()) else {
        return Ok(0);
    };
    let mut idx = vec![0u8; n * 16];
    f.read_exact(&mut idx)?;
    // Never let an invalid index publish arbitrary host addresses. Validate the
    // whole image before registering any entry; a partial load is not useful.
    for entry in idx.chunks_exact(16) {
        let pc = rd64(entry, 0);
        if pc < start || pc >= end || pc & 3 != 0 || rd64(entry, 8) >= code_len as u64 {
            return Ok(0);
        }
    }
    // Copy the code into the JIT code cache. (Mapping the file itself PROT_EXEC makes macOS
    // run a Gatekeeper assessment on every cache file and pop up "could not verify" dialogs.)
    let cc = crate::jit::code_cache();
    let base = cc.alloc(code_len as u64);
    {
        use std::io::Seek;
        f.seek(std::io::SeekFrom::Start(code_off as u64))?;
        let dst = unsafe { std::slice::from_raw_parts_mut(base as *mut u8, code_len) };
        f.read_exact(dst)?;
    }
    crate::jit::register_blocks(idx.chunks_exact(16).map(|entry| (rd64(entry, 0), base + rd64(entry, 8))));
    Ok(n)
}

fn read_profile(path: &std::path::Path) -> BTreeSet<u64> {
    let mut s = BTreeSet::new();
    if let Ok(d) = std::fs::read(path) {
        for c in d.chunks_exact(8) {
            s.insert(u64::from_le_bytes(c.try_into().unwrap()));
        }
    }
    s
}

/// Save this run's profile; if it contains new blocks, rebuild AOT files in
/// a background (forked) process.
pub fn save_profile() {
    if !ENABLED.load(Ordering::Relaxed) || SAVED.swap(true, Ordering::SeqCst) {
        return;
    }
    flush_pending();
    let imgs = IMAGES.lock().unwrap().clone();
    let seen = SEEN.lock().unwrap().clone();
    let _ = std::fs::create_dir_all(cache_dir());
    let mut rebuild = Vec::new();
    for (i, img) in imgs.iter().enumerate() {
        let path = file_for(img, "profile");
        let mut prof = read_profile(&path);
        let before = prof.len();
        prof.extend(seen[i].iter().copied());
        // A file left by an older translator is present but unusable. Rebuild
        // it even when this run saw no blocks beyond the existing profile.
        let aot_missing = !img.cached;
        if prof.len() != before || (aot_missing && (!prof.is_empty() || !img.entries.is_empty())) {
            let mut buf = Vec::with_capacity(prof.len() * 8);
            for pc in &prof {
                buf.extend_from_slice(&pc.to_le_bytes());
            }
            let _ = std::fs::write(&path, buf);
            rebuild.push((img.clone(), prof));
        }
    }
    if rebuild.is_empty() || std::env::var_os("MACLATOR_NO_AOT_BUILD").is_some() {
        return;
    }
    let sync = std::env::var_os("MACLATOR_AOT_SYNC").is_some();
    // Fork: the child still has the guest's memory mapped, so it can read
    // the code it needs to translate.
    let pid = if sync { 0 } else { unsafe { libc::fork() } };
    if pid != 0 {
        return;
    }
    if !sync {
        unsafe {
            libc::setsid();
            let null = libc::open(c"/dev/null".as_ptr(), libc::O_RDWR);
            libc::dup2(null, 0);
            libc::dup2(null, 1);
            libc::dup2(null, 2);
        }
    }
    for (img, prof) in rebuild {
        let _ = build(&img, &prof);
    }
    if !sync {
        unsafe { libc::_exit(0) };
    }
}

/// Collect block starts reachable from `entries` via direct branches.
fn discover(entries: &[u64], start: u64, end: u64, read: &dyn Fn(u64) -> Option<u32>, limit: usize) -> BTreeSet<u64> {
    let mut out = BTreeSet::new();
    let mut work: Vec<u64> = entries.to_vec();
    while let Some(pc) = work.pop() {
        if pc < start || pc >= end || pc & 3 != 0 || !out.insert(pc) || out.len() > limit {
            continue;
        }
        let mut cur = pc;
        for _ in 0..emit::MAX_BLOCK_INSNS {
            let Some(insn) = read(cur) else { break };
            if insn == 0 {
                break;
            }
            let next = cur + 4;
            let sext = |v: u64, n: u32| (((v << (64 - n)) as i64) >> (64 - n)) as u64;
            if insn & 0x7C00_0000 == 0x1400_0000 {
                let t = cur.wrapping_add(sext((insn & 0x03ff_ffff) as u64, 26) << 2);
                if insn & 0x8000_0000 != 0 {
                    work.push(next); // return point after BL
                } else {
                    work.push(t);
                }
                break;
            } else if insn & 0xFF00_0000 == 0x5400_0000 || insn & 0x7E00_0000 == 0x3400_0000 {
                work.push(cur.wrapping_add(sext(((insn >> 5) & 0x7ffff) as u64, 19) << 2));
                work.push(next);
                break;
            } else if insn & 0x7E00_0000 == 0x3600_0000 {
                work.push(cur.wrapping_add(sext(((insn >> 5) & 0x3fff) as u64, 14) << 2));
                work.push(next);
                break;
            } else if emit::is_terminator(insn) {
                if insn & 0xFE00_0000 == 0xD600_0000 && (insn >> 21) & 0xf == 1 {
                    work.push(next); // BLR returns here
                }
                if insn & 0xFFE0_001F == 0xD400_0001 {
                    work.push(next); // SVC continues
                }
                break;
            }
            cur = next;
        }
    }
    out
}

fn build(img: &Image, profile: &BTreeSet<u64>) -> std::io::Result<()> {
    let read = crate::jit::guest_reader();
    let mut blocks: BTreeSet<u64> = profile.clone();
    if !img.entries.is_empty() {
        blocks.extend(discover(&img.entries, img.start, img.end, &read, 400_000));
    }
    // Profile-guided expansion: also follow direct branches from hot blocks.
    let hot: Vec<u64> = profile.iter().copied().collect();
    blocks.extend(discover(&hot, img.start, img.end, &read, profile.len() * 4 + 1000));
    let blocks: Vec<u64> = blocks.into_iter().filter(|&pc| read(pc).is_some()).collect();
    if blocks.is_empty() {
        return Ok(());
    }
    // Translate in chunks so a single assembler run stays manageable.
    let mut code: Vec<u8> = Vec::new();
    let mut index: Vec<(u64, u64)> = Vec::new();
    for chunk in blocks.chunks(4096) {
        let mut u = Unit::new(StubKind::Plain, crate::jit::commpage_checks());
        for &pc in chunk {
            let l = u.a.create_label();
            u.labels.insert(pc, l);
        }
        for &pc in chunk {
            emit::emit_block(&mut u, pc, &read);
        }
        u.finish();
        let base = code.len() as u64;
        let res = match u.a.assemble_options(base, BlockEncoderOptions::RETURN_NEW_INSTRUCTION_OFFSETS) {
            Ok(r) => r,
            Err(_) => continue,
        };
        for &pc in chunk {
            if let Ok(ip) = res.label_ip(&u.labels[&pc]) {
                index.push((pc, ip));
            }
        }
        code.extend_from_slice(&res.inner.code_buffer);
        while code.len() % 16 != 0 {
            code.push(0xcc);
        }
    }
    let path = file_for(img, "aot");
    // Several applications can finish simultaneously. Each builder writes its
    // own temporary file, then atomically replaces the complete cache.
    let tmp = path.with_extension(format!("aot.{}.tmp", std::process::id()));
    let mut f = std::fs::File::create(&tmp)?;
    let mut hdr = Vec::new();
    hdr.extend_from_slice(MAGIC);
    hdr.extend_from_slice(&TRANSLATOR_VERSION.to_le_bytes());
    hdr.extend_from_slice(&flags_word().to_le_bytes());
    hdr.extend_from_slice(&(index.len() as u64).to_le_bytes());
    hdr.extend_from_slice(&(code.len() as u64).to_le_bytes());
    for (pc, off) in &index {
        hdr.extend_from_slice(&pc.to_le_bytes());
        hdr.extend_from_slice(&off.to_le_bytes());
    }
    while hdr.len() < code_offset(index.len()) {
        hdr.push(0);
    }
    f.write_all(&hdr)?;
    f.write_all(&code)?;
    drop(f);
    std::fs::rename(tmp, path)?;
    Ok(())
}

/// `maclator --aot <exe>`: translate the main executable ahead of time
/// without running it.
pub fn build_standalone(path: &str) -> std::io::Result<usize> {
    let m = crate::macho::MachO::open_arm64(path, false)?;
    let (lo, hi) = m.vm_range();
    let file = std::fs::File::open(path)?;
    use std::os::unix::io::AsRawFd;
    let slide = crate::loader::MAIN_BASE.wrapping_sub(lo);
    // Map __TEXT read-only at the runtime address.
    for seg in &m.segments {
        if seg.name != "__TEXT" {
            continue;
        }
        let addr = seg.vmaddr.wrapping_add(slide);
        let p = unsafe {
            libc::mmap(addr as *mut _, seg.filesize as usize, libc::PROT_READ, libc::MAP_PRIVATE | libc::MAP_FIXED, file.as_raw_fd(), (m.slice_offset + seg.fileoff) as i64)
        };
        if p as u64 != addr {
            return Err(std::io::Error::last_os_error());
        }
    }
    let entries = function_starts(path, &m, slide);
    let img_start = lo.wrapping_add(slide);
    let img_end = hi.wrapping_add(slide);
    let key = format!("main-{}-{}", hex(&m.uuid), fnv(&[path.as_bytes()]));
    let img = Image { name: path.to_string(), key, start: img_start, end: img_end, entries, loaded: false, cached: false };
    let _ = std::fs::create_dir_all(cache_dir());
    let prof = read_profile(&file_for(&img, "profile"));
    build(&img, &prof)?;
    let n = load_count(&file_for(&img, "aot"));
    Ok(n)
}

fn load_count(p: &std::path::Path) -> usize {
    // This query only needs the header, not another read of the entire code.
    let Ok(mut f) = std::fs::File::open(p) else { return 0 };
    let mut head = [0u8; 32];
    if f.read_exact(&mut head).is_err() {
        return 0;
    }
    f.metadata().ok().and_then(|m| cache_layout(&head, m.len())).map(|(n, _, _)| n).unwrap_or(0)
}

/// Function entry points from LC_FUNCTION_STARTS (runtime addresses).
pub fn function_starts(path: &str, m: &crate::macho::MachO, slide: u64) -> Vec<u64> {
    let Some((off, size)) = m.function_starts else { return vec![] };
    let mut f = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return vec![],
    };
    use std::io::Seek;
    let mut buf = vec![0u8; size as usize];
    if f.seek(std::io::SeekFrom::Start(m.slice_offset + off as u64)).is_err() || f.read_exact(&mut buf).is_err() {
        return vec![];
    }
    crate::macho::decode_function_starts(&buf, m.text_vmaddr()).into_iter().map(|a| a.wrapping_add(slide)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(n: u64, code_len: u64) -> [u8; 32] {
        let mut h = [0u8; 32];
        h[..8].copy_from_slice(MAGIC);
        h[8..12].copy_from_slice(&TRANSLATOR_VERSION.to_le_bytes());
        h[12..16].copy_from_slice(&flags_word().to_le_bytes());
        h[16..24].copy_from_slice(&n.to_le_bytes());
        h[24..32].copy_from_slice(&code_len.to_le_bytes());
        h
    }

    #[test]
    fn cache_header_rejects_stale_truncated_and_oversized_files() {
        let h = header(20, 128);
        let expected_len = (code_offset(20) + 128) as u64;
        assert_eq!(cache_layout(&h, expected_len), Some((20, 128, code_offset(20))));
        assert_eq!(cache_layout(&h, expected_len - 1), None);
        assert_eq!(cache_layout(&header(u64::MAX, 128), u64::MAX), None);
        assert_eq!(cache_layout(&header(20, u64::MAX), u64::MAX), None);
        assert_eq!(cache_layout(&header(0, 128), expected_len), None);
        let mut stale = h;
        stale[8..12].copy_from_slice(&(TRANSLATOR_VERSION - 1).to_le_bytes());
        assert_eq!(cache_layout(&stale, expected_len), None);
        let mut different_features = h;
        different_features[12..16].copy_from_slice(&(flags_word() ^ 2).to_le_bytes());
        assert_eq!(cache_layout(&different_features, expected_len), None);
        different_features[12..16].copy_from_slice(&(flags_word() ^ 4).to_le_bytes());
        assert_eq!(cache_layout(&different_features, expected_len), None);
        different_features[12..16].copy_from_slice(&(flags_word() ^ 8).to_le_bytes());
        assert_eq!(cache_layout(&different_features, expected_len), None);
        different_features[12..16].copy_from_slice(&(flags_word() ^ 16).to_le_bytes());
        assert_eq!(cache_layout(&different_features, expected_len), None);
        different_features[12..16].copy_from_slice(&(flags_word() ^ (1 << 11)).to_le_bytes());
        assert_eq!(cache_layout(&different_features, expected_len), None);
    }

    /// An arm64 MH_DYLIB header with two load commands (96 bytes): LC_UUID with `id` in every
    /// byte, then LC_SEGMENT_64 __TEXT from byte 56 to byte 128.
    fn dylib_header(id: u8) -> [u32; 32] {
        let u = u32::from_le_bytes([id; 4]);
        let mut w = [0u32; 32];
        w[..6].copy_from_slice(&[0xfeed_facf, 0x0100_000c, 0, 6, 2, 96]);
        w[8..14].copy_from_slice(&[0x1b, 24, u, u, u, u]);
        w[14..18].copy_from_slice(&[0x19, 72, u32::from_le_bytes(*b"__TE"), u32::from_le_bytes(*b"XT\0\0")]);
        w[22] = 0x4000; // vmsize
        w
    }

    /// Whether the image of `dylib_header(id)` is registered with its header at `addr`.
    /// (Registering one reads the cache directory for its AOT file, and never writes there.)
    fn registered(id: u8, addr: u64) -> bool {
        IMAGES.lock().unwrap().iter().any(|i| i.key == format!("lib-{}-{addr:x}", hex(&[id; 16])))
    }

    /// Register `w` placed 0x100 bytes before a page that cannot be read, by a mapping that runs on
    /// into that page, and return where the header was.
    fn register_before_unreadable_page(w: &[u32; 32]) -> u64 {
        let pg = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
        let m = unsafe { libc::mmap(std::ptr::null_mut(), 2 * pg, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_ANON | libc::MAP_PRIVATE, -1, 0) };
        assert_ne!(m, libc::MAP_FAILED);
        let at = unsafe { m.cast::<u8>().add(pg - 0x100) };
        unsafe { std::ptr::copy_nonoverlapping(w.as_ptr(), at.cast::<u32>(), w.len()) };
        assert_eq!(unsafe { libc::mprotect(m.cast::<u8>().add(pg).cast(), pg, libc::PROT_NONE) }, 0);
        register_image_at(at as u64, pg as u64 + 0x100);
        unsafe { libc::munmap(m, 2 * pg) };
        at as u64
    }

    #[test]
    fn image_header_is_read_only_within_the_mapping() {
        let w = dylib_header(0x5a);
        // A mapping that ends at byte 120 cuts the segment command short: the rest is not the image's.
        register_image_at(w.as_ptr() as u64, 120);
        assert!(!registered(0x5a, w.as_ptr() as u64));
        register_image_at(w.as_ptr() as u64, 128);
        assert!(registered(0x5a, w.as_ptr() as u64));
    }

    #[test]
    fn unreadable_load_commands_do_not_fault() {
        // A header claiming 0x3100 bytes of load commands, which run into the unreadable page
        // whatever the page size: registering it must return instead of faulting, and register
        // nothing.
        let mut w = dylib_header(0x6b);
        w[5] = 0x3100; // sizeofcmds
        assert!(!registered(0x6b, register_before_unreadable_page(&w)));
    }

    #[test]
    fn load_commands_are_read_only_up_to_sizeofcmds() {
        // Load commands that end before the unreadable page: the image is registered, though the
        // mapping runs on into that page.
        assert!(registered(0x3c, register_before_unreadable_page(&dylib_header(0x3c))));
    }

    #[test]
    fn short_load_commands_are_not_used() {
        // A last load command too short for its fields (an 8-byte LC_UUID or LC_SEGMENT_64), with
        // 16 more bytes of sizeofcmds after it: the image is not registered, whatever the missing
        // fields would be read as.
        for cmd in [0x1b, 0x19] {
            let mut w = [0u32; 38];
            w[..32].copy_from_slice(&dylib_header(0x2d));
            w[4..6].copy_from_slice(&[3, 120]); // ncmds, sizeofcmds
            w[32..].copy_from_slice(&[cmd, 8, 0x2d2d_2d2d, 0x2d2d_2d2d, 0x2d2d_2d2d, 0x2d2d_2d2d]);
            register_image_at(w.as_ptr() as u64, 152);
            assert!(!registered(0x2d, w.as_ptr() as u64), "cmd {cmd:#x}");
        }
    }

    #[test]
    fn load_rejects_invalid_index_before_publishing_code() {
        let p = std::env::temp_dir().join(format!("maclator-invalid-aot-{}", std::process::id()));
        let mut bytes = header(1, 16).to_vec();
        bytes.extend_from_slice(&0x1000u64.to_le_bytes());
        bytes.extend_from_slice(&16u64.to_le_bytes()); // exactly past the code
        bytes.resize(code_offset(1) + 16, 0);
        std::fs::write(&p, bytes).unwrap();
        assert_eq!(load_file(&p, 0x1000, 0x2000).unwrap(), 0);
        std::fs::remove_file(p).unwrap();
    }
}
