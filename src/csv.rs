// block scanner: separator masks are computed once per 64-byte block and walked with tzcnt
#[inline(always)]
unsafe fn masks(p: *const u8) -> (u64, u64, u64) {
    use std::arch::x86_64::*;
    let a = _mm256_loadu_si256(p as *const __m256i);
    let b = _mm256_loadu_si256(p.add(32) as *const __m256i);
    let m = |v: __m256i, c: u8| _mm256_movemask_epi8(_mm256_cmpeq_epi8(v, _mm256_set1_epi8(c as i8))) as u32 as u64;
    (m(a, b'\n') | (m(b, b'\n') << 32), m(a, b',') | (m(b, b',') << 32), m(a, b'"') | (m(b, b'"') << 32))
}

#[inline(always)]
fn emit<const NF: usize, F: FnMut(&[&[u8]; NF])>(data: &[u8], ls: usize, le: usize, cuts: &[usize; 8], nc: usize, nq: u32, f: &mut F) {
    if nc != NF - 1 {
        return;
    }
    let mut fields: [&[u8]; NF] = [&[]; NF];
    let mut s = ls;
    let mut k = 0u32;
    for i in 0..NF {
        let e = if i + 1 < NF { cuts[i] } else { le };
        let q = unsafe {
            let a = *data.get_unchecked(s) == b'"';
            let b = *data.get_unchecked(e.wrapping_sub(1)) == b'"';
            (a & b & (e >= s + 2)) as u32
        };
        k += q;
        fields[i] = unsafe { data.get_unchecked(s + q as usize..e - q as usize) };
        s = e + 1;
    }
    if nq != 2 * k {
        return;
    }
    f(&fields);
}

#[inline(always)]
pub fn scan<const NF: usize, F: FnMut(&[&[u8]; NF])>(data: &[u8], start: usize, end: usize, skip_first: bool, mut f: F) {
    let base = data.as_ptr();
    let mut pos = start;
    if skip_first {
        match data[start..end].iter().position(|&c| c == b'\n') {
            Some(i) => pos = start + i + 1,
            None => return,
        }
    }
    if pos >= end {
        return;
    }
    let mut cuts = [0usize; 8];
    let mut nc = 0usize;
    let mut nq = 0u32;
    let mut ls = pos;
    let mut blk = pos;
    let n = data.len();
    loop {
        let (nl, cm, qt) = if blk + 64 <= n {
            unsafe { masks(base.add(blk)) }
        } else {
            let mut buf = [0u8; 64];
            buf[..n - blk].copy_from_slice(&data[blk..]);
            unsafe { masks(buf.as_ptr()) }
        };
        unsafe {
            use std::arch::x86_64::*;
            _mm_prefetch(base.add(blk + 1024) as *const i8, _MM_HINT_T0);
        }
        let mut sep = nl | cm;
        let mut qrest = qt;
        while sep != 0 {
            let t = sep.trailing_zeros() as usize;
            let bit = 1u64 << t;
            sep &= sep - 1;
            let p = blk + t;
            if nl & bit == 0 {
                if nc < 8 {
                    cuts[nc] = p;
                }
                nc += 1;
            } else {
                let below = bit - 1;
                nq += (qrest & below).count_ones();
                qrest &= !below;
                emit::<NF, F>(data, ls, p, &cuts, nc, nq, &mut f);
                nc = 0;
                nq = 0;
                ls = p + 1;
                if ls >= end {
                    return;
                }
            }
        }
        nq += qrest.count_ones();
        blk += 64;
        if blk >= end {
            if ls < end {
                // final line without a newline
                let mut cc = [0usize; 8];
                let mut k = 0;
                let mut q = 0u32;
                for (i, &c) in data[ls..end].iter().enumerate() {
                    if c == b',' {
                        if k < 8 {
                            cc[k] = ls + i;
                        }
                        k += 1;
                    }
                    q += (c == b'"') as u32;
                }
                emit::<NF, F>(data, ls, end, &cc, k, q, &mut f);
            }
            return;
        }
    }
}

// first line start at or after `x`
pub fn line_start(d: &[u8], x: usize) -> usize {
    if x == 0 || x >= d.len() {
        return x.min(d.len());
    }
    match d[x - 1..].iter().position(|&c| c == b'\n') {
        Some(i) => x + i,
        None => d.len(),
    }
}

pub fn chunks(data: &[u8], target: usize) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let n = data.len();
    let mut s = 0;
    while s < n {
        let mut e = s + target;
        if e >= n {
            out.push((s, n));
            break;
        }
        match data[e..].iter().position(|&c| c == b'\n') {
            Some(i) => e = e + i + 1,
            None => e = n,
        }
        out.push((s, e));
        s = e;
    }
    out
}
