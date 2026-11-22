// Compare old vs new parser performance
// Run with: cargo run --release --bin compare_parsers -- <data_dir>

mod fast_parse;

use memmap2::Mmap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::Instant;
use xxhash_rust::xxh3::{xxh3_64, xxh3_128};
use memchr::memchr;

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

// Current parser (from main.rs)
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

type S = &'static [u8];

#[inline(always)]
fn for_records_current<const N: usize>(chunk: S, mut f: impl FnMut(&[S; N])) {
    let n = chunk.len();
    if n == 0 { return; }
    let base = chunk.as_ptr();
    let mut bnd = [0u32; N];
    let mut nf: usize = 1;
    let mut lq: u32 = 0;
    let mut qbase: u32 = 0;
    let mut pad = [0u8; 64];
    let mut blk = 0usize;
    let mut out: [S; N] = [&[]; N];
    
    macro_rules! emit { ($end:expr, $qend:expr) => {{
        let end = $end as usize;
        if nf == N {
            let mut quoted = 0u32;
            let mut bad = false;
            for k in 0..N {
                let st = bnd[k] as usize;
                let e = if k + 1 < N { bnd[k + 1] as usize - 1 } else { end };
                let ne = (e > st) as usize;
                let a = ((unsafe { *base.add(st.min(n - 1)) } == b'"') as usize) & ne;
                let b = ((unsafe { *base.add(e.max(1) - 1) } == b'"') as usize) & ne;
                let short = (e - st < 2) as usize;
                bad |= ((a ^ b) | (a & short)) != 0;
                quoted += a as u32;
                let fs = st + a;
                let fe = (e - a).max(fs);
                out[k] = unsafe { std::slice::from_raw_parts(base.add(fs), fe - fs) };
            }
            if !bad && ($qend - lq) == 2 * quoted { f(&out); }
        }
    }}}
    
    while blk < n {
        let avail = n - blk;
        let p = if avail >= 64 { unsafe { base.add(blk) } } else { pad = [0u8; 64]; pad[..avail].copy_from_slice(&chunk[blk..]); pad.as_ptr() };
        let (mut cm, qm, mut nm) = mask3(p);
        if avail < 64 { let lm = (1u64 << avail) - 1; cm &= lm; nm &= lm; }
        let qm = if avail < 64 { qm & ((1u64 << avail) - 1) } else { qm };
        let mut ev = cm | nm;
        while ev != 0 {
            let i = ev.trailing_zeros();
            let bit = 1u64 << i;
            let pos = (blk + i as usize) as u32;
            if nm & bit != 0 {
                let qend = qbase + (qm & (bit - 1)).count_ones();
                emit!(pos, qend);
                bnd[0] = pos + 1;
                nf = 1;
                lq = qend;
            } else {
                if nf < N { bnd[nf] = pos + 1; }
                nf += 1;
            }
            ev &= ev - 1;
        }
        qbase += qm.count_ones();
        blk += 64;
    }
    if (bnd[0] as usize) < n { emit!(n, qbase); }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let data = PathBuf::from(args.get(1).map(|s| s.as_str()).unwrap_or("testdata/5m"));
    
    let ev_p = list_csv(&data.join("events"));
    let mapped: Vec<&'static [u8]> = ev_p.iter().map(|p| map_file(p)).collect();
    let total_bytes: usize = mapped.iter().map(|m| m.len()).sum();
    
    println!("Total event CSV bytes: {} ({:.1} MB)", total_bytes, total_bytes as f64 / (1 << 20) as f64);
    
    // Warm up
    let _ = mapped.iter().map(|m| m.iter().fold(0u64, |a, &b| a.wrapping_add(b as u64))).sum::<u64>();
    
    // 1. Current parser - just field extraction
    let t = Instant::now();
    let mut count1 = 0u64;
    let mut hash_sum1 = 0u64;
    for m in &mapped {
        for_records_current::<7>(*m, |f| {
            count1 += 1;
            hash_sum1 = hash_sum1.wrapping_add(f[0].len() as u64);
        });
    }
    let time1 = t.elapsed();
    println!("1. Current parser (fields only): {:>6.1} ms  [count={} check={}]", 
             time1.as_secs_f64() * 1000.0, count1, hash_sum1);
    
    // 2. Current parser + hash event_id + user_id
    let t = Instant::now();
    let mut count2 = 0u64;
    let mut hash_sum2 = 0u64;
    for m in &mapped {
        for_records_current::<7>(*m, |f| {
            if !f[0].is_empty() && !f[1].is_empty() {
                count2 += 1;
                hash_sum2 = hash_sum2.wrapping_add(xxh3_128(f[0]) as u64);
                hash_sum2 = hash_sum2.wrapping_add(xxh3_64(f[1]));
            }
        });
    }
    let time2 = t.elapsed();
    println!("2. Current + hash eid+uid:       {:>6.1} ms  [count={} hash={}]", 
             time2.as_secs_f64() * 1000.0, count2, hash_sum2);
    
    // 3. Current parser + hash + timestamp parse
    let t = Instant::now();
    let mut count3 = 0u64;
    let mut ts_sum = 0i64;
    for m in &mapped {
        for_records_current::<7>(*m, |f| {
            if !f[0].is_empty() && !f[1].is_empty() {
                if let Some(ts) = fast_parse::parse_ts(f[2]) {
                    if let Some(ing) = fast_parse::parse_ts(f[3]) {
                        count3 += 1;
                        ts_sum = ts_sum.wrapping_add(ts).wrapping_add(ing);
                    }
                }
            }
        });
    }
    let time3 = t.elapsed();
    println!("3. Current + hash + timestamps:  {:>6.1} ms  [count={} ts_sum={}]", 
             time3.as_secs_f64() * 1000.0, count3, ts_sum);
    
    // 4. New fused parser
    let t = Instant::now();
    let mut count4 = 0u64;
    let mut hash_sum4 = 0u64;
    for (fi, m) in mapped.iter().enumerate() {
        let result = fast_parse::parse_events_fast(
            m,
            fi,
            0,
            0,          // ws - accept all
            i64::MAX,   // we 
            0,          // base_ts
            |_| None,   // no product lookup
            0,          // unknown_cat
        );
        count4 += result.events.len() as u64;
        for e in &result.events {
            hash_sum4 = hash_sum4.wrapping_add(e.pk);
        }
    }
    let time4 = t.elapsed();
    println!("4. New fused parser:             {:>6.1} ms  [count={} hash={}]", 
             time4.as_secs_f64() * 1000.0, count4, hash_sum4);
    
    // Summary
    println!("\n=== Summary ===");
    println!("Current parse+hash+ts: {:.1}ms", time3.as_secs_f64() * 1000.0);
    println!("New fused parser:      {:.1}ms", time4.as_secs_f64() * 1000.0);
    println!("Speedup: {:.2}x", time3.as_secs_f64() / time4.as_secs_f64());
}
