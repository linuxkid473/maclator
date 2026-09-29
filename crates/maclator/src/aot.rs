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
const TRANSLATOR_VERSION: u32 = 15;

#[derive(Clone)]
struct Image {
    name: String,
    key: String,
    start: u64,
    end: u64,
    /// Static entry points (function starts), if known.
    entries: Vec<u64>,
    loaded: bool,
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
    crate::jit::commpage_checks() as u32
}

/// Register the main executable (mapped at a fixed address).
pub fn register_main(path: &str, uuid: [u8; 16], start: u64, end: u64, entries: Vec<u64>) {
    let key = format!("main-{}-{}", hex(&uuid), fnv(&[path.as_bytes()]));
    add_image(Image { name: path.rsplit('/').next().unwrap_or(path).to_string(), key, start, end, entries, loaded: false });
}

/// Register dyld (also mapped at a fixed address).
pub fn register_dyld(path: &str, uuid: [u8; 16], start: u64, end: u64) {
    let key = format!("dyld-{}-{}", hex(&uuid), fnv(&[path.as_bytes()]));
    add_image(Image { name: "dyld".into(), key, start, end, entries: vec![], loaded: false });
}

/// Register the shared cache once dyld has had us map it.
pub fn register_cache(uuid: [u8; 16], start: u64, end: u64) {
    let key = format!("dsc-{}", hex(&uuid));
    add_image(Image { name: "dyld_shared_cache".into(), key, start, end, entries: vec![], loaded: false });
    ensure_loaded();
}

/// Register the arm64 Mach-O whose header is mapped at `addr` (a dyld-loaded library or
/// helper image). Keyed by UUID and load address, so it only hits the cache when the
/// library loads at the same place again.
pub fn register_image_at(addr: u64) {
    if !ENABLED.load(Ordering::Relaxed) {
        return;
    }
    let rd = |o: u64| unsafe { std::ptr::read_unaligned((addr + o) as *const u32) };
    // mach_header_64: magic, cputype, ..., ncmds at +16, sizeofcmds at +20
    if rd(0) != 0xfeed_facf || rd(4) != 0x0100_000c {
        if std::env::var_os("MACLATOR_STATS").is_some() {
            eprintln!("[maclator] aot: mmap exec at {addr:#x} not an arm64 mach-o ({:#x} {:#x})", rd(0), rd(4));
        }
        return;
    }
    let ncmds = rd(16);
    let mut off = 32u64;
    let mut uuid = [0u8; 16];
    let (mut text_vm, mut lo, mut hi) = (u64::MAX, u64::MAX, 0u64);
    for _ in 0..ncmds.min(512) {
        let cmd = rd(off);
        let size = rd(off + 4) as u64;
        if size < 8 || off + size > 0x4000 {
            return;
        }
        if cmd == 0x1b {
            for i in 0..16 {
                uuid[i] = unsafe { *((addr + off + 8 + i as u64) as *const u8) };
            }
        } else if cmd == 0x19 {
            let mut name = [0u8; 16];
            for i in 0..16 {
                name[i] = unsafe { *((addr + off + 8 + i as u64) as *const u8) };
            }
            let vmaddr = unsafe { std::ptr::read_unaligned((addr + off + 24) as *const u64) };
            let vmsize = unsafe { std::ptr::read_unaligned((addr + off + 32) as *const u64) };
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
    add_image(Image { name: format!("lib-{}", &hex(&uuid)[..8]), key, start, end, entries: vec![], loaded: false });
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
        match load_file(&file_for(img, "aot")) {
            Ok(n) if n > 0 => {
                if std::env::var_os("MACLATOR_TRACE").is_some() || std::env::var_os("MACLATOR_STATS").is_some() {
                    eprintln!("[maclator] aot: loaded {n} blocks for {}", img.name);
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

fn load_file(path: &std::path::Path) -> std::io::Result<usize> {
    let mut f = std::fs::File::open(path)?;
    let mut head = [0u8; 32];
    if f.read_exact(&mut head).is_err() || &head[..8] != MAGIC {
        return Ok(0);
    }
    let rd32 = |b: &[u8], o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
    let rd64 = |b: &[u8], o: usize| u64::from_le_bytes(b[o..o + 8].try_into().unwrap());
    if rd32(&head, 8) != TRANSLATOR_VERSION || rd32(&head, 12) != flags_word() {
        return Ok(0);
    }
    let n = rd64(&head, 16) as usize;
    let code_len = rd64(&head, 24) as usize;
    let code_off = code_offset(n);
    let mut idx = vec![0u8; n * 16];
    f.read_exact(&mut idx)?;
    if f.metadata()?.len() < (code_off + code_len) as u64 {
        return Ok(0);
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
    for i in 0..n {
        let pc = rd64(&idx, i * 16);
        let off = rd64(&idx, i * 16 + 8);
        crate::jit::register_block(pc, base + off);
    }
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
        let aot_missing = !file_for(img, "aot").exists();
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
    let tmp = path.with_extension("aot.tmp");
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
    let img = Image { name: path.to_string(), key, start: img_start, end: img_end, entries, loaded: false };
    let _ = std::fs::create_dir_all(cache_dir());
    let prof = read_profile(&file_for(&img, "profile"));
    build(&img, &prof)?;
    let n = load_count(&file_for(&img, "aot"));
    Ok(n)
}

fn load_count(p: &std::path::Path) -> usize {
    std::fs::read(p).ok().filter(|d| d.len() >= 32).map(|d| u64::from_le_bytes(d[16..24].try_into().unwrap()) as usize).unwrap_or(0)
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
