use crate::links::Links;
use crate::out::UserRows;
use crate::util::*;

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct E2 {
    pub person: u64,
    pub ts: u32,
    pub pay: u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct C2 {
    pub person: u64,
    pub cid: u64,
    pub ts: u32,
    pub camp: u32,
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

const WEEK: i64 = 7 * DAY_MS;
const GAP: u32 = 1_800_000;
const EMPTY: u64 = u64::MAX;

pub struct P3 {
    pub ws: i64,
    pub day0: i64,
    pub nd: usize,
    pub norg: usize,
}

pub struct Sess {
    pub rows: UserRows,
    pub a_sess: Vec<u32>,
    pub a_purch: Vec<u32>,
    pub a_rev: Vec<u64>,
    pub a_assist: Vec<u32>,
    mk: Vec<u64>,
    mv: Vec<u32>,
    li: Vec<u32>,
    pkeys: Vec<u64>,
    cnt: Vec<u32>,
    off: Vec<u32>,
    fill: Vec<u32>,
    ccnt: Vec<u32>,
    coff: Vec<u32>,
    e3: Vec<E3>,
    c3: Vec<C3>,
    cli: Vec<u32>,
    cur_a: Vec<u64>,
    cur_b: Vec<u64>,
    touched: Vec<u32>,
    sstart: Vec<u64>,
    sday: Vec<u32>,
    snext: Vec<u32>,
    catsum: Vec<u64>,
    cat_touched: Vec<u8>,
    name: Vec<u8>,
}

impl Sess {
    pub fn new(ncamp: usize, nd: usize) -> Sess {
        Sess {
            rows: UserRows::new(),
            a_sess: vec![0; (ncamp + 1) * nd],
            a_purch: vec![0; (ncamp + 1) * nd],
            a_rev: vec![0; (ncamp + 1) * nd],
            a_assist: vec![0; (ncamp + 1) * nd],
            mk: Vec::new(),
            mv: Vec::new(),
            li: Vec::new(),
            pkeys: Vec::new(),
            cnt: Vec::new(),
            off: Vec::new(),
            fill: Vec::new(),
            ccnt: Vec::new(),
            coff: Vec::new(),
            e3: Vec::new(),
            c3: Vec::new(),
            cli: Vec::new(),
            cur_a: vec![0; ncamp + 1],
            cur_b: vec![u64::MAX; ncamp + 1],
            touched: Vec::new(),
            sstart: Vec::new(),
            sday: Vec::new(),
            snext: Vec::new(),
            catsum: vec![0; 256],
            cat_touched: Vec::new(),
            name: Vec::new(),
        }
    }

    pub fn process(&mut self, ein: &[&[E2]], cks: &[&[C2]], p: &P3, links: &Links, it: &Interner) {
        let n: usize = ein.iter().map(|s| s.len()).sum();
        if n == 0 {
            return;
        }
        let (ws, day0, nd, norg) = (p.ws, p.day0, p.nd, p.norg);
        let mut size = (n / 8).next_power_of_two().max(256);
        let mut mask = size - 1;
        self.mk.clear();
        self.mk.resize(size, EMPTY);
        self.mv.clear();
        self.mv.resize(size, 0);
        self.li.clear();
        self.pkeys.clear();
        self.cnt.clear();
        for e in ein.iter().flat_map(|s| s.iter()) {
            let mut i = hash64(e.person) as usize & mask;
            let l = loop {
                let k = unsafe { *self.mk.get_unchecked(i) };
                if k == e.person {
                    break unsafe { *self.mv.get_unchecked(i) };
                }
                if k == EMPTY {
                    let l = self.pkeys.len() as u32;
                    self.pkeys.push(e.person);
                    self.cnt.push(0);
                    if self.pkeys.len() * 4 >= size * 3 {
                        size *= 2;
                        mask = size - 1;
                        self.mk.clear();
                        self.mk.resize(size, EMPTY);
                        self.mv.clear();
                        self.mv.resize(size, 0);
                        for (x, &pk) in self.pkeys.iter().enumerate() {
                            let mut j = hash64(pk) as usize & mask;
                            while self.mk[j] != EMPTY {
                                j = (j + 1) & mask;
                            }
                            self.mk[j] = pk;
                            self.mv[j] = x as u32;
                        }
                    } else {
                        self.mk[i] = e.person;
                        self.mv[i] = l;
                    }
                    break l;
                }
                i = (i + 1) & mask;
            };
            self.cnt[l as usize] += 1;
            self.li.push(l);
        }
        let np = self.pkeys.len();
        self.off.clear();
        let mut s = 0u32;
        for &c in &self.cnt {
            self.off.push(s);
            s += c;
        }
        self.off.push(s);
        self.e3.clear();
        self.e3.resize(n, E3::default());
        self.fill.clear();
        self.fill.extend_from_slice(&self.off[..np]);
        for (j, e) in ein.iter().flat_map(|s| s.iter()).enumerate() {
            let l = self.li[j] as usize;
            let f = &mut self.fill[l];
            self.e3[*f as usize] = E3 { ts: e.ts, pay: e.pay };
            *f += 1;
        }
        self.ccnt.clear();
        self.ccnt.resize(np, 0);
        self.cli.clear();
        let mut nc = 0usize;
        for c in cks.iter().flat_map(|s| s.iter()) {
            let mut i = hash64(c.person) as usize & mask;
            let l = loop {
                let k = self.mk[i];
                if k == c.person {
                    break self.mv[i];
                }
                if k == EMPTY {
                    break u32::MAX;
                }
                i = (i + 1) & mask;
            };
            self.cli.push(l);
            if l != u32::MAX {
                self.ccnt[l as usize] += 1;
                nc += 1;
            }
        }
        self.coff.clear();
        let mut s = 0u32;
        for &c in &self.ccnt {
            self.coff.push(s);
            s += c;
        }
        self.coff.push(s);
        self.c3.clear();
        self.c3.resize(nc, C3::default());
        self.fill.clear();
        self.fill.extend_from_slice(&self.coff[..np]);
        for (j, c) in cks.iter().flat_map(|s| s.iter()).enumerate() {
            let l = self.cli[j];
            if l != u32::MAX {
                let f = &mut self.fill[l as usize];
                self.c3[*f as usize] = C3 {
                    cid: c.cid,
                    ts: c.ts,
                    camp: c.camp,
                };
                *f += 1;
            }
        }
        for l in 0..np {
            let ev = &mut self.e3[self.off[l] as usize..self.off[l + 1] as usize];
            ev.sort_unstable_by_key(|e| e.ts);
            let ck = &mut self.c3[self.coff[l] as usize..self.coff[l + 1] as usize];
            if ck.len() > 1 {
                ck.sort_unstable_by(|a, b| a.ts.cmp(&b.ts).then_with(|| cmp_ids(a.cid, b.cid, it)));
            }
            let pk = self.pkeys[l];
            self.name.clear();
            if pk & EXOTIC == 0 && pk & LINKED != 0 {
                id_bytes(links.min_key[(pk & !LINKED) as usize], it, &mut self.name);
            } else {
                id_bytes(pk, it, &mut self.name);
            }
            let mut hi = 0usize;
            let mut cur_day: i64 = -1;
            let (mut d_sess, mut d_ev, mut d_rev) = (0i64, 0i64, 0i64);
            let mut i = 0;
            self.sstart.clear();
            self.sday.clear();
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
                        if self.catsum[c] == 0 {
                            self.cat_touched.push(c as u8);
                        }
                        self.catsum[c] += price as u64;
                    }
                    j += 1;
                }
                let nev = (j - i) as i64;
                let day = (ws + s0 as i64).div_euclid(DAY_MS) - day0;
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
                self.sstart.push(sc);
                self.sday.push(du as u32);
                let ai = credited * nd + du;
                self.a_sess[ai] += 1;
                self.a_purch[ai] += purch as u32;
                self.a_rev[ai] += rev as u64;
                if day != cur_day {
                    cur_day = day;
                    d_sess = 0;
                    d_ev = 0;
                    d_rev = 0;
                }
                d_sess += 1;
                d_ev += nev;
                d_rev += rev;
                let next_day = if j < ev.len() {
                    (ws + ev[j].ts as i64).div_euclid(DAY_MS) - day0
                } else {
                    -1
                };
                if next_day != cur_day {
                    let r = &mut self.rows;
                    r.user.push(&self.name);
                    r.day.push((day0 + cur_day) as i32);
                    r.sessions.push(d_sess);
                    r.events.push(d_ev);
                    r.revenue.push(d_rev);
                    let mut best: i32 = -1;
                    let mut bv = 0u64;
                    for &c in self.cat_touched.iter() {
                        let v = self.catsum[c as usize];
                        if v > bv || (v == bv && (c as i32) < best) {
                            bv = v;
                            best = c as i32;
                        }
                        self.catsum[c as usize] = 0;
                    }
                    self.cat_touched.clear();
                    r.topcat.push(best);
                }
                i = j;
            }
            if !ck.is_empty() {
                let ns = self.sstart.len();
                self.snext.clear();
                self.snext.resize(ns, ns as u32);
                for x in (0..ns.saturating_sub(1)).rev() {
                    self.snext[x] = if self.sday[x + 1] != self.sday[x] {
                        (x + 1) as u32
                    } else {
                        self.snext[x + 1]
                    };
                }
                let (sstart, sday, snext, acc) = (&self.sstart, &self.sday, &self.snext, &mut self.a_assist);
                let mut add = |c: usize, a: u64, b: u64| {
                    let lo = sstart.partition_point(|&x| x < a);
                    let hi = sstart.partition_point(|&x| x <= b);
                    let mut x = lo;
                    while x < hi {
                        let y = (snext[x] as usize).min(hi);
                        acc[c * nd + sday[x] as usize] += (y - x) as u32;
                        x = y;
                    }
                };
                for c in ck.iter() {
                    let cc = c.camp as usize;
                    let t = c.ts as u64;
                    if self.cur_b[cc] == u64::MAX {
                        self.touched.push(cc as u32);
                        self.cur_a[cc] = t;
                    } else if t > self.cur_b[cc] {
                        add(cc, self.cur_a[cc], self.cur_b[cc]);
                        self.cur_a[cc] = t;
                    }
                    self.cur_b[cc] = t + WEEK as u64;
                }
                for &cc in self.touched.iter() {
                    let cc = cc as usize;
                    add(cc, self.cur_a[cc], self.cur_b[cc]);
                    self.cur_b[cc] = u64::MAX;
                }
                self.touched.clear();
            }
        }
    }
}
