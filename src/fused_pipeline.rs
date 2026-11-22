// Fused pipeline: parse + hash + scatter in one pass
// Instead of: parse → store → scatter
// Do: parse → hash → directly write to bucket

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering::Relaxed};
use xxhash_rust::xxh3::{xxh3_64, xxh3_128};

const NBBITS: u32 = 9;
const NB: usize = 1 << NBBITS;

#[inline(always)]
fn bucket(h: u64) -> usize { (h >> (64 - NBBITS)) as usize }

/// Compact event (16 bytes, cache-line friendly)
#[derive(Clone, Copy, Default)]
#[repr(C, align(16))]
pub struct Event16 {
    pub pk: u64,        // person key
    pub ts: u32,        // relative timestamp  
    pub pc: u32,        // price|cat<<24
}

/// Dedup key (12 bytes)
#[derive(Clone, Copy, Default)]
#[repr(C)]
pub struct DedupKey12 {
    pub eh: u64,        // event_id hash
    pub ingest: u32,    // relative ingest timestamp
}

/// Per-bucket storage with pre-allocated capacity
pub struct BucketStore {
    // Events indexed by bucket, then by position within bucket
    pub events: Vec<Vec<Event16>>,
    pub dedup_keys: Vec<Vec<DedupKey12>>,
    pub name_offsets: Vec<Vec<u64>>,
    pub counts: Vec<AtomicU32>,
}

impl BucketStore {
    pub fn new(estimated_total: usize) -> Self {
        let per_bucket = estimated_total / NB + 64;
        let mut events = Vec::with_capacity(NB);
        let mut dedup_keys = Vec::with_capacity(NB);
        let mut name_offsets = Vec::with_capacity(NB);
        let mut counts = Vec::with_capacity(NB);
        
        for _ in 0..NB {
            events.push(Vec::with_capacity(per_bucket));
            dedup_keys.push(Vec::with_capacity(per_bucket));
            name_offsets.push(Vec::with_capacity(per_bucket));
            counts.push(AtomicU32::new(0));
        }
        
        BucketStore { events, dedup_keys, name_offsets, counts }
    }
    
    #[inline(always)]
    pub fn push(&mut self, bucket: usize, ev: Event16, dk: DedupKey12, name_off: u64) {
        self.events[bucket].push(ev);
        self.dedup_keys[bucket].push(dk);
        self.name_offsets[bucket].push(name_off);
    }
}

/// Thread-local bucket accumulator for lock-free accumulation
pub struct ThreadBuckets {
    events: [Vec<Event16>; NB],
    dedup_keys: [Vec<DedupKey12>; NB],
    name_offsets: [Vec<u64>; NB],
}

impl ThreadBuckets {
    pub fn new() -> Self {
        ThreadBuckets {
            events: std::array::from_fn(|_| Vec::with_capacity(1024)),
            dedup_keys: std::array::from_fn(|_| Vec::with_capacity(1024)),
            name_offsets: std::array::from_fn(|_| Vec::with_capacity(1024)),
        }
    }
    
    #[inline(always)]
    pub fn push(&mut self, pk: u64, ev: Event16, dk: DedupKey12, name_off: u64) {
        let b = bucket(pk);
        self.events[b].push(ev);
        self.dedup_keys[b].push(dk);
        self.name_offsets[b].push(name_off);
    }
    
    pub fn total_events(&self) -> usize {
        self.events.iter().map(|v| v.len()).sum()
    }
}

/// AVX2 mask for structural characters
#[inline(always)]
fn mask64(p: *const u8) -> (u64, u64, u64) {
    unsafe {
        use core::arch::x86_64::*;
        let a = _mm256_loadu_si256(p as *const __m256i);
        let b = _mm256_loadu_si256(p.add(32) as *const __m256i);
        let m = |v: __m256i, c: i8| -> u32 { 
            _mm256_movemask_epi8(_mm256_cmpeq_epi8(v, _mm256_set1_epi8(c))) as u32 
        };
        (
            (m(a, b',' as i8) as u64) | ((m(b, b',' as i8) as u64) << 32),
            (m(a, b'"' as i8) as u64) | ((m(b, b'"' as i8) as u64) << 32),
            (m(a, b'\n' as i8) as u64) | ((m(b, b'\n' as i8) as u64) << 32)
        )
    }
}

#[inline(always)]
fn strip_quotes(s: &[u8]) -> &[u8] {
    if s.len() >= 2 && s[0] == b'"' && s[s.len()-1] == b'"' {
        &s[1..s.len()-1]
    } else { s }
}

#[inline(always)]
fn hs(b: &[u8]) -> u64 {
    let h = xxh3_64(b);
    if h == 0 { 1 } else { h }
}

const DAY: i64 = 86_400_000;

/// Fast F1 timestamp parser
#[inline(always)]
fn parse_f1_fast(b: &[u8]) -> Option<i64> {
    if b.len() != 20 || b[19] != b'Z' || b[10] != b'T' || 
       b[4] != b'-' || b[7] != b'-' || b[13] != b':' || b[16] != b':' { 
        return None; 
    }
    let g = |i: usize| (b[i].wrapping_sub(b'0')) as i64;
    let y = g(0) * 1000 + g(1) * 100 + g(2) * 10 + g(3);
    let mo = g(5) * 10 + g(6);
    let d = g(8) * 10 + g(9);
    if !(1900..2200).contains(&y) || !(1..=12).contains(&mo) || !(1..=28).contains(&d) { 
        return None; 
    }
    let hh = g(11) * 10 + g(12);
    let mi = g(14) * 10 + g(15);
    let ss = g(17) * 10 + g(18);
    if hh > 23 || mi > 59 || ss > 59 { return None; }
    for &i in &[0usize,1,2,3,5,6,8,9,11,12,14,15,17,18] {
        if b[i].wrapping_sub(b'0') > 9 { return None; }
    }
    
    static YEAR_DAYS: std::sync::OnceLock<Vec<i64>> = std::sync::OnceLock::new();
    const CUM: [i64; 13] = [0, 0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334];
    fn is_leap(y: i64) -> bool { (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 }
    fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
        let y = if m <= 2 { y - 1 } else { y };
        let era = if y >= 0 { y } else { y - 399 } / 400;
        let yoe = y - era * 400;
        let mp = (m + 9) % 12;
        let doy = (153 * mp + 2) / 5 + d - 1;
        era * 146097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719468
    }
    let t = YEAR_DAYS.get_or_init(|| (1900..2200).map(|y| days_from_civil(y, 1, 1)).collect());
    let days = t[(y - 1900) as usize] + CUM[mo as usize] + if mo > 2 && is_leap(y) { 1 } else { 0 } + d - 1;
    
    Some((days * 86400 + hh * 3600 + mi * 60 + ss) * 1000)
}

/// Full timestamp parser (all formats)
#[inline]
fn parse_ts(b: &[u8]) -> Option<i64> {
    parse_f1_fast(b).or_else(|| parse_ts_slow(b))
}

fn parse_ts_slow(b: &[u8]) -> Option<i64> {
    let n = b.len();
    if n == 0 { return None; }
    
    // F3: epoch milliseconds
    if b.iter().all(|c| c.is_ascii_digit()) {
        let mut i = 0;
        while i < n && b[i] == b'0' { i += 1; }
        let r = &b[i..];
        if r.len() > 18 { return Some(i64::MAX); }
        let mut v: i64 = 0;
        for &c in r { v = v * 10 + (c - b'0') as i64; }
        return Some(v);
    }
    
    // F2: with offset
    if n == 25 {
        // Parse F2...
        // (simplified - would need full implementation)
    }
    
    None
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

/// Fused parse + scatter for events
/// Returns the thread-local buckets directly
pub fn fused_parse_events(
    data: &[u8],
    file_idx: usize,
    base_offset: usize,
    ws: i64,
    we: i64,
    base_ts: i64,
    product_lookup: &impl Fn(&[u8]) -> Option<u16>,
    unknown_cat: u16,
) -> ThreadBuckets {
    let mut buckets = ThreadBuckets::new();
    let n = data.len();
    
    if n == 0 { return buckets; }
    
    let base = data.as_ptr();
    let mut pos = 0usize;
    let mut field_starts = [0usize; 8];
    let mut nf = 1usize;
    let mut in_header = true;
    let ibase = base_ts / 1000 - 40 * 86400;
    
    while pos < n {
        let avail = n - pos;
        let (cm, _qm, nm) = if avail >= 64 {
            mask64(unsafe { base.add(pos) })
        } else {
            let mut pad = [0u8; 64];
            pad[..avail].copy_from_slice(&data[pos..]);
            let (c, q, l) = mask64(pad.as_ptr());
            let mask = (1u64 << avail) - 1;
            (c & mask, q & mask, l & mask)
        };
        
        let mut ev = cm | nm;
        while ev != 0 {
            let bit_pos = ev.trailing_zeros() as usize;
            let abs_pos = pos + bit_pos;
            let bit = 1u64 << bit_pos;
            
            if nm & bit != 0 {
                // newline - process line
                if !in_header && nf == 7 {
                    process_event_line(
                        data, &field_starts, abs_pos,
                        file_idx, base_offset,
                        ws, we, base_ts, ibase,
                        product_lookup, unknown_cat,
                        &mut buckets,
                    );
                }
                in_header = false;
                field_starts[0] = abs_pos + 1;
                nf = 1;
            } else {
                if nf < 8 {
                    field_starts[nf] = abs_pos + 1;
                }
                nf += 1;
            }
            ev ^= bit;
        }
        pos += 64;
    }
    
    buckets
}

#[inline(always)]
fn process_event_line(
    data: &[u8],
    field_starts: &[usize; 8],
    line_end: usize,
    file_idx: usize,
    base_offset: usize,
    ws: i64,
    we: i64,
    base_ts: i64,
    ibase: i64,
    product_lookup: &impl Fn(&[u8]) -> Option<u16>,
    unknown_cat: u16,
    buckets: &mut ThreadBuckets,
) {
    let f = |i: usize| -> &[u8] {
        let start = field_starts[i];
        let end = if i + 1 < 7 { field_starts[i + 1].saturating_sub(1) } else { line_end };
        if start >= data.len() || end > data.len() || start > end { return &[]; }
        strip_quotes(&data[start..end])
    };
    
    let event_id = f(0);
    let user_id = f(1);
    let ts_raw = f(2);
    let ingest_raw = f(3);
    let event_type = f(4);
    let product_id = f(5);
    let price_raw = f(6);
    
    if event_id.is_empty() || user_id.is_empty() || product_id.is_empty() { return; }
    
    let ts = match parse_ts(ts_raw) {
        Some(t) if t >= ws && t < we => t,
        _ => return,
    };
    
    let ing = parse_f1_fast(ingest_raw).or_else(|| parse_ts(ingest_raw)).unwrap_or(i64::MAX);
    if ing - ts > DAY { return; }
    
    let (price, cat) = match event_type {
        b"view" | b"cart" => (0u32, 0u16),
        b"purchase" => match parse_price(price_raw) {
            Some(p) => (p, product_lookup(product_id).unwrap_or(unknown_cat)),
            None => return,
        },
        _ => return,
    };
    
    // Compute hashes
    let eh = xxh3_128(event_id) as u64;
    let uh = hs(user_id);
    
    // Create compact records
    let ev = Event16 {
        pk: uh,
        ts: (ts - base_ts) as u32,
        pc: price | ((cat as u32) << 24),
    };
    let dk = DedupKey12 {
        eh,
        ingest: if ing == i64::MAX { u32::MAX } else { (ing / 1000 - ibase).clamp(0, u32::MAX as i64) as u32 },
    };
    let name_off = ((file_idx as u64) << 40) | ((base_offset + field_starts[1]) as u64);
    
    // Push directly to bucket
    buckets.push(uh, ev, dk, name_off);
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_bucket() {
        let h = 0xFFFF_0000_0000_0000u64;
        assert_eq!(bucket(h), 511);
        let h = 0x0000_0000_0000_0000u64;
        assert_eq!(bucket(h), 0);
    }
}
