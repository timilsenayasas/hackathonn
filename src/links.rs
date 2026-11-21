use crate::csv;
use crate::util::*;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::Mutex;

pub const EMPTY: u64 = u64::MAX;

#[derive(Clone, Copy)]
#[repr(C, align(16))]
pub struct Slot {
    pub k: u64,
    pub v: u32,
    _p: u32,
}
pub struct Links {
    slots: Vec<Slot>,
    smask: usize,
    sbits_slots: u32,
    sshift: u32,
    pub min_key: Vec<u64>,
    pub persons: usize,
}

impl Links {
    #[inline(always)]
    pub fn locate(&self, k: u64) -> usize {
        let h = hash64(k);
        (((h >> self.sshift) as usize) << self.sbits_slots) | ((h as usize & self.smask) << 2)
    }
    #[inline(always)]
    pub fn person(&self, k: u64) -> u64 {
        self.person_at(k, self.locate(k))
    }
    #[inline(always)]
    pub fn person_at(&self, k: u64, at: usize) -> u64 {
        let base = at & !((1usize << self.sbits_slots) - 1);
        let mut b = (at >> 2) & self.smask;
        loop {
            let s = unsafe { self.slots.as_ptr().add(base + (b << 2)) };
            let (k0, k1, k2, k3) = unsafe { ((*s).k, (*s.add(1)).k, (*s.add(2)).k, (*s.add(3)).k) };
            let m = (k0 == k) as u32 | ((k1 == k) as u32) << 1 | ((k2 == k) as u32) << 2 | ((k3 == k) as u32) << 3;
            let e = (k0 == EMPTY) as u32 | ((k1 == EMPTY) as u32) << 1 | ((k2 == EMPTY) as u32) << 2 | ((k3 == EMPTY) as u32) << 3;
            let j = (m | 16).trailing_zeros() as usize & 3;
            let v = unsafe { (*s.add(j)).v } as u64;
            if (m | e) != 0 {
                return std::hint::select_unpredictable(m != 0, LINKED | v, k);
            }
            b = (b + 1) & self.smask;
        }
    }
}

#[inline]
fn find(p: &[AtomicU32], mut x: u32) -> u32 {
    loop {
        let px = p[x as usize].load(Relaxed);
        if px == x {
            return x;
        }
        let gp = p[px as usize].load(Relaxed);
        if gp != px {
            p[x as usize].store(gp, Relaxed);
        }
        x = gp;
    }
}

#[inline]
fn union(p: &[AtomicU32], a: u32, b: u32) {
    loop {
        let ra = find(p, a);
        let rb = find(p, b);
        if ra == rb {
            return;
        }
        let (hi, lo) = if ra > rb { (ra, rb) } else { (rb, ra) };
        if p[hi as usize].compare_exchange(hi, lo, Relaxed, Relaxed).is_ok() {
            return;
        }
    }
}

fn par<F: Fn(usize) + Sync>(nt: usize, ntasks: usize, f: F) {
    let next = AtomicUsize::new(0);
    std::thread::scope(|sc| {
        for _ in 0..nt.min(ntasks.max(1)) {
            sc.spawn(|| loop {
                let t = next.fetch_add(1, Relaxed);
                if t >= ntasks {
                    break;
                }
                f(t);
            });
        }
    });
}

pub fn build(maps: &[Mapped], it: &Interner, nt: usize) -> Links {
    let t0 = std::time::Instant::now();
    let vb = std::env::var("FS_VERBOSE").is_ok();
    let lap = |n: &str| {
        if vb {
            eprintln!("   links {:>7.1} ms {}", t0.elapsed().as_secs_f64() * 1e3, n)
        }
    };
    let s_count: usize = 256;
    let sbits = s_count.trailing_zeros();
    let sshift = 64 - sbits;
    let mut tasks: Vec<(usize, usize, usize)> = Vec::new();
    for (i, m) in maps.iter().enumerate() {
        for (s, e) in csv::chunks(m.bytes(), 1 << 20) {
            tasks.push((i, s, e));
        }
    }
    struct L1 {
        edges: Vec<(u64, u64)>,
        // endpoints grouped by shard: shard s is flat[off[s]..off[s + 1]]
        flat: Vec<(u64, u32)>,
        off: Vec<u32>,
    }
    impl L1 {
        fn bucket(&self, s: usize) -> &[(u64, u32)] {
            &self.flat[self.off[s] as usize..self.off[s + 1] as usize]
        }
    }
    let l1: Vec<Mutex<Option<L1>>> = (0..tasks.len()).map(|_| Mutex::new(None)).collect();
    par(nt, tasks.len(), |t| {
        let (i, s, e) = tasks[t];
        let d = maps[i].bytes();
        let mut v = Vec::with_capacity((e - s) / 20 + 16);
        csv::scan::<2, _>(d, s, e, s == 0, |f| {
            if f[0].is_empty() || f[1].is_empty() {
                return;
            }
            v.push((id_key(f[0], it), id_key(f[1], it)));
        });
        // two allocations per chunk instead of one per shard: each small allocation used to
        // grow the thread's heap with its own mprotect, which stalls page faults in every thread
        let mut off = vec![0u32; s_count + 1];
        for &(a, b) in &v {
            off[(hash64(a) >> sshift) as usize + 1] += 1;
            off[(hash64(b) >> sshift) as usize + 1] += 1;
        }
        for i in 0..s_count {
            off[i + 1] += off[i];
        }
        let mut pos: Vec<u32> = off[..s_count].to_vec();
        let mut flat = vec![(0u64, 0u32); 2 * v.len()];
        for (j, &(a, b)) in v.iter().enumerate() {
            let sa = (hash64(a) >> sshift) as usize;
            flat[pos[sa] as usize] = (a, (2 * j) as u32);
            pos[sa] += 1;
            let sb = (hash64(b) >> sshift) as usize;
            flat[pos[sb] as usize] = (b, (2 * j + 1) as u32);
            pos[sb] += 1;
        }
        *l1[t].lock().unwrap() = Some(L1 { edges: v, flat, off });
    });
    let l1: Vec<L1> = l1.into_iter().map(|m| m.into_inner().unwrap().unwrap()).collect();
    let edges: Vec<&Vec<(u64, u64)>> = l1.iter().map(|x| &x.edges).collect();
    let nedges: usize = edges.iter().map(|v| v.len()).sum();
    lap("L1 parse");

    let ends: Vec<Vec<AtomicU32>> = edges
        .iter()
        .map(|v| (0..v.len() * 2).map(|_| AtomicU32::new(0)).collect())
        .collect();
    // node ids per shard by sorting the shard's endpoints (no hash table, nothing big to zero)
    let shard_out: Vec<Mutex<Vec<u64>>> = (0..s_count).map(|_| Mutex::new(Vec::new())).collect();
    par(nt, s_count, |s| {
        let cnt: usize = l1.iter().map(|x| x.bucket(s).len()).sum();
        let mut v: Vec<(u64, u32, u32)> = Vec::with_capacity(cnt);
        for (w, x) in l1.iter().enumerate() {
            for &(k, ep) in x.bucket(s) {
                v.push((k, w as u32, ep));
            }
        }
        v.sort_unstable_by_key(|e| e.0);
        let mut keys: Vec<u64> = Vec::with_capacity(cnt / 2 + 16);
        for &(k, w, ep) in &v {
            if keys.last() != Some(&k) {
                keys.push(k);
            }
            let l = keys.len() - 1;
            ends[w as usize][ep as usize].store(((l << sbits) | s) as u32, Relaxed);
        }
        *shard_out[s].lock().unwrap() = keys;
    });
    let skeys: Vec<Vec<u64>> = shard_out.into_iter().map(|m| m.into_inner().unwrap()).collect();
    let maxl = skeys.iter().map(|k| k.len()).max().unwrap_or(0);
    let ngid = (maxl << sbits).max(1);
    assert!(ngid < u32::MAX as usize);

    let parent: Vec<AtomicU32> = (0..ngid as u32).map(AtomicU32::new).collect();
    let ntask3 = edges.len();

    par(nt, ntask3, |w| {
        let e = &ends[w];
        for i in 0..edges[w].len() {
            union(&parent, e[2 * i].load(Relaxed), e[2 * i + 1].load(Relaxed));
        }
    });

    lap("L3 uf");
    let mink: Vec<AtomicU64> = (0..ngid).map(|_| AtomicU64::new(EMPTY)).collect();
    let exotic: Mutex<Vec<u32>> = Mutex::new(Vec::new());
    let persons = AtomicUsize::new(0);
    par(nt, s_count, |s| {
        let mut ex = Vec::new();
        let mut roots = 0;
        for (l, &k) in skeys[s].iter().enumerate() {
            let g = ((l << sbits) | s) as u32;
            let r = find(&parent, g);
            roots += (r == g) as usize;
            if k & EXOTIC == 0 {
                mink[r as usize].fetch_min(k, Relaxed);
            } else {
                ex.push(g);
            }
        }
        persons.fetch_add(roots, Relaxed);
        if !ex.is_empty() {
            exotic.lock().unwrap().extend(ex);
        }
    });
    let mut min_key: Vec<u64> = mink.into_iter().map(|a| a.into_inner()).collect();
    for g in exotic.into_inner().unwrap() {
        let s = g as usize & (s_count - 1);
        let l = g as usize >> sbits;
        let k = skeys[s][l];
        let r = find(&parent, g) as usize;
        if min_key[r] == EMPTY || cmp_ids(k, min_key[r], it) == std::cmp::Ordering::Less {
            min_key[r] = k;
        }
    }

    lap("L4 names");
    let nbk = ((maxl as f64 / (4.0 * 0.7)) as usize + 1).next_power_of_two().max(4);
    let per = nbk * 4;
    let sbits_slots = per.trailing_zeros();
    let smask = nbk - 1;
    let mut slots: Vec<Slot> = vec![Slot { k: EMPTY, v: 0, _p: 0 }; per * s_count];
    {
        let chunks: Vec<Mutex<&mut [Slot]>> = slots.chunks_mut(per).map(Mutex::new).collect();
        par(nt, s_count, |s| {
            let mut t = chunks[s].lock().unwrap();
            for (l, &k) in skeys[s].iter().enumerate() {
                let g = ((l << sbits) | s) as u32;
                let r = find(&parent, g);
                debug_assert_eq!((hash64(k) >> sshift) as usize, s);
                let mut b = hash64(k) as usize & smask;
                'ins: loop {
                    for j in 0..4 {
                        if t[(b << 2) + j].k == EMPTY {
                            t[(b << 2) + j] = Slot { k, v: r, _p: 0 };
                            break 'ins;
                        }
                    }
                    b = (b + 1) & smask;
                }
            }
        });
    }
    let _ = nedges;
    lap("L5 final");
    std::mem::forget(parent);
    std::mem::forget(ends);
    std::mem::forget(skeys);
    std::mem::forget(l1);
    Links {
        slots,
        smask,
        sbits_slots,
        sshift,
        min_key,
        persons: persons.into_inner(),
    }
}
