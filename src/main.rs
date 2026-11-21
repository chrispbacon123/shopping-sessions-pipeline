// Shopping Sessions pipeline: single Rust binary, exact rules.md implementation.
use hashbrown::HashMap;
use memchr::memchr;
use memmap2::Mmap;
use rayon::prelude::*;
use std::fs::File;
use std::hash::{BuildHasherDefault, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;
use xxhash_rust::xxh3::xxh3_64;

const DAY: i64 = 86_400_000;
const WEEK: i64 = 7 * DAY;
const NB: usize = 128;
const NBSHIFT: u32 = 64 - 7;
type S = &'static [u8];

// ---------- identity hasher for pre-hashed u64 keys ----------
#[derive(Default)]
struct IdH(u64);
impl Hasher for IdH {
    fn finish(&self) -> u64 { self.0 }
    fn write(&mut self, _: &[u8]) { unreachable!() }
    fn write_u64(&mut self, n: u64) { self.0 = n; }
}
type IdMap<V> = HashMap<u64, V, BuildHasherDefault<IdH>>;

#[inline(always)]
fn hs(b: &[u8]) -> u64 { xxh3_64(b) }
#[inline(always)]
fn bucket(h: u64) -> usize { (h >> NBSHIFT) as usize }

// ---------- CSV ----------
/// Splits one line into exactly N fields per rules.md. Returns false if malformed.
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

/// Returns epoch ms, or None for a bad timestamp. Out-of-range F3 returns i64::MAX (outside any window).
#[inline]
fn parse_ts(b: &[u8]) -> Option<i64> {
    let n = b.len();
    if n == 0 { return None; }
    // Quick check for F3 (epoch milliseconds): first char is digit and no dashes
    let c0 = b[0];
    if c0.is_ascii_digit() && (n < 5 || b[4] != b'-') {
        // F3: all digits
        for &c in b { if !c.is_ascii_digit() { return None; } }
        let mut i = 0;
        while i < n && b[i] == b'0' { i += 1; }
        let r = &b[i..];
        if r.len() > 18 { return Some(i64::MAX); }
        let mut v: i64 = 0;
        for &c in r { v = v * 10 + (c - b'0') as i64; }
        return Some(v);
    }
    if n != 20 && n != 25 { return None; }
    if b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || b[13] != b':' || b[16] != b':' { return None; }
    let y = d2(b, 0)? * 100 + d2(b, 2)?;
    let mo = d2(b, 5)?;
    let d = d2(b, 8)?;
    let hh = d2(b, 11)?;
    let mi = d2(b, 14)?;
    let ss = d2(b, 17)?;
    if !(1..=12).contains(&mo) || d < 1 || hh > 23 || mi > 59 || ss > 59 { return None; }
    let dim = match mo { 1 | 3 | 5 | 7 | 8 | 10 | 12 => 31, 4 | 6 | 9 | 11 => 30, _ => if is_leap(y) { 29 } else { 28 } };
    if d > dim { return None; }
    let mut secs = days_from_civil(y, mo, d) * 86400 + hh * 3600 + mi * 60 + ss;
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

#[inline]
fn parse_price(b: &[u8]) -> Option<u32> {
    if b.is_empty() || !b.iter().all(|c| c.is_ascii_digit()) { return None; }
    let mut i = 0;
    while i < b.len() && b[i] == b'0' { i += 1; }
    let r = &b[i..];
    if r.len() > 8 { return None; }
    let mut v: u32 = 0;
    for &c in r { v = v * 10 + (c - b'0') as u32; }
    if v >= 1 && v <= 10_000_000 { Some(v) } else { None }
}

// ---------- prefix-number lookup tables (p123 / c_123) with string fallback ----------
struct Lookup {
    prefix: &'static [u8],
    dense: Vec<u16>, // u16::MAX = missing
    other: HashMap<S, u16>,
}
impl Lookup {
    fn new(prefix: &'static [u8]) -> Self { Lookup { prefix, dense: Vec::new(), other: HashMap::new() } }
    fn canon(&self, b: &[u8]) -> Option<usize> {
        let p = self.prefix.len();
        if b.len() <= p || b.len() > p + 7 || &b[..p] != self.prefix { return None; }
        let r = &b[p..];
        if r[0] == b'0' || !r.iter().all(|c| c.is_ascii_digit()) { return None; }
        let mut v = 0usize;
        for &c in r { v = v * 10 + (c - b'0') as usize; }
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
        self.other.get(k).copied()
    }
}

// ---------- files & chunks ----------
fn map_file(p: &Path) -> S {
    let f = File::open(p).unwrap_or_else(|e| panic!("open {:?}: {}", p, e));
    let len = f.metadata().unwrap().len();
    if len == 0 { return &[]; }
    let m = unsafe { Mmap::map(&f).unwrap() };
    let _ = m.advise(memmap2::Advice::Sequential);
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
/// Chunk = slice of whole lines (header excluded).
fn chunks(files: &[S], target: usize) -> Vec<S> {
    let mut out = Vec::new();
    for &f in files {
        // skip header line
        let body_start = match memchr(b'\n', f) { Some(i) => i + 1, None => continue };
        let mut s = body_start;
        while s < f.len() {
            let mut e = (s + target).min(f.len());
            if e < f.len() {
                e = match memchr(b'\n', &f[e..]) { Some(i) => e + i + 1, None => f.len() };
            }
            out.push(&f[s..e]);
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

// ---------- records ----------
#[derive(Clone, Copy)]
struct ERaw {
    eh: u64,
    ingest: i64,
    ts: i64,
    uh: u64,
    eid: S,
    uid: S,
    price: u32,
    cat: u16,
    valid: bool,
}
#[derive(Clone, Copy)]
struct Ev { pk: u64, ts: i64, name: S, price: u32, cat: u16 } // price>0 => purchase
#[derive(Clone, Copy)]
struct Cl { pk: u64, ts: i64, cid: S, camp: u16 }

struct URow { name: S, day: i32, sessions: u32, events: u32, rev: u64, top: i32 }

fn new_buckets<T>() -> Vec<Vec<T>> { (0..NB).map(|_| Vec::new()).collect() }

struct People {
    idx: IdMap<u32>,
    info: Vec<(u64, S)>, // per id: (person key, person name)
}
impl People {
    #[inline]
    fn person(&self, uh: u64, uid: S) -> (u64, S) {
        match self.idx.get(&uh) { Some(&i) => self.info[i as usize], None => (uh, uid) }
    }
}

fn build_people(link_files: &[S]) -> People {
    let ch = chunks(link_files, 1 << 20);
    let pairs: Vec<Vec<(u64, S, u64, S)>> = ch.par_iter().map(|&c| {
        let mut v = Vec::with_capacity(c.len() / 20);
        let mut f: [S; 2] = [&[]; 2];
        for_lines(c, |l| {
            if !parse_line(l, &mut f) { return; }
            if f[0].is_empty() || f[1].is_empty() { return; }
            v.push((hs(f[0]), f[0], hs(f[1]), f[1]));
        });
        v
    }).collect();
    let total: usize = pairs.iter().map(|v| v.len()).sum();
    let mut idx: IdMap<u32> = IdMap::with_capacity_and_hasher(total * 2, Default::default());
    let mut names: Vec<S> = Vec::with_capacity(total * 2);
    let mut parent: Vec<u32> = Vec::with_capacity(total * 2);
    fn find(p: &mut [u32], mut x: u32) -> u32 {
        while p[x as usize] != x {
            let g = p[p[x as usize] as usize];
            p[x as usize] = g;
            x = g;
        }
        x
    }
    for v in &pairs {
        for &(ha, a, hb, b) in v {
            let ia = *idx.entry(ha).or_insert_with(|| { names.push(a); parent.push(parent.len() as u32); (names.len() - 1) as u32 });
            let ib = *idx.entry(hb).or_insert_with(|| { names.push(b); parent.push(parent.len() as u32); (names.len() - 1) as u32 });
            let ra = find(&mut parent, ia);
            let rb = find(&mut parent, ib);
            if ra != rb {
                if ra < rb { parent[rb as usize] = ra } else { parent[ra as usize] = rb }
            }
        }
    }
    let n = names.len();
    let mut best: Vec<u32> = (0..n as u32).collect();
    for i in 0..n as u32 {
        let r = find(&mut parent, i);
        if names[i as usize] < names[best[r as usize] as usize] { best[r as usize] = i; }
    }
    let info: Vec<(u64, S)> = (0..n).map(|i| {
        let r = parent[i] as usize; // fully compressed by find above? ensure
        let r = { let mut x = r; while parent[x] as usize != x { x = parent[x] as usize; } x };
        let nm = names[best[r] as usize];
        (hs(nm), nm)
    }).collect();
    People { idx, info }
}

struct Ctx {
    ws: i64,
    we: i64,
    ws_day: i64,
    ndays: usize,
    ncamp: usize, // organic = ncamp
    ncat: usize,
    unknown_cat: u16,
}

struct Acc {
    camp: Vec<[u64; 4]>, // [(camp*ndays+day)] sessions, purchasing, revenue, assisted
    rows: Vec<URow>,
    catsum: Vec<u64>,
    starts: Vec<i64>,
    sdays: Vec<u32>,
    tmp: Vec<(u16, i64)>,
}
impl Acc {
    fn new(c: &Ctx) -> Self {
        Acc { camp: vec![[0; 4]; (c.ncamp + 1) * c.ndays], rows: Vec::new(), catsum: vec![0; c.ncat], starts: Vec::new(), sdays: Vec::new(), tmp: Vec::new() }
    }
}

fn process_bucket(c: &Ctx, evs: &mut Vec<Ev>, cls: &mut Vec<Cl>, acc: &mut Acc) {
    evs.sort_unstable_by(|a, b| (a.pk, a.ts).cmp(&(b.pk, b.ts)));
    cls.sort_unstable_by(|a, b| a.pk.cmp(&b.pk).then(a.ts.cmp(&b.ts)).then_with(|| b.cid.cmp(a.cid)));
    let nd = c.ndays;
    let organic = c.ncamp;
    let mut i = 0;
    let mut j = 0;
    let n = evs.len();
    while i < n {
        let pk = evs[i].pk;
        let name = evs[i].name;
        let mut i1 = i + 1;
        while i1 < n && evs[i1].pk == pk { i1 += 1; }
        while j < cls.len() && cls[j].pk < pk { j += 1; }
        let mut j1 = j;
        while j1 < cls.len() && cls[j1].pk == pk { j1 += 1; }
        let pe = &evs[i..i1];
        let pc = &cls[j..j1];
        acc.starts.clear();
        acc.sdays.clear();
        let mut cp = 0usize;
        // day accumulators
        let mut cur_day: i64 = i64::MIN;
        let (mut d_sess, mut d_ev, mut d_rev) = (0u32, 0u32, 0u64);
        let mut d_purch = false;
        // session accumulators
        let mut s_start: i64 = 0;
        let mut s_rev: u64 = 0;
        let mut s_purch = false;
        let mut s_day: usize = 0;
        let mut prev_ts: i64 = i64::MIN;
        macro_rules! close_session { () => {{
            while cp < pc.len() && pc[cp].ts <= s_start { cp += 1; }
            let camp = if cp > 0 && s_start - pc[cp - 1].ts <= WEEK { pc[cp - 1].camp as usize } else { organic };
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
            if prev_ts == i64::MIN || e.ts - prev_ts > 1_800_000 {
                if prev_ts != i64::MIN { close_session!(); }
                s_start = e.ts;
                s_rev = 0;
                s_purch = false;
                let day = s_start.div_euclid(DAY);
                s_day = (day - c.ws_day) as usize;
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
            if e.price > 0 {
                let p = e.price as u64;
                s_rev += p; s_purch = true;
                d_rev += p; d_purch = true;
                acc.catsum[e.cat as usize] += p;
            }
        }
        close_session!();
        flush_day!();
        // assists
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
                let mut e = a + WEEK;
                k += 1;
                loop {
                    let more = k < t.len() && t[k].0 == camp;
                    if more && t[k].1 <= e {
                        e = t[k].1 + WEEK;
                        k += 1;
                        continue;
                    }
                    // emit [a, e]
                    let mut lo = starts.partition_point(|&s| s < a);
                    while lo < starts.len() && starts[lo] <= e {
                        acc.camp[camp as usize * nd + acc.sdays[lo] as usize][3] += 1;
                        lo += 1;
                    }
                    if !more { break; }
                    a = t[k].1;
                    e = a + WEEK;
                    k += 1;
                }
            }
        }
        i = i1;
        j = j1;
    }
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

    // manifest
    let man = std::fs::read_to_string(data.join("manifest.json")).expect("manifest");
    let get = |k: &str| -> i64 {
        let p = man.find(k).expect("key");
        let rest = &man[p + k.len()..];
        let q1 = rest.find(':').unwrap();
        let rest = &rest[q1 + 1..];
        let a = rest.find('"').unwrap();
        let rest = &rest[a + 1..];
        let b = rest.find('"').unwrap();
        parse_ts(rest[..b].as_bytes()).expect("manifest ts")
    };
    let ws = get("\"window_start\"");
    let we = get("\"window_end\"");
    let ws_day = ws.div_euclid(DAY);
    let ndays = ((we - 1).div_euclid(DAY) - ws_day + 1).max(1) as usize;

    // products
    let prod = map_file(&data.join("products.csv"));
    let mut prods: Vec<(S, S)> = Vec::new();
    for c in chunks(&[prod], usize::MAX / 4) {
        let mut f: [S; 2] = [&[]; 2];
        for_lines(c, |l| if parse_line(l, &mut f) { prods.push((f[0], f[1])) });
    }
    let mut cats: Vec<S> = prods.iter().map(|p| p.1).collect();
    cats.push(b"unknown");
    cats.sort();
    cats.dedup();
    let unknown_cat = cats.iter().position(|&c| c == b"unknown").unwrap() as u16;
    let mut plook = Lookup::new(b"p");
    for &(p, cat) in &prods { plook.insert(p, cats.binary_search(&cat).unwrap() as u16); }
    // campaigns
    let campf = map_file(&data.join("campaigns.csv"));
    let mut camps: Vec<(S, S)> = Vec::new();
    for c in chunks(&[campf], usize::MAX / 4) {
        let mut f: [S; 2] = [&[]; 2];
        for_lines(c, |l| if parse_line(l, &mut f) { camps.push((f[0], f[1])) });
    }
    let mut clook = Lookup::new(b"c_");
    for (i, &(cid, _)) in camps.iter().enumerate() { clook.insert(cid, i as u16); }
    let ctx = Ctx { ws, we, ws_day, ndays, ncamp: camps.len(), ncat: cats.len(), unknown_cat };
    lap("small files");

    let ev_files: Vec<S> = list_csv(&data.join("events")).iter().map(|p| map_file(p)).collect();
    let cl_files: Vec<S> = list_csv(&data.join("clicks")).iter().map(|p| map_file(p)).collect();
    let ln_files: Vec<S> = list_csv(&data.join("links")).iter().map(|p| map_file(p)).collect();

    // Phase 1: links (people) in parallel with events parse
    let (people, ev_parts) = rayon::join(
        || build_people(&ln_files),
        || {
            let ch = chunks(&ev_files, 4 << 20);
            ch.par_iter().fold(new_buckets::<ERaw>, |mut bk, &c| {
                let mut f: [S; 7] = [&[]; 7];
                let (ws, we) = (ctx.ws, ctx.we);
                for_lines(c, |l| {
                    if !parse_line(l, &mut f) { return; }
                    let eid = f[0];
                    if eid.is_empty() { return; }
                    let ingest = parse_ts(f[3]).unwrap_or(i64::MAX);
                    let uid = f[1];
                    let mut valid = !uid.is_empty() && !f[5].is_empty();
                    let ts = match parse_ts(f[2]) { Some(t) if t >= ws && t < we => t, _ => { valid = false; 0 } };
                    let mut price = 0u32;
                    let mut cat = 0u16;
                    if valid {
                        match f[4] {
                            b"view" | b"cart" => {}
                            b"purchase" => match parse_price(f[6]) {
                                Some(p) => { price = p; cat = plook.get(f[5]).unwrap_or(ctx.unknown_cat); }
                                None => valid = false,
                            },
                            _ => valid = false,
                        }
                        if valid && ingest - ts > DAY { valid = false; }
                    }
                    let eh = hs(eid);
                    let uh = if valid { hs(uid) } else { 0 };
                    bk[bucket(eh)].push(ERaw { eh, ingest, ts, uh, eid, uid, price, cat, valid });
                });
                bk
            }).collect::<Vec<_>>()
        },
    );
    lap("links + events parsed");

    // Phase 2: clicks parse (needs people)
    let cl_parts: Vec<Vec<Vec<Cl>>> = {
        let ch = chunks(&cl_files, 4 << 20);
        let cws = ctx.ws - WEEK;
        ch.par_iter().fold(new_buckets::<Cl>, |mut bk, &c| {
            let mut f: [S; 4] = [&[]; 4];
            for_lines(c, |l| {
                if !parse_line(l, &mut f) { return; }
                if f[0].is_empty() || f[1].is_empty() { return; }
                let ts = match parse_ts(f[3]) { Some(t) if t >= cws && t < ctx.we => t, _ => return };
                let camp = match clook.get(f[2]) { Some(c) => c, None => return };
                let (pk, _) = people.person(hs(f[1]), f[1]);
                bk[bucket(pk)].push(Cl { pk, ts, cid: f[0], camp });
            });
            bk
        }).collect()
    };
    lap("clicks parsed");

    // Phase 3: dedup events per eid bucket, map to person buckets
    let ev_by_person: Vec<Vec<Vec<Ev>>> = (0..NB).into_par_iter().fold(new_buckets::<Ev>, |mut out, b| {
        let total: usize = ev_parts.iter().map(|p| p[b].len()).sum();
        let mut best: IdMap<ERaw> = IdMap::with_capacity_and_hasher(total, Default::default());
        let mut coll: Vec<ERaw> = Vec::new();
        for part in &ev_parts {
            for r in &part[b] {
                match best.entry(r.eh) {
                    hashbrown::hash_map::Entry::Vacant(v) => { v.insert(*r); }
                    hashbrown::hash_map::Entry::Occupied(mut o) => {
                        let cur = o.get_mut();
                        if cur.eid != r.eid { coll.push(*r); }
                        else if r.ingest < cur.ingest { *cur = *r; }
                    }
                }
            }
        }
        let mut emit = |r: &ERaw| {
            if !r.valid { return; }
            let (pk, name) = people.person(r.uh, r.uid);
            out[bucket(pk)].push(Ev { pk, ts: r.ts, name, price: r.price, cat: r.cat });
        };
        for r in best.values() { emit(r); }
        if !coll.is_empty() {
            let mut m: HashMap<S, ERaw> = HashMap::new();
            for r in coll {
                let e = m.entry(r.eid).or_insert(r);
                if r.ingest < e.ingest { *e = r; }
            }
            for r in m.values() { emit(r); }
        }
        out
    }).collect();
    drop(ev_parts);
    lap("events deduped");

    // Phase 4: sessionize per person bucket
    let ev_by_person = Arc::new(ev_by_person);
    let accs: Vec<Acc> = (0..NB).into_par_iter().fold(|| Acc::new(&ctx), |mut acc, b| {
        let ne: usize = ev_by_person.iter().map(|p| p[b].len()).sum();
        let mut evs: Vec<Ev> = Vec::with_capacity(ne);
        for p in ev_by_person.iter() { evs.extend_from_slice(&p[b]); }
        let nc: usize = cl_parts.iter().map(|p| p[b].len()).sum();
        let mut cls: Vec<Cl> = Vec::with_capacity(nc);
        for p in cl_parts.iter() { cls.extend_from_slice(&p[b]); }
        process_bucket(&ctx, &mut evs, &mut cls, &mut acc);
        acc
    }).collect();
    lap("sessionized");

    // Phase 5: outputs
    let ud = outd.join("user_daily");
    let cd = outd.join("campaign_daily");
    std::fs::create_dir_all(&ud).unwrap();
    std::fs::create_dir_all(&cd).unwrap();
    let catnames: Vec<&str> = cats.iter().map(|c| std::str::from_utf8(c).unwrap_or("?")).collect();
    rayon::join(
        || {
            accs.par_iter().enumerate().for_each(|(k, a)| {
                out::write_user(&ud.join(format!("part-{:05}.parquet", k)), &a.rows, &catnames);
            });
        },
        || {
            let mut tot = vec![[0u64; 4]; (ctx.ncamp + 1) * ctx.ndays];
            for a in &accs { for (t, x) in tot.iter_mut().zip(a.camp.iter()) { for q in 0..4 { t[q] += x[q]; } } }
            out::write_campaign(&cd.join("part-00000.parquet"), &tot, &camps, ctx.ndays, ctx.ws_day);
        },
    );
    lap("written");
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
            .set_max_row_group_size(1 << 24)
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
        let bytes: usize = rows.iter().map(|r| r.name.len()).sum();
        let mut ub = StringBuilder::with_capacity(rows.len(), bytes);
        let mut tb = StringBuilder::with_capacity(rows.len(), rows.len() * 8);
        for r in rows {
            ub.append_value(s(r.name));
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
        let mut w = ArrowWriter::try_new(std::io::BufWriter::with_capacity(1 << 20, f), schema, Some(props())).unwrap();
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
