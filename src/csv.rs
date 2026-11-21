// AVX2 line-at-a-time scanner for the rules.md CSV dialect.
// For every well-formed data line with exactly NF fields, calls f(fields, line_start_offset)
// with quotes stripped. Malformed lines are skipped.

#[inline(always)]
unsafe fn m64(p: *const u8) -> (u64, u64, u64) {
    use std::arch::x86_64::*;
    let a = _mm256_loadu_si256(p as *const __m256i);
    let b = _mm256_loadu_si256(p.add(32) as *const __m256i);
    let nl = _mm256_set1_epi8(b'\n' as i8);
    let cm = _mm256_set1_epi8(b',' as i8);
    let qt = _mm256_set1_epi8(b'"' as i8);
    let m = |v: __m256i, c: __m256i| _mm256_movemask_epi8(_mm256_cmpeq_epi8(v, c)) as u32 as u64;
    (m(a, nl) | (m(b, nl) << 32), m(a, cm) | (m(b, cm) << 32), m(a, qt) | (m(b, qt) << 32))
}

#[inline(always)]
fn low(n: u32) -> u128 {
    if n >= 128 {
        !0
    } else {
        (1u128 << n) - 1
    }
}

/// Split one line (bytes [ls, ls+len) of `data`) given its comma/quote masks (relative to ls).
/// Branch-free field and quote handling.
#[inline(always)]
fn line<const NF: usize, F: FnMut(&[&[u8]; NF])>(data: &[u8], ls: usize, len: usize, mut cm: u128, qm: u128, f: &mut F) {
    if cm.count_ones() as usize != NF - 1 {
        return;
    }
    let mut fields: [&[u8]; NF] = [&[]; NF];
    let mut s = 0usize;
    let mut k = 0u32;
    for i in 0..NF {
        let e = if i + 1 < NF {
            let t = cm.trailing_zeros() as usize;
            cm &= cm - 1;
            t
        } else {
            len
        };
        let q = unsafe {
            let a = *data.get_unchecked(ls + s) == b'"';
            let b = *data.get_unchecked((ls + e).wrapping_sub(1)) == b'"';
            (a & b & (e >= s + 2)) as u32
        };
        k += q;
        let fs = s + q as usize;
        let fe = e - q as usize;
        fields[i] = unsafe { data.get_unchecked(ls + fs..ls + fe) };
        s = e + 1;
    }
    if qm.count_ones() != 2 * k {
        return;
    }
    f(&fields);
}

/// Generic slow path for one line [ls, le) (long lines).
fn slow_line<const NF: usize, F: FnMut(&[&[u8]; NF])>(data: &[u8], ls: usize, le: usize, f: &mut F) {
    let l = &data[ls..le];
    let mut ends = [0usize; 8];
    let mut nf = 0;
    for (i, &c) in l.iter().enumerate() {
        if c == b',' {
            if nf < 8 {
                ends[nf] = i;
            }
            nf += 1;
        }
    }
    if nf + 1 != NF {
        return;
    }
    ends[nf] = l.len();
    let mut fields: [&[u8]; NF] = [&[]; NF];
    let mut s = 0;
    let mut k = 0;
    let q = l.iter().filter(|&&c| c == b'"').count();
    for i in 0..NF {
        let mut v = &l[s..ends[i]];
        if v.len() >= 2 && v[0] == b'"' && v[v.len() - 1] == b'"' {
            k += 1;
            v = &v[1..v.len() - 1];
        }
        fields[i] = v;
        s = ends[i] + 1;
    }
    if q != 2 * k {
        return;
    }
    f(&fields);
}

/// Scan data[start..end). `end` is just after a '\n' or the end of data.
#[inline(always)]
pub fn scan<const NF: usize, F: FnMut(&[&[u8]; NF])>(data: &[u8], start: usize, end: usize, skip_first: bool, mut f: F) {
    let n = data.len();
    let base = data.as_ptr();
    let mut pos = start;
    if skip_first {
        match data[start..end].iter().position(|&c| c == b'\n') {
            Some(i) => pos = start + i + 1,
            None => return,
        }
    }
    // fast region: a full 128-byte window is readable
    while pos < end && pos + 128 <= n {
        let p = unsafe { base.add(pos) };
        // always classify the full 128-byte window: no data-dependent branch on line length
        let (nl0, cm0, qt0) = unsafe { m64(p) };
        let (nl1, cm1, qt1) = unsafe { m64(p.add(64)) };
        let nl = nl0 as u128 | ((nl1 as u128) << 64);
        if nl != 0 {
            let len = nl.trailing_zeros();
            let mm = (1u128 << len) - 1;
            let cm = (cm0 as u128 | ((cm1 as u128) << 64)) & mm;
            let qm = (qt0 as u128 | ((qt1 as u128) << 64)) & mm;
            line::<NF, F>(data, pos, len as usize, cm, qm, &mut f);
            pos += len as usize + 1;
            continue;
        }
        // long line
        let le = match data[pos..end].iter().position(|&c| c == b'\n') {
            Some(i) => pos + i,
            None => end,
        };
        slow_line::<NF, F>(data, pos, le, &mut f);
        pos = le + 1;
    }
    // tail: near the end of the file
    while pos < end {
        let le = match data[pos..end].iter().position(|&c| c == b'\n') {
            Some(i) => pos + i,
            None => end,
        };
        slow_line::<NF, F>(data, pos, le, &mut f);
        pos = le + 1;
    }
}

/// Split [0, len) into chunks of about `target` bytes, each ending just after a '\n' (or at EOF).
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
