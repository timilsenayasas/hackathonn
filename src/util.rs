// Shared helpers: ID keys, interner, timestamp parsing, hashing, mmap.

use std::collections::HashMap;
use std::sync::Mutex;

pub const DAY_MS: i64 = 86_400_000;

// ---------------------------------------------------------------- ID keys
// Exact, order-preserving 64-bit keys for the common ID shape:
//   one arbitrary first byte + 0..=8 chars from [0-9a-z]
//   key = first_byte << 48 | 6-bit codes of chars 1..8 (code 0 = padding)   (< 2^56)
// Integer order == byte-wise order for two compact keys.
// Anything else is "exotic": EXOTIC | interned id (compare via bytes).
pub const EXOTIC: u64 = 1 << 63;
pub const LINKED: u64 = 1 << 62; // person tag for linked components

static C6: [u8; 256] = {
    let mut t = [0xFFu8; 256];
    let mut c = 0;
    while c < 10 {
        t[b'0' as usize + c] = 1 + c as u8;
        c += 1;
    }
    let mut c = 0;
    while c < 26 {
        t[b'a' as usize + c] = 11 + c as u8;
        c += 1;
    }
    t
};
const D6: &[u8; 37] = b"\x000123456789abcdefghijklmnopqrstuvwxyz";

/// Scalar reference version.
#[inline]
pub fn compact_key_slow(s: &[u8]) -> Option<u64> {
    let n = s.len();
    if n == 0 || n > 9 {
        return None;
    }
    let mut v: u64 = 0;
    for i in 1..9 {
        let d = if i < n { C6[s[i] as usize] } else { 0 };
        if d == 0xFF {
            return None;
        }
        v = (v << 6) | d as u64;
    }
    Some(((s[0] as u64) << 48) | v)
}

/// SIMD version. Requires 9 readable bytes at s.as_ptr() (mmaps are padded).
#[inline(always)]
pub fn compact_key(s: &[u8]) -> Option<u64> {
    use std::arch::x86_64::*;
    let n = s.len();
    if n == 0 || n > 9 {
        return None;
    }
    unsafe {
        let p = s.as_ptr();
        let v = _mm_loadl_epi64(p.add(1) as *const __m128i);
        let d = _mm_sub_epi8(v, _mm_set1_epi8(b'0' as i8));
        let isd = _mm_cmpeq_epi8(_mm_min_epu8(d, _mm_set1_epi8(9)), d);
        let l = _mm_sub_epi8(v, _mm_set1_epi8(b'a' as i8));
        let isl = _mm_cmpeq_epi8(_mm_min_epu8(l, _mm_set1_epi8(25)), l);
        let code = _mm_blendv_epi8(_mm_add_epi8(l, _mm_set1_epi8(11)), _mm_add_epi8(d, _mm_set1_epi8(1)), isd);
        let ok = (_mm_movemask_epi8(_mm_or_si128(isd, isl)) as u32) & 0xFF;
        let k = (n - 1) as u32;
        let need = (1u32 << k) - 1;
        if ok & need != need {
            return None;
        }
        let c = _mm_cvtsi128_si64(code) as u64;
        let c = if k == 0 { 0 } else { c & (!0u64 >> (64 - 8 * k)) };
        let packed = _pext_u64(c.swap_bytes(), 0x3F3F_3F3F_3F3F_3F3F);
        Some(((*p as u64) << 48) | packed)
    }
}

pub fn decode_compact(k: u64, out: &mut Vec<u8>) {
    out.push((k >> 48) as u8);
    for i in (0..8).rev() {
        let x = ((k >> (6 * i)) & 63) as usize;
        if x == 0 {
            break;
        }
        out.push(D6[x]);
    }
}

pub struct Interner {
    shards: Vec<Mutex<(HashMap<Box<[u8]>, u32>, Vec<Box<[u8]>>)>>,
}

impl Interner {
    pub fn new() -> Self {
        Interner { shards: (0..64).map(|_| Mutex::new((HashMap::new(), Vec::new()))).collect() }
    }
    pub fn intern(&self, s: &[u8]) -> u64 {
        let h = hash_bytes(s);
        let sh = (h >> 58) as usize;
        let mut g = self.shards[sh].lock().unwrap();
        if let Some(&id) = g.0.get(s) {
            return EXOTIC | ((id as u64) << 6) | sh as u64;
        }
        let id = g.1.len() as u32;
        let b: Box<[u8]> = s.into();
        g.1.push(b.clone());
        g.0.insert(b, id);
        EXOTIC | ((id as u64) << 6) | sh as u64
    }
    pub fn bytes(&self, k: u64, out: &mut Vec<u8>) {
        let sh = (k & 63) as usize;
        let id = ((k & !EXOTIC) >> 6) as usize;
        let g = self.shards[sh].lock().unwrap();
        out.extend_from_slice(&g.1[id]);
    }
}

#[inline(always)]
pub fn id_key(s: &[u8], it: &Interner) -> u64 {
    match compact_key(s) {
        Some(k) => k,
        None => it.intern(s),
    }
}

pub fn id_bytes(k: u64, it: &Interner, out: &mut Vec<u8>) {
    if k & EXOTIC != 0 {
        it.bytes(k, out)
    } else {
        decode_compact(k, out)
    }
}

pub fn cmp_ids(a: u64, b: u64, it: &Interner) -> std::cmp::Ordering {
    if (a | b) & EXOTIC == 0 {
        return a.cmp(&b);
    }
    let (mut x, mut y) = (Vec::new(), Vec::new());
    id_bytes(a, it, &mut x);
    id_bytes(b, it, &mut y);
    x.cmp(&y)
}

// ---------------------------------------------------------------- hashing
#[inline(always)]
pub fn hash64(k: u64) -> u64 {
    let x = k.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    x ^ (x >> 29)
}
#[inline(always)]
pub fn hash64b(k: u64) -> u64 {
    // independent second hash (for a different partitioning than hash64)
    let mut x = k ^ 0xD6E8_FEB8_6659_FD93;
    x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x ^= x >> 31;
    x = x.wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 29)
}
pub fn hash_bytes(s: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in s {
        h ^= b as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    hash64b(h)
}

// ---------------------------------------------------------------- time
pub fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
    era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468
}

static MSTART: std::sync::OnceLock<Vec<i64>> = std::sync::OnceLock::new();
const MS_Y0: i64 = 1900;
const MS_YN: i64 = 256;
/// Epoch seconds of the first day of (y, m); table for 1900..2156.
#[inline(always)]
fn month_start(y: i64, m: i64) -> i64 {
    if y >= MS_Y0 && y < MS_Y0 + MS_YN {
        let t = MSTART.get_or_init(|| {
            let mut v = Vec::with_capacity((MS_YN * 12) as usize);
            for yy in MS_Y0..MS_Y0 + MS_YN {
                for mm in 1..=12 {
                    v.push(days_from_civil(yy, mm, 1) * 86_400);
                }
            }
            v
        });
        unsafe { *t.get_unchecked(((y - MS_Y0) * 12 + m - 1) as usize) }
    } else {
        days_from_civil(y, m, 1) * 86_400
    }
}

#[inline(always)]
fn dg(c: u8) -> u32 {
    c.wrapping_sub(b'0') as u32
}
#[inline(always)]
fn d2(s: &[u8], i: usize) -> Option<i64> {
    let a = dg(s[i]);
    let b = dg(s[i + 1]);
    if a < 10 && b < 10 {
        Some((a * 10 + b) as i64)
    } else {
        None
    }
}

#[inline(always)]
fn dim(y: i64, m: i64) -> i64 {
    const T: [i64; 13] = [0, 31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    if m == 2 && (y % 4 == 0 && (y % 100 != 0 || y % 400 == 0)) {
        29
    } else {
        T[m as usize]
    }
}

/// "YYYY-MM-DDTHH:MM:SS" prefix (first 19 bytes) → epoch seconds (local).
#[inline(always)]
fn iso_secs(s: &[u8]) -> Option<i64> {
    if s[4] != b'-' || s[7] != b'-' || s[10] != b'T' || s[13] != b':' || s[16] != b':' {
        return None;
    }
    let y = d2(s, 0)? * 100 + d2(s, 2)?;
    let mo = d2(s, 5)?;
    let d = d2(s, 8)?;
    let h = d2(s, 11)?;
    let mi = d2(s, 14)?;
    let se = d2(s, 17)?;
    if mo < 1 || mo > 12 || d < 1 || d > dim(y, mo) || h > 23 || mi > 59 || se > 59 {
        return None;
    }
    Some(month_start(y, mo) + (d - 1) * 86_400 + h * 3600 + mi * 60 + se)
}

/// Direct-mapped cache of validated "YYYY-MM-DD" prefixes -> epoch seconds of that midnight.
pub struct DateCache {
    k: [u64; 64],
    k2: [u16; 64],
    v: [i64; 64],
}
impl DateCache {
    pub fn new() -> Self {
        DateCache { k: [0; 64], k2: [0; 64], v: [i64::MIN; 64] }
    }
    /// s.len() >= 19 and the shape is checked by the caller's length test only.
    #[inline(always)]
    fn day(&mut self, s: &[u8]) -> Option<i64> {
        let a = u64::from_le_bytes(s[0..8].try_into().unwrap());
        let b = u16::from_le_bytes([s[8], s[9]]);
        let h = ((a >> 40) ^ (a >> 26) ^ (b as u64) ^ ((b as u64) >> 5)) as usize & 63;
        if self.k[h] == a && self.k2[h] == b && self.v[h] != i64::MIN {
            return Some(self.v[h]);
        }
        if s[4] != b'-' || s[7] != b'-' {
            return None;
        }
        let y = d2(s, 0)? * 100 + d2(s, 2)?;
        let mo = d2(s, 5)?;
        let d = d2(s, 8)?;
        if mo < 1 || mo > 12 || d < 1 || d > dim(y, mo) {
            return None;
        }
        let v = month_start(y, mo) + (d - 1) * 86_400;
        self.k[h] = a;
        self.k2[h] = b;
        self.v[h] = v;
        Some(v)
    }
    /// "...THH:MM:SS" part of an ISO timestamp (first 19 bytes) -> epoch seconds (local).
    #[inline(always)]
    fn iso(&mut self, s: &[u8]) -> Option<i64> {
        if s[10] != b'T' || s[13] != b':' || s[16] != b':' {
            return None;
        }
        let base = self.day(s)?;
        let h = d2(s, 11)?;
        let mi = d2(s, 14)?;
        let se = d2(s, 17)?;
        if h > 23 || mi > 59 || se > 59 {
            return None;
        }
        Some(base + h * 3600 + mi * 60 + se)
    }
    #[inline(always)]
    pub fn f1_secs(&mut self, s: &[u8]) -> Option<i64> {
        if s.len() != 20 || s[19] != b'Z' {
            return None;
        }
        self.iso(s)
    }
    #[inline(always)]
    pub fn ts(&mut self, s: &[u8]) -> Option<i64> {
        let n = s.len();
        if n >= 20 && s[4] == b'-' {
            if n == 20 {
                if s[19] != b'Z' {
                    return None;
                }
                return self.iso(s).map(|x| x * 1000);
            }
            if n == 25 {
                let sign = match s[19] {
                    b'+' => 1,
                    b'-' => -1,
                    _ => return None,
                };
                if s[22] != b':' {
                    return None;
                }
                let oh = d2(s, 20)?;
                let om = d2(s, 23)?;
                if oh > 23 || om > 59 {
                    return None;
                }
                let base = self.iso(s)?;
                return Some((base - sign * (oh * 3600 + om * 60)) * 1000);
            }
            return None;
        }
        parse_ts(s)
    }
}

/// F1 only → epoch seconds.
#[inline(always)]
pub fn parse_f1_secs(s: &[u8]) -> Option<i64> {
    if s.len() != 20 || s[19] != b'Z' {
        return None;
    }
    iso_secs(s)
}

/// ts in F1/F2/F3 → epoch ms. None = bad. Huge F3 → None (outside any window).
#[inline(always)]
pub fn parse_ts(s: &[u8]) -> Option<i64> {
    let n = s.len();
    if n == 0 {
        return None;
    }
    if n >= 20 && s[4] == b'-' {
        if n == 20 {
            if s[19] != b'Z' {
                return None;
            }
            return iso_secs(s).map(|x| x * 1000);
        }
        if n == 25 {
            let sign = match s[19] {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            if s[22] != b':' {
                return None;
            }
            let oh = d2(s, 20)?;
            let om = d2(s, 23)?;
            if oh > 23 || om > 59 {
                return None;
            }
            let base = iso_secs(s)?;
            return Some((base - sign * (oh * 3600 + om * 60)) * 1000);
        }
        return None;
    }
    // F3: digits only
    let mut v: u64 = 0;
    let mut sig = 0usize;
    for &c in s {
        let x = dg(c);
        if x >= 10 {
            return None;
        }
        if sig > 0 || x != 0 {
            sig += 1;
            if sig > 19 {
                // keep validating digits but value is out of range
                v = u64::MAX;
                continue;
            }
            v = v * 10 + x as u64;
        }
    }
    if v > i64::MAX as u64 {
        // valid form, but outside every window: report as an out-of-window instant
        return Some(i64::MAX);
    }
    Some(v as i64)
}

/// price: digits only, 1..=10,000,000
#[inline(always)]
pub fn parse_price(s: &[u8]) -> Option<u32> {
    if s.is_empty() {
        return None;
    }
    let mut v: u64 = 0;
    for &c in s {
        let x = dg(c);
        if x >= 10 {
            return None;
        }
        v = v * 10 + x as u64;
        if v > 10_000_000 {
            // keep scanning for non-digits? any non-digit → None either way, value too big → None
            return None;
        }
    }
    if v >= 1 {
        Some(v as u32)
    } else {
        None
    }
}

// ---------------------------------------------------------------- mmap
pub struct Mapped {
    pub ptr: *const u8,
    pub len: usize,
}
unsafe impl Send for Mapped {}
unsafe impl Sync for Mapped {}
impl Mapped {
    pub fn open(path: &std::path::Path) -> std::io::Result<Mapped> {
        use std::os::unix::io::AsRawFd;
        let f = std::fs::File::open(path)?;
        let len = f.metadata()?.len() as usize;
        // Reserve len + 64 KiB of zero pages, then map the file over the front, so
        // SIMD loads may read past EOF safely.
        let page = 4096;
        let flen = (len + page - 1) / page * page;
        let total = flen + 65536;
        unsafe {
            let r = libc::mmap(std::ptr::null_mut(), total, libc::PROT_READ, libc::MAP_PRIVATE | libc::MAP_ANONYMOUS, -1, 0);
            if r == libc::MAP_FAILED {
                return Err(std::io::Error::last_os_error());
            }
            if len > 0 {
                let p = libc::mmap(r, len, libc::PROT_READ, libc::MAP_PRIVATE | libc::MAP_FIXED, f.as_raw_fd(), 0);
                if p == libc::MAP_FAILED {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(Mapped { ptr: r as *const u8, len })
        }
    }
    pub fn bytes(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
}
