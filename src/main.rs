mod arena;
mod csv;
mod links;
mod out;
mod pq;
mod sess;
mod util;

use arena::Scatter;
use links::Links;
use sess::{Sess, C2, E2, P3};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Instant;
use util::*;

#[global_allocator]
static GLOBAL: arena::HugeAlloc = arena::HugeAlloc;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Eid {
    a: u64,
    b: u64,
}

const WEEK: i64 = 7 * DAY_MS;
const EMPTY: u64 = u64::MAX;
const NOLOC: u32 = u32::MAX;

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
        let mut maxv = 0usize;
        let mut any = false;
        for (k, _) in entries {
            if let Some(x) = Self::pattern(prefix, k) {
                maxv = maxv.max(x);
                any = true;
            }
        }
        let use_direct = any && maxv < 4_000_000;
        let mut direct = Vec::new();
        let mut map = HashMap::new();
        if use_direct {
            direct = vec![0xFFFFu16; maxv + 1];
        }
        for (k, v) in entries {
            match (use_direct, Self::pattern(prefix, k)) {
                (true, Some(x)) => {
                    if direct[x] == 0xFFFF {
                        direct[x] = *v;
                    }
                }
                _ => {
                    map.entry(k.clone()).or_insert(*v);
                }
            }
        }
        Lookup {
            prefix,
            direct,
            use_direct,
            map,
        }
    }
    #[inline(always)]
    fn get(&self, s: &[u8]) -> Option<u16> {
        if self.use_direct {
            if let Some(x) = Self::pattern(self.prefix, s) {
                if let Some(&v) = self.direct.get(x) {
                    return if v != 0xFFFF { Some(v) } else { None };
                }
            }
        }
        if self.map.is_empty() {
            return None;
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
    shift2: u32,
    pbits: u32,
}

fn list_csv(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = match std::fs::read_dir(dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                !name.starts_with('.') && name.ends_with(".csv") && p.is_file()
            })
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

fn read_small(path: &Path) -> Vec<(Vec<u8>, Vec<u8>)> {
    let m = Mapped::open(path).expect("open small csv");
    let d = m.bytes();
    let mut v = Vec::new();
    csv::scan::<2, _>(d, 0, d.len(), true, |f| v.push((f[0].to_vec(), f[1].to_vec())));
    v
}

struct Out1 {
    ps: Scatter<E2>,
    es: Scatter<Eid>,
    cs: Scatter<C2>,
}

#[inline(always)]
fn ingest_rel(ingest_s: i64, ws_s: i64) -> i32 {
    (ingest_s - ws_s).clamp(i32::MIN as i64 + 1, i32::MAX as i64) as i32
}

fn parse_events(d: &[u8], s: usize, e: usize, cx: &Ctx, w: u32, o: &mut Out1) {
    let (ws, we) = (cx.ws, cx.we);
    let ws_s = ws.div_euclid(1000);
    let shift_u = cx.shift2;
    let mut dc = DateCache::new();
    csv::scan::<7, _>(d, s, e, s == 0, |f| {
        if f[0].is_empty() {
            return;
        }
        let h = hash64(id_key(f[0], &cx.it));
        let ingest = dc.f1_secs(f[3]).unwrap_or(i64::MAX / 4);
        let fpart = (h >> (64 - cx.pbits)) as usize;
        let lowm = (1u64 << (64 - cx.pbits)) - 1;
        let ing = (ingest_rel(ingest, ws_s) as u32 as u64) << 32;
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
            let cat = if price > 0 {
                cx.prod.get(f[5]).unwrap_or(cx.unknown_cat) as u32
            } else {
                0
            };
            let user = id_key(f[1], &cx.it);
            let u = (hash64(user) >> shift_u) as usize;
            let idx = o.ps.push_counted(
                u,
                E2 {
                    person: user,
                    ts: (ts - ws) as u32,
                    pay: price | (cat << 24),
                },
            );
            debug_assert!(idx < (1 << 28));
            let loc = (w << 28) | idx;
            o.es.push(
                fpart,
                Eid {
                    a: (h & lowm) | ((u as u64) << (64 - cx.pbits)),
                    b: ing | loc as u64,
                },
            );
            return;
        }
        // invalid copies still count for dedup
        o.es.push(
            fpart,
            Eid {
                a: h & lowm,
                b: ing | NOLOC as u64,
            },
        );
    });
}

fn parse_clicks(d: &[u8], s: usize, e: usize, cx: &Ctx, out: &mut Scatter<C2>) {
    let lo = cx.ws - WEEK;
    let we = cx.we;
    let shift_u = cx.shift2;
    let mut dc = DateCache::new();
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
        let user = id_key(f[1], &cx.it);
        out.push(
            (hash64(user) >> shift_u) as usize,
            C2 {
                person: user,
                cid: id_key(f[0], &cx.it),
                ts: (ts - lo) as u32,
                camp,
            },
        );
    });
}

fn main() {
    let t0 = Instant::now();
    let data = PathBuf::from(std::env::var("DATA_DIR").unwrap_or_else(|_| "/data".into()));
    let outp = PathBuf::from(std::env::var("OUTPUT_DIR").unwrap_or_else(|_| "/output".into()));
    let nt: usize = std::env::var("NCPU")
        .ok()
        .and_then(|x| x.parse().ok())
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(8, |x| x.get()))
        .clamp(1, 15);
    let timing = std::env::var("TIMING").is_ok();
    let lap = |name: &str| {
        if timing {
            let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
            unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) };
            let cpu = ru.ru_utime.tv_sec as f64 + ru.ru_utime.tv_usec as f64 * 1e-6 + ru.ru_stime.tv_sec as f64 + ru.ru_stime.tv_usec as f64 * 1e-6;
            let anon = std::fs::read_to_string("/proc/self/status")
                .ok()
                .and_then(|t| t.lines().find(|l| l.starts_with("RssAnon:")).and_then(|l| l.split_whitespace().nth(1)).and_then(|v| v.parse::<u64>().ok()))
                .unwrap_or(0);
            eprintln!("{:8.1} ms  cpu {:7.1} ms  flt {:8}  rss {:5} MB  anon {:5} MB  {}", t0.elapsed().as_secs_f64() * 1e3, cpu * 1e3, ru.ru_minflt, ru.ru_maxrss / 1024, anon / 1024, name);
        }
    };

    let man = std::fs::read_to_string(data.join("manifest.json")).expect("manifest");
    let ws = manifest_field(&man, "window_start");
    let we = manifest_field(&man, "window_end");
    assert!(we > ws && we - ws < 40 * DAY_MS, "window length unsupported");

    let ev_files = list_csv(&data.join("events"));
    let ck_files = list_csv(&data.join("clicks"));
    let lk_files = list_csv(&data.join("links"));
    let mapper = {
        let (a, b, c) = (ev_files.clone(), ck_files.clone(), lk_files.clone());
        std::thread::spawn(move || {
            let m = |v: &Vec<PathBuf>| -> Vec<Mapped> { v.iter().map(|p| Mapped::open(p).unwrap()).collect() };
            (m(&a), m(&b), m(&c))
        })
    };

    let mut catnames: Vec<Vec<u8>> = Vec::new();
    let mut ptmp: Vec<(Vec<u8>, u16)> = Vec::new();
    let mut pdir: Vec<(u32, u16)> = Vec::new();
    {
        let m = Mapped::open(&data.join("products.csv")).expect("products.csv");
        let d = m.bytes();
        csv::scan::<2, _>(d, 0, d.len(), true, |f| {
            let ci = match catnames.iter().position(|c| c.as_slice() == f[1]) {
                Some(i) => i,
                None => {
                    catnames.push(f[1].to_vec());
                    catnames.len() - 1
                }
            } as u16;
            match Lookup::pattern(b"p", f[0]) {
                Some(x) if x < 4_000_000 => pdir.push((x as u32, ci)),
                _ => ptmp.push((f[0].to_vec(), ci)),
            }
        });
    }
    if !catnames.iter().any(|c| c.as_slice() == b"unknown") {
        catnames.push(b"unknown".to_vec());
    }
    let mut sorted: Vec<(Vec<u8>, usize)> = catnames.iter().cloned().zip(0..).collect();
    sorted.sort();
    assert!(sorted.len() <= 255, "too many categories");
    let mut rank_of = vec![0u16; sorted.len()];
    for (r, (_, i)) in sorted.iter().enumerate() {
        rank_of[*i] = r as u16;
    }
    let unknown_cat = rank_of[catnames.iter().position(|c| c.as_slice() == b"unknown").unwrap()];
    let catnames: Vec<Vec<u8>> = sorted.into_iter().map(|(c, _)| c).collect();
    let prod = {
        let maxv = pdir.iter().map(|&(x, _)| x as usize).max();
        let mut direct = Vec::new();
        if let Some(mx) = maxv {
            direct = vec![0xFFFFu16; mx + 1];
            for &(x, ci) in &pdir {
                if direct[x as usize] == 0xFFFF {
                    direct[x as usize] = rank_of[ci as usize];
                }
            }
        }
        let mut map = HashMap::new();
        for (k, ci) in ptmp {
            map.entry(k).or_insert(rank_of[ci as usize]);
        }
        Lookup {
            prefix: b"p",
            use_direct: maxv.is_some(),
            direct,
            map,
        }
    };
    let camps = read_small(&data.join("campaigns.csv"));
    let ncamp = camps.len();
    assert!(ncamp < 65000);
    let centries: Vec<(Vec<u8>, u16)> = camps.iter().enumerate().map(|(i, (c, _))| (c.clone(), i as u16)).collect();
    let (ev_maps, ck_maps, lk_maps) = mapper.join().unwrap();
    lap("setup");

    let ev_bytes: usize = ev_maps.iter().map(|m| m.len).sum();
    let est = ev_bytes / 78 + 1;
    // ~40k events per partition; event-id and user partitions share the bit width so an
    // event-id record packs (low hash bits | user partition) into exactly 64 bits
    let pbits = ((est / 40_000).max(1).ilog2()).clamp(4, 11);
    let p2 = 1usize << pbits;
    let nf = p2;
    let shift2 = 64 - pbits;
    let lowm: u64 = (1u64 << (64 - pbits)) - 1;

    let cx = Ctx {
        ws,
        we,
        it: Interner::new(),
        prod,
        camp: Lookup::build(b"c_", &centries),
        unknown_cat,
        shift2,
        pbits,
    };

    // fixed 2 MiB byte ranges; a worker moves both ends to line starts itself, so nothing
    // touches the input before the readahead thread and the workers start
    const CHUNK: usize = 2 << 20;
    let mut tasks: Vec<(u8, u32, usize, usize)> = Vec::new();
    for (k, maps) in [&ev_maps, &ck_maps].into_iter().enumerate() {
        for (i, m) in maps.iter().enumerate() {
            let n = m.bytes().len();
            for a in (0..n).step_by(CHUNK) {
                tasks.push((k as u8, i as u32, a, (a + CHUNK).min(n)));
            }
        }
    }
    let next = AtomicUsize::new(0);
    const GB: usize = 1 << 30;
    let (res1, links): (Vec<Out1>, Links) = std::thread::scope(|sc| {
        // ask the kernel to read ahead in the order the input will be consumed (links first,
        // they are built at the start); costs a page-cache lookup per page when already cached
        {
            let (tasks, ev_maps, ck_maps, lk_maps) = (&tasks, &ev_maps, &ck_maps, &lk_maps);
            sc.spawn(move || {
                let willneed = |d: &[u8], a: usize, b: usize| {
                    let a = a & !4095;
                    if b > a {
                        unsafe { libc::madvise(d.as_ptr().add(a) as *mut libc::c_void, b - a, libc::MADV_WILLNEED) };
                    }
                };
                for m in lk_maps.iter() {
                    willneed(m.bytes(), 0, m.bytes().len());
                }
                for &(k, fi, a, b) in tasks.iter() {
                    let d = if k == 0 { ev_maps[fi as usize].bytes() } else { ck_maps[fi as usize].bytes() };
                    willneed(d, a, b);
                }
            });
        }
        let hs: Vec<_> = (0..nt)
            .map(|w| {
                let (cx, tasks, next, ev_maps, ck_maps, lk_maps) = (&cx, &tasks, &next, &ev_maps, &ck_maps, &lk_maps);
                sc.spawn(move || {
                    // links aren't needed until the user -> person step: worker 0 builds them
                    // while the others parse, so exactly NCPU threads stay busy
                    let lk = if w == 0 { Some(links::build(lk_maps, &cx.it, 1)) } else { None };
                    let mut o = Out1 {
                        ps: Scatter::new(p2, 8 * GB),
                        es: Scatter::new(nf, 8 * GB),
                        cs: Scatter::new(p2, 4 * GB),
                    };
                    loop {
                        let t = next.fetch_add(1, Ordering::Relaxed);
                        if t >= tasks.len() {
                            break;
                        }
                        let (k, fi, a, b) = tasks[t];
                        let d = if k == 0 {
                            ev_maps[fi as usize].bytes()
                        } else {
                            ck_maps[fi as usize].bytes()
                        };
                        // this chunk owns the lines that start inside [a, b)
                        let (s, e) = (csv::line_start(d, a), csv::line_start(d, b));
                        if s >= e {
                            continue;
                        }
                        if k == 0 {
                            parse_events(d, s, e, cx, w as u32, &mut o);
                        } else {
                            parse_clicks(d, s, e, cx, &mut o.cs);
                        }
                        let a = (s + 4095) & !4095;
                        let b = e & !4095;
                        if b > a + (1 << 16) {
                            unsafe { libc::madvise(d.as_ptr().add(a) as *mut libc::c_void, b - a, libc::MADV_DONTNEED) };
                        }
                    }
                    o.ps.finish();
                    o.es.finish();
                    o.cs.finish();
                    (o, lk)
                })
            })
            .collect();
        let mut lk = None;
        let mut r: Vec<Out1> = Vec::with_capacity(nt);
        for h in hs {
            let (o, l) = h.join().unwrap();
            if l.is_some() {
                lk = l;
            }
            r.push(o);
        }
        (r, lk.unwrap())
    });
    lap(&format!("phase1 parse (+links: {} persons)", links.persons));

    // dedup: one bit per parsed event, set for every copy that loses
    let mut doff: Vec<Vec<usize>> = Vec::with_capacity(nt);
    let drop: Vec<Vec<AtomicU64>> = res1
        .iter()
        .map(|o| {
            let mut off = Vec::with_capacity(p2 + 1);
            let mut acc = 0usize;
            for q in 0..p2 {
                off.push(acc);
                acc += (o.ps.cnt[q] as usize + 63) / 64;
            }
            off.push(acc);
            doff.push(off);
            (0..acc).map(|_| AtomicU64::new(0)).collect()
        })
        .collect();
    let next = AtomicUsize::new(0);
    std::thread::scope(|sc| {
        for _ in 0..nt {
            let (res1, next, drop, doff) = (&res1, &next, &drop, &doff);
            sc.spawn(move || {
                let mark = |e: &Eid| {
                    let loc = e.b as u32;
                    if loc != NOLOC {
                        let w = (loc >> 28) as usize;
                        let idx = (loc & ((1 << 28) - 1)) as usize;
                        let q = (e.a >> shift2) as usize;
                        drop[w][doff[w][q] + (idx >> 6)].fetch_or(1u64 << (idx & 63), Ordering::Relaxed);
                    }
                };
                let mut tab: Vec<Eid> = Vec::new();
                loop {
                    let f = next.fetch_add(1, Ordering::Relaxed);
                    if f >= nf {
                        break;
                    }
                    let n: usize = res1.iter().map(|o| o.es.len(f)).sum();
                    if n < 2 {
                        continue;
                    }
                    let size = (n + n / 2).next_power_of_two();
                    let mask = size - 1;
                    tab.clear();
                    tab.resize(size, Eid { a: EMPTY, b: 0 });
                    for o in res1.iter() {
                        for sl in o.es.slices(f) {
                            for e in sl {
                                let key = e.a & lowm;
                                let mut i = (key ^ (key >> 21)) as usize & mask;
                                loop {
                                    let d = unsafe { tab.get_unchecked_mut(i) };
                                    if d.a == EMPTY {
                                        *d = *e;
                                        break;
                                    }
                                    if d.a & lowm == key {
                                        // earliest ingest_ts wins, ties are identical copies
                                        if ((e.b >> 32) as u32 as i32) < ((d.b >> 32) as u32 as i32) {
                                            mark(d);
                                            *d = *e;
                                        } else {
                                            mark(e);
                                        }
                                        break;
                                    }
                                    i = (i + 1) & mask;
                                }
                            }
                        }
                    }
                }
            });
        }
    });
    lap("phase2 dedup");

    let day0 = ws.div_euclid(DAY_MS);
    let nd = ((we - 1).div_euclid(DAY_MS) - day0 + 1) as usize;
    let p3 = P3 { ws, day0, nd, norg: ncamp };
    let ud = outp.join("user_daily");
    let cd = outp.join("campaign_daily");
    std::fs::create_dir_all(&ud).unwrap();
    std::fs::create_dir_all(&cd).unwrap();
    let next = AtomicUsize::new(0);
    let res2: Vec<(Scatter<E2>, Scatter<C2>, Sess)> = std::thread::scope(|sc| {
        let hs: Vec<_> = (0..nt)
            .map(|w| {
                let (res1, next, drop, doff, links, cx, p3) = (&res1, &next, &drop, &doff, &links, &cx, &p3);
                sc.spawn(move || {
                    let mut ev2: Scatter<E2> = Scatter::new(p2, 8 * GB);
                    let mut ck2: Scatter<C2> = Scatter::new(p2, 4 * GB);
                    // the event-id stream is dead after dedup: its memory backs our output
                    for f in 0..nf {
                        res1[w].es.block_ptrs(f, &mut ev2.free);
                    }
                    let mut st = Sess::new(ncamp, nd);
                    let mut lev: Vec<E2> = Vec::new();
                    let mut lck: Vec<C2> = Vec::new();
                    loop {
                        let u = next.fetch_add(1, Ordering::Relaxed);
                        if u >= p2 {
                            break;
                        }
                        lev.clear();
                        lck.clear();
                        for (wi, o) in res1.iter().enumerate() {
                            let bits = &drop[wi][doff[wi][u]..doff[wi][u + 1]];
                            let mut idx = 0usize;
                            for sl in o.ps.slices(u) {
                                for e in sl {
                                    if bits[idx >> 6].load(Ordering::Relaxed) >> (idx & 63) & 1 == 0 {
                                        let person = links.person(e.person);
                                        if person == e.person {
                                            lev.push(*e);
                                        } else {
                                            ev2.push(
                                                (hash64(person) >> shift2) as usize,
                                                E2 {
                                                    person,
                                                    ts: e.ts,
                                                    pay: e.pay,
                                                },
                                            );
                                        }
                                    }
                                    idx += 1;
                                }
                            }
                            for sl in o.cs.slices(u) {
                                for c in sl {
                                    let person = links.person(c.person);
                                    if person == c.person {
                                        lck.push(*c);
                                    } else {
                                        ck2.push((hash64(person) >> shift2) as usize, C2 { person, ..*c });
                                    }
                                }
                            }
                        }
                        // partition u has been read: its blocks are free for reuse
                        for o in res1.iter() {
                            o.ps.block_ptrs(u, &mut ev2.free);
                            o.cs.block_ptrs(u, &mut ck2.free);
                        }
                        st.process(&[&lev[..]], &[&lck[..]], p3, links, &cx.it);
                    }
                    ev2.finish();
                    ck2.finish();
                    (ev2, ck2, st)
                })
            })
            .collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    });
    lap("phase2b persons (+unlinked sessions)");

    // big partitions first so a whale doesn't end up last
    let mut order: Vec<(usize, usize)> = (0..p2).map(|q| (res2.iter().map(|o| o.0.len(q)).sum::<usize>(), q)).collect();
    order.sort_unstable_by(|a, b| b.cmp(a));
    let mut sts: Vec<Option<Sess>> = Vec::with_capacity(nt);
    let mut scat: Vec<(Scatter<E2>, Scatter<C2>)> = Vec::with_capacity(nt);
    for (a, b, st) in res2 {
        scat.push((a, b));
        sts.push(Some(st));
    }
    let next = AtomicUsize::new(0);
    let accs: Vec<Sess> = std::thread::scope(|sc| {
        let hs: Vec<_> = sts
            .iter_mut()
            .enumerate()
            .map(|(w, sto)| {
                let mut st = sto.take().unwrap();
                let (scat, next, links, cx, order, ud, catnames, p3) = (&scat, &next, &links, &cx, &order, &ud, &catnames, &p3);
                sc.spawn(move || {
                    let mut ein: Vec<&[E2]> = Vec::new();
                    let mut cin: Vec<&[C2]> = Vec::new();
                    loop {
                        let oi = next.fetch_add(1, Ordering::Relaxed);
                        if oi >= p2 {
                            break;
                        }
                        let (n0, q) = order[oi];
                        if n0 == 0 {
                            continue;
                        }
                        ein.clear();
                        cin.clear();
                        for o in scat.iter() {
                            ein.extend(o.0.slices(q));
                            cin.extend(o.1.slices(q));
                        }
                        st.process(&ein, &cin, p3, links, &cx.it);
                    }
                    if st.rows.len() > 0 || w == 0 {
                        out::write_user(&ud.join(format!("part-{:03}.parquet", w)), &st.rows, catnames, p3.day0, p3.nd).unwrap();
                    }
                    st.rows = out::UserRows::new();
                    st
                })
            })
            .collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    });
    lap("phase3 sessions+user files");

    let mut tot: Vec<[i64; 4]> = vec![[0; 4]; (ncamp + 1) * nd];
    for a in accs.iter() {
        for (i, t) in tot.iter_mut().enumerate() {
            t[0] += a.a_sess[i] as i64;
            t[1] += a.a_purch[i] as i64;
            t[2] += a.a_rev[i] as i64;
            t[3] += a.a_assist[i] as i64;
        }
    }
    let mut crow = Vec::new();
    for c in 0..=ncamp {
        for d in 0..nd {
            let t = tot[c * nd + d];
            if t[0] > 0 || t[3] > 0 {
                let (id, ch) = if c == ncamp {
                    (b"organic".to_vec(), b"organic".to_vec())
                } else {
                    (camps[c].0.clone(), camps[c].1.clone())
                };
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
    out::write_campaign(&cd.join("part-000.parquet"), &crow, day0, nd).unwrap();
    lap("campaign file");
    unsafe { libc::_exit(0) };
}
