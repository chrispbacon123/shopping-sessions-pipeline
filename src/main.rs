// Shopping Sessions pipeline v2: single Rust binary, exact rules.md implementation.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use memchr::{memchr, memchr3};
use memmap2::Mmap;
use rayon::prelude::*;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, OnceLock};
use std::time::Instant;
use xxhash_rust::xxh3::{xxh3_128, xxh3_64};

const DAY: i64 = 86_400_000;
const WEEK: i64 = 7 * DAY;
const WEEKU: u32 = WEEK as u32;
const NBBITS: u32 = 9;
const NB: usize = 1 << NBBITS;
type S = &'static [u8];

static T_GATHER: AtomicU64 = AtomicU64::new(0);
static T_SORT: AtomicU64 = AtomicU64::new(0);
static T_PROC: AtomicU64 = AtomicU64::new(0);
static FILES: OnceLock<Vec<S>> = OnceLock::new();
#[inline(always)]
fn files() -> &'static [S] { FILES.get().unwrap() }
/// Value of the field starting at encoded offset (file<<40 | pos); stops at , \n " or EOF.
#[inline]
fn field_at(off: u64) -> S {
    let f = files()[(off >> 40) as usize];
    let p = (off & ((1u64 << 40) - 1)) as usize;
    let r = &f[p..];
    match memchr3(b',', b'\n', b'"', r) { Some(e) => &r[..e], None => r }
}

#[inline(always)]
fn hs(b: &[u8]) -> u64 { let h = xxh3_64(b); if h == 0 { 1 } else { h } }
#[inline(always)]
fn bucket(h: u64) -> usize { (h >> (64 - NBBITS)) as usize }

// ---------- CSV ----------
#[inline(always)]
fn parse_line<const N: usize>(line: S, out: &mut [S; N]) -> bool {
    let mut start = 0usize;
    let mut k = 0usize;
    let mut has_quote = false;
    for (i, &c) in line.iter().enumerate() {
        if c == b',' {
            if k + 1 >= N { return false; }
            out[k] = &line[start..i];
            k += 1;
            start = i + 1;
        } else if c == b'"' {
            has_quote = true;
        }
    }
    if k != N - 1 { return false; }
    out[N - 1] = &line[start..];
    if has_quote {
        for f in out.iter_mut() {
            let b = *f;
            if !b.is_empty() && b[0] == b'"' {
                if b.len() < 2 || b[b.len() - 1] != b'"' { return false; }
                let inner = &b[1..b.len() - 1];
                if memchr(b'"', inner).is_some() { return false; }
                *f = inner;
            } else if memchr(b'"', b).is_some() {
                return false;
            }
        }
    }
    true
}

// ---------- timestamps ----------
#[inline(always)]
fn d2(b: &[u8], i: usize) -> Option<i64> {
    let a = b[i].wrapping_sub(b'0');
    let c = b[i + 1].wrapping_sub(b'0');
    if a > 9 || c > 9 { return None; }
    Some((a as i64) * 10 + c as i64)
}
#[inline]
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}
#[inline]
fn is_leap(y: i64) -> bool { (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 }

static YEAR_DAYS: OnceLock<Vec<i64>> = OnceLock::new();
const CUM: [i64; 13] = [0, 0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334];
#[inline(always)]
fn civil_days(y: i64, m: i64, d: i64) -> i64 {
    if (1900..2200).contains(&y) {
        let t = YEAR_DAYS.get_or_init(|| (1900..2200).map(|y| days_from_civil(y, 1, 1)).collect());
        t[(y - 1900) as usize] + CUM[m as usize] + if m > 2 && is_leap(y) { 1 } else { 0 } + d - 1
    } else { days_from_civil(y, m, d) }
}
#[inline]
fn parse_ts(b: &[u8]) -> Option<i64> {
    let n = b.len();
    if n == 0 { return None; }
    if n != 20 && n != 25 || b[4] != b'-' {
        if !b.iter().all(|c| c.is_ascii_digit()) { return None; }
        let mut i = 0;
        while i < n && b[i] == b'0' { i += 1; }
        let r = &b[i..];
        if r.len() > 18 { return Some(i64::MAX); }
        let mut v: i64 = 0;
        for &c in r { v = v * 10 + (c - b'0') as i64; }
        return Some(v);
    }
    if b[7] != b'-' || b[10] != b'T' || b[13] != b':' || b[16] != b':' { return None; }
    let g = |i: usize| b[i].wrapping_sub(b'0');
    let ds = [g(0), g(1), g(2), g(3), g(5), g(6), g(8), g(9), g(11), g(12), g(14), g(15), g(17), g(18)];
    if ds.iter().fold(0u8, |a, &x| a | (x > 9) as u8) != 0 { return None; }
    let v2 = |i: usize| (ds[i] as i64) * 10 + ds[i + 1] as i64;
    let y = v2(0) * 100 + v2(2);
    let mo = v2(4);
    let d = v2(6);
    let hh = v2(8);
    let mi = v2(10);
    let ss = v2(12);
    if !(1..=12).contains(&mo) || d < 1 || hh > 23 || mi > 59 || ss > 59 { return None; }
    let dim = match mo { 1 | 3 | 5 | 7 | 8 | 10 | 12 => 31, 4 | 6 | 9 | 11 => 30, _ => if is_leap(y) { 29 } else { 28 } };
    if d > dim { return None; }
    let mut secs = civil_days(y, mo, d) * 86400 + hh * 3600 + mi * 60 + ss;
    if n == 20 {
        if b[19] != b'Z' { return None; }
    } else {
        let sign = match b[19] { b'+' => 1, b'-' => -1, _ => return None };
        if b[22] != b':' { return None; }
        let oh = d2(b, 20)?;
        let om = d2(b, 23)?;
        if oh > 23 || om > 59 { return None; }
        secs -= sign * (oh * 3600 + om * 60);
    }
    Some(secs * 1000)
}

/// Fast path for well-formed F1 (ingest_ts is always valid F1 per rules); None -> use parse_ts.
#[inline(always)]
fn parse_f1_fast(b: &[u8]) -> Option<i64> {
    if b.len() != 20 || b[19] != b'Z' || b[10] != b'T' || b[4] != b'-' || b[7] != b'-' || b[13] != b':' || b[16] != b':' { return None; }
    let g = |i: usize| (b[i].wrapping_sub(b'0')) as i64;
    let y = g(0) * 1000 + g(1) * 100 + g(2) * 10 + g(3);
    let mo = g(5) * 10 + g(6);
    let d = g(8) * 10 + g(9);
    if !(1900..2200).contains(&y) || !(1..=12).contains(&mo) || !(1..=28).contains(&d) { return None; }
    let hh = g(11) * 10 + g(12);
    let mi = g(14) * 10 + g(15);
    let ss = g(17) * 10 + g(18);
    if hh > 23 || mi > 59 || ss > 59 || [1usize,2,3,5,6,8,9,11,12,14,15,17,18].iter().any(|&i| b[i].wrapping_sub(b'0') > 9) { return None; }
    Some((civil_days(y, mo, d) * 86400 + hh * 3600 + mi * 60 + ss) * 1000)
}

#[inline]
fn parse_price(b: &[u8]) -> Option<u32> {
    if b.is_empty() || !b.iter().all(|c| c.is_ascii_digit()) { return None; }
    let mut i = 0;
    while i < b.len() && b[i] == b'0' { i += 1; }
    let r = &b[i..];
    if r.len() > 8 { return None; }
    let mut v: u32 = 0;
    for &c in r { v = v * 10 + (c - b'0') as u32; }
    if (1..=10_000_000).contains(&v) { Some(v) } else { None }
}

// ---------- prefix-number lookup tables (p123 / c_123) with string fallback ----------
struct Lookup { prefix: &'static [u8], dense: Vec<u16>, other: std::collections::HashMap<S, u16> }
impl Lookup {
    fn new(prefix: &'static [u8]) -> Self { Lookup { prefix, dense: Vec::new(), other: Default::default() } }
    #[inline]
    fn canon(&self, b: &[u8]) -> Option<usize> {
        let p = self.prefix.len();
        if b.len() <= p || b.len() > p + 7 || &b[..p] != self.prefix { return None; }
        let r = &b[p..];
        if r[0] == b'0' { return None; }
        let mut v = 0usize;
        for &c in r { let d = c.wrapping_sub(b'0'); if d > 9 { return None; } v = v * 10 + d as usize; }
        Some(v)
    }
    fn insert(&mut self, k: S, v: u16) {
        if let Some(n) = self.canon(k) {
            if n < 4_000_000 {
                if self.dense.len() <= n { self.dense.resize(n + 1, u16::MAX); }
                self.dense[n] = v;
                return;
            }
        }
        self.other.insert(k, v);
    }
    #[inline]
    fn get(&self, k: &[u8]) -> Option<u16> {
        if let Some(n) = self.canon(k) {
            if n < 4_000_000 {
                return match self.dense.get(n) { Some(&v) if v != u16::MAX => Some(v), _ => None };
            }
        }
        if self.other.is_empty() { return None; }
        self.other.get(k).copied()
    }
}

// ---------- files & chunks ----------
fn map_file(p: &Path) -> S {
    let f = File::open(p).unwrap_or_else(|e| panic!("open {:?}: {}", p, e));
    let len = f.metadata().unwrap().len();
    if len == 0 { return &[]; }
    let m = unsafe { memmap2::MmapOptions::new().map(&f).unwrap() };
    let _ = m.advise(memmap2::Advice::WillNeed);
    let m: &'static Mmap = Box::leak(Box::new(m));
    &m[..]
}
fn list_csv(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = match std::fs::read_dir(dir) {
        Ok(rd) => rd.filter_map(|e| e.ok()).map(|e| e.path())
            .filter(|p| p.extension().map(|x| x == "csv").unwrap_or(false) && p.is_file()).collect(),
        Err(_) => vec![],
    };
    v.sort();
    v
}
#[derive(Clone, Copy)]
struct Chunk { fi: usize, start: usize, end: usize }
impl Chunk {
    #[inline] fn data(&self) -> S { &files()[self.fi][self.start..self.end] }
    #[inline] fn off(&self, field: S) -> u64 { ((self.fi as u64) << 40) | (field.as_ptr() as usize - files()[self.fi].as_ptr() as usize) as u64 }
}
fn chunks(fis: &[usize], target: usize) -> Vec<Chunk> {
    let mut out = Vec::new();
    for &fi in fis {
        let f = files()[fi];
        let body_start = match memchr(b'\n', f) { Some(i) => i + 1, None => continue };
        let mut s = body_start;
        while s < f.len() {
            let mut e = (s + target).min(f.len());
            if e < f.len() {
                e = match memchr(b'\n', &f[e..]) { Some(i) => e + i + 1, None => f.len() };
            }
            out.push(Chunk { fi, start: s, end: e });
            s = e;
        }
    }
    out
}
#[inline(always)]
fn for_lines(chunk: S, mut f: impl FnMut(S)) {
    let mut s = 0;
    let n = chunk.len();
    while s < n {
        let e = match memchr(b'\n', &chunk[s..]) { Some(i) => s + i, None => n };
        f(&chunk[s..e]);
        s = e + 1;
    }
}


/// Bitmasks of ',', '\n', '"' over 64 bytes at p.
#[inline(always)]
fn masks64(p: &[u8; 64]) -> (u64, u64, u64) {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        use core::arch::x86_64::*;
        let a = _mm256_loadu_si256(p.as_ptr() as *const __m256i);
        let b = _mm256_loadu_si256(p.as_ptr().add(32) as *const __m256i);
        let m = |v: __m256i, c: i8| -> u32 { _mm256_movemask_epi8(_mm256_cmpeq_epi8(v, _mm256_set1_epi8(c))) as u32 };
        let cm = (m(a, b',' as i8) as u64) | ((m(b, b',' as i8) as u64) << 32);
        let nl = (m(a, b'\n' as i8) as u64) | ((m(b, b'\n' as i8) as u64) << 32);
        let qt = (m(a, b'"' as i8) as u64) | ((m(b, b'"' as i8) as u64) << 32);
        (cm, nl, qt)
    }
}

#[inline(always)]
fn mask2(p: *const u8) -> (u64, u64) {
    unsafe {
        use core::arch::x86_64::*;
        let a = _mm256_loadu_si256(p as *const __m256i);
        let b = _mm256_loadu_si256(p.add(32) as *const __m256i);
        let m = |v: __m256i, c: i8| -> u32 { _mm256_movemask_epi8(_mm256_cmpeq_epi8(v, _mm256_set1_epi8(c))) as u32 };
        ((m(a, b',' as i8) as u64) | ((m(b, b',' as i8) as u64) << 32),
         (m(a, b'"' as i8) as u64) | ((m(b, b'"' as i8) as u64) << 32))
    }
}

#[inline(always)]
fn mask3(p: *const u8) -> (u64, u64, u64) {
    unsafe {
        use core::arch::x86_64::*;
        let a = _mm256_loadu_si256(p as *const __m256i);
        let b = _mm256_loadu_si256(p.add(32) as *const __m256i);
        let m = |v: __m256i, c: i8| -> u32 { _mm256_movemask_epi8(_mm256_cmpeq_epi8(v, _mm256_set1_epi8(c))) as u32 };
        ((m(a, b',' as i8) as u64) | ((m(b, b',' as i8) as u64) << 32),
         (m(a, b'"' as i8) as u64) | ((m(b, b'"' as i8) as u64) << 32),
         (m(a, b'\n' as i8) as u64) | ((m(b, b'\n' as i8) as u64) << 32))
    }
}

#[inline(always)]
fn fix_quotes<const N: usize>(out: &mut [S; N]) -> bool {
    for fl in out.iter_mut() {
        let b = *fl;
        if !b.is_empty() && b[0] == b'"' {
            if b.len() < 2 || b[b.len() - 1] != b'"' { return false; }
            let inner = &b[1..b.len() - 1];
            if memchr(b'"', inner).is_some() { return false; }
            *fl = inner;
        } else if memchr(b'"', b).is_some() {
            return false;
        }
    }
    true
}

/// Parses every line of a chunk into exactly N fields (rules.md dialect), calling f for each well-formed line.
/// `safe_end`: bytes of the underlying file available after chunk end (for 64-byte over-reads).
#[inline(always)]
fn for_records<const N: usize>(chunk: S, mut f: impl FnMut(&[S; N])) {
    let mut out: [S; N] = [&[]; N];
    let n = chunk.len();
    let mut s = 0usize;
    let mut pad = [0u8; 128];
    while s < n {
        let avail = n - s;
        let p = if avail >= 128 { unsafe { chunk.as_ptr().add(s) } } else { pad = [0u8; 128]; pad[..avail].copy_from_slice(&chunk[s..]); pad.as_ptr() };
        let (c0, q0, n0) = mask3(p);
        let (len, c1, q1) = if n0 != 0 { (n0.trailing_zeros() as usize, 0u64, 0u64) } else {
            let (c1, q1, n1) = mask3(unsafe { p.add(64) });
            if n1 != 0 { (64 + n1.trailing_zeros() as usize, c1, q1) } else if avail <= 128 { (avail, c1, q1) } else { (usize::MAX, 0, 0) }
        };
        if len == usize::MAX {
            let e = match memchr(b'\n', &chunk[s..]) { Some(i) => s + i, None => n };
            let line = &chunk[s..e];
            let mut k = 0usize;
            let mut st = 0usize;
            let mut ok = true;
            for (i, &c) in line.iter().enumerate() {
                if c == b',' {
                    if k + 1 >= N { ok = false; break; }
                    out[k] = &line[st..i];
                    k += 1;
                    st = i + 1;
                }
            }
            if ok && k == N - 1 {
                out[N - 1] = &line[st..];
                if memchr(b'"', line).is_none() || fix_quotes(&mut out) { f(&out); }
            }
            s = e + 1;
            continue;
        }
        let e = s + len;
        let line = unsafe { chunk.get_unchecked(s..e) };
        if len > 0 {
            let lm: u128 = if len >= 128 { u128::MAX } else { (1u128 << len) - 1 };
            let mut cm = ((c0 as u128) | ((c1 as u128) << 64)) & lm;
            let qt = ((q0 as u128) | ((q1 as u128) << 64)) & lm;
            if cm.count_ones() as usize == N - 1 {
                let mut st = 0usize;
                if qt == 0 {
                    for k in 0..N - 1 {
                        let t = cm.trailing_zeros() as usize;
                        out[k] = unsafe { line.get_unchecked(st..t) };
                        st = t + 1;
                        cm &= cm - 1;
                    }
                    out[N - 1] = unsafe { line.get_unchecked(st..len) };
                    f(&out);
                } else {
                    let mut bad = false;
                    let mut cnt = 0u32;
                    let qb = |i: usize| -> bool { ((qt >> i) as u64 & 1) != 0 };
                    for k in 0..N {
                        let t = if k + 1 < N { cm.trailing_zeros() as usize } else { len };
                        out[k] = unsafe { line.get_unchecked(st..t) };
                        if t > st {
                            let a = qb(st);
                            let b = qb(t - 1);
                            if a | b {
                                bad |= (a != b) | (t - st < 2);
                                cnt += 2;
                                out[k] = unsafe { line.get_unchecked(st + 1..t.max(st + 1) - 1 + (t - st < 2) as usize) };
                            }
                        }
                        st = t + 1;
                        if k + 1 < N { cm &= cm - 1; }
                    }
                    let ok = !bad && cnt == qt.count_ones();
                    if ok { f(&out); }
                }
            }
        }
        s = e + 1;
    }
}

fn scatter_b<T: Copy>(v: &[T], bk: &[u16]) -> (Vec<T>, Vec<u32>) {
    let mut cnt = vec![0u32; NB + 1];
    for &b in bk { cnt[b as usize + 1] += 1; }
    for i in 0..NB { cnt[i + 1] += cnt[i]; }
    let mut pos = cnt.clone();
    let mut out: Vec<T> = Vec::with_capacity(v.len());
    unsafe {
        out.set_len(v.len());
        let p = out.as_mut_ptr();
        for (x, &b) in v.iter().zip(bk) { let b = b as usize; *p.add(pos[b] as usize) = *x; pos[b] += 1; }
    }
    (out, cnt)
}
/// Counting-sort scatter of items into NB buckets: returns (sorted, offsets[NB+1]).
fn scatter<T: Copy>(v: &[T], key: impl Fn(&T) -> usize) -> (Vec<T>, Vec<u32>) {
    let mut cnt = vec![0u32; NB + 1];
    for x in v { cnt[key(x) + 1] += 1; }
    for i in 0..NB { cnt[i + 1] += cnt[i]; }
    let mut pos = cnt.clone();
    let mut out: Vec<T> = Vec::with_capacity(v.len());
    unsafe {
        out.set_len(v.len());
        let p = out.as_mut_ptr();
        for x in v { let b = key(x); *p.add(pos[b] as usize) = *x; pos[b] += 1; }
    }
    (out, cnt)
}

// ---------- records ----------
#[derive(Clone, Copy)]
struct KeyRec { eh: u64, ingest: u32, rid: u32 }
#[derive(Clone, Copy)]
struct Ev { pk: u64, ts: u32, rid: u32, pc: u32, name: u32 } // pc = price | cat<<24; name: bit31 => people slot idx, else offset in chunk
#[derive(Clone, Copy)]
struct Cl { pk: u64, cid: u64, ts: u32, camp: u16 }

struct URow { name: u64, day: i32, sessions: u32, events: u32, rev: u64, top: i32 }

// ---------- people: concurrent open-addressing table + lock-free union-find ----------
struct Slot { key: AtomicU64, off: AtomicU64, pk: AtomicU64 }
struct People { slots: Vec<Slot>, mask: usize, look: Vec<(u64, u64)> }

fn zeroed_vec<T>(n: usize) -> Vec<T> {
    // T must be valid when all-zero (atomics of ints)
    unsafe {
        let layout = std::alloc::Layout::array::<T>(n.max(1)).unwrap();
        let p = std::alloc::alloc_zeroed(layout) as *mut T;
        Vec::from_raw_parts(p, n, n.max(1))
    }
}

impl People {
    #[inline(always)]
    fn insert(&self, h: u64, off: u64) -> u32 {
        let mut i = h as usize & self.mask;
        loop {
            let s = &self.slots[i];
            let k = s.key.load(Relaxed);
            if k == h { return i as u32; }
            if k == 0 {
                match s.key.compare_exchange(0, h, Relaxed, Relaxed) {
                    Ok(_) => { s.off.store(off, Relaxed); return i as u32; }
                    Err(k2) => if k2 == h { return i as u32; },
                }
            }
            i = (i + 1) & self.mask;
        }
    }
    #[inline(always)]
    fn prefetch(&self, h: u64) {
        #[cfg(target_arch = "x86_64")]
        unsafe {
            let p = self.look.as_ptr().add(h as usize & self.mask) as *const i8;
            core::arch::x86_64::_mm_prefetch(p, core::arch::x86_64::_MM_HINT_T0);
        }
    }
    #[inline(always)]
    fn get_idx(&self, h: u64) -> Option<u32> {
        let mut i = h as usize & self.mask;
        loop {
            let k = self.slots[i].key.load(Relaxed);
            if k == h { return Some(i as u32); }
            if k == 0 { return None; }
            i = (i + 1) & self.mask;
        }
    }
    /// Pipelined lookup: for each item, key h -> Some((pk, slot idx)).
    #[inline(always)]
    fn resolve_all<T>(&self, items: &mut [T], key: impl Fn(&T) -> u64, mut set: impl FnMut(&mut T, u64, u32)) {
        const D: usize = 24;
        let n = items.len();
        for i in 0..n.min(D) { self.prefetch(key(&items[i])); }
        for i in 0..n {
            if i + D < n { self.prefetch(key(&items[i + D])); }
            let h = key(&items[i]);
            let mut q = h as usize & self.mask;
            loop {
                let (k, pk) = unsafe { *self.look.get_unchecked(q) };
                if k == h { set(&mut items[i], pk, q as u32); break; }
                if k == 0 { break; }
                q = (q + 1) & self.mask;
            }
        }
    }
    /// Returns (person key, name offset) or None if the ID is in no link.
    #[inline(always)]
    fn get(&self, h: u64) -> Option<(u64, u64)> {
        let mut i = h as usize & self.mask;
        loop {
            let s = &self.slots[i];
            let k = s.key.load(Relaxed);
            if k == h { return Some((s.pk.load(Relaxed), s.off.load(Relaxed))); }
            if k == 0 { return None; }
            i = (i + 1) & self.mask;
        }
    }
}

fn build_people(link_fis: &[usize]) -> People {
    let ch = chunks(link_fis, 1 << 20);
    let pairs: Vec<Vec<(u64, u64, u64, u64)>> = ch.par_iter().map(|c| {
        let d = c.data();
        let mut v = Vec::with_capacity(d.len() / 20);
        for_records::<2>(d, |f| {
            if f[0].is_empty() || f[1].is_empty() { return; }
            v.push((hs(f[0]), c.off(f[0]), hs(f[1]), c.off(f[1])));
        });
        v
    }).collect();
    let total: usize = pairs.iter().map(|v| v.len()).sum();
    let cap = (total * 5 / 2 + 16).next_power_of_two();
    let mut ppl = People { slots: zeroed_vec(cap), mask: cap - 1, look: Vec::new() };
    let parent: Vec<AtomicU32> = zeroed_vec(cap); // 0 = root, else parent+1
    let find = |mut x: u32| -> u32 {
        loop {
            let p = parent[x as usize].load(Relaxed);
            if p == 0 { return x; }
            let pp = parent[(p - 1) as usize].load(Relaxed);
            if pp == 0 { return p - 1; }
            parent[x as usize].store(pp, Relaxed);
            x = pp - 1;
        }
    };
    pairs.par_iter().for_each(|v| {
        for &(ha, oa, hb, ob) in v {
            let mut a = ppl.insert(ha, oa);
            let mut b = ppl.insert(hb, ob);
            loop {
                a = find(a);
                b = find(b);
                if a == b { break; }
                if a < b { std::mem::swap(&mut a, &mut b); }
                // link larger index a under smaller b
                if parent[a as usize].compare_exchange(0, b + 1, Relaxed, Relaxed).is_ok() { break; }
            }
        }
    });
    // smallest name per root: use root slot's pk field as best (off+1)
    let used = |i: usize| ppl.slots[i].key.load(Relaxed) != 0;
    (0..cap).into_par_iter().with_min_len(4096).for_each(|i| {
        if !used(i) { return; }
        let r = find(i as u32) as usize;
        let my = ppl.slots[i].off.load(Relaxed);
        let best = &ppl.slots[r].pk;
        let mut cur = best.load(Relaxed);
        loop {
            if cur != 0 && field_at(cur - 1) <= field_at(my) { break; }
            match best.compare_exchange(cur, my + 1, Relaxed, Relaxed) { Ok(_) => break, Err(c) => cur = c }
        }
    });
    (0..cap).into_par_iter().with_min_len(4096).for_each(|i| {
        if !used(i) || parent[i].load(Relaxed) != 0 { return; }
        let s = &ppl.slots[i];
        let off = s.pk.load(Relaxed) - 1;
        s.off.store(off, Relaxed);
        s.pk.store(hs(field_at(off)), Relaxed);
    });
    (0..cap).into_par_iter().with_min_len(4096).for_each(|i| {
        if !used(i) || parent[i].load(Relaxed) == 0 { return; }
        let r = find(i as u32) as usize;
        let s = &ppl.slots[i];
        s.off.store(ppl.slots[r].off.load(Relaxed), Relaxed);
        s.pk.store(ppl.slots[r].pk.load(Relaxed), Relaxed);
    });
    let look: Vec<(u64, u64)> = ppl.slots.par_iter().map(|sl| (sl.key.load(Relaxed), sl.pk.load(Relaxed))).collect();
    ppl.look = look;
    ppl
}

struct Ctx<'a> { base: i64, ndays: usize, ws_day: i64, ncamp: usize, ncat: usize, people: &'a People, ech: &'a [Chunk], lbits: u32 }
impl<'a> Ctx<'a> {
    #[inline]
    fn resolve_name(&self, name: u32, rid: u32) -> u64 {
        if name >> 31 == 1 { self.people.slots[(name & 0x7FFF_FFFF) as usize].off.load(Relaxed) }
        else { let c = &self.ech[(rid >> self.lbits) as usize]; ((c.fi as u64) << 40) | (c.start as u64 + name as u64) }
    }
}

struct Acc { camp: Vec<[u64; 4]>, asst: Vec<u32>, evbuf: Vec<Ev>, clbuf: Vec<Cl>, rows: Vec<URow>, catsum: Vec<u64>, starts: Vec<u32>, sdays: Vec<u32>, tmp: Vec<(u16, u32)>,
    map: Vec<(u64, u32)>, pidx: Vec<u32>, cnt: Vec<u32>, estart: Vec<u32>, cstart: Vec<u32>, cpid: Vec<u32>, evs: Vec<Ev>, cls: Vec<Cl> }
impl Acc {
    fn new(c: &Ctx<'_>) -> Self {
        Acc { camp: vec![[0; 4]; (c.ncamp + 1) * c.ndays], asst: vec![0; (c.ncamp + 1) * c.ndays], evbuf: Vec::new(), clbuf: Vec::new(), rows: Vec::new(), catsum: vec![0; c.ncat], starts: Vec::new(), sdays: Vec::new(), tmp: Vec::new(),
            map: Vec::new(), pidx: Vec::new(), cnt: Vec::new(), estart: Vec::new(), cstart: Vec::new(), cpid: Vec::new(), evs: Vec::new(), cls: Vec::new() }
    }
}

fn process_bucket(c: &Ctx<'_>, evs_in: &mut Vec<Ev>, cls_in: &mut Vec<Cl>, acc: &mut Acc) {
    let t0 = Instant::now();
    // group by person with a local hash map (pk -> idx), counting-sort, then sort each person by ts
    let n_in = evs_in.len();
    let cap = (n_in * 2 + 64).next_power_of_two();
    let mask = cap - 1;
    acc.map.clear();
    acc.map.resize(cap, (0u64, u32::MAX));
    acc.pidx.clear();
    acc.pidx.reserve(n_in);
    let mut np: u32 = 0;
    acc.cnt.clear();
    for e in evs_in.iter() {
        let mut q = (e.pk.rotate_left(NBBITS) as usize) & mask;
        let id = loop {
            let m = &mut acc.map[q];
            if m.1 == u32::MAX { *m = (e.pk, np); np += 1; acc.cnt.push(0); break m.1; }
            if m.0 == e.pk { break m.1; }
            q = (q + 1) & mask;
        };
        acc.pidx.push(id);
        acc.cnt[id as usize] += 1;
    }
    let npu = np as usize;
    // prefix
    acc.estart.clear(); acc.estart.reserve(npu + 1);
    let mut run = 0u32;
    for &k in acc.cnt.iter() { acc.estart.push(run); run += k; }
    acc.estart.push(run);
    acc.evs.clear(); acc.evs.reserve(n_in);
    unsafe { acc.evs.set_len(n_in); }
    {
        let mut pos: Vec<u32> = acc.estart[..npu].to_vec();
        for (e, &id) in evs_in.iter().zip(acc.pidx.iter()) {
            let p = &mut pos[id as usize];
            acc.evs[*p as usize] = *e;
            *p += 1;
        }
    }
    // clicks: only persons with events matter
    acc.cpid.clear();
    for cc in acc.cnt.iter_mut() { *cc = 0; }
    for cl in cls_in.iter() {
        let mut q = (cl.pk.rotate_left(NBBITS) as usize) & mask;
        let id = loop {
            let m = &acc.map[q];
            if m.1 == u32::MAX { break u32::MAX; }
            if m.0 == cl.pk { break m.1; }
            q = (q + 1) & mask;
        };
        acc.cpid.push(id);
        if id != u32::MAX { acc.cnt[id as usize] += 1; }
    }
    acc.cstart.clear(); acc.cstart.reserve(npu + 1);
    let mut run = 0u32;
    for &k in acc.cnt.iter() { acc.cstart.push(run); run += k; }
    acc.cstart.push(run);
    let ncl = run as usize;
    acc.cls.clear(); acc.cls.reserve(ncl);
    unsafe { acc.cls.set_len(ncl); }
    {
        let mut pos: Vec<u32> = acc.cstart[..npu].to_vec();
        for (cl, &id) in cls_in.iter().zip(acc.cpid.iter()) {
            if id == u32::MAX { continue; }
            let p = &mut pos[id as usize];
            acc.cls[*p as usize] = *cl;
            *p += 1;
        }
    }
    T_SORT.fetch_add(t0.elapsed().as_nanos() as u64, Relaxed);
    let t1 = Instant::now();
    struct D(Instant); impl Drop for D { fn drop(&mut self) { T_PROC.fetch_add(self.0.elapsed().as_nanos() as u64, Relaxed); } }
    let _d = D(t1);
    let nd = c.ndays;
    let organic = c.ncamp;
    let day0 = c.ws_day;
    let mut evs = std::mem::take(&mut acc.evs);
    let mut cls = std::mem::take(&mut acc.cls);
    for pi in 0..npu {
        let (i, i1) = (acc.estart[pi] as usize, acc.estart[pi + 1] as usize);
        let (j, j1) = (acc.cstart[pi] as usize, acc.cstart[pi + 1] as usize);
        evs[i..i1].sort_unstable_by_key(|e| e.ts);
        if j1 - j > 1 {
            cls[j..j1].sort_unstable_by(|a, b| a.ts.cmp(&b.ts).then_with(|| if a.cid == b.cid { std::cmp::Ordering::Equal } else { field_at(b.cid).cmp(field_at(a.cid)) }));
        }
        let name = c.resolve_name(evs[i].name, evs[i].rid);
        let pe = &evs[i..i1];
        let pc = &cls[j..j1];
        acc.starts.clear();
        acc.sdays.clear();
        let mut cp = 0usize;
        let mut cur_day: i64 = i64::MIN;
        let (mut d_sess, mut d_ev, mut d_rev) = (0u32, 0u32, 0u64);
        let mut d_purch = false;
        let mut s_start: u32 = 0;
        let mut s_rev: u64 = 0;
        let mut s_purch = false;
        let mut s_day: usize = 0;
        let mut first = true;
        let mut prev_ts: u32 = 0;
        macro_rules! close_session { () => {{
            while cp < pc.len() && pc[cp].ts <= s_start { cp += 1; }
            let camp = if cp > 0 && s_start - pc[cp - 1].ts <= WEEKU { pc[cp - 1].camp as usize } else { organic };
            let a = &mut acc.camp[camp * nd + s_day];
            a[0] += 1;
            if s_purch { a[1] += 1; }
            a[2] += s_rev;
        }}}
        macro_rules! flush_day { () => {{
            if d_sess > 0 {
                let mut top = -1i32;
                if d_purch {
                    let mut bv = 0u64;
                    for (k, v) in acc.catsum.iter_mut().enumerate() {
                        if *v > bv { bv = *v; top = k as i32; }
                        *v = 0;
                    }
                }
                acc.rows.push(URow { name, day: cur_day as i32, sessions: d_sess, events: d_ev, rev: d_rev, top });
            }
        }}}
        for e in pe {
            if first || e.ts - prev_ts > 1_800_000 {
                if !first { close_session!(); }
                first = false;
                s_start = e.ts;
                s_rev = 0;
                s_purch = false;
                let day = (c.base + s_start as i64).div_euclid(DAY);
                s_day = (day - day0) as usize;
                acc.starts.push(s_start);
                acc.sdays.push(s_day as u32);
                if day != cur_day {
                    flush_day!();
                    cur_day = day;
                    d_sess = 0; d_ev = 0; d_rev = 0; d_purch = false;
                }
                d_sess += 1;
            }
            prev_ts = e.ts;
            d_ev += 1;
            let price = e.pc & 0xFF_FFFF;
            if price > 0 {
                let p = price as u64;
                s_rev += p; s_purch = true;
                d_rev += p; d_purch = true;
                acc.catsum[(e.pc >> 24) as usize] += p;
            }
        }
        close_session!();
        flush_day!();
        if !pc.is_empty() {
            acc.tmp.clear();
            acc.tmp.extend(pc.iter().map(|x| (x.camp, x.ts)));
            acc.tmp.sort_unstable();
            let t = &acc.tmp;
            let starts = &acc.starts;
            let mut k = 0;
            while k < t.len() {
                let camp = t[k].0;
                let mut a = t[k].1;
                let mut e = a + WEEKU;
                k += 1;
                loop {
                    let more = k < t.len() && t[k].0 == camp;
                    if more && t[k].1 <= e { e = t[k].1 + WEEKU; k += 1; continue; }
                    let mut lo = starts.partition_point(|&s| s < a);
                    while lo < starts.len() && starts[lo] <= e {
                        acc.asst[camp as usize * nd + acc.sdays[lo] as usize] += 1;
                        lo += 1;
                    }
                    if !more { break; }
                    a = t[k].1;
                    e = a + WEEKU;
                    k += 1;
                }
            }
        }
    }
    acc.evs = evs;
    acc.cls = cls;
}

fn main() {
    let t0 = Instant::now();
    let verbose = std::env::var("VERBOSE").is_ok();
    let lap = |what: &str| if verbose { eprintln!("{:>8.1} ms  {}", t0.elapsed().as_secs_f64() * 1000.0, what) };
    let args: Vec<String> = std::env::args().collect();
    let data = PathBuf::from(if args.len() >= 3 { args[1].clone() } else { std::env::var("DATA_DIR").unwrap_or("/data".into()) });
    let outd = PathBuf::from(if args.len() >= 3 { args[2].clone() } else { std::env::var("OUTPUT_DIR").unwrap_or("/output".into()) });
    let ncpu: usize = std::env::var("NCPU").ok().and_then(|s| s.parse().ok()).unwrap_or(8);
    rayon::ThreadPoolBuilder::new().num_threads(ncpu).build_global().unwrap();

    let man = std::fs::read_to_string(data.join("manifest.json")).expect("manifest");
    let get = |k: &str| -> i64 {
        let p = man.find(k).expect("key");
        let rest = &man[p + k.len()..];
        let rest = &rest[rest.find(':').unwrap() + 1..];
        let rest = &rest[rest.find('"').unwrap() + 1..];
        let b = rest.find('"').unwrap();
        parse_ts(rest[..b].as_bytes()).expect("manifest ts")
    };
    let ws = get("\"window_start\"");
    let we = get("\"window_end\"");
    let ws_day = ws.div_euclid(DAY);
    let ndays = ((we - 1).div_euclid(DAY) - ws_day + 1).max(1) as usize;
    let base = ws - WEEK;

    // map every file in parallel
    let ev_p = list_csv(&data.join("events"));
    let cl_p = list_csv(&data.join("clicks"));
    let ln_p = list_csv(&data.join("links"));
    let mut all: Vec<PathBuf> = vec![data.join("products.csv"), data.join("campaigns.csv")];
    all.extend(ln_p.iter().cloned());
    all.extend(cl_p.iter().cloned());
    all.extend(ev_p.iter().cloned());
    let mapped: Vec<S> = all.par_iter().map(|p| map_file(p)).collect();
    FILES.set(mapped).ok();
    let ln_fis: Vec<usize> = (2..2 + ln_p.len()).collect();
    let cl_fis: Vec<usize> = (2 + ln_p.len()..2 + ln_p.len() + cl_p.len()).collect();
    let ev_fis: Vec<usize> = (2 + ln_p.len() + cl_p.len()..all.len()).collect();
    lap("mapped");
    if std::env::var("BENCH").is_ok() { bench(&ev_fis); return; }

    let small = |fi: usize| -> Vec<(S, S)> {
        let mut v = Vec::new();
        let mut f: [S; 2] = [&[]; 2];
        for c in chunks(&[fi], usize::MAX / 4) { for_lines(c.data(), |l| if parse_line(l, &mut f) { v.push((f[0], f[1])) }); }
        v
    };
    let prods = small(0);
    let camps = small(1);
    let mut cats: Vec<S> = prods.iter().map(|p| p.1).collect();
    cats.push(b"unknown");
    cats.sort();
    cats.dedup();
    let unknown_cat = cats.iter().position(|&c| c == b"unknown").unwrap() as u16;
    let mut plook = Lookup::new(b"p");
    for &(p, cat) in &prods { plook.insert(p, cats.binary_search(&cat).unwrap() as u16); }
    let mut clook = Lookup::new(b"c_");
    for (i, &(cid, _)) in camps.iter().enumerate() { clook.insert(cid, i as u16); }

    // Phase 1: links -> people
    let people = build_people(&ln_fis);
    lap("people built");

    // Phase 2: events + clicks parse (people resolved inline)
    let ev_total: usize = ev_fis.iter().map(|&fi| files()[fi].len()).sum();
    let target = (4usize << 20).max(ev_total / 3000 + 1);
    let ech = chunks(&ev_fis, target);
    let cbits = (usize::BITS - ech.len().max(2).next_power_of_two().leading_zeros() - 1).max(1);
    let lbits = 32 - cbits;
    let lmask: u32 = (1u32 << lbits) - 1;
    struct EvPart { keys: (Vec<KeyRec>, Vec<u32>), evs: (Vec<Ev>, Vec<u32>), n: u32 }
    let (mut ev_parts, cl_parts): (Vec<EvPart>, Vec<(Vec<Cl>, Vec<u32>)>) = rayon::join(
        || {
            ech.par_iter().enumerate().map_init(
                || (Vec::<KeyRec>::new(), Vec::<Ev>::new(), Vec::<u16>::new()),
                |(kb, eb, kbk), (ci, c)| {
                let d = c.data();
                kb.clear(); eb.clear(); kbk.clear();
                kb.reserve(d.len() / 40); eb.reserve(d.len() / 40); kbk.reserve(d.len() / 40);
                let cbase = c.start;
                let ibase = base / 1000 - 40 * 86400;
                let cid = (ci as u32) << lbits;
                let mut local: u32 = 0;
                for_records::<7>(d, |f| {
                    let eid = f[0];
                    if eid.is_empty() { return; }
                    let ing_ms = parse_f1_fast(f[3]).unwrap_or_else(|| parse_ts(f[3]).unwrap_or(i64::MAX));
                    let ingest = if ing_ms == i64::MAX { u32::MAX } else { (ing_ms / 1000 - ibase).clamp(0, u32::MAX as i64) as u32 };
                    let h = xxh3_128(eid);
                    let rid = cid | local;
                    local += 1;
                    kb.push(KeyRec { eh: h as u64, ingest, rid });
                    kbk.push(bucket((h >> 64) as u64) as u16);
                    let uid = f[1];
                    if uid.is_empty() || f[5].is_empty() { return; }
                    let ts = match parse_ts(f[2]) { Some(t) if t >= ws && t < we => t, _ => return };
                    let mut price = 0u32;
                    let mut cat = 0u16;
                    match f[4] {
                        b"view" | b"cart" => {}
                        b"purchase" => match parse_price(f[6]) {
                            Some(p) => { price = p; cat = plook.get(f[5]).unwrap_or(unknown_cat); }
                            None => return,
                        },
                        _ => return,
                    }
                    if ing_ms - ts > DAY { return; }
                    let uh = hs(uid);
                    let noff = (uid.as_ptr() as usize - d.as_ptr() as usize + (c.start - cbase)) as u32;
                    eb.push(Ev { pk: uh, ts: (ts - base) as u32, rid, pc: price | ((cat as u32) << 24), name: noff });
                });
                assert!(local <= lmask, "chunk too large");
                people.resolve_all(eb, |e| e.pk, |e, pk, i| { e.pk = pk; e.name = (1u32 << 31) | i; });
                EvPart { keys: scatter_b(kb, kbk), evs: scatter(eb, |e| bucket(e.pk)), n: local }
            }).collect::<Vec<_>>()
        },
        || {
            let ch = chunks(&cl_fis, 4 << 20);
            let cws = ws - WEEK;
            ch.par_iter().map(|c| {
                let d = c.data();
                let mut raw: Vec<Cl> = Vec::with_capacity(d.len() / 45);
                for_records::<4>(d, |f| {
                    if f[0].is_empty() || f[1].is_empty() { return; }
                    let ts = match parse_ts(f[3]) { Some(t) if t >= cws && t < we => t, _ => return };
                    let camp = match clook.get(f[2]) { Some(c) => c, None => return };
                    raw.push(Cl { pk: hs(f[1]), cid: c.off(f[0]), ts: (ts - base) as u32, camp });
                });
                resolve_clicks(&people, &mut raw);
                scatter(&raw, |r| bucket(r.pk))
            }).collect()
        },
    );
    lap("events + clicks parsed");

    // Phase 3: dedup on event_id -> loser bitmap
    let mut prefix: Vec<u64> = Vec::with_capacity(ev_parts.len() + 1);
    prefix.push(0);
    for p in &ev_parts { prefix.push(prefix.last().unwrap() + p.n as u64); }
    let nrec = *prefix.last().unwrap() as usize;
    let losers: Vec<AtomicU64> = zeroed_vec(nrec / 64 + 1);
    let gidx = |rid: u32| -> usize { (prefix[(rid >> lbits) as usize] + (rid & lmask) as u64) as usize };
    let mark = |rid: u32| { let g = gidx(rid); losers[g >> 6].fetch_or(1u64 << (g & 63), Relaxed); };
    (0..NB).into_par_iter().for_each_init(
        || Vec::<(u64, *const KeyRec)>::new(),
        |table, b| {
            let total: usize = ev_parts.iter().map(|p| (p.keys.1[b + 1] - p.keys.1[b]) as usize).sum();
            let cap = (total * 2 + 16).next_power_of_two();
            let mask = cap - 1;
            table.clear();
            table.resize(cap, (0, std::ptr::null()));
            for p in &ev_parts {
                let (v, off) = &p.keys;
                for r in &v[off[b] as usize..off[b + 1] as usize] {
                    let mut q = (r.eh.rotate_left(NBBITS) as usize) & mask;
                    loop {
                        let e = &mut table[q];
                        if e.1.is_null() { *e = (r.eh, r as *const KeyRec); break; }
                        if e.0 == r.eh {
                            let cur = unsafe { &*e.1 };
                            if r.ingest < cur.ingest { mark(cur.rid); e.1 = r; }
                            else { mark(r.rid); }
                            break;
                        }
                        q = (q + 1) & mask;
                    }
                }
            }
        });
    lap("events deduped");
    for p in ev_parts.iter_mut() { p.keys = (Vec::new(), Vec::new()); }
    let ctx = Ctx { base, ndays, ws_day, ncamp: camps.len(), ncat: cats.len(), people: &people, ech: &ech, lbits };
    assert!(cats.len() < 256);

    // Phase 4: sessionize per person bucket
    let nthreads = rayon::current_num_threads();
    let slots: Vec<std::sync::Mutex<Option<Acc>>> = (0..nthreads + 1).map(|_| std::sync::Mutex::new(None)).collect();
    // biggest buckets first for better balance
    let mut order: Vec<(usize, usize)> = (0..NB).map(|b| (ev_parts.iter().map(|p| (p.evs.1[b + 1] - p.evs.1[b]) as usize).sum::<usize>(), b)).collect();
    order.sort_unstable_by(|x, y| y.cmp(x));
    order.par_iter().with_max_len(1).for_each(|&(_, b)| {
        let ti = rayon::current_thread_index().unwrap_or(nthreads);
        let mut g = slots[ti].lock().unwrap();
        let acc = g.get_or_insert_with(|| { let mut a = Acc::new(&ctx); a.rows.reserve(nrec / 7 / nthreads + 1024); a });
        let t0 = Instant::now();
        let mut evs = std::mem::take(&mut acc.evbuf);
        let mut cls = std::mem::take(&mut acc.clbuf);
        evs.clear();
        evs.reserve(ev_parts.iter().map(|p| (p.evs.1[b + 1] - p.evs.1[b]) as usize).sum());
        cls.clear();
        cls.reserve(cl_parts.iter().map(|p| (p.1[b + 1] - p.1[b]) as usize).sum());
        for p in &ev_parts {
            let (v, off) = &p.evs;
            for e in &v[off[b] as usize..off[b + 1] as usize] {
                let gi = gidx(e.rid);
                if losers[gi >> 6].load(Relaxed) & (1u64 << (gi & 63)) == 0 { evs.push(*e); }
            }
        }
        cls.clear();
        for (v, off) in &cl_parts { cls.extend_from_slice(&v[off[b] as usize..off[b + 1] as usize]); }
        T_GATHER.fetch_add(t0.elapsed().as_nanos() as u64, Relaxed);
        process_bucket(&ctx, &mut evs, &mut cls, acc);
        acc.evbuf = evs;
        acc.clbuf = cls;
    });
    let accs: Vec<Acc> = slots.into_iter().filter_map(|m| m.into_inner().unwrap()).collect();
    lap("sessionized");
    if verbose { eprintln!("  thread-ms gather {} sort {} proc {}", T_GATHER.load(Relaxed)/1000000, T_SORT.load(Relaxed)/1000000, T_PROC.load(Relaxed)/1000000); }

    let ud = outd.join("user_daily");
    let cd = outd.join("campaign_daily");
    std::fs::create_dir_all(&ud).unwrap();
    std::fs::create_dir_all(&cd).unwrap();
    let catnames: Vec<&str> = cats.iter().map(|c| std::str::from_utf8(c).unwrap_or("?")).collect();
    rayon::join(
        || accs.par_iter().enumerate().for_each(|(k, a)| out::write_user(&ud.join(format!("part-{:05}.parquet", k)), &a.rows, &catnames)),
        || {
            let mut tot = vec![[0u64; 4]; (ctx.ncamp + 1) * ctx.ndays];
            for a in &accs { for (i, (t, x)) in tot.iter_mut().zip(a.camp.iter()).enumerate() { for q in 0..3 { t[q] += x[q]; } t[3] += a.asst[i] as u64; } }
            out::write_campaign(&cd.join("part-00000.parquet"), &tot, &camps, ctx.ndays, ctx.ws_day);
        },
    );
    lap("written");
    if verbose { let st = std::fs::read_to_string("/proc/self/stat").unwrap_or_default(); let f: Vec<&str> = st.rsplit(") ").next().unwrap_or("").split(' ').collect(); eprintln!("  minflt {} majflt {} utime {} stime {}", f[7], f[9], f[11], f[12]); }
    // Skip destructors / munmap of multi-GB buffers: all files are closed above.
    std::process::exit(0);
}


fn bench(ev_fis: &[usize]) {
    let ch = chunks(ev_fis, 8 << 20);
    let mut acc = 0u64;
    let t = Instant::now();
    for c in &ch { for_lines(c.data(), |l| acc += l.len() as u64); }
    eprintln!("lines only {:.0} ms", t.elapsed().as_secs_f64()*1e3);
    let t = Instant::now();
    for c in &ch { let d=c.data(); let mut b=0; while b+64<=d.len() { let (x,y,z)=masks64(d[b..b+64].try_into().unwrap()); let mut m=x|y; while m!=0 { acc+= (m.trailing_zeros() as u64) ^ (y>>1 &1); m&=m-1;} acc+=z; b+=64; } }
    eprintln!("mask floor {:.0} ms", t.elapsed().as_secs_f64()*1e3);
    let t = Instant::now();
    for c in &ch { let d=c.data(); let n=d.len(); let mut s=0; while s<n { let e = match memchr(b'\n', &d[s..]) { Some(i) => s + i, None => n }; if s+128<=n { let (c0,_)=mask2(d[s..].as_ptr()); let (c1,_)=mask2(d[s+64..].as_ptr()); acc += (c0.count_ones()+c1.count_ones()) as u64; } s=e+1; } }
    eprintln!("lines+mask {:.0} ms", t.elapsed().as_secs_f64()*1e3);
    let t = Instant::now();
    for c in &ch { for_records::<7>(c.data(), |f| acc += f[0].len() as u64); }
    eprintln!("records {:.0} ms", t.elapsed().as_secs_f64()*1e3);
    let t = Instant::now();
    for c in &ch { for_records::<7>(c.data(), |f| { acc += parse_ts(f[2]).unwrap_or(0) as u64 + parse_ts(f[3]).unwrap_or(0) as u64; }); }
    eprintln!("records+ts {:.0} ms", t.elapsed().as_secs_f64()*1e3);
    let t = Instant::now();
    for c in &ch { for_records::<7>(c.data(), |f| { acc += (xxh3_128(f[0]) as u64) ^ hs(f[1]); }); }
    eprintln!("records+hash {:.0} ms", t.elapsed().as_secs_f64()*1e3);
    eprintln!("{}", acc);
}

fn resolve_clicks(people: &People, raw: &mut [Cl]) {
    people.resolve_all(raw, |c| c.pk, |c, pk, _| c.pk = pk);
}

mod out {
    use super::*;
    use arrow_array::builder::StringBuilder;
    use arrow_array::{ArrayRef, Date32Array, Int64Array, RecordBatch};
    use arrow_schema::{DataType, Field, Schema};
    use parquet::arrow::ArrowWriter;
    use parquet::basic::Compression;
    use parquet::file::properties::{EnabledStatistics, WriterProperties};

    fn props() -> WriterProperties {
        WriterProperties::builder()
            .set_compression(Compression::UNCOMPRESSED)
            .set_dictionary_enabled(false)
            .set_statistics_enabled(EnabledStatistics::None)
            .set_max_row_group_size(1 << 30)
            .set_data_page_size_limit(8 << 20)
            .set_write_batch_size(1 << 16)
            .build()
    }
    #[inline]
    fn s(b: &[u8]) -> &str { unsafe { std::str::from_utf8_unchecked(b) } }

    pub fn write_user(path: &Path, rows: &[URow], cats: &[&str]) {
        let schema = Arc::new(Schema::new(vec![
            Field::new("user_id", DataType::Utf8, false),
            Field::new("day", DataType::Date32, false),
            Field::new("sessions", DataType::Int64, false),
            Field::new("events", DataType::Int64, false),
            Field::new("revenue_cents", DataType::Int64, false),
            Field::new("top_category", DataType::Utf8, true),
        ]));
        let mut ub = StringBuilder::with_capacity(rows.len(), rows.len() * 10);
        let mut tb = StringBuilder::with_capacity(rows.len(), rows.len() * 8);
        for r in rows {
            ub.append_value(s(field_at(r.name)));
            if r.top >= 0 { tb.append_value(cats[r.top as usize]) } else { tb.append_null() }
        }
        let cols: Vec<ArrayRef> = vec![
            Arc::new(ub.finish()),
            Arc::new(Date32Array::from_iter_values(rows.iter().map(|r| r.day))),
            Arc::new(Int64Array::from_iter_values(rows.iter().map(|r| r.sessions as i64))),
            Arc::new(Int64Array::from_iter_values(rows.iter().map(|r| r.events as i64))),
            Arc::new(Int64Array::from_iter_values(rows.iter().map(|r| r.rev as i64))),
            Arc::new(tb.finish()),
        ];
        let batch = RecordBatch::try_new(schema.clone(), cols).unwrap();
        let f = File::create(path).unwrap();
        let mut w = ArrowWriter::try_new(std::io::BufWriter::with_capacity(4 << 20, f), schema, Some(props())).unwrap();
        w.write(&batch).unwrap();
        w.close().unwrap();
    }

    pub fn write_campaign(path: &Path, tot: &[[u64; 4]], camps: &[(S, S)], nd: usize, ws_day: i64) {
        let schema = Arc::new(Schema::new(vec![
            Field::new("campaign_id", DataType::Utf8, false),
            Field::new("channel", DataType::Utf8, false),
            Field::new("day", DataType::Date32, false),
            Field::new("sessions", DataType::Int64, false),
            Field::new("purchasing_sessions", DataType::Int64, false),
            Field::new("revenue_cents", DataType::Int64, false),
            Field::new("assisted_sessions", DataType::Int64, false),
        ]));
        let mut cb = StringBuilder::new();
        let mut chb = StringBuilder::new();
        let (mut day, mut se, mut ps, mut rv, mut asd) = (vec![], vec![], vec![], vec![], vec![]);
        for c in 0..=camps.len() {
            let (id, ch): (&str, &str) = if c == camps.len() { ("organic", "organic") } else { (s(camps[c].0), s(camps[c].1)) };
            for d in 0..nd {
                let a = tot[c * nd + d];
                if a[0] == 0 && a[3] == 0 { continue; }
                cb.append_value(id);
                chb.append_value(ch);
                day.push((ws_day + d as i64) as i32);
                se.push(a[0] as i64); ps.push(a[1] as i64); rv.push(a[2] as i64); asd.push(a[3] as i64);
            }
        }
        let cols: Vec<ArrayRef> = vec![
            Arc::new(cb.finish()), Arc::new(chb.finish()), Arc::new(Date32Array::from(day)),
            Arc::new(Int64Array::from(se)), Arc::new(Int64Array::from(ps)), Arc::new(Int64Array::from(rv)), Arc::new(Int64Array::from(asd)),
        ];
        let batch = RecordBatch::try_new(schema.clone(), cols).unwrap();
        let f = File::create(path).unwrap();
        let mut w = ArrowWriter::try_new(f, schema, Some(props())).unwrap();
        w.write(&batch).unwrap();
        w.close().unwrap();
    }
}
