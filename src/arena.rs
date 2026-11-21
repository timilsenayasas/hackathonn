// Per-thread bump arena on transparent huge pages + block-list radix scatter.

pub struct Arena {
    base: *mut u8,
    cap: usize,
    used: usize,
}
unsafe impl Send for Arena {}

impl Arena {
    pub fn new(cap: usize) -> Arena {
        unsafe {
            let p = libc::mmap(
                std::ptr::null_mut(),
                cap,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE,
                -1,
                0,
            );
            assert!(p != libc::MAP_FAILED, "arena mmap failed");
            libc::madvise(p, cap, libc::MADV_HUGEPAGE);
            Arena { base: p as *mut u8, cap, used: 0 }
        }
    }
    #[inline]
    pub fn alloc(&mut self, bytes: usize) -> *mut u8 {
        let a = (self.used + 63) & !63;
        assert!(a + bytes <= self.cap, "arena exhausted");
        self.used = a + bytes;
        unsafe { self.base.add(a) }
    }
}

#[derive(Clone, Copy)]
struct Tail<T> {
    cur: *mut T,
    end: *mut T,
}

/// P partitions, each a list of fixed-size blocks, filled through per-partition
/// 64-byte write-combining lines flushed with non-temporal stores
/// (when size_of::<T>() divides 64).
pub struct Scatter<T: Copy> {
    tails: Vec<Tail<T>>,
    blocks: Vec<Vec<(*mut T, usize)>>, // completed blocks per partition (ptr, len)
    block: usize,                     // records per block (multiple of per_line)
    wc: *mut u8,                      // P * 64 bytes, 64-aligned
    wcn: Vec<u8>,
    per_line: usize,
    pub cnt: Vec<u32>, // records pushed per partition (only maintained by push_counted)
    pub arena: Arena,
}
unsafe impl<T: Copy> Send for Scatter<T> {}
unsafe impl<T: Copy> Sync for Scatter<T> {}

impl<T: Copy> Scatter<T> {
    pub fn new(parts: usize, block: usize, arena_bytes: usize) -> Self {
        let n = std::ptr::null_mut();
        let sz = std::mem::size_of::<T>();
        let per_line = if 64 % sz == 0 { 64 / sz } else { 0 };
        let mut arena = Arena::new(arena_bytes);
        let wc = arena.alloc(parts * 64);
        let block = if per_line > 0 { (block + per_line - 1) / per_line * per_line } else { block };
        Scatter { tails: vec![Tail { cur: n, end: n }; parts], blocks: (0..parts).map(|_| Vec::new()).collect(), block, wc, wcn: vec![0; parts], per_line, cnt: vec![0; parts], arena }
    }
    #[inline(always)]
    pub fn push(&mut self, p: usize, x: T) {
        unsafe {
            if self.per_line == 0 {
                let t = self.tails.get_unchecked_mut(p);
                if t.cur == t.end {
                    self.grow(p);
                }
                let t = self.tails.get_unchecked_mut(p);
                t.cur.write(x);
                t.cur = t.cur.add(1);
                return;
            }
            let k = *self.wcn.get_unchecked(p) as usize;
            let line = self.wc.add(p * 64) as *mut T;
            line.add(k).write(x);
            if k + 1 == self.per_line {
                *self.wcn.get_unchecked_mut(p) = 0;
                let t = self.tails.get_unchecked_mut(p);
                if t.cur == t.end {
                    self.grow(p);
                }
                let t = self.tails.get_unchecked_mut(p);
                use std::arch::x86_64::*;
                let src = line as *const __m256i;
                let dst = t.cur as *mut __m256i;
                _mm256_stream_si256(dst, _mm256_load_si256(src));
                _mm256_stream_si256(dst.add(1), _mm256_load_si256(src.add(1)));
                t.cur = t.cur.add(self.per_line);
            } else {
                *self.wcn.get_unchecked_mut(p) = (k + 1) as u8;
            }
        }
    }
    /// Push and return the record's index within partition p (for this Scatter).
    #[inline(always)]
    pub fn push_counted(&mut self, p: usize, x: T) -> u32 {
        let i = unsafe { *self.cnt.get_unchecked(p) };
        unsafe { *self.cnt.get_unchecked_mut(p) = i + 1 };
        self.push(p, x);
        i
    }
    /// Flush partially filled write-combining lines. Call once, by the owner, before reading.
    pub fn finish(&mut self) {
        if self.per_line == 0 {
            return;
        }
        for p in 0..self.tails.len() {
            let k = self.wcn[p] as usize;
            for i in 0..k {
                let x = unsafe { (self.wc.add(p * 64) as *const T).add(i).read() };
                let t = self.tails[p];
                if t.cur == t.end {
                    self.grow(p);
                }
                let t = &mut self.tails[p];
                unsafe {
                    t.cur.write(x);
                    t.cur = t.cur.add(1);
                }
            }
            self.wcn[p] = 0;
        }
        unsafe { std::arch::x86_64::_mm_sfence() };
    }
    #[cold]
    #[inline(never)]
    fn grow(&mut self, p: usize) {
        let t = self.tails[p];
        if !t.end.is_null() {
            let start = unsafe { t.end.sub(self.block) };
            self.blocks[p].push((start, self.block));
        }
        let b = self.arena.alloc(self.block * std::mem::size_of::<T>()) as *mut T;
        self.tails[p] = Tail { cur: b, end: unsafe { b.add(self.block) } };
    }
    /// All records of partition p as slices.
    pub fn slices(&self, p: usize) -> impl Iterator<Item = &[T]> + '_ {
        let t = self.tails[p];
        let last: &[T] = if t.end.is_null() {
            &[]
        } else {
            unsafe {
                let start = t.end.sub(self.block);
                std::slice::from_raw_parts(start, t.cur.offset_from(start) as usize)
            }
        };
        self.blocks[p]
            .iter()
            .map(|&(ptr, n)| unsafe { std::slice::from_raw_parts(ptr as *const T, n) })
            .chain(std::iter::once(last))
    }
    pub fn len(&self, p: usize) -> usize {
        self.slices(p).map(|s| s.len()).sum()
    }
}

#[inline(always)]
pub fn prefetch<T>(p: *const T) {
    unsafe { std::arch::x86_64::_mm_prefetch(p as *const i8, std::arch::x86_64::_MM_HINT_T0) }
}

/// Global allocator: allocations >= 2 MiB go straight to 2 MiB-aligned mmap with
/// MADV_HUGEPAGE (fewer TLB misses and page faults); everything else uses the system allocator.
pub struct HugeAlloc;
const HUGE: usize = 1 << 21;
#[inline]
fn round_huge(n: usize) -> usize {
    (n + HUGE - 1) & !(HUGE - 1)
}
unsafe fn huge_map(size: usize) -> *mut u8 {
    let total = size + HUGE;
    let p = libc::mmap(std::ptr::null_mut(), total, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE, -1, 0);
    if p == libc::MAP_FAILED {
        return std::ptr::null_mut();
    }
    let a = (p as usize + HUGE - 1) & !(HUGE - 1);
    let head = a - p as usize;
    if head > 0 {
        libc::munmap(p, head);
    }
    let tail = total - head - size;
    if tail > 0 {
        libc::munmap((a + size) as *mut libc::c_void, tail);
    }
    libc::madvise(a as *mut libc::c_void, size, libc::MADV_HUGEPAGE);
    a as *mut u8
}
unsafe impl std::alloc::GlobalAlloc for HugeAlloc {
    unsafe fn alloc(&self, l: std::alloc::Layout) -> *mut u8 {
        if l.size() >= HUGE && l.align() <= HUGE {
            huge_map(round_huge(l.size()))
        } else {
            std::alloc::System.alloc(l)
        }
    }
    unsafe fn alloc_zeroed(&self, l: std::alloc::Layout) -> *mut u8 {
        if l.size() >= HUGE && l.align() <= HUGE {
            huge_map(round_huge(l.size()))
        } else {
            std::alloc::System.alloc_zeroed(l)
        }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: std::alloc::Layout) {
        if l.size() >= HUGE && l.align() <= HUGE {
            libc::munmap(p as *mut libc::c_void, round_huge(l.size()));
        } else {
            std::alloc::System.dealloc(p, l)
        }
    }
    unsafe fn realloc(&self, p: *mut u8, l: std::alloc::Layout, new: usize) -> *mut u8 {
        let old_big = l.size() >= HUGE && l.align() <= HUGE;
        let new_big = new >= HUGE && l.align() <= HUGE;
        if !old_big && !new_big {
            return std::alloc::System.realloc(p, l, new);
        }
        if old_big && new_big {
            let (a, b) = (round_huge(l.size()), round_huge(new));
            if a == b {
                return p;
            }
            let q = huge_map(b);
            if q.is_null() {
                return q;
            }
            std::ptr::copy_nonoverlapping(p, q, l.size().min(new));
            libc::munmap(p as *mut libc::c_void, a);
            return q;
        }
        let nl = std::alloc::Layout::from_size_align_unchecked(new, l.align());
        let q = self.alloc(nl);
        if !q.is_null() {
            std::ptr::copy_nonoverlapping(p, q, l.size().min(new));
            self.dealloc(p, l);
        }
        q
    }
}
