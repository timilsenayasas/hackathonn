// Linked IDs: fully parallel build.
//   L1 parse link chunks (all threads)            -> per-worker edge lists
//   L2 shard node tables by hash(key) (all threads) -> endpoint node ids
//   L3 lock-free union-find over all edges (all threads)
//   L4 component name = byte-wise min key (atomic fetch_min, exotic fix-up)
//   L5 shard table values -> root node id
// Person key for a linked ID = LINKED | root node id.

use crate::arena::prefetch;
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
pub struct KTab {
    mask: usize,
    pub slots: Vec<Slot>,
}
impl KTab {
    pub fn with_size_for(n: usize, load: f64) -> KTab {
        let size = (((n as f64) / load) as usize + 1).next_power_of_two().max(16);
        KTab { mask: size - 1, slots: vec![Slot { k: EMPTY, v: 0, _p: 0 }; size] }
    }
    #[inline(always)]
    fn idx(&self, k: u64) -> usize {
        hash64b(k) as usize & self.mask
    }
    #[inline(always)]
    pub fn prefetch(&self, k: u64) {
        prefetch(unsafe { self.slots.as_ptr().add(self.idx(k)) });
    }
    #[inline(always)]
    pub fn get(&self, k: u64) -> Option<u32> {
        let mut i = self.idx(k);
        loop {
            let s = unsafe { self.slots.get_unchecked(i) };
            if s.k == k {
                return Some(s.v);
            }
            if s.k == EMPTY {
                return None;
            }
            i = (i + 1) & self.mask;
        }
    }
    /// Returns (value, inserted).
    #[inline(always)]
    pub fn get_or_insert(&mut self, k: u64, v: u32) -> (u32, bool) {
        let mut i = self.idx(k);
        loop {
            let s = &mut self.slots[i];
            if s.k == k {
                return (s.v, false);
            }
            if s.k == EMPTY {
                s.k = k;
                s.v = v;
                return (v, true);
            }
            i = (i + 1) & self.mask;
        }
    }
}

pub struct Links {
    slots: Vec<Slot>, // s_count shards of (smask+1) slots each, contiguous (huge pages)
    smask: usize,
    sbits_slots: u32,
    sshift: u32,
    pub min_key: Vec<u64>, // by root node id
    pub persons: usize,
}

impl Links {
    // Bucketized: 4 slots (one 64-byte line) per bucket. One hash: top bits pick the shard,
    // low bits the bucket. `locate` gives the slot index of the home bucket.
    #[inline(always)]
    pub fn locate(&self, k: u64) -> usize {
        let h = hash64(k);
        (((h >> self.sshift) as usize) << self.sbits_slots) | ((h as usize & self.smask) << 2)
    }
    #[inline(always)]
    pub fn prefetch_at(&self, at: usize) {
        prefetch(unsafe { self.slots.as_ptr().add(at) });
    }
    #[inline(always)]
    pub fn prefetch(&self, k: u64) {
        self.prefetch_at(self.locate(k));
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
            p[x as usize].store(gp, Relaxed); // path halving: gp is an ancestor of x
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
    let lap = |n: &str| if vb { eprintln!("   links {:>7.1} ms {}", t0.elapsed().as_secs_f64()*1e3, n) };
    // L1: parse, bucketing endpoints by shard
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
        buckets: Vec<Vec<(u64, u32)>>, // per shard: (key, endpoint index 2*i+side)
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
        let mut buckets: Vec<Vec<(u64, u32)>> = (0..s_count).map(|_| Vec::with_capacity(v.len() * 2 / s_count + 8)).collect();
        for (j, &(a, b)) in v.iter().enumerate() {
            buckets[(hash64(a) >> sshift) as usize].push((a, (2 * j) as u32));
            buckets[(hash64(b) >> sshift) as usize].push((b, (2 * j + 1) as u32));
        }
        *l1[t].lock().unwrap() = Some(L1 { edges: v, buckets });
    });
    let l1: Vec<L1> = l1.into_iter().map(|m| m.into_inner().unwrap().unwrap()).collect();
    let edges: Vec<&Vec<(u64, u64)>> = l1.iter().map(|x| &x.edges).collect();
    let nedges: usize = edges.iter().map(|v| v.len()).sum();
    lap("L1 parse");

    // L2: shard node tables (each fits in L2)
    let ends: Vec<Vec<AtomicU32>> = edges.iter().map(|v| (0..v.len() * 2).map(|_| AtomicU32::new(0)).collect()).collect();
    let shard_out: Vec<Mutex<Option<(KTab, Vec<u64>)>>> = (0..s_count).map(|_| Mutex::new(None)).collect();
    par(nt, s_count, |s| {
        let cnt: usize = l1.iter().map(|x| x.buckets[s].len()).sum();
        let mut tab = KTab::with_size_for(cnt + 16, 0.7);
        let mut keys: Vec<u64> = Vec::with_capacity(cnt / 2 + 16);
        for (w, x) in l1.iter().enumerate() {
            for &(k, ep) in &x.buckets[s] {
                let (l, ins) = tab.get_or_insert(k, keys.len() as u32);
                if ins {
                    keys.push(k);
                }
                ends[w][ep as usize].store(((l as usize) << sbits | s) as u32, Relaxed);
            }
        }
        *shard_out[s].lock().unwrap() = Some((tab, keys));
    });
    let mut skeys: Vec<Vec<u64>> = Vec::with_capacity(s_count);
    for m in shard_out {
        let (t, k) = m.into_inner().unwrap().unwrap();
        std::mem::forget(t);
        skeys.push(k);
    }
    let maxl = skeys.iter().map(|k| k.len()).max().unwrap_or(0);
    let ngid = (maxl << sbits).max(1);
    assert!(ngid < u32::MAX as usize);

    // L3: union-find
    let parent: Vec<AtomicU32> = (0..ngid as u32).map(AtomicU32::new).collect();
    let ntask3 = edges.len();

    par(nt, ntask3, |w| {
        let e = &ends[w];
        for i in 0..edges[w].len() {
            union(&parent, e[2 * i].load(Relaxed), e[2 * i + 1].load(Relaxed));
        }
    });

    lap("L3 uf");
    // L4: component names
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
    // L5: compact final map key -> root node id: equal-size shards of 4-slot buckets, contiguous
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
    Links { slots, smask, sbits_slots, sshift, min_key, persons: persons.into_inner() }
}
