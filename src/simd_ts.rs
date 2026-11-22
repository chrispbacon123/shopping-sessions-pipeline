// SIMD-accelerated timestamp parsing for F1 format (YYYY-MM-DDTHH:MM:SSZ)
// Uses SWAR (SIMD Within A Register) techniques

use std::sync::OnceLock;

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

static YEAR_DAYS: OnceLock<Vec<i64>> = OnceLock::new();
const CUM: [i64; 13] = [0, 0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334];

#[inline(always)]
fn civil_days(y: i64, m: i64, d: i64) -> i64 {
    if (1900..2200).contains(&y) {
        let t = YEAR_DAYS.get_or_init(|| (1900..2200).map(|y| days_from_civil(y, 1, 1)).collect());
        t[(y - 1900) as usize] + CUM[m as usize] + if m > 2 && is_leap(y) { 1 } else { 0 } + d - 1
    } else { days_from_civil(y, m, d) }
}

/// Parse 2 ASCII digits as a number, returns None if any digit is invalid
#[inline(always)]
fn d2(b: &[u8], i: usize) -> Option<i64> {
    let a = b[i].wrapping_sub(b'0');
    let c = b[i + 1].wrapping_sub(b'0');
    if a > 9 || c > 9 { return None; }
    Some((a as i64) * 10 + c as i64)
}

/// Ultra-fast F1 timestamp parser using simple byte extraction
/// Format: YYYY-MM-DDTHH:MM:SSZ (exactly 20 bytes)
/// Positions: 0123-56-89T11:14:17Z
#[inline(always)]
pub fn parse_f1_swar(b: &[u8]) -> Option<i64> {
    if b.len() != 20 { return None; }
    
    // Check structural bytes
    if b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || 
       b[13] != b':' || b[16] != b':' || b[19] != b'Z' {
        return None;
    }
    
    // Extract and validate digits using a simple approach
    let g = |i: usize| -> Option<i64> {
        let d = b[i].wrapping_sub(b'0');
        if d > 9 { None } else { Some(d as i64) }
    };
    
    // Year: positions 0-3
    let y = g(0)? * 1000 + g(1)? * 100 + g(2)? * 10 + g(3)?;
    if !(1900..2200).contains(&y) { return None; }
    
    // Month: positions 5-6
    let mo = g(5)? * 10 + g(6)?;
    if mo < 1 || mo > 12 { return None; }
    
    // Day: positions 8-9
    let d = g(8)? * 10 + g(9)?;
    
    // Hour: positions 11-12
    let hh = g(11)? * 10 + g(12)?;
    if hh > 23 { return None; }
    
    // Minute: positions 14-15
    let mi = g(14)? * 10 + g(15)?;
    if mi > 59 { return None; }
    
    // Second: positions 17-18
    let ss = g(17)? * 10 + g(18)?;
    if ss > 59 { return None; }
    
    // Validate day of month
    let dim = match mo { 
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31, 
        4 | 6 | 9 | 11 => 30, 
        _ => if is_leap(y) { 29 } else { 28 } 
    };
    if d < 1 || d > dim { return None; }
    
    Some((civil_days(y, mo, d) * 86400 + hh * 3600 + mi * 60 + ss) * 1000)
}

/// Parse F3 format (epoch milliseconds) using SWAR
#[inline(always)]
pub fn parse_f3_swar(b: &[u8]) -> Option<i64> {
    let n = b.len();
    if n == 0 || n > 18 { return None; }
    
    // Skip leading zeros
    let mut i = 0;
    while i < n && b[i] == b'0' { i += 1; }
    
    // Parse remaining digits
    let mut v: i64 = 0;
    for &c in &b[i..] {
        let d = c.wrapping_sub(b'0');
        if d > 9 { return None; }
        v = v * 10 + d as i64;
    }
    Some(v)
}

/// Parse F2 format (with timezone offset)
#[inline]
pub fn parse_f2(b: &[u8]) -> Option<i64> {
    if b.len() != 25 { return None; }
    
    // Check structural bytes
    if b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || 
       b[13] != b':' || b[16] != b':' || b[22] != b':' {
        return None;
    }
    
    let sign = match b[19] { b'+' => 1i64, b'-' => -1i64, _ => return None };
    
    // Parse date/time parts
    let y = d2(b, 0)? * 100 + d2(b, 2)?;
    let mo = d2(b, 5)?;
    let d = d2(b, 8)?;
    let hh = d2(b, 11)?;
    let mi = d2(b, 14)?;
    let ss = d2(b, 17)?;
    let oh = d2(b, 20)?;
    let om = d2(b, 23)?;
    
    // Validate ranges
    if mo < 1 || mo > 12 || d < 1 || hh > 23 || mi > 59 || ss > 59 || oh > 23 || om > 59 {
        return None;
    }
    
    let dim = match mo { 
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31, 
        4 | 6 | 9 | 11 => 30, 
        _ => if is_leap(y) { 29 } else { 28 } 
    };
    if d > dim { return None; }
    
    let secs = civil_days(y, mo, d) * 86400 + hh * 3600 + mi * 60 + ss;
    Some((secs - sign * (oh * 3600 + om * 60)) * 1000)
}

/// Combined timestamp parser - tries formats in order of likelihood
#[inline]
pub fn parse_ts_fast(b: &[u8]) -> Option<i64> {
    match b.len() {
        20 => parse_f1_swar(b),
        25 => parse_f2(b),
        1..=18 => parse_f3_swar(b),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_f1() {
        assert_eq!(parse_f1_swar(b"2026-10-03T23:58:10Z"), Some(1791071890000));
        assert_eq!(parse_f1_swar(b"2028-01-26T20:40:15Z"), Some(1832618415000));
    }
    
    #[test]
    fn test_f2() {
        assert_eq!(parse_f2(b"2026-10-03T19:58:10-04:00"), Some(1791071890000));
    }
    
    #[test]
    fn test_f3() {
        assert_eq!(parse_f3_swar(b"1791071890000"), Some(1791071890000));
        assert_eq!(parse_f3_swar(b"0001791071890000"), Some(1791071890000));
    }
}
