// Bandwidth experiment: measures theoretical limits for CSV processing
// Run with: cargo run --release --bin bandwidth_exp -- <data_dir>

use memmap2::Mmap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::Instant;

fn map_file(p: &Path) -> &'static [u8] {
    let f = File::open(p).expect("open");
    let len = f.metadata().unwrap().len();
    if len == 0 { return &[]; }
    let m = unsafe { memmap2::MmapOptions::new().map(&f).unwrap() };
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

#[inline(always)]
fn mask64(p: *const u8) -> (u64, u64, u64) {
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

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let data = PathBuf::from(args.get(1).map(|s| s.as_str()).unwrap_or("testdata/5m"));
    
    let ev_p = list_csv(&data.join("events"));
    let mapped: Vec<&[u8]> = ev_p.iter().map(|p| map_file(p)).collect();
    let total_bytes: usize = mapped.iter().map(|m| m.len()).sum();
    
    println!("Total event CSV bytes: {} ({:.1} MB)", total_bytes, total_bytes as f64 / (1 << 20) as f64);
    
    // Warm up
    let _ = mapped.iter().map(|m| m.iter().fold(0u64, |a, &b| a.wrapping_add(b as u64))).sum::<u64>();
    
    // 1. Raw sequential read (sum all bytes)
    let t = Instant::now();
    let mut acc = 0u64;
    for m in &mapped {
        for &b in m.iter() {
            acc = acc.wrapping_add(b as u64);
        }
    }
    let raw_read = t.elapsed();
    println!("1. Raw byte sum:     {:>6.1} ms  ({:.1} GB/s)  [acc={}]", 
             raw_read.as_secs_f64() * 1000.0,
             total_bytes as f64 / raw_read.as_secs_f64() / 1e9, acc);
    
    // 2. AVX2 structural scan (count commas, quotes, newlines)
    let t = Instant::now();
    let (mut commas, mut quotes, mut newlines) = (0u64, 0u64, 0u64);
    for m in &mapped {
        let mut i = 0;
        while i + 64 <= m.len() {
            let (cm, qm, nm) = mask64(m[i..].as_ptr());
            commas += cm.count_ones() as u64;
            quotes += qm.count_ones() as u64;
            newlines += nm.count_ones() as u64;
            i += 64;
        }
        // tail
        for &b in &m[i..] {
            match b {
                b',' => commas += 1,
                b'"' => quotes += 1,
                b'\n' => newlines += 1,
                _ => {}
            }
        }
    }
    let avx_scan = t.elapsed();
    println!("2. AVX2 scan:        {:>6.1} ms  ({:.1} GB/s)  [commas={} quotes={} newlines={}]", 
             avx_scan.as_secs_f64() * 1000.0,
             total_bytes as f64 / avx_scan.as_secs_f64() / 1e9, commas, quotes, newlines);
    
    // 3. AVX2 scan + field extraction (count fields per line, verify 7 fields)
    let t = Instant::now();
    let (mut valid_lines, mut invalid_lines) = (0u64, 0u64);
    for m in &mapped {
        let mut i = 0;
        let mut field_count = 0u32;
        let mut in_header = true;
        while i + 64 <= m.len() {
            let (cm, _, nm) = mask64(m[i..].as_ptr());
            let mut ev = cm | nm;
            while ev != 0 {
                let bit = ev & ev.wrapping_neg();
                let pos = ev.trailing_zeros();
                if (nm >> pos) & 1 != 0 {
                    // newline
                    if !in_header {
                        if field_count == 6 { valid_lines += 1; } else { invalid_lines += 1; }
                    }
                    in_header = false;
                    field_count = 0;
                } else {
                    field_count += 1;
                }
                ev ^= bit;
            }
            i += 64;
        }
        // tail
        for &b in &m[i..] {
            if b == b'\n' {
                if !in_header {
                    if field_count == 6 { valid_lines += 1; } else { invalid_lines += 1; }
                }
                in_header = false;
                field_count = 0;
            } else if b == b',' {
                field_count += 1;
            }
        }
    }
    let field_count = t.elapsed();
    println!("3. Field counting:   {:>6.1} ms  ({:.1} GB/s)  [valid={} invalid={}]", 
             field_count.as_secs_f64() * 1000.0,
             total_bytes as f64 / field_count.as_secs_f64() / 1e9, valid_lines, invalid_lines);
    
    // 4. Full field extraction + xxh3 hash of event_id
    let t = Instant::now();
    let mut hash_sum = 0u64;
    let mut line_count = 0u64;
    for m in &mapped {
        let mut i = 0;
        let n = m.len();
        // skip header
        while i < n && m[i] != b'\n' { i += 1; }
        i += 1;
        
        while i < n {
            // find first comma (end of event_id)
            let start = i;
            while i < n && m[i] != b',' && m[i] != b'\n' { i += 1; }
            if i < n && m[i] == b',' {
                let field = &m[start..i];
                let field = if !field.is_empty() && field[0] == b'"' && field.len() >= 2 && field[field.len()-1] == b'"' {
                    &field[1..field.len()-1]
                } else { field };
                hash_sum = hash_sum.wrapping_add(xxhash_rust::xxh3::xxh3_64(field));
                line_count += 1;
            }
            // skip to end of line
            while i < n && m[i] != b'\n' { i += 1; }
            i += 1;
        }
    }
    let hash_time = t.elapsed();
    println!("4. Hash event_id:    {:>6.1} ms  ({:.1} GB/s)  [lines={} hash_sum={}]", 
             hash_time.as_secs_f64() * 1000.0,
             total_bytes as f64 / hash_time.as_secs_f64() / 1e9, line_count, hash_sum);
    
    // 5. Full 7-field extraction with AVX2 + hash first 2 fields
    let t = Instant::now();
    let mut processed = 0u64;
    let mut hash_sum2 = 0u64;
    for m in &mapped {
        process_csv_fast(m, |eid, uid| {
            hash_sum2 = hash_sum2.wrapping_add(xxhash_rust::xxh3::xxh3_64(eid));
            hash_sum2 = hash_sum2.wrapping_add(xxhash_rust::xxh3::xxh3_64(uid));
            processed += 1;
        });
    }
    let fast_parse = t.elapsed();
    println!("5. Fast parse+hash:  {:>6.1} ms  ({:.1} GB/s)  [records={} hash={}]", 
             fast_parse.as_secs_f64() * 1000.0,
             total_bytes as f64 / fast_parse.as_secs_f64() / 1e9, processed, hash_sum2);
    
    // Summary
    println!("\n=== Summary ===");
    println!("At {:.0} MB, theoretical floor for 7-field parse+hash: {:.0}ms", 
             total_bytes as f64 / (1 << 20) as f64,
             fast_parse.as_secs_f64() * 1000.0);
    println!("Scale to 20M events (~1.9GB): ~{:.0}ms expected",
             fast_parse.as_secs_f64() * 1000.0 * (1900.0 / (total_bytes as f64 / (1 << 20) as f64)));
}

/// Fused streaming parser: scans data in 64-byte AVX2 blocks, extracts first 2 fields per line
#[inline(always)]
fn process_csv_fast(data: &[u8], mut callback: impl FnMut(&[u8], &[u8])) {
    let n = data.len();
    if n == 0 { return; }
    
    let base = data.as_ptr();
    let mut pos = 0usize;
    let mut line_start = 0usize;
    let mut field_starts = [0usize; 8]; // positions of field starts
    let mut nf = 0usize; // number of fields found (nf = number of commas + 1)
    let mut in_header = true;
    
    while pos < n {
        let avail = n - pos;
        let (cm, _, nm) = if avail >= 64 {
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
                    let eid = &data[field_starts[0]..field_starts[1].saturating_sub(1)];
                    let uid = &data[field_starts[1]..field_starts[2].saturating_sub(1)];
                    let eid = strip_quotes(eid);
                    let uid = strip_quotes(uid);
                    if !eid.is_empty() && !uid.is_empty() {
                        callback(eid, uid);
                    }
                }
                in_header = false;
                line_start = abs_pos + 1;
                field_starts[0] = line_start;
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
    if !in_header && nf == 7 && line_start < n {
        field_starts[7] = n + 1; // fake end
        let eid = &data[field_starts[0]..field_starts[1].saturating_sub(1)];
        let uid = &data[field_starts[1]..field_starts[2].saturating_sub(1)];
        let eid = strip_quotes(eid);
        let uid = strip_quotes(uid);
        if !eid.is_empty() && !uid.is_empty() {
            callback(eid, uid);
        }
    }
}

#[inline(always)]
fn strip_quotes(s: &[u8]) -> &[u8] {
    if s.len() >= 2 && s[0] == b'"' && s[s.len()-1] == b'"' {
        &s[1..s.len()-1]
    } else {
        s
    }
}
