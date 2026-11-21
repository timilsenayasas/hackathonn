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
            Arena {
                base: p as *mut u8,
                cap,
                used: 0,
            }
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

pub struct Scatter<T: Copy> {
    tails: Vec<Tail<T>>,
    blocks: Vec<Vec<(*mut T, usize)>>,
    block: usize,
    wc: *mut u8,
    wcn: Vec<u8>,
    per_line: usize,
    pub cnt: Vec<u32>,
    pub free: Vec<*mut u8>,
    pub arena: Arena,
}

pub const BLK: usize = 16 << 10;
unsafe impl<T: Copy> Send for Scatter<T> {}
unsafe impl<T: Copy> Sync for Scatter<T> {}

impl<T: Copy> Scatter<T> {
    pub fn new(parts: usize, arena_bytes: usize) -> Self {
        let n = std::ptr::null_mut();
        let sz = std::mem::size_of::<T>();
        let per_line = if 64 % sz == 0 { 64 / sz } else { 0 };
        let mut arena = Arena::new(arena_bytes);
        let wc = arena.alloc(parts * 64);
        let block = BLK / sz;
        Scatter {
            tails: vec![Tail { cur: n, end: n }; parts],
            blocks: (0..parts).map(|_| Vec::new()).collect(),
            block,
            wc,
            wcn: vec![0; parts],
            per_line,
            cnt: vec![0; parts],
            free: Vec::new(),
            arena,
        }
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
    #[inline(always)]
    pub fn push_counted(&mut self, p: usize, x: T) -> u32 {
        let i = unsafe { *self.cnt.get_unchecked(p) };
        unsafe { *self.cnt.get_unchecked_mut(p) = i + 1 };
        self.push(p, x);
        i
    }
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
        let b = match self.free.pop() {
            Some(b) => b as *mut T,
            None => self.arena.alloc(BLK) as *mut T,
        };
        self.tails[p] = Tail {
            cur: b,
            end: unsafe { b.add(self.block) },
        };
    }
    pub fn block_ptrs(&self, p: usize, out: &mut Vec<*mut u8>) {
        for &(b, _) in &self.blocks[p] {
            out.push(b as *mut u8);
        }
        let t = self.tails[p];
        if !t.end.is_null() {
            out.push(unsafe { t.end.sub(self.block) } as *mut u8);
        }
    }
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

pub struct HugeAlloc;
const HUGE: usize = 1 << 21;
#[inline]
fn round_huge(n: usize) -> usize {
    (n + HUGE - 1) & !(HUGE - 1)
}
unsafe fn huge_map(size: usize) -> *mut u8 {
    let total = size + HUGE;
    let p = libc::mmap(
        std::ptr::null_mut(),
        total,
        libc::PROT_READ | libc::PROT_WRITE,
        libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE,
        -1,
        0,
    );
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
            let q = libc::mremap(p as *mut libc::c_void, a, b, libc::MREMAP_MAYMOVE);
            if q == libc::MAP_FAILED {
                return std::ptr::null_mut();
            }
            libc::madvise(q, b, libc::MADV_HUGEPAGE);
            return q as *mut u8;
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
