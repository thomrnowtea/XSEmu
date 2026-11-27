//! Console unified memory (UMA). On the console CPU and GPU share memory and shaders reach
//! it through plain addresses in SGPRs. XCL keeps the title's graphics memory in regions
//! that D3D12 can open as heaps (`ID3D12Device3::OpenExistingHeapFromAddress`), so the GPU
//! works on them directly.
//!
//! A heap is smaller than 4 GiB, so the memory is up to 4 *windows*: window k is one region
//! at `base + k · 4 GiB` with `base` 16 GiB-aligned. The window of a console GPU address is
//! then `(address >> 32) & 3` and its low 32 bits are the offset in that window's heap – the
//! recompiler's `mem` (window 0), `mem1` … buffers. Allocations never straddle windows.
//!
//! Measured on Windows 11 + RX 9070 (2026-11-20): the heap must cover a fully committed
//! `VirtualAlloc` allocation (a reservation with partial commit is rejected); up to
//! 4095 MiB works (exactly 4 GiB is rejected), opening 4 GiB takes ~0.5 s.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Mutex, OnceLock};

use windows_sys::Win32::System::Memory::{VirtualAlloc, VirtualFree, MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE};

/// VirtualAlloc: track CPU writes (GetWriteWatch)
const MEM_WRITE_WATCH: u32 = 0x20_0000;

/// allocation granularity (as VirtualAlloc)
pub const GRAIN: usize = 0x1_0000;
/// largest region D3D12 opens as a heap
pub const MAX_SIZE: usize = (4096 << 20) - GRAIN;
/// distance between windows
pub const WINDOW: usize = 1 << 32;
pub const MAX_WINDOWS: usize = 4;

/// bytes of sparse window 0 only 32-bit descriptor heaps get (`Uma::desc_range`)
const DESC_HEADROOM: usize = 512 << 20;
/// cap on the eager committed coherent window in `for_runtime` (per-draw args, readback buffers)
const RUNTIME_COMMITTED_CAP: usize = 512 << 20;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum WindowKind {
    None,
    Committed(usize),
    Sparse,
}

pub struct Uma {
    base: usize,
    kinds: [WindowKind; MAX_WINDOWS],
    /// VirtualAlloc base pointers owned by this Uma (freed on Drop)
    win_allocs: Vec<usize>,
    inner: Mutex<Inner>,
    /// free ranges of the sparse windows (address → length) and the reservations in them
    sparse_free: Mutex<SparseRanges>,
    /// XCL's own pool in window 0 for `Arena` blocks: (start, end, free blocks). The title
    /// cannot take it – a title that commits all of unified memory at start (Unicorn
    /// Overlord: 2046 of 2048 MB) left no room for draw arguments, and draws were skipped.
    pool: Mutex<(usize, usize, Vec<usize>)>,
    /// windows remade as driver heaps (`hle::d3d12x::uma_driver`), one bit each
    driver: std::sync::atomic::AtomicU32,
}

/// bytes of XCL's arena pool (`Uma::new`)
pub const POOL: usize = 64 << 20;

/// (diagnostics) A block of a committed window handed out or given back: where, when (s of the
/// run), and the XCL code that asked.
#[derive(Clone, Copy)]
struct Event {
    at: usize,
    len: usize,
    t: f32,
    by: &'static std::panic::Location<'static>,
    back: bool,
}

/// The last blocks handed out and given back in the committed windows, the oldest first. The
/// window is one allocator for the title's memory and for XCL's own GPU-written blocks, first
/// fit, and a block given back is handed out again at once: a crash record asks whose an
/// address was before it was whose it is now.
static HISTORY: Mutex<std::collections::VecDeque<Event>> = Mutex::new(std::collections::VecDeque::new());
const HISTORY_KEPT: usize = 65536;

fn note_event(at: usize, len: usize, back: bool, by: &'static std::panic::Location<'static>) {
    let mut h = HISTORY.lock().unwrap_or_else(|e| e.into_inner());
    if h.len() == HISTORY_KEPT {
        h.pop_front();
    }
    h.push_back(Event { at, len, t: crate::log::uptime() as f32, by, back });
}

/// (diagnostics, `memdiag`) The block history: (events, bytes of one, bytes it holds).
pub fn diag_history() -> (usize, usize, usize) {
    let h = HISTORY.lock().unwrap_or_else(|e| e.into_inner());
    (h.len(), std::mem::size_of::<Event>(), h.capacity() * std::mem::size_of::<Event>())
}

/// (diagnostics, `memdiag`) The blocks handed out in the committed windows, by address: where,
/// how many bytes, and the XCL code that asked (None: no longer in the history; the title's
/// allocations come by `kernelx`).
pub fn diag_blocks() -> Vec<(usize, usize, Option<&'static std::panic::Location<'static>>)> {
    let Some(u) = made() else { return Vec::new() };
    let blocks: Vec<(usize, usize)> = u.inner.lock().unwrap_or_else(|e| e.into_inner()).used.iter().map(|(&at, &len)| (u.base + at, len)).collect();
    let mut by = std::collections::HashMap::new();
    for e in HISTORY.lock().unwrap_or_else(|e| e.into_inner()).iter().filter(|e| !e.back) {
        by.insert(e.at, e.by);
    }
    blocks.into_iter().map(|(at, len)| (at, len, by.get(&at).copied())).collect()
}

/// (diagnostics, `alias::report`) Whose the memory at `p` in a committed window is and was:
/// the block it is in now – when it was handed out and to which code of XCL's (the title's
/// allocations come by `kernelx`) – and the blocks that were there before, the latest first.
pub fn history_at(p: usize) -> Vec<String> {
    if !get().is_some_and(|u| u.is_committed(p)) {
        return Vec::new();
    }
    let Some(h) = crate::alias::try_lock(&HISTORY) else {
        return vec!["the blocks handed out at this address: (locked by another thread)".into()];
    };
    let events: Vec<Event> = h.iter().copied().collect();
    describe_history(&events, p, crate::log::uptime() as f32)
}

fn describe_history(events: &[Event], p: usize, now: f32) -> Vec<String> {
    let src = |e: &Event| {
        let f = e.by.file().replace('\\', "/");
        format!("{}:{}", f.rsplit_once("src/").map_or(f.as_str(), |x| x.1), e.by.line())
    };
    let here: Vec<&Event> = events.iter().rev().filter(|e| e.at <= p && p - e.at < e.len).take(8).collect();
    let Some(last) = here.first() else {
        return vec![format!("the block at this address was handed out before the last {} blocks of the window were handed out or given back (or never)", events.len())];
    };
    let one = |e: &Event| format!("{:.3} s ({:.3} s before): {:#x} + {:#x} {} {}", e.t, now - e.t, e.at, e.len, if e.back { "given back by" } else { "handed out to" }, src(e));
    let mut out = vec![if last.back {
        format!("this address is in no block now: the last one was given back {:.3} s before ({})", now - last.t, one(last))
    } else {
        format!("this address is in a block handed out {:.3} s before: {}", now - last.t, one(last))
    }];
    if here.len() > 1 {
        out.push(format!("the blocks at this address before, the latest first: {}", here[1..].iter().map(|e| one(e)).collect::<Vec<_>>().join("; ")));
    }
    out
}

#[cfg(test)]
mod history_tests {
    use super::{describe_history, Event};

    /// An address names the block it is in with when and for whom, the blocks there before,
    /// and says so when nothing is known of it.
    #[test]
    fn an_address_names_its_block_and_the_blocks_before() {
        let by = std::panic::Location::caller();
        let e = |at, len, t, back| Event { at, len, t, by, back };
        let events = [e(0x10_0000, 0x2_0000, 1.0, false), e(0x10_0000, 0x2_0000, 5.0, true), e(0x11_0000, 0x1_0000, 5.5, false), e(0x40_0000, 0x1_0000, 6.0, false)];
        let at = describe_history(&events, 0x11_0040, 7.0);
        assert_eq!(at.len(), 2, "{at:?}");
        assert!(at[0].starts_with("this address is in a block handed out 1.500 s before: 5.500 s (1.500 s before): 0x110000 + 0x10000 handed out to gpu/uma.rs:"), "{at:?}");
        assert!(at[1].contains("5.000 s (2.000 s before): 0x100000 + 0x20000 given back by") && at[1].contains("; 1.000 s (6.000 s before): 0x100000 + 0x20000 handed out to"), "{at:?}");
        let gone = describe_history(&events, 0x10_0040, 7.0);
        assert!(gone[0].starts_with("this address is in no block now: the last one was given back 2.000 s before"), "{gone:?}");
        assert!(describe_history(&events, 0x90_0000, 7.0)[0].contains("before the last 4 blocks"));
    }
}

#[derive(Default)]
struct Inner {
    /// free ranges: offset → length (coalesced)
    free: BTreeMap<usize, usize>,
    /// live allocations: offset → length
    used: BTreeMap<usize, usize>,
    used_bytes: usize,
    /// the most `used_bytes` was
    peak_bytes: usize,
    reserved: BTreeSet<usize>,
}

#[derive(Default)]
struct SparseRanges {
    ranges: (BTreeMap<usize, usize>, BTreeMap<usize, usize>),
    used_bytes: usize,
    /// reservations (their starts) whose tiles get video memory when it is first needed, not
    /// when they are committed (`sparse_set_lazy`, `d3d12x::sparse`)
    lazy: BTreeSet<usize>,
}

impl SparseRanges {
    fn take(&mut self, lo: usize, hi: usize, size: usize, align: usize, single_window: bool) -> Option<usize> {
        let at = take_sparse_in(&mut self.ranges, lo, hi, size, align, single_window)?;
        self.used_bytes += size;
        Some(at)
    }

    fn take_top(&mut self, lo: usize, hi: usize, size: usize, align: usize) -> Option<usize> {
        let at = take_sparse_top(&mut self.ranges, lo, hi, size, align)?;
        self.used_bytes += size;
        Some(at)
    }
}

/// `take_sparse_in` from the other end: the highest `align`-aligned block of `size` bytes (a
/// window at most, and never across two) that `free` has within `[lo_bound, hi_bound)`. What is
/// taken from the top leaves the low addresses in one piece for a large reservation.
fn take_sparse_top(g: &mut (BTreeMap<usize, usize>, BTreeMap<usize, usize>), lo_bound: usize, hi_bound: usize, size: usize, align: usize) -> Option<usize> {
    if lo_bound >= hi_bound || size == 0 || size > WINDOW {
        return None;
    }
    let last_fit = |(&o, &l): (&usize, &usize)| {
        let lo = o.max(lo_bound);
        let hi = o.saturating_add(l).min(hi_bound);
        let mut at = hi.checked_sub(size)? & !(align - 1);
        while at >= lo {
            if at / WINDOW == (at + size - 1) / WINDOW {
                return Some((o, l, at));
            }
            at = ((at + size - 1) / WINDOW * WINDOW).checked_sub(size)? & !(align - 1);
        }
        None
    };
    let (start, len, at) = g.0.range(..hi_bound).rev().take_while(|&(&o, &l)| o.saturating_add(l) > lo_bound).find_map(last_fit)?;
    g.0.remove(&start);
    if at > start {
        g.0.insert(start, at - start);
    }
    if at + size < start + len {
        g.0.insert(at + size, start + len - (at + size));
    }
    g.1.insert(at, size);
    Some(at)
}

/// The parts of `[a, b)` that lie in the reservations of `reserved` (start → bytes) whose start
/// is in `lazy`, as (start, end) by address.
fn lazy_parts(lazy: &BTreeSet<usize>, reserved: &BTreeMap<usize, usize>, a: usize, b: usize) -> Vec<(usize, usize)> {
    if a >= b {
        return Vec::new();
    }
    let from = lazy.range(..=a).next_back().copied().unwrap_or(a);
    lazy.range(from..b)
        .filter_map(|&start| {
            let (lo, hi) = (start.max(a), start.saturating_add(*reserved.get(&start)?).min(b));
            (lo < hi).then_some((lo, hi))
        })
        .collect()
}

/// Carve an `align`-aligned block of `size` bytes from `free` within `[lo_bound, hi_bound)`.
/// When `single_window` is true and `size <= WINDOW`, the returned block never straddles a
/// 4 GiB window boundary.
fn take_sparse_in(
    g: &mut (BTreeMap<usize, usize>, BTreeMap<usize, usize>),
    lo_bound: usize,
    hi_bound: usize,
    size: usize,
    align: usize,
    single_window: bool,
) -> Option<usize> {
    if lo_bound >= hi_bound {
        return None;
    }
    let first_fit = |(&o, &l): (&usize, &usize)| {
        let lo = o.max(lo_bound);
        let hi = o.saturating_add(l).min(hi_bound);
        if lo >= hi {
            return None;
        }
        let mut at = (lo.checked_add(align - 1)?) & !(align - 1);
        while at.checked_add(size).is_some_and(|e| e <= hi) {
            if !single_window || size > WINDOW || at / WINDOW == (at + size - 1) / WINDOW {
                return Some((o, l, at));
            }
            let next_win = (at / WINDOW + 1).checked_mul(WINDOW)?;
            at = (next_win.checked_add(align - 1)?) & !(align - 1);
        }
        None
    };
    let found = g.0.range(..lo_bound).next_back().and_then(first_fit)
        .or_else(|| g.0.range(lo_bound..hi_bound).find_map(first_fit));
    let (start, len, at) = found?;
    g.0.remove(&start);
    if at > start {
        g.0.insert(start, at - start);
    }
    if at + size < start + len {
        g.0.insert(at + size, start + len - (at + size));
    }
    g.1.insert(at, size);
    Some(at)
}

impl Uma {
    /// Commit `size` bytes (split into as few equal windows as fit, at most 4) at the first
    /// free 16 GiB-aligned address from 16 GiB up, with remaining windows reserved as sparse.
    pub fn new(size: usize) -> Option<Uma> {
        let size = ((size + GRAIN - 1) & !(GRAIN - 1)).max(GRAIN);
        let n = size.div_ceil(MAX_SIZE).clamp(1, MAX_WINDOWS);
        let mut u = Uma::with_windows(n, (size.div_ceil(n) + GRAIN - 1) & !(GRAIN - 1))?;
        u.reserve_sparse_windows();
        u.reserve_pool(POOL);
        Some(u)
    }

    /// XCL's arena pool: `bytes` at the end of the first committed window taken now, before
    /// the title can (a quarter of that window at most); the title's allocations still start
    /// at the window's start.
    pub fn reserve_pool(&self, bytes: usize) {
        let Some(win) = self.kinds.iter().position(|w| matches!(w, WindowKind::Committed(_))) else { return };
        let win_size = self.window(win).1;
        let bytes = (bytes.min(win_size / 4) / ARENA_BLOCK) * ARENA_BLOCK;
        if bytes == 0 {
            return;
        }
        let at = {
            let mut g = self.inner.lock().unwrap();
            // the free range of the committed window that reaches furthest, its last `bytes`
            let Some((start, len)) = g.free.iter().filter(|(&o, &l)| o / WINDOW == win && l >= bytes).max_by_key(|(&o, &l)| o + l).map(|(&o, &l)| (o, l)) else { return };
            let at = (start + len - bytes) & !(GRAIN - 1);
            if at < start {
                return;
            }
            g.free.remove(&start);
            if at > start {
                g.free.insert(start, at - start);
            }
            if at + bytes < start + len {
                g.free.insert(at + bytes, start + len - at - bytes);
            }
            g.used.insert(at, bytes);
            g.used_bytes += bytes;
            g.peak_bytes = g.peak_bytes.max(g.used_bytes);
            at
        };
        let p = self.base + at;
        unsafe { std::ptr::write_bytes(p as *mut u8, 0, bytes) };
        *self.pool.lock().unwrap() = (p, p + bytes, (p..p + bytes).step_by(ARENA_BLOCK).rev().collect());
    }

    /// A block of `ARENA_BLOCK` bytes from XCL's pool (None: the pool is used up or absent).
    fn pool_block(&self) -> Option<usize> {
        self.pool.lock().unwrap().2.pop()
    }

    /// Runtime layout: windows 0..3 (12 GiB) reserved as write-watched sparse VRAM windows
    /// (window 0 holds 32-bit-addressed descriptor heaps + buffers + XMem, windows 1..2 hold
    /// large title reservations and overflow), and window 3 as a smaller committed coherent
    /// window (`OpenExistingHeapFromAddress`) for per-draw arguments and CPU readback buffers.
    pub fn for_runtime(size: usize) -> Option<Uma> {
        Self::for_runtime_with(size, RUNTIME_COMMITTED_CAP)
    }

    /// `for_runtime` with a coherent window of up to `coherent` bytes (`coherent_hint`).
    pub fn for_runtime_with(size: usize, coherent: usize) -> Option<Uma> {
        let committed = ((size.min(coherent.max(RUNTIME_COMMITTED_CAP)) + GRAIN - 1) & !(GRAIN - 1)).clamp(GRAIN, MAX_SIZE);
        let sparse_len = (MAX_WINDOWS - 1) * WINDOW;
        for slot in (4usize..512).step_by(MAX_WINDOWS) {
            let base = slot << 32;
            let sp = unsafe { VirtualAlloc(base as *const std::ffi::c_void, sparse_len, MEM_RESERVE | MEM_WRITE_WATCH, PAGE_READWRITE) };
            if sp as usize != base {
                if !sp.is_null() {
                    unsafe { VirtualFree(sp, 0, MEM_RELEASE) };
                }
                continue;
            }
            let c_base = base + (MAX_WINDOWS - 1) * WINDOW;
            let cp = unsafe { VirtualAlloc(c_base as *const std::ffi::c_void, committed, MEM_RESERVE | MEM_COMMIT | MEM_WRITE_WATCH, PAGE_READWRITE) };
            if cp as usize != c_base {
                if !cp.is_null() {
                    unsafe { VirtualFree(cp, 0, MEM_RELEASE) };
                }
                unsafe { VirtualFree(sp, 0, MEM_RELEASE) };
                continue;
            }
            let mut inner = Inner::default();
            inner.free.insert((MAX_WINDOWS - 1) * WINDOW, committed);
            let mut sparse_free = SparseRanges::default();
            sparse_free.ranges.0.insert(base, sparse_len);
            let u = Uma {
                base,
                kinds: [WindowKind::Sparse, WindowKind::Sparse, WindowKind::Sparse, WindowKind::Committed(committed)],
                win_allocs: vec![base, c_base],
                inner: Mutex::new(inner),
                sparse_free: Mutex::new(sparse_free),
                pool: Mutex::new((0, 0, Vec::new())),
                driver: Default::default(),
            };
            u.reserve_pool(POOL);
            return Some(u);
        }
        Uma::new(size)
    }

    /// The windows after the committed ones as one write-watched reservation (sparse windows).
    fn reserve_sparse_windows(&mut self) {
        let committed = self.windows();
        if committed >= MAX_WINDOWS {
            return;
        }
        let start = self.base + committed * WINDOW;
        let len = (MAX_WINDOWS - committed) * WINDOW;
        let p = unsafe { VirtualAlloc(start as *const std::ffi::c_void, len, MEM_RESERVE | MEM_WRITE_WATCH, PAGE_READWRITE) };
        if p as usize == start {
            for k in committed..MAX_WINDOWS {
                self.kinds[k] = WindowKind::Sparse;
            }
            self.win_allocs.push(start);
            self.sparse_free.lock().unwrap().ranges.0.insert(start, len);
        } else if !p.is_null() {
            unsafe { VirtualFree(p, 0, MEM_RELEASE) };
        }
    }

    /// The range of sparse window 0 kept for 32-bit descriptor heaps (`sparse_alloc_in(0)`):
    /// after the window's first 64 KiB, which stay unused (a console address of 0 reads as
    /// null). The title's memory never takes it, and it sits at the start of the window so the
    /// title still gets one contiguous range of 11.5 GiB: XDefiant reserves 8 GiB + 4 MiB first
    /// thing, more than windows 1..2 hold, and with the range at the top of window 0 that
    /// reservation took all of it – no descriptor heap could be made, every frame was black.
    fn desc_range(&self) -> Option<(usize, usize)> {
        self.is_sparse_window(0).then(|| (self.base + GRAIN, self.base + GRAIN + DESC_HEADROOM))
    }

    /// The first sparse address the title's memory may use (after `desc_range`).
    fn title_sparse_start(&self) -> usize {
        self.desc_range().map_or(self.base, |(_, end)| end)
    }

    /// Whether window `k` is a sparse VRAM window.
    pub fn is_sparse_window(&self, k: usize) -> bool {
        self.kinds.get(k).is_some_and(|w| matches!(w, WindowKind::Sparse))
    }

    /// Whether window `k` is a committed coherent window.
    pub fn is_committed_window(&self, k: usize) -> bool {
        self.kinds.get(k).is_some_and(|w| matches!(w, WindowKind::Committed(_)))
    }

    /// Whether this Uma has any sparse VRAM windows.
    pub fn has_sparse(&self) -> bool {
        self.kinds.iter().any(|w| matches!(w, WindowKind::Sparse))
    }

    /// Windows translated shaders address (`mem`, `mem1` …): highest active window + 1.
    pub fn shader_windows(&self) -> usize {
        self.kinds.iter().rposition(|w| !matches!(w, WindowKind::None)).map_or(0, |i| i + 1)
    }

    /// Whether `p` is in a sparse window.
    pub fn is_sparse(&self, p: usize) -> bool {
        self.window_of(p).is_some_and(|k| self.is_sparse_window(k))
    }

    /// Whether `p` is in a committed window.
    pub fn is_committed(&self, p: usize) -> bool {
        self.window_of(p).is_some_and(|k| self.is_committed_window(k))
    }

    /// Window `k` is a driver heap now (`hle::d3d12x::uma_driver`).
    pub fn set_driver_window(&self, k: usize) {
        self.driver.fetch_or(1 << k, std::sync::atomic::Ordering::Relaxed);
    }

    /// Whether window `k` is a driver heap (`set_driver_window`).
    pub fn is_driver_window(&self, k: usize) -> bool {
        k < 32 && self.driver.load(std::sync::atomic::Ordering::Relaxed) & (1 << k) != 0
    }

    /// Whether `[p, p + len)` lies in a committed coherent window – memory that is committed and
    /// read-write as a whole, so it needs no `VirtualQuery` (which walks a region to its end to
    /// size it: on a 4 GiB window ~40 ms a call, a retail game's DirectStorage requests crawled).
    pub fn in_committed_window(&self, p: usize, len: usize) -> bool {
        len > 0 && self.is_committed(p) && p.checked_add(len - 1).is_some_and(|e| self.window_of(e) == self.window_of(p))
    }

    /// Address space for a title reservation of `size` bytes in the sparse windows (64 KiB
    /// aligned, first fit; not committed). Prefers windows 1+ so window 0 stays uncluttered for
    /// small allocations, falling back to all of the title's sparse space (never `desc_range`).
    pub fn sparse_reserve(&self, size: usize) -> Option<usize> {
        if !self.has_sparse() {
            return None;
        }
        let size = (size.max(1) + GRAIN - 1) & !(GRAIN - 1);
        let mut g = self.sparse_free.lock().unwrap();
        let end = self.base + MAX_WINDOWS * WINDOW;
        let lo = self.title_sparse_start();
        if let Some(at) = g.take((self.base + WINDOW).max(lo), end, size, GRAIN, false) {
            return Some(at);
        }
        g.take(lo, end, size, GRAIN, false)
    }

    /// Reserve an aligned range strictly inside sparse window `k` (window 0: 32-bit descriptor
    /// heaps, from `desc_range` first, then whatever of the window is free). Returns
    /// `(address, rounded_size)`.
    pub fn sparse_alloc_in(&self, k: usize, size: usize, align: usize) -> Option<(usize, usize)> {
        if !self.is_sparse_window(k) {
            return None;
        }
        let size = (size.max(1) + GRAIN - 1) & !(GRAIN - 1);
        let align = align.max(GRAIN).next_power_of_two();
        let (w_start, w_len) = self.window(k);
        let mut g = self.sparse_free.lock().unwrap();
        let desc = if k == 0 { self.desc_range() } else { None };
        if let Some((lo, hi)) = desc {
            if let Some(at) = g.take(lo, hi, size, align, true) {
                return Some((at, size));
            }
        }
        let lo = desc.map_or(w_start, |(lo, _)| lo);
        g.take(lo, w_start + w_len, size, align, true).map(|at| (at, size))
    }

    /// Reserve an aligned range in the sparse windows that never straddles a 4 GiB window
    /// boundary (when `size <= WINDOW`): window 0 first, then windows 1+, never `desc_range`.
    /// Returns `(address, rounded_size)`.
    pub fn sparse_alloc(&self, size: usize, align: usize) -> Option<(usize, usize)> {
        if !self.has_sparse() {
            return None;
        }
        let size = size.max(1).checked_next_multiple_of(GRAIN)?;
        let align = align.max(GRAIN).checked_next_power_of_two()?;
        let mut g = self.sparse_free.lock().unwrap();
        let end = self.base + MAX_WINDOWS * WINDOW;
        let lo = self.title_sparse_start();
        let above_0 = (self.base + WINDOW).max(lo);
        if size <= WINDOW {
            if self.is_sparse_window(0) {
                if let Some(at) = g.take(lo, self.base + WINDOW, size, align, true) {
                    return Some((at, size));
                }
            }
            g.take(above_0, end, size, align, true).map(|at| (at, size))
        } else {
            g.take(above_0, end, size, align, false)
                .or_else(|| g.take(lo, end, size, align, false))
                .map(|at| (at, size))
        }
    }

    /// `sparse_alloc` from the other end, for the GPU heaps of the title's that the coherent
    /// window did not take (`d3d12x::sparse::alloc_diverted`): the highest free block of windows
    /// 1+. Those heaps came from window 3 and need nothing of window 0, which descriptor heaps
    /// and 32-bit addresses do; and taken from the top they leave the low addresses in one piece
    /// for a large reservation (`desc_range`). Without room there, as `sparse_alloc`.
    pub fn sparse_alloc_top(&self, size: usize, align: usize) -> Option<(usize, usize)> {
        if !self.has_sparse() {
            return None;
        }
        let rounded = size.max(1).checked_next_multiple_of(GRAIN)?;
        let pow = align.max(GRAIN).checked_next_power_of_two()?;
        let above_0 = (self.base + WINDOW).max(self.title_sparse_start());
        let top = self.sparse_free.lock().unwrap().take_top(above_0, self.base + MAX_WINDOWS * WINDOW, rounded, pow);
        top.map(|at| (at, rounded)).or_else(|| self.sparse_alloc(size, align))
    }

    /// The sparse reservation starting at `p` gets video memory for its tiles when it is first
    /// needed (`d3d12x::sparse`: a lazy heap), until it is released. False: none starts there.
    pub fn sparse_set_lazy(&self, p: usize) -> bool {
        let mut g = self.sparse_free.lock().unwrap();
        g.ranges.1.contains_key(&p) && {
            g.lazy.insert(p);
            true
        }
    }

    /// The parts of `[a, b)` in reservations marked by `sparse_set_lazy`: (start, end), by address.
    pub fn sparse_lazy_in(&self, a: usize, b: usize) -> Vec<(usize, usize)> {
        let g = self.sparse_free.lock().unwrap();
        lazy_parts(&g.lazy, &g.ranges.1, a, b)
    }

    /// The bytes of the sparse reservation starting at `p` (it is gone after this).
    pub fn sparse_release(&self, p: usize) -> Option<usize> {
        let mut g = self.sparse_free.lock().unwrap();
        let size = g.ranges.1.remove(&p)?;
        g.lazy.remove(&p);
        g.used_bytes -= size;
        let free = &mut g.ranges.0;
        let (mut start, mut len) = (p, size);
        if let Some((&o, &l)) = free.range(..p).next_back() {
            if o + l == p {
                free.remove(&o);
                start = o;
                len += l;
            }
        }
        if let Some(l) = free.remove(&(p + size)) {
            len += l;
        }
        free.insert(start, len);
        Some(size)
    }

    /// The sparse reservation holding `p`: (start, bytes).
    pub fn sparse_reservation(&self, p: usize) -> Option<(usize, usize)> {
        let g = self.sparse_free.lock().unwrap();
        g.ranges.1.range(..=p).next_back().map(|(&a, &l)| (a, l)).filter(|&(a, l)| p < a + l)
    }

    /// Bytes currently reserved/allocated in sparse windows.
    pub fn sparse_used(&self) -> usize {
        self.sparse_free.lock().unwrap().used_bytes
    }

    /// `windows` committed windows of `size` bytes each.
    pub fn with_windows(windows: usize, size: usize) -> Option<Uma> {
        let size = ((size + GRAIN - 1) & !(GRAIN - 1)).clamp(GRAIN, MAX_SIZE);
        let windows = windows.clamp(1, MAX_WINDOWS);
        for slot in (4usize..512).step_by(MAX_WINDOWS) {
            let base = slot << 32;
            let mut got = Vec::new();
            for k in 0..windows {
                let want = base + k * WINDOW;
                // write-watched: textures the title keeps here are uploaded again after CPU writes
                let p = unsafe { VirtualAlloc(want as *const std::ffi::c_void, size, MEM_RESERVE | MEM_COMMIT | MEM_WRITE_WATCH, PAGE_READWRITE) };
                if p.is_null() {
                    break;
                }
                got.push(p);
            }
            if got.len() == windows {
                let mut inner = Inner::default();
                let mut kinds = [WindowKind::None; MAX_WINDOWS];
                let mut win_allocs = Vec::with_capacity(windows + 1);
                for k in 0..windows {
                    inner.free.insert(k * WINDOW, size);
                    kinds[k] = WindowKind::Committed(size);
                    win_allocs.push(got[k] as usize);
                }
                return Some(Uma {
                    base,
                    kinds,
                    win_allocs,
                    inner: Mutex::new(inner),
                    sparse_free: Mutex::new(Default::default()),
                    pool: Mutex::new((0, 0, Vec::new())),
                    driver: Default::default(),
                });
            }
            for p in got {
                unsafe { VirtualFree(p, 0, MEM_RELEASE) };
            }
        }
        None
    }

    pub fn base(&self) -> usize {
        self.base
    }

    /// usable bytes across all committed windows
    pub fn size(&self) -> usize {
        self.kinds
            .iter()
            .map(|w| match *w {
                WindowKind::Committed(s) => s,
                _ => 0,
            })
            .sum()
    }

    /// number of committed windows
    pub fn windows(&self) -> usize {
        self.kinds.iter().filter(|w| matches!(w, WindowKind::Committed(_))).count()
    }

    /// (address, bytes) of window `k` (a sparse window: all 4 GiB)
    pub fn window(&self, k: usize) -> (usize, usize) {
        let len = match self.kinds.get(k).copied().unwrap_or(WindowKind::None) {
            WindowKind::Committed(s) => s,
            WindowKind::Sparse => WINDOW,
            WindowKind::None => 0,
        };
        (self.base + k * WINDOW, len)
    }

    /// Underlying write-watched `VirtualAlloc` regions `(base, size)` for `GetWriteWatch`:
    /// contiguous sparse windows belong to a single reservation, while each committed window
    /// is its own allocation.
    pub fn watch_regions(&self) -> Vec<(usize, usize)> {
        let mut out: Vec<(usize, usize)> = Vec::with_capacity(MAX_WINDOWS);
        let mut k = 0;
        while k < MAX_WINDOWS {
            match self.kinds[k] {
                WindowKind::None => k += 1,
                WindowKind::Committed(s) => {
                    out.push((self.base + k * WINDOW, s));
                    k += 1;
                }
                WindowKind::Sparse => {
                    let start = self.base + k * WINDOW;
                    let mut len = 0usize;
                    while k < MAX_WINDOWS && matches!(self.kinds[k], WindowKind::Sparse) {
                        len += WINDOW;
                        k += 1;
                    }
                    out.push((start, len));
                }
            }
        }
        out
    }

    pub fn window_of(&self, p: usize) -> Option<usize> {
        let k = p.checked_sub(self.base)? / WINDOW;
        let (start, len) = self.window(k);
        (k < MAX_WINDOWS && len > 0 && p - start < len).then_some(k)
    }

    pub fn contains(&self, p: usize) -> bool {
        self.window_of(p).is_some()
    }

    /// Whether `p` is memory the title has here: a block handed out in a committed window, or
    /// one of its reservations in a sparse window. The windows' ranges are wide (4 GiB each) and
    /// a PC resource's GPU address can fall in one by number – that address is not the
    /// window's memory (Minecraft's dynamic vertex buffer, a PC upload buffer, was read from an
    /// empty sparse window: nothing drew).
    pub fn is_title_address(&self, p: usize) -> bool {
        if self.is_sparse(p) {
            return self.sparse_reservation(p).is_some();
        }
        if !self.is_committed(p) {
            return false;
        }
        let off = p - self.base;
        let g = self.inner.lock().unwrap();
        g.used.range(..=off).next_back().is_some_and(|(&o, &l)| off < o + l)
    }

    pub fn committed_reservation(&self, p: usize) -> Option<(usize, usize)> {
        if !self.is_committed(p) {
            return None;
        }
        let off = p - self.base;
        let g = self.inner.lock().unwrap();
        let (&start, &size) = g.used.range(..=off).next_back()?;
        (off - start < size && g.reserved.contains(&start)).then_some((self.base + start, size))
    }

    /// Zeroed block of `size` bytes aligned to `align` (at least 64 KiB), first fit (window 0
    /// first).
    #[track_caller]
    pub fn alloc(&self, size: usize, align: usize) -> Option<*mut u8> {
        self.alloc_where(size, align, None, true)
    }

    /// `alloc` for reserved address space: not zeroed (a large reservation is not touched –
    /// what the title commits or maps there it writes; never-used memory is zero anyway).
    #[track_caller]
    pub fn reserve(&self, size: usize, align: usize) -> Option<*mut u8> {
        self.alloc_where(size, align, None, false)
    }

    /// `alloc` inside window `k` (memory the console addresses with 32 bits, e.g. descriptor
    /// heaps: window 0).
    #[track_caller]
    pub fn alloc_in(&self, k: usize, size: usize, align: usize) -> Option<*mut u8> {
        self.alloc_where(size, align, Some(k), true)
    }

    /// `XCL_TRACE_MEM`: each committed-window allocation of 1 MB or more and each one that
    /// fails, with the XCL code that asked for it (what fills the window).
    #[track_caller]
    fn trace(&self, size: usize, got: Option<usize>, used: usize) {
        let at = std::panic::Location::caller();
        match got {
            Some(p) => crate::xlog_info!("UMA: {} KB at {p:#x} for {}:{} ({} MB used)", size >> 10, at.file(), at.line(), used >> 20),
            None => crate::xlog_info!("UMA: {} KB for {}:{} – no room ({} MB used)", size >> 10, at.file(), at.line(), used >> 20),
        }
    }

    fn tracing() -> bool {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *ON.get_or_init(|| std::env::var_os("XCL_TRACE_MEM").is_some())
    }

    #[track_caller]
    fn alloc_where(&self, size: usize, align: usize, window: Option<usize>, zero: bool) -> Option<*mut u8> {
        let size = size.max(1).checked_next_multiple_of(GRAIN)?;
        let align = align.max(GRAIN).checked_next_power_of_two()?;
        let mut g = self.inner.lock().unwrap();
        let first_fit = |(&o, &l): (&usize, &usize)| {
            let at = self.base.checked_add(o)?.checked_next_multiple_of(align)? - self.base;
            (at.checked_add(size).is_some_and(|end| end <= o + l)).then_some((o, l, at))
        };
        let found = match window {
            Some(k) if k < MAX_WINDOWS => g.free.range(k * WINDOW..(k + 1) * WINDOW).find_map(first_fit),
            Some(_) => None,
            None => g.free.iter().find_map(first_fit),
        };
        let Some((start, len, at)) = found else {
            if Self::tracing() {
                let used = g.used_bytes;
                drop(g);
                self.trace(size, None, used);
            }
            return None;
        };
        g.free.remove(&start);
        if at > start {
            g.free.insert(start, at - start);
        }
        if at + size < start + len {
            g.free.insert(at + size, start + len - at - size);
        }
        g.used.insert(at, size);
        g.used_bytes += size;
        g.peak_bytes = g.peak_bytes.max(g.used_bytes);
        if !zero {
            g.reserved.insert(at);
        }
        let used = (Self::tracing() && size >= 1 << 20).then_some(g.used_bytes);
        drop(g);
        let p = (self.base + at) as *mut u8;
        note_event(p as usize, size, false, std::panic::Location::caller());
        if let Some(used) = used {
            self.trace(size, Some(p as usize), used);
        }
        if zero {
            unsafe { std::ptr::write_bytes(p, 0, size) };
        }
        Some(p)
    }

    /// The bytes of the block handed out at `p` in a committed window (None: none starts there).
    pub fn block_size(&self, p: usize) -> Option<usize> {
        if !self.is_committed(p) {
            return None;
        }
        self.inner.lock().unwrap().used.get(&(p - self.base)).copied()
    }

    /// Release a block from `alloc` (false: not one of ours).
    #[track_caller]
    pub fn free(&self, p: usize) -> bool {
        if !self.is_committed(p) {
            return false;
        }
        {
            // a block of XCL's pool goes back to the pool
            let mut g = self.pool.lock().unwrap();
            if p >= g.0 && p < g.1 {
                g.2.push(p);
                return true;
            }
        }
        let mut g = self.inner.lock().unwrap();
        let off = p - self.base;
        let Some(orig) = g.used.remove(&off) else { return false };
        note_event(p, orig, true, std::panic::Location::caller());
        g.used_bytes -= orig;
        g.reserved.remove(&off);
        let (mut start, mut len) = (off, orig);
        // merge with the free neighbours
        if let Some((&o, &l)) = g.free.range(..off).next_back() {
            if o + l == off {
                g.free.remove(&o);
                start = o;
                len += l;
            }
        }
        if let Some(l) = g.free.remove(&(off + orig)) {
            len += l;
        }
        g.free.insert(start, len);
        true
    }

    /// Bytes handed out.
    pub fn used(&self) -> usize {
        self.inner.lock().unwrap().used_bytes
    }

    /// The most bytes that were handed out at once.
    pub fn peak(&self) -> usize {
        self.inner.lock().unwrap().peak_bytes
    }

    /// Bytes of XCL's arena pool: (kept for it, handed out as arena blocks).
    pub fn pool_bytes(&self) -> (usize, usize) {
        let g = self.pool.lock().unwrap();
        let kept = g.1 - g.0;
        (kept, kept - g.2.len() * ARENA_BLOCK)
    }
}

/// Non-local video memory by owner: (the coherent window, the rest). The window is system memory
/// D3D12 opened as a heap – all of it counts as non-local from then on, used or not; the rest is
/// what the CPU sees of everything else (upload and readback buffers: XCL's, the driver's).
pub fn non_local_split(non_local: u64, coherent_window: u64) -> (u64, u64) {
    let window = coherent_window.min(non_local);
    (window, non_local - window)
}

impl Drop for Uma {
    fn drop(&mut self) {
        for &p in &self.win_allocs {
            unsafe {
                VirtualFree(p as *mut std::ffi::c_void, 0, MEM_RELEASE);
            }
        }
    }
}

/// Small pieces of committed unified memory for XCL's own per-draw data (argument tables,
/// user SGPRs): a block of `ARENA_BLOCK` bytes cut into aligned pieces, a new block when one
/// is used up. `Uma::alloc` hands out whole 64 KiB grains – three of those per draw filled
/// 2 GB within seconds in a scene of thousands of draws (Minecraft's world), and the draws
/// that found no room were skipped (the world flickered). The blocks go to `blocks`; their
/// owner frees them when the GPU is done (a command allocator's reset) and starts a fresh
/// arena.
#[derive(Default)]
pub struct Arena {
    at: usize,
    end: usize,
}

/// block size of an `Arena`
pub const ARENA_BLOCK: usize = 4 * GRAIN;

impl Arena {
    /// `n` bytes at an `align`-aligned address in committed unified memory (not zeroed), or
    /// None when committed unified memory is full.
    pub fn take(&mut self, u: &Uma, n: usize, align: usize, blocks: &mut Vec<usize>) -> Option<*mut u8> {
        let align = align.max(16).next_power_of_two();
        let n = n.max(1);
        let at = (self.at + align - 1) & !(align - 1);
        if self.at != 0 && at + n <= self.end {
            self.at = at + n;
            return Some(at as *mut u8);
        }
        if n > ARENA_BLOCK / 2 {
            // a large piece: a block of its own (the current one stays in use)
            let p = u.alloc(n, align)?;
            blocks.push(p as usize);
            return Some(p);
        }
        // XCL's pool first, then any free part of committed unified memory
        let p = match u.pool_block() {
            Some(p) => p,
            None => u.alloc(ARENA_BLOCK, GRAIN)? as usize,
        };
        blocks.push(p);
        self.at = p + n;
        self.end = p + ARENA_BLOCK;
        Some(p as *mut u8)
    }
}

static GLOBAL: OnceLock<Option<Uma>> = OnceLock::new();

/// What the coherent window has to hold (the title's GPU heaps, XCL's per-draw arguments and
/// readback buffers) is known only as the title runs, and the window is committed when it is
/// made – and resident from then on: D3D12 opens it as a heap. Its size is the most that was in
/// use in it at once in earlier runs (`<data>/cache/uma_coherent_peak.txt`,
/// `note_coherent_need`) and `COHERENT_HEADROOM`, at most a window. Until a run has written
/// that file it is what `<data>/cache/uma_coherent.txt` gives: every byte the title asked for
/// in an earlier run and 512 MB – a sum that counted memory the title had freed again (a retail
/// game asked for 4.6 GB, held 3.0 GB at once and got a window of 4095 MB, 1 GB of it resident
/// for nothing). `XCL_COHERENT_MB` chooses it.
fn coherent_file(name: &str) -> std::path::PathBuf {
    crate::state::data_dir().join("cache").join(name)
}

const COHERENT_PEAK: &str = "uma_coherent_peak.txt";
const COHERENT_ASKED: &str = "uma_coherent.txt";
/// room in the coherent window past the most an earlier run used (a run that goes further than
/// any before it has that much more before its GPU heaps no longer fit). All of it is RAM from
/// the start whether the title ever takes it: D3D12 opens the window as a heap, which makes
/// every page of it resident (1024 MiB of untouched memory took 1040 MiB of the system's
/// available RAM when opened), refuses a block that is only partly committed, and lets no page
/// of an opened heap be decommitted (`gpu_imported_heap_residency_probe`) – the window cannot
/// grow or shrink as the title runs, so what it does not use is held for nothing. A retail
/// game's most in use differed by 27 MB between three runs and grew by 53 MB in the last 470 s
/// of one; 512 MB of room were 539 MB resident and unused. 256 MB leave the title's largest
/// allocation its 96 MB of XCL's own (`kernelx`) and 160 MB more than any earlier run took.
/// `XCL_COHERENT_HEADROOM_MB` chooses another (`coherent_headroom`).
const COHERENT_HEADROOM: usize = 256 << 20;

/// `XCL_COHERENT_HEADROOM_MB` (`v`: its value) in bytes, at most a window; `COHERENT_HEADROOM`
/// when it is not set or not a number.
fn headroom_bytes(v: Option<&str>) -> usize {
    v.and_then(|v| v.trim().parse::<usize>().ok()).map_or(COHERENT_HEADROOM, |mb| mb.saturating_mul(1 << 20).min(MAX_SIZE))
}

/// The room the coherent window is made with past the most an earlier run used:
/// `COHERENT_HEADROOM`, or what `XCL_COHERENT_HEADROOM_MB` gives (logged once when it differs).
fn coherent_headroom() -> usize {
    static BYTES: OnceLock<usize> = OnceLock::new();
    *BYTES.get_or_init(|| {
        let bytes = headroom_bytes(std::env::var("XCL_COHERENT_HEADROOM_MB").ok().as_deref());
        if bytes != COHERENT_HEADROOM {
            crate::xlog_info!(
                "UMA: {} MB of room in the coherent window past the most an earlier run used (XCL_COHERENT_HEADROOM_MB; {} MB when it is not set)",
                bytes >> 20,
                COHERENT_HEADROOM >> 20
            );
        }
        bytes
    })
}

fn coherent_read(name: &str) -> Option<usize> {
    std::fs::read_to_string(coherent_file(name)).ok().and_then(|s| s.trim().parse::<usize>().ok()).filter(|&b| b > 0)
}

/// Bytes of the coherent window for the most an earlier run used of it at once (`peak`) and
/// `headroom`, else for the sum of what the title asked for in one (`asked`), else XCL's own
/// share.
fn coherent_bytes(peak: Option<usize>, asked: Option<usize>, headroom: usize) -> usize {
    match (peak, asked) {
        (Some(peak), _) => peak.saturating_add(headroom).clamp(RUNTIME_COMMITTED_CAP, MAX_SIZE),
        (None, Some(asked)) => asked.saturating_add(RUNTIME_COMMITTED_CAP).min(MAX_SIZE),
        (None, None) => RUNTIME_COMMITTED_CAP,
    }
}

/// Bytes the coherent window is made with.
pub fn coherent_hint() -> usize {
    if let Some(mb) = std::env::var("XCL_COHERENT_MB").ok().and_then(|v| v.parse::<usize>().ok()) {
        return (mb << 20).clamp(GRAIN, MAX_SIZE);
    }
    let (peak, asked) = (coherent_read(COHERENT_PEAK), coherent_read(COHERENT_ASKED));
    // only with the commit for all of it: the title's ~13 GB, video memory for its render targets
    // and textures (WDDM charges commit for each video-memory allocation too) and the window
    // itself – a retail game took ~18 GB; with less free it runs without (its GPU heaps outside
    // unified memory: no image) rather than taking the system's commit
    if (peak.is_some() || asked.is_some()) && free_commit() < NEEDS_FOR_COHERENT_HEAPS {
        crate::xlog_warn!(
            "UMA: {} MB of commit free, under {} MB: the title's GPU heaps stay outside unified memory (a larger page file gives them the coherent window)",
            free_commit() >> 20,
            NEEDS_FOR_COHERENT_HEAPS >> 20
        );
        return RUNTIME_COMMITTED_CAP;
    }
    coherent_bytes(peak, asked, coherent_headroom())
}

/// Commit free before the title starts for its GPU heaps to go to the coherent window.
const NEEDS_FOR_COHERENT_HEAPS: usize = 17 << 30;

fn free_commit() -> usize {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    let mut m: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    m.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
    if unsafe { GlobalMemoryStatusEx(&mut m) } == 0 {
        return usize::MAX;
    }
    m.ullAvailPageFile as usize
}

/// The coherent window has had to hold `bytes` at once (the most that was in use in it, or what
/// was in use and an allocation it had no room for): a later run makes the window for that (this
/// one has the size it was made with). Kept in steps of 16 MiB, and only as it grows.
pub fn note_coherent_need(bytes: usize) {
    static NOTED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let bytes = bytes.next_multiple_of(16 << 20);
    if NOTED.fetch_max(bytes, std::sync::atomic::Ordering::Relaxed) >= bytes {
        return;
    }
    let before = coherent_read(COHERENT_PEAK).unwrap_or(0);
    if bytes > before {
        let f = coherent_file(COHERENT_PEAK);
        if let Some(d) = f.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        let _ = std::fs::write(&f, bytes.to_string());
        if bytes >> 28 != before >> 28 {
            crate::xlog_info!("UMA: {} MB in use in the coherent window at once (a window of {} MB from the next run)", bytes >> 20, coherent_bytes(Some(bytes), None, coherent_headroom()) >> 20);
        }
    }
}

impl Uma {
    /// The committed coherent window (None: none).
    pub fn committed_window(&self) -> Option<usize> {
        (0..MAX_WINDOWS).find(|&k| self.is_committed_window(k))
    }
}

/// The title's graphics memory if it has been made (`get` makes it).
pub fn made() -> Option<&'static Uma> {
    GLOBAL.get().and_then(|u| u.as_ref())
}

/// The title's graphics memory (created on first use; `XCL_UMA_MB`, default 2048).
pub fn get() -> Option<&'static Uma> {
    GLOBAL
        .get_or_init(|| {
            let mb = std::env::var("XCL_UMA_MB").ok().and_then(|v| v.parse::<usize>().ok()).unwrap_or(2048);
            let coherent = coherent_hint();
            let u = Uma::for_runtime_with((mb << 20).max(coherent), coherent);
            match &u {
                Some(u) => {
                    let sparse_n = (0..MAX_WINDOWS).filter(|&k| u.is_sparse_window(k)).count();
                    crate::xlog_info!(
                        "UMA: {} sparse VRAM window(s) ({} GiB VA) + {} MB coherent window at {:#x}",
                        sparse_n,
                        sparse_n * 4,
                        u.size() >> 20,
                        u.base()
                    );
                }
                None => crate::xlog_warn!("UMA: could not reserve {mb} MB at a 4 GiB-aligned address – plain allocations"),
            }
            u
        })
        .as_ref()
}

#[cfg(test)]
mod tests {
    /// The coherent window is made for the most an earlier run had in it at once; the sum of
    /// what a title asked for sizes it only until a run has measured that.
    #[test]
    fn the_coherent_window_is_sized_by_peak_use() {
        const MIB: usize = 1 << 20;
        // a title that asked for 4582 MB in all and held 3033 MB at once
        assert_eq!(coherent_bytes(None, Some(4582 * MIB), COHERENT_HEADROOM), MAX_SIZE);
        assert_eq!(coherent_bytes(Some(3033 * MIB), Some(4582 * MIB), COHERENT_HEADROOM), 3289 * MIB);
        // XCL's own share at least, a window at most
        assert_eq!(coherent_bytes(None, None, COHERENT_HEADROOM), RUNTIME_COMMITTED_CAP);
        assert_eq!(coherent_bytes(Some(64 * MIB), None, COHERENT_HEADROOM), RUNTIME_COMMITTED_CAP);
        assert_eq!(coherent_bytes(Some(400 * MIB), None, COHERENT_HEADROOM), 656 * MIB);
        assert_eq!(coherent_bytes(Some(3900 * MIB), None, COHERENT_HEADROOM), MAX_SIZE);
        assert_eq!(coherent_bytes(Some(usize::MAX), Some(1), COHERENT_HEADROOM), MAX_SIZE);
    }

    /// `XCL_COHERENT_HEADROOM_MB` is the room past an earlier run's peak; the window stays XCL's
    /// own share at least and a window at most, and what is not a number changes nothing.
    #[test]
    fn the_coherent_headroom_is_settable() {
        const MIB: usize = 1 << 20;
        assert_eq!(headroom_bytes(None), COHERENT_HEADROOM);
        assert_eq!(headroom_bytes(Some("512")), 512 * MIB);
        assert_eq!(headroom_bytes(Some(" 64 ")), 64 * MIB);
        assert_eq!(headroom_bytes(Some("0")), 0);
        assert_eq!(headroom_bytes(Some("-1")), COHERENT_HEADROOM);
        assert_eq!(headroom_bytes(Some("many")), COHERENT_HEADROOM);
        assert_eq!(headroom_bytes(Some("99999999999999999999")), COHERENT_HEADROOM);
        assert_eq!(headroom_bytes(Some("17592186044416")), MAX_SIZE);
        assert_eq!(coherent_bytes(Some(3033 * MIB), None, headroom_bytes(Some("512"))), 3545 * MIB);
        assert_eq!(coherent_bytes(Some(3033 * MIB), None, headroom_bytes(Some("0"))), 3033 * MIB);
        assert_eq!(coherent_bytes(Some(64 * MIB), None, headroom_bytes(Some("0"))), RUNTIME_COMMITTED_CAP);
        assert_eq!(coherent_bytes(Some(3900 * MIB), None, headroom_bytes(Some("4096"))), MAX_SIZE);
        assert_eq!(coherent_bytes(None, Some(1000 * MIB), headroom_bytes(Some("0"))), 1512 * MIB);
    }

    /// The window is non-local video memory in full; before D3D12 has opened it (and without
    /// one) nothing of non-local is its.
    #[test]
    fn non_local_video_memory_is_the_window_first() {
        const MIB: u64 = 1 << 20;
        assert_eq!(non_local_split(4291 * MIB, 3552 * MIB), (3552 * MIB, 739 * MIB));
        assert_eq!(non_local_split(0, 3552 * MIB), (0, 0));
        assert_eq!(non_local_split(23 * MIB, 0), (0, 23 * MIB));
    }

    /// XCL's pool counts as in use from the start; what it handed out is known apart.
    #[test]
    fn the_pool_reports_what_it_handed_out() {
        let u = Uma::with_windows(1, 64 << 20).unwrap();
        assert_eq!(u.pool_bytes(), (0, 0));
        u.reserve_pool(4 * ARENA_BLOCK);
        assert_eq!(u.pool_bytes(), (4 * ARENA_BLOCK, 0));
        let mut arena = Arena::default();
        let mut blocks = Vec::new();
        arena.take(&u, 64, 16, &mut blocks).unwrap();
        assert_eq!(u.pool_bytes(), (4 * ARENA_BLOCK, ARENA_BLOCK));
    }

    /// `peak` is the most that was handed out at once, whatever was freed since.
    #[test]
    fn peak_use_outlives_frees() {
        let u = Uma::with_windows(1, 64 << 20).unwrap();
        let a = u.alloc(8 << 20, GRAIN).unwrap() as usize;
        let b = u.alloc(16 << 20, GRAIN).unwrap() as usize;
        assert_eq!((u.used(), u.peak()), (24 << 20, 24 << 20));
        assert!(u.free(a));
        let c = u.alloc(4 << 20, GRAIN).unwrap() as usize;
        assert_eq!((u.used(), u.peak()), (20 << 20, 24 << 20));
        assert!(u.free(b) && u.free(c));
        assert_eq!((u.used(), u.peak()), (0, 24 << 20));
    }

    #[test]
    fn sparse_first_fit_checks_the_hole_straddling_the_lower_bound() {
        let lo = WINDOW;
        let hi = lo + 8 * GRAIN;
        let mut ranges = (
            BTreeMap::from([(GRAIN, GRAIN), (lo - GRAIN, 4 * GRAIN), (lo + 4 * GRAIN, 4 * GRAIN), (hi, 4 * GRAIN)]),
            BTreeMap::new(),
        );

        assert_eq!(take_sparse_in(&mut ranges, lo, hi, 2 * GRAIN, GRAIN, true), Some(lo));
        assert_eq!(ranges.0.get(&(lo - GRAIN)), Some(&GRAIN));
        assert_eq!(ranges.0.get(&(lo + 2 * GRAIN)), Some(&GRAIN));
        assert_eq!(take_sparse_in(&mut ranges, lo, hi, 2 * GRAIN, 4 * GRAIN, true), Some(lo + 4 * GRAIN));
        let after = ranges.clone();
        assert_eq!(take_sparse_in(&mut ranges, lo, hi, 3 * GRAIN, GRAIN, true), None);
        assert_eq!(ranges, after);
    }

    #[test]
    fn sparse_first_fit_preserves_the_single_window_constraint() {
        let lo = WINDOW - GRAIN;
        let hi = WINDOW + 2 * GRAIN;
        let free = BTreeMap::from([(GRAIN, GRAIN), (lo - GRAIN, 4 * GRAIN)]);
        let mut single = (free.clone(), BTreeMap::new());
        let mut cross = (free, BTreeMap::new());
        assert_eq!(take_sparse_in(&mut single, lo, hi, 2 * GRAIN, GRAIN, true), Some(WINDOW));
        assert_eq!(take_sparse_in(&mut cross, lo, hi, 2 * GRAIN, GRAIN, false), Some(lo));
    }
    use super::*;

    /// A heap taken from the top is the highest block that fits, aligned, inside one window and
    /// inside the bounds; the low addresses stay in one piece.
    #[test]
    fn sparse_top_fit_takes_the_highest_block_of_one_window() {
        let (lo, hi) = (WINDOW, 3 * WINDOW);
        let free = BTreeMap::from([(GRAIN, 3 * WINDOW - GRAIN)]);
        let mut g = (free.clone(), BTreeMap::new());
        assert_eq!(take_sparse_top(&mut g, lo, hi, 16 * GRAIN, GRAIN), Some(hi - 16 * GRAIN));
        assert_eq!(take_sparse_top(&mut g, lo, hi, 3 * GRAIN, 32 * GRAIN), Some(hi - 32 * GRAIN));
        assert_eq!(g.0, BTreeMap::from([(GRAIN, hi - 32 * GRAIN - GRAIN), (hi - 29 * GRAIN, 13 * GRAIN)]));
        assert_eq!(g.1, BTreeMap::from([(hi - 32 * GRAIN, 3 * GRAIN), (hi - 16 * GRAIN, 16 * GRAIN)]));
        assert_eq!(take_sparse_top(&mut g, lo, hi, 13 * GRAIN, GRAIN), Some(hi - 29 * GRAIN));
        assert_eq!(take_sparse_top(&mut g, lo, hi, WINDOW - 32 * GRAIN, GRAIN), Some(2 * WINDOW));
        assert_eq!(take_sparse_top(&mut g, lo, hi, WINDOW, GRAIN), Some(WINDOW));
        assert_eq!(take_sparse_top(&mut g, lo, hi, GRAIN, GRAIN), None, "windows 1 and 2 are full");
        assert_eq!(g.0, BTreeMap::from([(GRAIN, WINDOW - GRAIN)]), "window 0 was not touched");
        let mut g = (free, BTreeMap::new());
        assert_eq!(take_sparse_top(&mut g, lo, hi, WINDOW + GRAIN, GRAIN), None, "more than a window");
        assert_eq!(take_sparse_top(&mut g, lo, hi, 0, GRAIN), None);
        assert_eq!(take_sparse_top(&mut g, hi, lo, GRAIN, GRAIN), None);
        assert_eq!(take_sparse_top(&mut g, lo, lo + GRAIN / 2, GRAIN, GRAIN), None);
    }

    /// A block that would lie across two windows moves below the boundary; a free range that
    /// reaches past the bounds is used only inside them.
    #[test]
    fn sparse_top_fit_never_straddles_a_window() {
        let mut g = (BTreeMap::from([(WINDOW + GRAIN, WINDOW + 2 * GRAIN)]), BTreeMap::new());
        assert_eq!(take_sparse_top(&mut g, WINDOW, 3 * WINDOW, 4 * GRAIN, GRAIN), Some(2 * WINDOW - 4 * GRAIN));
        assert_eq!(g.0, BTreeMap::from([(WINDOW + GRAIN, WINDOW - 5 * GRAIN), (2 * WINDOW, 3 * GRAIN)]));
        assert_eq!(take_sparse_top(&mut g, WINDOW, 3 * WINDOW, 3 * GRAIN, 2 * GRAIN), Some(2 * WINDOW));
        assert_eq!(take_sparse_top(&mut g, WINDOW, 3 * WINDOW, 3 * GRAIN, 2 * GRAIN), Some(2 * WINDOW - 8 * GRAIN));
        let mut g = (BTreeMap::from([(GRAIN, 3 * WINDOW - GRAIN)]), BTreeMap::new());
        assert_eq!(take_sparse_top(&mut g, WINDOW + GRAIN, 2 * WINDOW + GRAIN, 2 * GRAIN, GRAIN), Some(2 * WINDOW - 2 * GRAIN));
        assert_eq!(take_sparse_top(&mut g, WINDOW + GRAIN, WINDOW + 3 * GRAIN, 2 * GRAIN, GRAIN), Some(WINDOW + GRAIN));
        assert_eq!(take_sparse_top(&mut g, WINDOW + GRAIN, WINDOW + 3 * GRAIN, GRAIN, GRAIN), None);
        let mut g = (BTreeMap::from([(WINDOW - GRAIN, 2 * GRAIN), (2 * WINDOW - GRAIN, 2 * GRAIN)]), BTreeMap::new());
        assert_eq!(take_sparse_top(&mut g, WINDOW, 3 * WINDOW, 2 * GRAIN, GRAIN), None, "each free range lies across a boundary");
        assert_eq!(take_sparse_top(&mut g, WINDOW, 3 * WINDOW, GRAIN, GRAIN), Some(2 * WINDOW));
        assert_eq!(take_sparse_top(&mut g, WINDOW, 3 * WINDOW, GRAIN, GRAIN), Some(2 * WINDOW - GRAIN));
        assert_eq!(take_sparse_top(&mut g, WINDOW, 3 * WINDOW, GRAIN, GRAIN), Some(WINDOW));
        assert_eq!(take_sparse_top(&mut g, WINDOW, 3 * WINDOW, GRAIN, GRAIN), None, "what is left is below the bounds");
    }

    /// A range is lazy where it lies in a reservation that was marked, and nowhere else.
    #[test]
    fn lazy_parts_are_the_marked_reservations_in_a_range() {
        let t = GRAIN;
        let reserved = BTreeMap::from([(4 * t, 4 * t), (8 * t, 2 * t), (10 * t, 6 * t), (32 * t, 8 * t)]);
        let lazy = BTreeSet::from([4 * t, 10 * t, 32 * t]);
        assert_eq!(lazy_parts(&lazy, &reserved, 0, 64 * t), vec![(4 * t, 8 * t), (10 * t, 16 * t), (32 * t, 40 * t)]);
        assert_eq!(lazy_parts(&lazy, &reserved, 5 * t, 11 * t), vec![(5 * t, 8 * t), (10 * t, 11 * t)]);
        assert_eq!(lazy_parts(&lazy, &reserved, 6 * t + 0x1000, 6 * t + 0x3000), vec![(6 * t + 0x1000, 6 * t + 0x3000)]);
        assert!(lazy_parts(&lazy, &reserved, 8 * t, 10 * t).is_empty(), "a reservation that is not marked");
        assert!(lazy_parts(&lazy, &reserved, 16 * t, 32 * t).is_empty(), "between reservations");
        assert!(lazy_parts(&lazy, &reserved, 0, 4 * t).is_empty());
        assert!(lazy_parts(&lazy, &reserved, 40 * t, 48 * t).is_empty());
        assert!(lazy_parts(&lazy, &reserved, 6 * t, 6 * t).is_empty());
        assert!(lazy_parts(&lazy, &reserved, 7 * t, 5 * t).is_empty());
        assert!(lazy_parts(&BTreeSet::new(), &reserved, 0, 64 * t).is_empty());
        assert!(lazy_parts(&BTreeSet::from([20 * t]), &reserved, 0, 64 * t).is_empty(), "a mark without its reservation");
    }

    #[test]
    fn committed_usage_counts_pool_and_window_allocations() {
        let u = Uma::with_windows(2, 16 << 20).unwrap();
        u.reserve_pool(ARENA_BLOCK);
        assert_eq!(u.used(), ARENA_BLOCK);
        let p = u.alloc_in(1, GRAIN + 1, 0).unwrap() as usize;
        assert_eq!(u.window_of(p), Some(1));
        assert_eq!(u.used(), ARENA_BLOCK + 2 * GRAIN);
        assert!(u.alloc_in(MAX_WINDOWS, 1, 0).is_none());
        assert!(u.free(p));
        assert!(!u.free(p));
        assert_eq!(u.used(), ARENA_BLOCK);
    }

    #[test]
    fn sparse_usage_counts_rounded_reservations() {
        let u = Uma::for_runtime(16 << 20).unwrap();
        let (a, len) = u.sparse_alloc(GRAIN + 1, GRAIN).unwrap();
        let b = u.sparse_reserve(1).unwrap();
        assert_eq!(len, 2 * GRAIN);
        assert_eq!(u.sparse_used(), 3 * GRAIN);
        assert_eq!(u.sparse_release(a), Some(len));
        assert_eq!(u.sparse_release(a), None);
        assert_eq!(u.sparse_used(), GRAIN);
        assert_eq!(u.sparse_release(b), Some(GRAIN));
        assert_eq!(u.sparse_used(), 0);
    }

    /// Arena blocks come from XCL's pool while the title holds the rest of window 0, and go
    /// back to it when freed.
    #[test]
    fn arena_blocks_come_from_the_pool_when_the_title_took_the_rest() {
        let u = Uma::with_windows(1, 8 << 20).unwrap();
        u.reserve_pool(4 * ARENA_BLOCK);
        // the pool is the window's end: the title's first allocation is still its start
        assert_eq!(u.pool.lock().unwrap().1, u.base() + (8 << 20));
        assert_eq!(u.alloc(GRAIN, GRAIN).map(|p| p as usize), Some(u.base()));
        // the title takes everything else
        let mut title = Vec::new();
        while let Some(p) = u.alloc(GRAIN, GRAIN) {
            title.push(p as usize);
        }
        assert!(u.alloc_in(0, ARENA_BLOCK, GRAIN).is_none());
        let (mut a, mut blocks) = (Arena::default(), Vec::new());
        for _ in 0..4 * (ARENA_BLOCK / 256) {
            a.take(&u, 200, 256, &mut blocks).expect("a piece from the pool");
        }
        assert_eq!(blocks.len(), 4);
        assert!(a.take(&u, 200, 256, &mut blocks).is_none(), "pool used up, the rest is the title's");
        for b in blocks.drain(..) {
            assert!(u.free(b));
        }
        let mut a = Arena::default();
        assert!(a.take(&u, 200, 256, &mut blocks).is_some(), "freed blocks are back in the pool");
    }

    /// Many small pieces share a block (aligned, disjoint); a large one gets its own.
    #[test]
    fn arena_cuts_blocks_into_aligned_pieces() {
        let u = Uma::with_windows(1, 4 << 20).unwrap();
        let (mut a, mut blocks) = (Arena::default(), Vec::new());
        let mut got = Vec::new();
        for k in 0..1000usize {
            let n = 16 + (k % 7) * 40;
            let p = a.take(&u, n, 256, &mut blocks).unwrap() as usize;
            assert_eq!(p % 256, 0);
            assert_eq!(u.window_of(p), Some(0));
            got.push((p, n));
        }
        got.sort_unstable();
        assert!(got.windows(2).all(|w| w[0].0 + w[0].1 <= w[1].0), "pieces overlap");
        // 1000 pieces of at most 256 aligned bytes: 1000 · 256 B = 4 blocks, not 1000 grains
        assert_eq!(blocks.len(), (1000 * 256usize).div_ceil(ARENA_BLOCK));
        let big = a.take(&u, ARENA_BLOCK, 256, &mut blocks).unwrap() as usize;
        assert_eq!(blocks.last(), Some(&big));
        for b in blocks {
            assert!(u.free(b));
        }
        assert_eq!(u.used(), 0);
    }

    #[test]
    fn windows_are_4_gib_apart_and_allocations_stay_inside() {
        let u = Uma::with_windows(2, 4 << 20).unwrap();
        assert_eq!(u.base() % (MAX_WINDOWS * WINDOW), 0, "16 GiB alignment");
        assert_eq!((u.windows(), u.size()), (2, 8 << 20));
        let a = u.alloc(3 << 20, 0).unwrap() as usize;
        let b = u.alloc(3 << 20, 0).unwrap() as usize; // no room left in window 0
        assert_eq!(u.window_of(a), Some(0));
        assert_eq!(u.window_of(b), Some(1));
        assert_eq!(b, u.base() + WINDOW);
        assert_eq!((b >> 32) & 3, 1, "window from the high address bits");
        assert!(u.alloc_in(0, 2 << 20, 0).is_none(), "window 0 full");
        assert!(u.alloc(2 << 20, 0).is_none());
        unsafe { *(b as *mut u32) = 7 };
        assert!(u.free(a) && u.free(b));
        assert!(!u.contains(u.base() + (4 << 20)) && !u.contains(u.base() + 2 * WINDOW));
    }

    #[test]
    fn alloc_free_coalesce() {
        // without XCL's arena pool (`Uma::new` takes it at the start of window 0)
        let u = Uma::with_windows(1, 16 << 20).unwrap();
        assert_eq!(u.base() & 0xFFFF_FFFF, 0, "4 GiB alignment");
        let a = u.alloc(100, 0).unwrap() as usize;
        let b = u.alloc(0x2_0000, 0).unwrap() as usize;
        let c = u.alloc(0x1_0000, 0x10_0000).unwrap() as usize;
        assert_eq!(a, u.base());
        assert_eq!(b, u.base() + GRAIN);
        assert_eq!(c % 0x10_0000, 0, "alignment");
        assert_eq!(u.used(), 4 * GRAIN);
        unsafe { *(b as *mut u32) = 0xDEAD };
        assert!(u.free(b) && !u.free(b) && !u.free(12345));
        assert!(u.free(a));
        // a + b coalesced: a 3-grain block fits at the base again, zeroed
        let d = u.alloc(3 * GRAIN, 0).unwrap() as usize;
        assert_eq!(d, u.base());
        assert_eq!(unsafe { *((d + GRAIN) as *const u32) }, 0);
        assert!(u.free(c) && u.free(d));
        assert_eq!(u.used(), 0);
        // everything merged back into one range
        assert!(u.alloc(16 << 20, 0).is_some());
    }

    /// Only handed-out blocks are the title's memory: an address in a window's range that is
    /// not one (a PC GPU address by number) is not.
    #[test]
    fn title_addresses_are_blocks_handed_out() {
        let u = Uma::new(16 << 20).unwrap();
        let a = u.alloc(0x2_0000, 0).unwrap() as usize;
        assert!(u.is_title_address(a) && u.is_title_address(a + 0x1_FFFF));
        assert!(!u.is_title_address(a + 0x2_0000) && !u.is_title_address(u.base() + (8 << 20)));
        assert!(!u.is_title_address(u.base() - 1) && !u.is_title_address(u.base() + (32 << 20)));
        assert!(u.free(a));
        assert!(!u.is_title_address(a));
    }

    /// XDefiant's first reservation is 8 GiB + 4 MiB, more than windows 1..2 hold: it has to go
    /// across window 0 without taking the room its 32-bit descriptor heaps need there.
    #[test]
    fn a_reservation_larger_than_windows_1_and_2_leaves_window_0_room_for_descriptor_heaps() {
        let u = Uma::for_runtime(16 << 20).unwrap();
        let big = 0x2_0041_0000;
        let r = u.sparse_reserve(big).unwrap();
        let heap = 442_368 * 32;
        let (d, ld) = u.sparse_alloc_in(0, heap, 256).expect("a descriptor heap in window 0");
        assert_eq!(u.window_of(d), Some(0));
        assert!(d + ld <= r || d >= r + big, "the heap is not inside the title's reservation");
        assert_ne!(d, u.base(), "no descriptor table at console address 0");
        let (x, _) = u.sparse_alloc(0x2_0000, 0).expect("the title's graphics memory after its reservation");
        assert!(x >= r + big || x + 0x2_0000 <= r);
        assert_eq!(u.sparse_release(x), Some(0x2_0000));
        assert_eq!(u.sparse_release(d), Some(ld));
        assert_eq!(u.sparse_release(r), Some(big));
        assert_eq!(u.sparse_used(), 0);
    }

    #[test]
    fn runtime_layout_places_sparse_in_0_to_2_and_committed_in_3() {
        let u = Uma::for_runtime(16 << 20).unwrap();
        assert_eq!(u.base() % (MAX_WINDOWS * WINDOW), 0);
        assert_eq!(u.shader_windows(), 4);
        assert_eq!(u.windows(), 1);
        assert!(u.is_sparse_window(0) && u.is_sparse_window(1) && u.is_sparse_window(2));
        assert!(u.is_committed_window(3));

        // Committed allocations land in window 3
        let c = u.alloc(0x2_0000, 0).unwrap() as usize;
        assert_eq!(u.window_of(c), Some(3));
        assert!(u.is_committed(c) && !u.is_sparse(c) && u.is_title_address(c));
        assert!(u.free(c));

        // First sparse_alloc lands in window 0, after the descriptor range
        let (a, la) = u.sparse_alloc(0x2_0000, 0).unwrap();
        assert_eq!(a, u.base() + GRAIN + DESC_HEADROOM);
        assert_eq!(la, 0x2_0000);
        assert_eq!(u.window_of(a), Some(0));
        assert!(u.is_sparse(a) && u.is_title_address(a));

        // sparse_alloc_in(0, ...) stays inside window 0
        let (d, ld) = u.sparse_alloc_in(0, 4096, 256).unwrap();
        assert_eq!(u.window_of(d), Some(0));
        assert_eq!(ld, GRAIN);

        // Large sparse_reserve prefers window 1+
        let r = u.sparse_reserve(256 << 20).unwrap();
        assert_eq!(u.window_of(r), Some(1));

        // Release and coalesce
        assert_eq!(u.sparse_release(a), Some(la));
        assert_eq!(u.sparse_release(d), Some(ld));
        assert_eq!(u.sparse_release(r), Some(256 << 20));
        assert_eq!(u.sparse_used(), 0);
        assert!(!u.is_title_address(a));

        // Allocations <= 4 GiB never straddle a window boundary, and DESC_HEADROOM is preserved
        let fill_w0 = WINDOW - DESC_HEADROOM - GRAIN;
        let (x, _) = u.sparse_alloc(fill_w0, GRAIN).unwrap();
        assert_eq!(x, u.base() + GRAIN + DESC_HEADROOM);
        let (y, _) = u.sparse_alloc(GRAIN, GRAIN).unwrap();
        assert_eq!(u.window_of(y), Some(1), "overflows to window 1 before touching window 0 descriptor headroom");
        let (desc, _) = u.sparse_alloc_in(0, GRAIN, GRAIN).unwrap();
        assert_eq!(u.window_of(desc), Some(0), "descriptor headroom in window 0 remains available");
        assert_eq!(u.sparse_release(x), Some(fill_w0));
        assert_eq!(u.sparse_release(y), Some(GRAIN));
        assert_eq!(u.sparse_release(desc), Some(GRAIN));
        assert_eq!(u.sparse_used(), 0);
        // Contiguous sparse windows 0..2 are reported as one OS watch region; window 3 as another.
        assert_eq!(u.watch_regions(), vec![(u.base(), 3 * WINDOW), (u.base() + 3 * WINDOW, 16 << 20)]);
    }
}
