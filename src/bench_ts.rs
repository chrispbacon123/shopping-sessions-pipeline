// Benchmark timestamp parsing implementations
// Run with: cargo run --release --bin bench_ts

mod simd_ts;

use std::time::Instant;

const DAY: i64 = 86_400_000;

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

// Current implementation from main.rs
#[inline(always)]
fn parse_f1_fast_current(b: &[u8]) -> Option<i64> {
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

fn main() {
    // Generate test timestamps
    let mut timestamps: Vec<Vec<u8>> = Vec::new();
    
    // F1 format samples
    for y in [2025, 2026, 2027, 2028] {
        for m in 1..=12 {
            for d in 1..=28 {
                for h in [0, 12, 23] {
                    for mi in [0, 30, 59] {
                        for s in [0, 30, 59] {
                            timestamps.push(format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", y, m, d, h, mi, s).into_bytes());
                        }
                    }
                }
            }
        }
    }
    
    // F3 format samples
    for base in [1700000000000i64, 1800000000000, 1900000000000] {
        for off in 0..10000 {
            timestamps.push(format!("{}", base + off * 1000).into_bytes());
        }
    }
    
    println!("Generated {} timestamps", timestamps.len());
    
    // Warmup
    let _ = timestamps.iter().map(|t| simd_ts::parse_ts_fast(t)).count();
    
    // Benchmark current F1 parser
    let t = Instant::now();
    let mut sum1 = 0i64;
    for _ in 0..100 {
        for ts in &timestamps {
            if let Some(v) = parse_f1_fast_current(ts) {
                sum1 = sum1.wrapping_add(v);
            }
        }
    }
    let time1 = t.elapsed();
    println!("Current F1 parser:    {:>6.1} ms  [sum={}]", time1.as_secs_f64() * 1000.0, sum1);
    
    // Benchmark new SIMD parser  
    let t = Instant::now();
    let mut sum2 = 0i64;
    for _ in 0..100 {
        for ts in &timestamps {
            if let Some(v) = simd_ts::parse_f1_swar(ts) {
                sum2 = sum2.wrapping_add(v);
            }
        }
    }
    let time2 = t.elapsed();
    println!("SIMD F1 parser:       {:>6.1} ms  [sum={}]", time2.as_secs_f64() * 1000.0, sum2);
    
    // Benchmark combined parser
    let t = Instant::now();
    let mut sum3 = 0i64;
    for _ in 0..100 {
        for ts in &timestamps {
            if let Some(v) = simd_ts::parse_ts_fast(ts) {
                sum3 = sum3.wrapping_add(v);
            }
        }
    }
    let time3 = t.elapsed();
    println!("Combined parser:      {:>6.1} ms  [sum={}]", time3.as_secs_f64() * 1000.0, sum3);
    
    // Verify correctness
    let mut mismatches = 0;
    for ts in &timestamps {
        let old = parse_f1_fast_current(ts);
        let new = simd_ts::parse_f1_swar(ts);
        if old != new {
            mismatches += 1;
            if mismatches <= 5 {
                println!("Mismatch: {:?} old={:?} new={:?}", 
                    String::from_utf8_lossy(ts), old, new);
            }
        }
    }
    println!("F1 mismatches: {}", mismatches);
    
    println!("\nSpeedup: {:.2}x", time1.as_secs_f64() / time2.as_secs_f64());
}
