// Fast fused CSV parser for events
// Streams through data in 64-byte AVX2 blocks, extracts fields inline, validates, and produces compact events

use xxhash_rust::xxh3::{xxh3_64, xxh3_128};

/// Compact event record (16 bytes)
#[derive(Clone, Copy)]
#[repr(C)]
pub struct CompactEvent {
    pub pk: u64,        // person key (hash of user_id or resolved person)
    pub ts: u32,        // relative timestamp (ms since base)
    pub pc: u32,        // price_cents (24 bits) | category (8 bits high)
}

/// Dedup key (16 bytes)  
#[derive(Clone, Copy)]
#[repr(C)]
pub struct DedupKey {
    pub eh: u64,        // event_id hash
    pub ingest: u32,    // relative ingest timestamp
    pub idx: u32,       // index into events array
}

/// AVX2 mask for comma, quote, newline in 64 bytes
#[inline(always)]
pub fn mask64(p: *const u8) -> (u64, u64, u64) {
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

/// Strip surrounding quotes if present
#[inline(always)]
fn strip_quotes(s: &[u8]) -> &[u8] {
    if s.len() >= 2 && s[0] == b'"' && s[s.len()-1] == b'"' {
        &s[1..s.len()-1]
    } else {
        s
    }
}

/// Hash for user_id (never 0)
#[inline(always)]
pub fn hs(b: &[u8]) -> u64 {
    let h = xxh3_64(b);
    if h == 0 { 1 } else { h }
}

/// Parse price: digits only, 1-10,000,000
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

const DAY: i64 = 86_400_000;

/// Fast timestamp parser for F1 format (most common)
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
    // Check all digits are valid
    for &i in &[0usize,1,2,3,5,6,8,9,11,12,14,15,17,18] {
        if b[i].wrapping_sub(b'0') > 9 { return None; }
    }
    Some((civil_days(y, mo, d) * 86400 + hh * 3600 + mi * 60 + ss) * 1000)
}

#[inline]
fn is_leap(y: i64) -> bool { (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 }

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

static YEAR_DAYS: std::sync::OnceLock<Vec<i64>> = std::sync::OnceLock::new();
const CUM: [i64; 13] = [0, 0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334];

#[inline(always)]
fn civil_days(y: i64, m: i64, d: i64) -> i64 {
    if (1900..2200).contains(&y) {
        let t = YEAR_DAYS.get_or_init(|| (1900..2200).map(|y| days_from_civil(y, 1, 1)).collect());
        t[(y - 1900) as usize] + CUM[m as usize] + if m > 2 && is_leap(y) { 1 } else { 0 } + d - 1
    } else { days_from_civil(y, m, d) }
}

/// Full timestamp parser (all formats)
#[inline]
pub fn parse_ts(b: &[u8]) -> Option<i64> {
    let n = b.len();
    if n == 0 { return None; }
    if n != 20 && n != 25 || b.get(4).copied() != Some(b'-') {
        // F3: epoch milliseconds
        if !b.iter().all(|c| c.is_ascii_digit()) { return None; }
        let mut i = 0;
        while i < n && b[i] == b'0' { i += 1; }
        let r = &b[i..];
        if r.len() > 18 { return Some(i64::MAX); }
        let mut v: i64 = 0;
        for &c in r { v = v * 10 + (c - b'0') as i64; }
        return Some(v);
    }
    // F1 or F2
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
        let d2 = |i: usize| -> Option<i64> {
            let a = b[i].wrapping_sub(b'0');
            let c = b[i + 1].wrapping_sub(b'0');
            if a > 9 || c > 9 { return None; }
            Some((a as i64) * 10 + c as i64)
        };
        let oh = d2(20)?;
        let om = d2(23)?;
        if oh > 23 || om > 59 { return None; }
        secs -= sign * (oh * 3600 + om * 60);
    }
    Some(secs * 1000)
}

/// Streaming event parser result
pub struct ParseResult {
    pub events: Vec<CompactEvent>,
    pub dedup_keys: Vec<DedupKey>,
    pub name_offsets: Vec<u64>,  // file offset for user_id string (for name resolution)
}

/// Process a single valid record
#[inline(always)]
fn process_record(
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
    events: &mut Vec<CompactEvent>,
    dedup_keys: &mut Vec<DedupKey>,
    name_offsets: &mut Vec<u64>,
) {
    // Extract fields
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
    
    // Validate basic fields
    if event_id.is_empty() || user_id.is_empty() || product_id.is_empty() {
        return;
    }
    
    // Parse timestamps
    let ts = match parse_f1_fast(ts_raw).or_else(|| parse_ts(ts_raw)) {
        Some(t) => t,
        None => return,
    };
    
    if ts < ws || ts >= we {
        return;
    }
    
    let ing = parse_f1_fast(ingest_raw).unwrap_or(i64::MAX);
    
    // Check ingest - ts <= 24 hours
    if ing - ts > DAY {
        return;
    }
    
    // Check event type and price
    let (price, cat) = match event_type {
        b"view" | b"cart" => (0u32, 0u16),
        b"purchase" => {
            match parse_price(price_raw) {
                Some(p) => (p, product_lookup(product_id).unwrap_or(unknown_cat)),
                None => return,
            }
        }
        _ => return,
    };
    
    // Compute hashes
    let eh = xxh3_128(event_id);
    let uh = hs(user_id);
    
    // Relative timestamps
    let rel_ts = (ts - base_ts) as u32;
    let rel_ing = if ing == i64::MAX { u32::MAX } 
                  else { (ing / 1000 - ibase).clamp(0, u32::MAX as i64) as u32 };
    
    // Store
    let idx = events.len() as u32;
    events.push(CompactEvent {
        pk: uh,
        ts: rel_ts,
        pc: price | ((cat as u32) << 24),
    });
    dedup_keys.push(DedupKey {
        eh: eh as u64,
        ingest: rel_ing,
        idx,
    });
    // Store file offset for user_id
    let uid_off = ((file_idx as u64) << 40) | 
                  ((base_offset + field_starts[1]) as u64);
    name_offsets.push(uid_off);
}

/// Parse events from a chunk using AVX2 streaming
/// Returns events that passed basic validation (non-empty event_id, user_id, valid ts in window, valid event_type)
pub fn parse_events_fast(
    data: &[u8],
    file_idx: usize,
    base_offset: usize,
    ws: i64,          // window_start
    we: i64,          // window_end  
    base_ts: i64,     // base timestamp for relative storage
    product_lookup: impl Fn(&[u8]) -> Option<u16>,  // product_id -> category
    unknown_cat: u16,
) -> ParseResult {
    let n = data.len();
    let mut events = Vec::with_capacity(n / 80);
    let mut dedup_keys = Vec::with_capacity(n / 80);
    let mut name_offsets = Vec::with_capacity(n / 80);
    
    if n == 0 { return ParseResult { events, dedup_keys, name_offsets }; }
    
    let base = data.as_ptr();
    let mut pos = 0usize;
    let mut field_starts = [0usize; 8];
    let mut nf = 1usize;
    let mut in_header = true;
    
    // For relative ingest timestamp
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
                    process_record(
                        data, &field_starts, abs_pos,
                        file_idx, base_offset,
                        ws, we, base_ts, ibase,
                        &product_lookup, unknown_cat,
                        &mut events, &mut dedup_keys, &mut name_offsets,
                    );
                }
                in_header = false;
                field_starts[0] = abs_pos + 1;
                nf = 1;
            } else {
                // comma
                if nf < 8 {
                    field_starts[nf] = abs_pos + 1;
                }
                nf += 1;
            }
            ev ^= bit;
        }
        pos += 64;
    }
    
    // Handle last line without trailing newline
    if !in_header && nf == 7 && field_starts[0] < n {
        process_record(
            data, &field_starts, n,
            file_idx, base_offset,
            ws, we, base_ts, ibase,
            &product_lookup, unknown_cat,
            &mut events, &mut dedup_keys, &mut name_offsets,
        );
    }
    
    ParseResult { events, dedup_keys, name_offsets }
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_parse_ts() {
        assert_eq!(parse_ts(b"2026-10-03T23:58:10Z"), Some(1791071890000));
        assert_eq!(parse_ts(b"1791071890000"), Some(1791071890000));
        assert_eq!(parse_ts(b"2026-10-03T19:58:10-04:00"), Some(1791071890000));
    }
    
    #[test] 
    fn test_strip_quotes() {
        assert_eq!(strip_quotes(b"\"hello\""), b"hello");
        assert_eq!(strip_quotes(b"hello"), b"hello");
        assert_eq!(strip_quotes(b"\"\""), b"");
    }
}
