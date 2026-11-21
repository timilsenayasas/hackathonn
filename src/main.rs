mod arena;
mod csv;
mod links;
mod out;
mod util;

use arena::Scatter;
use links::Links;

#[global_allocator]
static GLOBAL: arena::HugeAlloc = arena::HugeAlloc;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;
use util::*;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ERec {
    eid: u64,
    ingest: i64,
    user: u64,
    ts: u32, // ms since window_start, NOT_OK if the copy is invalid or late
    pay: u32, // price (24 bits, 0 = not a purchase) | category rank << 24
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CRec {
    user: u64,
    cid: u64,
    ts: u32, // ms since window_start - 7d
    camp: u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct E2 {
    person: u64,
    ts: u32,
    pay: u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct C2 {
    person: u64,
    cid: u64,
    ts: u32,
    camp: u32,
}
#[derive(Clone, Copy, Default)]
struct E3 {
    ts: u32,
    pay: u32,
}
#[derive(Clone, Copy, Default)]
struct C3 {
    cid: u64,
    ts: u32,
    camp: u32,
}

const NOT_OK: u32 = u32::MAX;
const WEEK: i64 = 7 * DAY_MS;
const GAP: u32 = 1_800_000;
const EMPTY: u64 = u64::MAX;

// ---------------------------------------------------------------- lookups for products / campaigns
struct Lookup {
    prefix: &'static [u8],
    direct: Vec<u16>,
    use_direct: bool,
    map: HashMap<Vec<u8>, u16>,
}
impl Lookup {
    #[inline(always)]
    fn pattern(prefix: &[u8], s: &[u8]) -> Option<usize> {
        let pl = prefix.len();
        if s.len() <= pl || s.len() > pl + 9 || &s[..pl] != prefix || s[pl] == b'0' {
            return None;
        }
        let mut v = 0usize;
        for &c in &s[pl..] {
            let d = c.wrapping_sub(b'0');
            if d >= 10 {
                return None;
            }
            v = v * 10 + d as usize;
        }
        Some(v)
    }
    fn build(prefix: &'static [u8], entries: &[(Vec<u8>, u16)]) -> Lookup {
        let mut map = HashMap::new();
        let mut maxv = 0usize;
        let mut any = false;
        for (k, v) in entries {
            map.insert(k.clone(), *v);
            if let Some(x) = Self::pattern(prefix, k) {
                maxv = maxv.max(x);
                any = true;
            }
        }
        let use_direct = any && maxv < 4_000_000;
        let mut direct = Vec::new();
        if use_direct {
            direct = vec![0xFFFFu16; maxv + 1];
            for (k, v) in entries {
                if let Some(x) = Self::pattern(prefix, k) {
                    direct[x] = *v;
                }
            }
        }
        Lookup { prefix, direct, use_direct, map }
    }
    #[inline(always)]
    fn get(&self, s: &[u8]) -> Option<u16> {
        if self.use_direct {
            if let Some(x) = Self::pattern(self.prefix, s) {
                return match self.direct.get(x) {
                    Some(&v) if v != 0xFFFF => Some(v),
                    _ => None,
                };
            }
        }
        self.map.get(s).copied()
    }
}

struct Ctx {
    ws: i64,
    we: i64,
    it: Interner,
    prod: Lookup,
    camp: Lookup,
    unknown_cat: u16,
    shift1: u32,
}

fn list_csv(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = match std::fs::read_dir(dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().map_or(false, |x| x == "csv") && p.is_file())
            .collect(),
        Err(_) => vec![],
    };
    v.sort();
    v
}

fn manifest_field(text: &str, name: &str) -> i64 {
    let at = text.find(&format!("\"{}\"", name)).expect("manifest field");
    let rest = &text[at + name.len() + 2..];
    let q1 = rest.find('"').unwrap();
    let rest = &rest[q1 + 1..];
    let q2 = rest.find('"').unwrap();
    parse_ts(rest[..q2].as_bytes()).expect("manifest timestamp")
}

// ---------------------------------------------------------------- phase 1 parsers
fn parse_events(d: &[u8], s: usize, e: usize, cx: &Ctx, parts: &mut Scatter<ERec>) {
    let (ws, we) = (cx.ws, cx.we);
    let mut dc = DateCache::new();
    csv::scan::<7, _>(d, s, e, s == 0, |f| {
        if f[0].is_empty() {
            return;
        }
        let eid = id_key(f[0], &cx.it);
        let ingest = dc.f1_secs(f[3]).unwrap_or(i64::MAX / 4);
        let mut r = ERec { eid, ingest, user: 0, ts: NOT_OK, pay: 0 };
        'v: {
            if f[1].is_empty() || f[5].is_empty() {
                break 'v;
            }
            let ts = match dc.ts(f[2]) {
                Some(t) => t,
                None => break 'v,
            };
            if ts < ws || ts >= we {
                break 'v;
            }
            let ty = f[4];
            let price = if ty == b"view" || ty == b"cart" {
                0
            } else if ty == b"purchase" {
                match parse_price(f[6]) {
                    Some(p) => p,
                    None => break 'v,
                }
            } else {
                break 'v;
            };
            if ingest * 1000 - ts > DAY_MS {
                break 'v;
            }
            let cat = if price > 0 { cx.prod.get(f[5]).unwrap_or(cx.unknown_cat) as u32 } else { 0 };
            r.user = id_key(f[1], &cx.it);
            r.ts = (ts - ws) as u32;
            r.pay = price | (cat << 24);
        }
        let p = (hash64(eid) >> cx.shift1) as usize;
        parts.push(p, r);
    });
}

fn parse_clicks(d: &[u8], s: usize, e: usize, cx: &Ctx, out: &mut Scatter<CRec>) {
    let lo = cx.ws - WEEK;
    let mut dc = DateCache::new();
    let we = cx.we;
    csv::scan::<4, _>(d, s, e, s == 0, |f| {
        if f[0].is_empty() || f[1].is_empty() {
            return;
        }
        let ts = match dc.ts(f[3]) {
            Some(t) => t,
            None => return,
        };
        if ts < lo || ts >= we {
            return;
        }
        let camp = match cx.camp.get(f[2]) {
            Some(c) => c as u32,
            None => return,
        };
        out.push(0, CRec { user: id_key(f[1], &cx.it), cid: id_key(f[0], &cx.it), ts: (ts - lo) as u32, camp });
    });
}

fn read_small(path: &Path) -> Vec<(Vec<u8>, Vec<u8>)> {
    let m = Mapped::open(path).expect("open small csv");
    let d = m.bytes();
    let mut v = Vec::new();
    csv::scan::<2, _>(d, 0, d.len(), true, |f| v.push((f[0].to_vec(), f[1].to_vec())));
    v
}

struct SendPtr<T>(*mut T);
unsafe impl<T> Send for SendPtr<T> {}
unsafe impl<T> Sync for SendPtr<T> {}

fn main() {
    let t0 = Instant::now();
    let args: Vec<String> = std::env::args().collect();
    let data_dir = std::env::var("DATA_DIR").unwrap_or_else(|_| "/data".into());
    let out_dir = std::env::var("OUTPUT_DIR").unwrap_or_else(|_| "/output".into());
    let _ = args;
    let data = PathBuf::from(data_dir);
    let outp = PathBuf::from(out_dir);
    let nt: usize = std::env::var("NCPU")
        .ok()
        .and_then(|x| x.parse().ok())
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(8, |x| x.get()))
        .max(1);
    let verbose = std::env::var("FS_VERBOSE").is_ok();
    let cpu = || unsafe {
        let mut ru: libc::rusage = std::mem::zeroed();
        libc::getrusage(libc::RUSAGE_SELF, &mut ru);
        (ru.ru_utime.tv_sec as f64 + ru.ru_utime.tv_usec as f64 * 1e-6) + (ru.ru_stime.tv_sec as f64 + ru.ru_stime.tv_usec as f64 * 1e-6)
    };
    let lap = |name: &str| {
        if verbose {
            eprintln!("{:>8.1} ms  cpu {:>7.1} ms  {}", t0.elapsed().as_secs_f64() * 1e3, cpu() * 1e3, name)
        }
    };

    let man = std::fs::read_to_string(data.join("manifest.json")).expect("manifest");
    let ws = manifest_field(&man, "window_start");
    let we = manifest_field(&man, "window_end");
    assert!(we > ws && we - ws < 40 * DAY_MS, "window length unsupported");

    // products / categories
    let prods = read_small(&data.join("products.csv"));
    let mut catnames: Vec<Vec<u8>> = prods.iter().map(|(_, c)| c.clone()).collect();
    catnames.push(b"unknown".to_vec());
    catnames.sort();
    catnames.dedup();
    assert!(catnames.len() <= 255, "too many categories");
    let catrank: HashMap<Vec<u8>, u16> = catnames.iter().enumerate().map(|(i, c)| (c.clone(), i as u16)).collect();
    let unknown_cat = catrank[&b"unknown".to_vec()];
    let pentries: Vec<(Vec<u8>, u16)> = prods.iter().map(|(p, c)| (p.clone(), catrank[c])).collect();
    // campaigns
    let camps = read_small(&data.join("campaigns.csv"));
    let ncamp = camps.len();
    assert!(ncamp < 65000);
    let centries: Vec<(Vec<u8>, u16)> = camps.iter().enumerate().map(|(i, (c, _))| (c.clone(), i as u16)).collect();

    let ev_files = list_csv(&data.join("events"));
    let ck_files = list_csv(&data.join("clicks"));
    let lk_files = list_csv(&data.join("links"));
    let ev_maps: Vec<Mapped> = ev_files.iter().map(|p| Mapped::open(p).unwrap()).collect();
    let ck_maps: Vec<Mapped> = ck_files.iter().map(|p| Mapped::open(p).unwrap()).collect();
    let lk_maps: Vec<Mapped> = lk_files.iter().map(|p| Mapped::open(p).unwrap()).collect();
    lap("mmap");

    let ev_bytes: usize = ev_maps.iter().map(|m| m.len).sum();
    let est = ev_bytes / 78 + 1;
    let envp = |k: &str| std::env::var(k).ok().and_then(|x| x.parse::<usize>().ok());
    let p1 = envp("FS_P1").unwrap_or((est / 16384).next_power_of_two().clamp(16, 4096));
    let p2 = envp("FS_P2").unwrap_or(p1);
    let shift1 = 64 - p1.trailing_zeros();
    let shift2 = 64 - p2.trailing_zeros();

    let cx = Ctx {
        ws,
        we,
        it: Interner::new(),
        prod: Lookup::build(b"p", &pentries),
        camp: Lookup::build(b"c_", &centries),
        unknown_cat,
        shift1,
    };

    // ---------------------------------------------------------------- phase 1: parse
    let mut tasks: Vec<(u8, u32, usize, usize)> = Vec::new();
    for (i, m) in ck_maps.iter().enumerate() {
        for (s, e) in csv::chunks(m.bytes(), 2 << 20) {
            tasks.push((1, i as u32, s, e));
        }
    }
    for (i, m) in ev_maps.iter().enumerate() {
        for (s, e) in csv::chunks(m.bytes(), 2 << 20) {
            tasks.push((0, i as u32, s, e));
        }
    }
    let next = AtomicUsize::new(0);
    const GB: usize = 1 << 30;
    let res1: Vec<(Scatter<ERec>, Scatter<CRec>, Option<Links>)> = std::thread::scope(|sc| {
        let hs: Vec<_> = (0..nt)
            .map(|w| {
                let (cx, tasks, next, ev_maps, ck_maps, lk_maps) = (&cx, &tasks, &next, &ev_maps, &ck_maps, &lk_maps);
                sc.spawn(move || {
                    let links = if w == 0 {
                        let t = Instant::now();
                        let l = links::build(lk_maps, &cx.it, 2);
                        if verbose {
                            eprintln!("links: {:.1} ms, {} persons", t.elapsed().as_secs_f64() * 1e3, l.persons);
                        }
                        Some(l)
                    } else {
                        None
                    };
                    let mut parts: Scatter<ERec> = Scatter::new(p1, 512, 8 * GB);
                    let mut cks: Scatter<CRec> = Scatter::new(1, 1 << 14, 4 * GB);
                    loop {
                        let t = next.fetch_add(1, Ordering::Relaxed);
                        if t >= tasks.len() {
                            break;
                        }
                        let (k, fi, s, e) = tasks[t];
                        if k == 0 {
                            parse_events(ev_maps[fi as usize].bytes(), s, e, cx, &mut parts);
                        } else {
                            parse_clicks(ck_maps[fi as usize].bytes(), s, e, cx, &mut cks);
                        }
                    }
                    parts.finish();
                    cks.finish();
                    (parts, cks, links)
                })
            })
            .collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    });
    lap("phase1 parse");
    let mut res1 = res1;
    let links = res1[0].2.take().unwrap();

    // ---------------------------------------------------------------- phase 2: dedup + resolve persons + scatter
    // click work items: (worker, block slice)
    let click_items: Vec<(usize, usize)> = (0..nt).flat_map(|w| (0..res1[w].1.slices(0).count()).map(move |b| (w, b))).collect();
    let ntasks2 = p1 + click_items.len();
    let next = AtomicUsize::new(0);
    let res2: Vec<(Scatter<E2>, Scatter<C2>)> = std::thread::scope(|sc| {
        let hs: Vec<_> = (0..nt)
            .map(|_| {
                let (res1, next, links, click_items) = (&res1, &next, &links, &click_items);
                sc.spawn(move || {
                    let mut ev2: Scatter<E2> = Scatter::new(p2, 1024, 8 * GB);
                    let mut ck2: Scatter<C2> = Scatter::new(p2, 512, 4 * GB);
                    let mut tab: Vec<ERec> = Vec::new();
                    const B: usize = 32; // prefetch distance (ring)
                    let mut ring: [(u64, u32, u32, usize); B] = [(0, 0, 0, 0); B];
                    let mut nb = 0usize; // total pushed into ring
                    let lm = links;
                    #[inline(always)]
                    fn emit(lm: &Links, ev2: &mut Scatter<E2>, shift2: u32, (u, ts, pay, at): (u64, u32, u32, usize)) {
                        let person = lm.person_at(u, at);
                        let q = (hash64(person) >> shift2) as usize;
                        ev2.push(q, E2 { person, ts, pay });
                    }
                    loop {
                        let t = next.fetch_add(1, Ordering::Relaxed);
                        if t >= ntasks2 {
                            break;
                        }
                        if t < p1 {
                            let p = t;
                            let n: usize = res1.iter().map(|r| r.0.len(p)).sum();
                            if n == 0 {
                                continue;
                            }
                            let size = (n + n / 2).next_power_of_two();
                            let mask = size - 1;
                            tab.clear();
                            tab.resize(size, ERec { eid: EMPTY, ..Default::default() });
                            for r1 in res1.iter() {
                                for sl in r1.0.slices(p) {
                                    for r in sl {
                                        let mut i = hash64b(r.eid) as usize & mask;
                                        loop {
                                            let d = unsafe { tab.get_unchecked_mut(i) };
                                            if d.eid == EMPTY {
                                                *d = *r;
                                                break;
                                            }
                                            if d.eid == r.eid {
                                                if r.ingest < d.ingest {
                                                    *d = *r;
                                                }
                                                break;
                                            }
                                            i = (i + 1) & mask;
                                        }
                                    }
                                }
                            }
                            for r in tab.iter() {
                                if r.eid != EMPTY && r.ts != NOT_OK {
                                    let at = lm.locate(r.user);
                                    lm.prefetch_at(at);
                                    let slot = nb % B;
                                    if nb >= B {
                                        emit(lm, &mut ev2, shift2, ring[slot]);
                                    }
                                    ring[slot] = (r.user, r.ts, r.pay, at);
                                    nb += 1;
                                }
                            }
                        } else {
                            let (w, bi) = click_items[t - p1];
                            let sl = res1[w].1.slices(0).nth(bi).unwrap();
                            for chunk in sl.chunks(32) {
                                for c in chunk {
                                    lm.prefetch(c.user);
                                }
                                for c in chunk {
                                    let person = lm.person(c.user);
                                    let q = (hash64(person) >> shift2) as usize;
                                    ck2.push(q, C2 { person, cid: c.cid, ts: c.ts, camp: c.camp });
                                }
                            }
                        }
                    }
                    let start = nb.saturating_sub(B);
                    for i in start..nb {
                        emit(lm, &mut ev2, shift2, ring[i % B]);
                    }
                    ev2.finish();
                    ck2.finish();
                    (ev2, ck2)
                })
            })
            .collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    });
    lap("phase2 dedup+scatter");
    std::mem::forget(res1);

    // ---------------------------------------------------------------- phase 3: sessionize + attribute + aggregate
    let day0 = ws.div_euclid(DAY_MS);
    let nd = ((we - 1).div_euclid(DAY_MS) - day0 + 1) as usize;
    let norg = ncamp; // organic index
    let next = AtomicUsize::new(0);
    let res3: Vec<(out::UserRows, Vec<[i64; 4]>)> = std::thread::scope(|sc| {
        let hs: Vec<_> = (0..nt)
            .map(|_| {
                let (res2, next, links, cx) = (&res2, &next, &links, &cx);
                sc.spawn(move || {
                    let mut rows = out::UserRows::new();
                    let mut acc: Vec<[i64; 4]> = vec![[0; 4]; (ncamp + 1) * nd];
                    let mut mk: Vec<u64> = Vec::new();
                    let mut mv: Vec<u32> = Vec::new();
                    let mut li: Vec<u32> = Vec::new();
                    let mut pkeys: Vec<u64> = Vec::new();
                    let mut cnt: Vec<u32> = Vec::new();
                    let mut ccnt: Vec<u32> = Vec::new();
                    let mut e3: Vec<E3> = Vec::new();
                    let mut c3: Vec<C3> = Vec::new();
                    let mut cli: Vec<u32> = Vec::new();
                    let mut cur_a: Vec<u64> = vec![0; ncamp + 1];
                    let mut cur_b: Vec<u64> = vec![u64::MAX; ncamp + 1];
                    let mut touched: Vec<u32> = Vec::new();
                    let mut sstart: Vec<u64> = Vec::new();
                    let mut sday: Vec<u32> = Vec::new();
                    let mut snext: Vec<u32> = Vec::new();
                    let mut catsum: Vec<u64> = vec![0; 256];
                    let mut cat_touched: Vec<u8> = Vec::new();
                    let mut name: Vec<u8> = Vec::new();
                    let it = &cx.it;
                    loop {
                        let q = next.fetch_add(1, Ordering::Relaxed);
                        if q >= p2 {
                            break;
                        }
                        let n: usize = res2.iter().map(|r| r.0.len(q)).sum();
                        if n == 0 {
                            continue;
                        }
                        // person map: sized for ~n/8 persons, doubled when 3/4 full
                        let mut size = (n / 8).next_power_of_two().max(256);
                        let mut mask = size - 1;
                        mk.clear();
                        mk.resize(size, EMPTY);
                        mv.clear();
                        mv.resize(size, 0);
                        li.clear();
                        pkeys.clear();
                        cnt.clear();
                        for e in res2.iter().flat_map(|r| r.0.slices(q)).flatten() {
                            let mut i = hash64b(e.person) as usize & mask;
                            let l = loop {
                                let k = unsafe { *mk.get_unchecked(i) };
                                if k == e.person {
                                    break unsafe { *mv.get_unchecked(i) };
                                }
                                if k == EMPTY {
                                    let l = pkeys.len() as u32;
                                    pkeys.push(e.person);
                                    cnt.push(0);
                                    if pkeys.len() * 4 >= size * 3 {
                                        // grow and re-insert everything (indices are stable)
                                        size *= 2;
                                        mask = size - 1;
                                        mk.clear();
                                        mk.resize(size, EMPTY);
                                        mv.clear();
                                        mv.resize(size, 0);
                                        for (x, &pk) in pkeys.iter().enumerate() {
                                            let mut j = hash64b(pk) as usize & mask;
                                            while mk[j] != EMPTY {
                                                j = (j + 1) & mask;
                                            }
                                            mk[j] = pk;
                                            mv[j] = x as u32;
                                        }
                                    } else {
                                        mk[i] = e.person;
                                        mv[i] = l;
                                    }
                                    break l;
                                }
                                i = (i + 1) & mask;
                            };
                            cnt[l as usize] += 1;
                            li.push(l);
                        }
                        let np = pkeys.len();
                        // offsets
                        let mut off: Vec<u32> = Vec::with_capacity(np + 1);
                        let mut s = 0u32;
                        for &c in &cnt {
                            off.push(s);
                            s += c;
                        }
                        off.push(s);
                        e3.clear();
                        e3.resize(n, E3::default());
                        {
                            let mut fill: Vec<u32> = off[..np].to_vec();
                            let mut j = 0;
                            for e in res2.iter().flat_map(|r| r.0.slices(q)).flatten() {
                                {
                                    let l = li[j] as usize;
                                    j += 1;
                                    e3[fill[l] as usize] = E3 { ts: e.ts, pay: e.pay };
                                    fill[l] += 1;
                                }
                            }
                        }
                        // clicks
                        ccnt.clear();
                        ccnt.resize(np, 0);
                        cli.clear();
                        let mut nc = 0usize;
                        for c in res2.iter().flat_map(|r| r.1.slices(q)).flatten() {
                            {
                                let mut i = hash64b(c.person) as usize & mask;
                                let l = loop {
                                    let k = mk[i];
                                    if k == c.person {
                                        break mv[i];
                                    }
                                    if k == EMPTY {
                                        break u32::MAX;
                                    }
                                    i = (i + 1) & mask;
                                };
                                cli.push(l);
                                if l != u32::MAX {
                                    ccnt[l as usize] += 1;
                                    nc += 1;
                                }
                            }
                        }
                        let mut coff: Vec<u32> = Vec::with_capacity(np + 1);
                        let mut s = 0u32;
                        for &c in &ccnt {
                            coff.push(s);
                            s += c;
                        }
                        coff.push(s);
                        c3.clear();
                        c3.resize(nc, C3::default());
                        {
                            let mut fill: Vec<u32> = coff[..np].to_vec();
                            let mut j = 0;
                            for c in res2.iter().flat_map(|r| r.1.slices(q)).flatten() {
                                {
                                    let l = cli[j];
                                    j += 1;
                                    if l != u32::MAX {
                                        let l = l as usize;
                                        c3[fill[l] as usize] = C3 { cid: c.cid, ts: c.ts, camp: c.camp };
                                        fill[l] += 1;
                                    }
                                }
                            }
                        }
                        // per person
                        for l in 0..np {
                            let ev = &mut e3[off[l] as usize..off[l + 1] as usize];
                            ev.sort_unstable_by_key(|e| e.ts);
                            let ck = &mut c3[coff[l] as usize..coff[l + 1] as usize];
                            ck.sort_unstable_by(|a, b| a.ts.cmp(&b.ts).then_with(|| cmp_ids(a.cid, b.cid, it)));
                            let pk = pkeys[l];
                            name.clear();
                            if pk & EXOTIC == 0 && pk & LINKED != 0 {
                                id_bytes(links.min_key[(pk & !LINKED) as usize], it, &mut name);
                            } else {
                                id_bytes(pk, it, &mut name);
                            }
                            let mut hi = 0usize;
                            let mut cur_day: i64 = -1;
                            let (mut d_sess, mut d_ev, mut d_rev) = (0i64, 0i64, 0i64);
                            let mut i = 0;
                            sstart.clear();
                            sday.clear();
                            let flush = |rows: &mut out::UserRows,
                                         cur_day: i64,
                                         d_sess: i64,
                                         d_ev: i64,
                                         d_rev: i64,
                                         catsum: &mut Vec<u64>,
                                         cat_touched: &mut Vec<u8>| {
                                rows.user.push(&name);
                                rows.day.push((day0 + cur_day) as i32);
                                rows.sessions.push(d_sess);
                                rows.events.push(d_ev);
                                rows.revenue.push(d_rev);
                                let mut best: i32 = -1;
                                let mut bv = 0u64;
                                for &c in cat_touched.iter() {
                                    let v = catsum[c as usize];
                                    if v > bv || (v == bv && (c as i32) < best) {
                                        bv = v;
                                        best = c as i32;
                                    }
                                    catsum[c as usize] = 0;
                                }
                                cat_touched.clear();
                                rows.topcat.push(best);
                            };
                            while i < ev.len() {
                                let s0 = ev[i].ts;
                                let mut prev = s0;
                                let mut j = i;
                                let mut rev = 0i64;
                                let mut purch = false;
                                while j < ev.len() && ev[j].ts - prev <= GAP {
                                    prev = ev[j].ts;
                                    let pay = ev[j].pay;
                                    let price = pay & 0xFF_FFFF;
                                    if price > 0 {
                                        purch = true;
                                        rev += price as i64;
                                        let c = (pay >> 24) as usize;
                                        if catsum[c] == 0 {
                                            cat_touched.push(c as u8);
                                        }
                                        catsum[c] += price as u64;
                                    }
                                    j += 1;
                                }
                                let nev = (j - i) as i64;
                                let day = (ws + s0 as i64).div_euclid(DAY_MS) - day0;
                                // credit: latest click with sc-7d <= ts <= sc (click frame = +7d), ties -> smallest click_id
                                let sc = s0 as u64 + WEEK as u64;
                                while hi < ck.len() && (ck[hi].ts as u64) <= sc {
                                    hi += 1;
                                }
                                let credited = if hi > 0 && (ck[hi - 1].ts as u64) >= s0 as u64 {
                                    let t = ck[hi - 1].ts;
                                    let mut k = hi - 1;
                                    while k > 0 && ck[k - 1].ts == t {
                                        k -= 1;
                                    }
                                    ck[k].camp as usize
                                } else {
                                    norg
                                };
                                let du = day as usize;
                                sstart.push(sc);
                                sday.push(du as u32);
                                let a = &mut acc[credited * nd + du];
                                a[0] += 1;
                                if purch {
                                    a[1] += 1;
                                }
                                a[2] += rev;
                                if day != cur_day {
                                    cur_day = day;
                                    d_sess = 0;
                                    d_ev = 0;
                                    d_rev = 0;
                                }
                                d_sess += 1;
                                d_ev += nev;
                                d_rev += rev;
                                // flush when the next session starts another day (or no more sessions)
                                let next_day = if j < ev.len() { (ws + ev[j].ts as i64).div_euclid(DAY_MS) - day0 } else { -1 };
                                if next_day != cur_day {
                                    flush(&mut rows, cur_day, d_sess, d_ev, d_rev, &mut catsum, &mut cat_touched);
                                }
                                i = j;
                            }
                            // assists: per campaign, merge click windows [t, t+7d] (click frame) and count
                            // the sessions per day whose start falls inside each merged window.
                            if !ck.is_empty() {
                                let ns = sstart.len();
                                snext.clear();
                                snext.resize(ns, ns as u32);
                                for x in (0..ns.saturating_sub(1)).rev() {
                                    snext[x] = if sday[x + 1] != sday[x] { (x + 1) as u32 } else { snext[x + 1] };
                                }
                                let mut add = |c: usize, a: u64, b: u64, acc: &mut Vec<[i64; 4]>| {
                                    let lo = sstart.partition_point(|&x| x < a);
                                    let hi = sstart.partition_point(|&x| x <= b);
                                    let mut x = lo;
                                    while x < hi {
                                        let y = (snext[x] as usize).min(hi);
                                        acc[c * nd + sday[x] as usize][3] += (y - x) as i64;
                                        x = y;
                                    }
                                };
                                for c in ck.iter() {
                                    let cc = c.camp as usize;
                                    let t = c.ts as u64;
                                    if cur_b[cc] == u64::MAX {
                                        touched.push(cc as u32);
                                        cur_a[cc] = t;
                                    } else if t > cur_b[cc] {
                                        add(cc, cur_a[cc], cur_b[cc], &mut acc);
                                        cur_a[cc] = t;
                                    }
                                    cur_b[cc] = t + WEEK as u64;
                                }
                                for &cc in touched.iter() {
                                    let cc = cc as usize;
                                    add(cc, cur_a[cc], cur_b[cc], &mut acc);
                                    cur_b[cc] = u64::MAX;
                                }
                                touched.clear();
                            }
                        }
                    }
                    (rows, acc)
                })
            })
            .collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    });
    lap("phase3 sessions");

    // ---------------------------------------------------------------- phase 4: write
    let ud = outp.join("user_daily");
    let cd = outp.join("campaign_daily");
    std::fs::create_dir_all(&ud).unwrap();
    std::fs::create_dir_all(&cd).unwrap();
    std::thread::scope(|sc| {
        for (w, (rows, _)) in res3.iter().enumerate() {
            if rows.day.is_empty() && w != 0 {
                continue;
            }
            let ud = &ud;
            let catnames = &catnames;
            sc.spawn(move || {
                out::write_user(&ud.join(format!("part-{:03}.parquet", w)), rows, catnames).unwrap();
            });
        }
        let mut tot: Vec<[i64; 4]> = vec![[0; 4]; (ncamp + 1) * nd];
        for (_, a) in res3.iter() {
            for (t, x) in tot.iter_mut().zip(a.iter()) {
                for k in 0..4 {
                    t[k] += x[k];
                }
            }
        }
        let mut crow = Vec::new();
        for c in 0..=ncamp {
            for d in 0..nd {
                let t = tot[c * nd + d];
                if t[0] > 0 || t[3] > 0 {
                    let (id, ch) = if c == ncamp { (b"organic".to_vec(), b"organic".to_vec()) } else { (camps[c].0.clone(), camps[c].1.clone()) };
                    crow.push(out::CampRow {
                        campaign: id,
                        channel: ch,
                        day: (day0 + d as i64) as i32,
                        sessions: t[0],
                        purchasing: t[1],
                        revenue: t[2],
                        assisted: if c == ncamp { 0 } else { t[3] },
                    });
                }
            }
        }
        out::write_campaign(&cd.join("part-000.parquet"), &crow).unwrap();
    });
    lap("phase4 write");
    unsafe { libc::_exit(0) };
}
