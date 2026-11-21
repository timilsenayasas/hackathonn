use std::io::Write;

struct T {
    b: Vec<u8>,
    last: Vec<i16>,
}
impl T {
    fn new() -> T {
        T { b: Vec::new(), last: vec![0] }
    }
    fn varint(&mut self, mut v: u64) {
        while v >= 0x80 {
            self.b.push((v as u8) | 0x80);
            v >>= 7;
        }
        self.b.push(v as u8);
    }
    fn zz(&mut self, v: i64) {
        self.varint(((v << 1) ^ (v >> 63)) as u64);
    }
    fn fh(&mut self, id: i16, ty: u8) {
        let last = *self.last.last().unwrap();
        let d = id - last;
        if d > 0 && d <= 15 {
            self.b.push(((d as u8) << 4) | ty);
        } else {
            self.b.push(ty);
            self.zz(id as i64);
        }
        *self.last.last_mut().unwrap() = id;
    }
    fn i32f(&mut self, id: i16, v: i32) {
        self.fh(id, 5);
        self.zz(v as i64);
    }
    fn i64f(&mut self, id: i16, v: i64) {
        self.fh(id, 6);
        self.zz(v);
    }
    fn strf(&mut self, id: i16, s: &[u8]) {
        self.fh(id, 8);
        self.varint(s.len() as u64);
        self.b.extend_from_slice(s);
    }
    fn begin(&mut self, id: i16) {
        self.fh(id, 12);
        self.last.push(0);
    }
    fn begin_elem(&mut self) {
        self.last.push(0);
    }
    fn end(&mut self) {
        self.b.push(0);
        self.last.pop();
    }
    fn list(&mut self, id: i16, elem: u8, n: usize) {
        self.fh(id, 9);
        if n < 15 {
            self.b.push(((n as u8) << 4) | elem);
        } else {
            self.b.push(0xF0 | elem);
            self.varint(n as u64);
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
pub enum Kind {
    I64,
    Date,
    Str,
}

struct ColMeta {
    name: &'static str,
    kind: Kind,
    optional: bool,
    dict: bool,
    dict_off: i64,
    data_off: i64,
    size: i64,
}

// bare-bones parquet: one row group, an optional dictionary page plus one data page per column
pub struct Writer {
    f: std::io::BufWriter<std::fs::File>,
    pos: i64,
    nrows: usize,
    cols: Vec<ColMeta>,
    body: Vec<u8>,
}

fn bits_for(n: usize) -> u32 {
    if n <= 2 {
        1
    } else {
        usize::BITS - (n - 1).leading_zeros()
    }
}

// RLE/bit-packed hybrid as a single bit-packed run
fn bitpack(out: &mut Vec<u8>, n: usize, bw: u32, mut val: impl FnMut(usize) -> u32) {
    let groups = (n + 7) / 8;
    let mut h = ((groups as u64) << 1) | 1;
    while h >= 0x80 {
        out.push((h as u8) | 0x80);
        h >>= 7;
    }
    out.push(h as u8);
    let mut acc: u64 = 0;
    let mut nb: u32 = 0;
    for i in 0..groups * 8 {
        let v = if i < n { val(i) as u64 } else { 0 };
        acc |= v << nb;
        nb += bw;
        while nb >= 8 {
            out.push(acc as u8);
            acc >>= 8;
            nb -= 8;
        }
    }
    if nb > 0 {
        out.push(acc as u8);
    }
}

impl Writer {
    pub fn create(path: &std::path::Path, nrows: usize) -> std::io::Result<Writer> {
        let mut f = std::io::BufWriter::with_capacity(1 << 20, std::fs::File::create(path)?);
        f.write_all(b"PAR1")?;
        Ok(Writer { f, pos: 4, nrows, cols: Vec::new(), body: Vec::new() })
    }

    fn page(&mut self, dict_values: Option<usize>) -> std::io::Result<i64> {
        let mut t = T::new();
        match dict_values {
            Some(nv) => {
                t.i32f(1, 2);
                t.i32f(2, self.body.len() as i32);
                t.i32f(3, self.body.len() as i32);
                t.begin(7);
                t.i32f(1, nv as i32);
                t.i32f(2, 0);
                t.end();
            }
            None => {
                t.i32f(1, 0);
                t.i32f(2, self.body.len() as i32);
                t.i32f(3, self.body.len() as i32);
                t.begin(5);
                t.i32f(1, self.nrows as i32);
                t.i32f(2, if self.cols.last().map_or(false, |c| c.dict) { 8 } else { 0 });
                t.i32f(3, 3);
                t.i32f(4, 3);
                t.end();
            }
        }
        t.b.push(0);
        let off = self.pos;
        self.f.write_all(&t.b)?;
        self.f.write_all(&self.body)?;
        self.pos += (t.b.len() + self.body.len()) as i64;
        Ok(off)
    }

    fn begin_col(&mut self, name: &'static str, kind: Kind, optional: bool, dict: bool) {
        self.cols.push(ColMeta { name, kind, optional, dict, dict_off: 0, data_off: 0, size: 0 });
    }

    fn end_col(&mut self, start: i64) {
        let c = self.cols.last_mut().unwrap();
        c.size = self.pos - start;
    }

    pub fn plain_i64(&mut self, name: &'static str, v: impl Iterator<Item = i64>) -> std::io::Result<()> {
        self.begin_col(name, Kind::I64, false, false);
        let start = self.pos;
        self.body.clear();
        for x in v {
            self.body.extend_from_slice(&x.to_le_bytes());
        }
        let off = self.page(None)?;
        self.cols.last_mut().unwrap().data_off = off;
        self.end_col(start);
        Ok(())
    }

    pub fn plain_str<'a>(&mut self, name: &'static str, v: impl Iterator<Item = &'a [u8]>) -> std::io::Result<()> {
        self.begin_col(name, Kind::Str, false, false);
        let start = self.pos;
        self.body.clear();
        for s in v {
            self.body.extend_from_slice(&(s.len() as u32).to_le_bytes());
            self.body.extend_from_slice(s);
        }
        let off = self.page(None)?;
        self.cols.last_mut().unwrap().data_off = off;
        self.end_col(start);
        Ok(())
    }

    fn dict_data(&mut self, ndict: usize, valid: Option<&dyn Fn(usize) -> bool>, idx: &dyn Fn(usize) -> u32) -> std::io::Result<()> {
        let n = self.nrows;
        self.body.clear();
        let bw = bits_for(ndict);
        let mut nvals = n;
        if let Some(ok) = valid {
            let mut lv = Vec::new();
            bitpack(&mut lv, n, 1, |i| ok(i) as u32);
            self.body.extend_from_slice(&(lv.len() as u32).to_le_bytes());
            self.body.extend_from_slice(&lv);
            nvals = (0..n).filter(|&i| ok(i)).count();
            self.body.push(bw as u8);
            let rows: Vec<u32> = (0..n).filter(|&i| ok(i)).map(|i| idx(i)).collect();
            let mut b = std::mem::take(&mut self.body);
            bitpack(&mut b, nvals, bw, |j| rows[j]);
            self.body = b;
        } else {
            self.body.push(bw as u8);
            let mut b = std::mem::take(&mut self.body);
            bitpack(&mut b, nvals, bw, idx);
            self.body = b;
        }
        let off = self.page(None)?;
        self.cols.last_mut().unwrap().data_off = off;
        Ok(())
    }

    /// dictionary-encoded INT64 (or DATE as INT32) column
    pub fn dict_int(&mut self, name: &'static str, kind: Kind, dict: &[i64], idx: &dyn Fn(usize) -> u32) -> std::io::Result<()> {
        self.begin_col(name, kind, false, true);
        let start = self.pos;
        self.body.clear();
        for &x in dict {
            if kind == Kind::Date {
                self.body.extend_from_slice(&(x as i32).to_le_bytes());
            } else {
                self.body.extend_from_slice(&x.to_le_bytes());
            }
        }
        let off = self.page(Some(dict.len()))?;
        self.cols.last_mut().unwrap().dict_off = off;
        self.dict_data(dict.len(), None, idx)?;
        self.end_col(start);
        Ok(())
    }

    /// dictionary-encoded string column; `valid` = Some for an OPTIONAL column
    pub fn dict_str<'a>(
        &mut self,
        name: &'static str,
        dict: impl Iterator<Item = &'a [u8]>,
        valid: Option<&dyn Fn(usize) -> bool>,
        idx: &dyn Fn(usize) -> u32,
    ) -> std::io::Result<()> {
        self.begin_col(name, Kind::Str, valid.is_some(), true);
        let start = self.pos;
        self.body.clear();
        let mut nd = 0;
        for s in dict {
            self.body.extend_from_slice(&(s.len() as u32).to_le_bytes());
            self.body.extend_from_slice(s);
            nd += 1;
        }
        let off = self.page(Some(nd))?;
        self.cols.last_mut().unwrap().dict_off = off;
        self.dict_data(nd, valid, idx)?;
        self.end_col(start);
        Ok(())
    }

    pub fn finish(mut self) -> std::io::Result<()> {
        let mut t = T::new();
        t.i32f(1, 1);
        t.list(2, 12, self.cols.len() + 1);
        t.begin_elem();
        t.strf(4, b"schema");
        t.i32f(5, self.cols.len() as i32);
        t.end();
        for c in &self.cols {
            t.begin_elem();
            match c.kind {
                Kind::I64 => {
                    t.i32f(1, 2);
                    t.i32f(3, c.optional as i32);
                    t.strf(4, c.name.as_bytes());
                }
                Kind::Date => {
                    t.i32f(1, 1);
                    t.i32f(3, c.optional as i32);
                    t.strf(4, c.name.as_bytes());
                    t.i32f(6, 6);
                    t.begin(10);
                    t.begin(6);
                    t.end();
                    t.end();
                }
                Kind::Str => {
                    t.i32f(1, 6);
                    t.i32f(3, c.optional as i32);
                    t.strf(4, c.name.as_bytes());
                    t.i32f(6, 0);
                    t.begin(10);
                    t.begin(1);
                    t.end();
                    t.end();
                }
            }
            t.end();
        }
        t.i64f(3, self.nrows as i64);
        t.list(4, 12, 1);
        t.begin_elem();
        t.list(1, 12, self.cols.len());
        let mut total = 0i64;
        for c in &self.cols {
            t.begin_elem();
            t.i64f(2, if c.dict { c.dict_off } else { c.data_off });
            t.begin(3);
            t.i32f(1, match c.kind {
                Kind::I64 => 2,
                Kind::Date => 1,
                Kind::Str => 6,
            });
            if c.dict {
                t.list(2, 5, 3);
                t.zz(0);
                t.zz(3);
                t.zz(8);
            } else {
                t.list(2, 5, 2);
                t.zz(0);
                t.zz(3);
            }
            t.list(3, 8, 1);
            t.varint(c.name.len() as u64);
            t.b.extend_from_slice(c.name.as_bytes());
            t.i32f(4, 0);
            t.i64f(5, self.nrows as i64);
            t.i64f(6, c.size);
            t.i64f(7, c.size);
            t.i64f(9, c.data_off);
            if c.dict {
                t.i64f(11, c.dict_off);
            }
            t.end();
            t.end();
            total += c.size;
        }
        t.i64f(2, total);
        t.i64f(3, self.nrows as i64);
        t.end();
        t.strf(6, b"fastsess");
        t.b.push(0);
        self.f.write_all(&t.b)?;
        self.f.write_all(&(t.b.len() as u32).to_le_bytes())?;
        self.f.write_all(b"PAR1")?;
        self.f.flush()
    }
}
