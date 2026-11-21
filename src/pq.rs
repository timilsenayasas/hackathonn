pub enum Col<'a> {
    I32 {
        name: &'a str,
        date: bool,
        v: &'a [i32],
    },
    I64 {
        name: &'a str,
        v: &'a [i64],
    },
    Str {
        name: &'a str,
        data: &'a [u8],
        offs: &'a [u32],
        valid: Option<&'a [bool]>,
    },
}

struct T {
    b: Vec<u8>,
    last: Vec<i16>,
}
impl T {
    fn new() -> T {
        T {
            b: Vec::new(),
            last: vec![0],
        }
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
        self.list_hdr(elem, n);
    }
    fn list_hdr(&mut self, elem: u8, n: usize) {
        if n < 15 {
            self.b.push(((n as u8) << 4) | elem);
        } else {
            self.b.push(0xF0 | elem);
            self.varint(n as u64);
        }
    }
}

struct Meta {
    ptype: i32,
    name: String,
    num_values: i64,
    offset: i64,
    size: i64,
}

// bare-bones parquet: one row group, one plain uncompressed page per column
pub fn write(path: &std::path::Path, nrows: usize, cols: &[Col]) -> std::io::Result<()> {
    let mut f: Vec<u8> = Vec::with_capacity(64 + nrows * 48);
    f.extend_from_slice(b"PAR1");
    let mut metas: Vec<Meta> = Vec::new();
    for c in cols {
        let mut body: Vec<u8> = Vec::new();
        let (ptype, name) = match c {
            Col::I32 { name, v, .. } => {
                body.reserve(v.len() * 4);
                for x in v.iter() {
                    body.extend_from_slice(&x.to_le_bytes());
                }
                (1, *name)
            }
            Col::I64 { name, v } => {
                body.reserve(v.len() * 8);
                for x in v.iter() {
                    body.extend_from_slice(&x.to_le_bytes());
                }
                (2, *name)
            }
            Col::Str { name, data, offs, valid } => {
                if let Some(valid) = valid {
                    let groups = (nrows + 7) / 8;
                    let mut lv: Vec<u8> = Vec::with_capacity(groups + 10);
                    let mut h = ((groups as u64) << 1) | 1;
                    while h >= 0x80 {
                        lv.push((h as u8) | 0x80);
                        h >>= 7;
                    }
                    lv.push(h as u8);
                    let start = lv.len();
                    lv.resize(start + groups, 0);
                    for (i, &ok) in valid.iter().enumerate() {
                        if ok {
                            lv[start + (i >> 3)] |= 1 << (i & 7);
                        }
                    }
                    body.extend_from_slice(&(lv.len() as u32).to_le_bytes());
                    body.extend_from_slice(&lv);
                }
                body.reserve(data.len() + offs.len() * 4);
                for i in 0..offs.len() - 1 {
                    if let Some(valid) = valid {
                        if !valid[i] {
                            continue;
                        }
                    }
                    let s = &data[offs[i] as usize..offs[i + 1] as usize];
                    body.extend_from_slice(&(s.len() as u32).to_le_bytes());
                    body.extend_from_slice(s);
                }
                (6, *name)
            }
        };
        let mut t = T::new();
        t.i32f(1, 0); // data page
        t.i32f(2, body.len() as i32);
        t.i32f(3, body.len() as i32);
        t.begin(5);
        t.i32f(1, nrows as i32);
        t.i32f(2, 0);
        t.i32f(3, 3);
        t.i32f(4, 3);
        t.end();
        t.b.push(0);
        let offset = f.len() as i64;
        f.extend_from_slice(&t.b);
        f.extend_from_slice(&body);
        metas.push(Meta {
            ptype,
            name: name.to_string(),
            num_values: nrows as i64,
            offset,
            size: f.len() as i64 - offset,
        });
    }
    let mut t = T::new();
    t.i32f(1, 1);
    t.list(2, 12, cols.len() + 1);
    t.begin_elem();
    t.strf(4, b"schema");
    t.i32f(5, cols.len() as i32);
    t.end();
    for c in cols {
        t.begin_elem();
        match c {
            Col::I32 { name, date, .. } => {
                t.i32f(1, 1);
                t.i32f(3, 0);
                t.strf(4, name.as_bytes());
                if *date {
                    t.i32f(6, 6);
                    t.begin(10);
                    t.begin(6);
                    t.end();
                    t.end();
                }
            }
            Col::I64 { name, .. } => {
                t.i32f(1, 2);
                t.i32f(3, 0);
                t.strf(4, name.as_bytes());
            }
            Col::Str { name, valid, .. } => {
                t.i32f(1, 6);
                t.i32f(3, if valid.is_some() { 1 } else { 0 });
                t.strf(4, name.as_bytes());
                t.i32f(6, 0);
                t.begin(10);
                t.begin(1);
                t.end();
                t.end();
            }
        }
        t.end();
    }
    t.i64f(3, nrows as i64);
    t.list(4, 12, 1);
    t.begin_elem();
    t.list(1, 12, metas.len());
    let mut total = 0i64;
    for m in &metas {
        t.begin_elem();
        t.i64f(2, m.offset);
        t.begin(3);
        t.i32f(1, m.ptype);
        t.list(2, 5, 2);
        t.zz(0);
        t.zz(3);
        t.list(3, 8, 1);
        t.varint(m.name.len() as u64);
        t.b.extend_from_slice(m.name.as_bytes());
        t.i32f(4, 0);
        t.i64f(5, m.num_values);
        t.i64f(6, m.size);
        t.i64f(7, m.size);
        t.i64f(9, m.offset);
        t.end();
        t.end();
        total += m.size;
    }
    t.i64f(2, total);
    t.i64f(3, nrows as i64);
    t.end();
    t.strf(6, b"fastsess");
    t.b.push(0);
    f.extend_from_slice(&t.b);
    f.extend_from_slice(&(t.b.len() as u32).to_le_bytes());
    f.extend_from_slice(b"PAR1");
    std::fs::write(path, &f)
}
