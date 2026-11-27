use std::collections::HashMap;
use std::ffi::c_void;
use std::ops::Range;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use windows_sys::Win32::System::Memory::{
    VirtualQuery, MEMORY_BASIC_INFORMATION, MEM_COMMIT, PAGE_EXECUTE_READWRITE, PAGE_EXECUTE_WRITECOPY, PAGE_GUARD, PAGE_READWRITE, PAGE_WRITECOPY,
};

use super::title_textures::{GetWriteWatch, REFRESH};

const PAGE: usize = 4096;

#[derive(Clone, Copy)]
pub(super) struct Backing {
    pub title: usize,
    pub mirror: usize,
    pub bytes: usize,
    /// outside write-watched memory (views of physical pages): what the title's bytes held at the
    /// last look (0: watched)
    seen: usize,
    /// where the next look's comparison starts
    cursor: usize,
    /// the mirror it is in, by that mirror's address (`Table::mirrors`): its own, or the one of
    /// the buffer it was placed inside
    block: usize,
}

/// The title's buffers outside unified memory and their mirrors. A mirror is a block of XCL's
/// video memory that holds what a span of the title's memory holds, kept in step page by page;
/// a buffer is a part of one. On the console, resources placed over the same memory are the
/// same memory: a buffer placed inside a span that is mirrored already is placed in that
/// mirror (the same video memory, whatever writes it), and the mirror lives as long as any
/// buffer in it. A buffer that only overlaps a mirrored span, or reaches beyond it, gets a
/// mirror of its own: both are kept in step with the title's memory, which is what they share.
#[derive(Default)]
struct Table {
    /// resource → its part of a mirror
    buffers: HashMap<usize, Backing>,
    /// mirror address → (the whole span it mirrors, buffers placed in it and holds on it)
    mirrors: HashMap<usize, (Backing, usize)>,
}

impl Table {
    /// One user of the mirror at `block` less: its address when that was the last (the mirror
    /// is forgotten; the caller gives the video memory back).
    fn release(&mut self, block: usize) -> Option<usize> {
        let (span, users) = self.mirrors.get_mut(&block)?;
        *users -= 1;
        if *users > 0 {
            return None;
        }
        if span.seen != 0 {
            drop(unsafe { Box::from_raw(std::ptr::slice_from_raw_parts_mut(span.seen as *mut u8, span.bytes)) });
        }
        self.mirrors.remove(&block);
        Some(block)
    }
}

/// What each look compares of a buffer outside write-watched memory (XCL's own writes there are
/// reported at once: `physical::note_written`; the title's are found within a few looks)
const LOOK_BYTES: usize = 1 << 20;

static BUFFERS: Mutex<Option<Table>> = Mutex::new(None);
static GENERATION: AtomicU64 = AtomicU64::new(0);

pub(super) fn generation() -> u64 {
    GENERATION.load(Ordering::Acquire)
}

pub(super) fn ranges() -> Vec<(usize, usize)> {
    BUFFERS.lock().unwrap().as_ref().map_or_else(Vec::new, |t| {
        t.mirrors.values().map(|(b, _)| (b.title & !(PAGE - 1), b.title + b.bytes)).collect()
    })
}

pub(super) fn get(res: usize) -> Option<Backing> {
    BUFFERS.lock().unwrap().as_ref()?.buffers.get(&res).copied()
}

/// (diagnostics, `memdiag`) (mirrors, the bytes they mirror, the bytes of their seen copies).
pub(super) fn diag_bytes() -> (usize, usize, usize) {
    BUFFERS.lock().unwrap().as_ref().map_or((0, 0, 0), |t| {
        t.mirrors.values().fold((0, 0, 0), |(n, all, seen), (b, _)| (n + 1, all + b.bytes, seen + if b.seen != 0 { b.bytes } else { 0 }))
    })
}

/// Why a title buffer outside unified memory cannot be mirrored (None: it can).
pub(super) fn unsupported(title: usize, bytes: usize) -> Option<&'static str> {
    let Some(end) = title.checked_add(bytes) else {
        return Some("its end overflows");
    };
    if bytes == 0 {
        return Some("it is empty");
    }
    crate::hle::kernelx::lazy::ensure(title, bytes);
    if !writable(title, end) {
        return Some("its memory is not committed read-write");
    }
    None
}

fn watched(title: usize, end: usize) -> bool {
    let mut covered = title;
    for (base, size) in crate::hle::kernelx::graphics_reservations() {
        if base + size <= covered {
            continue;
        }
        if base > covered {
            return false;
        }
        covered = base + size;
        if covered >= end {
            return true;
        }
    }
    false
}

fn writable(mut start: usize, end: usize) -> bool {
    if let Some(a) = crate::memmap::access(start, end.saturating_sub(start)) {
        return a == crate::memmap::Access::Write;
    }
    while start < end {
        let mut info = MEMORY_BASIC_INFORMATION::default();
        if unsafe {
            VirtualQuery(
                start as *const c_void,
                &mut info,
                std::mem::size_of_val(&info),
            )
        } == 0
            || info.State != MEM_COMMIT
            || info.Protect & PAGE_GUARD != 0
            || !matches!(info.Protect & 0xFF, PAGE_READWRITE | PAGE_WRITECOPY | PAGE_EXECUTE_READWRITE | PAGE_EXECUTE_WRITECOPY)
        {
            return false;
        }
        start = info.BaseAddress as usize + info.RegionSize;
    }
    true
}

/// A hold on the live mirror that has all of the title's `[title, title + bytes)`, if there is
/// one (the largest of several): (where the span is in it, the mirror). A buffer placed there
/// is placed at that address (`register_in`); a hold not used is given back (`unhold`).
pub(super) fn hold(title: usize, bytes: usize) -> Option<(usize, usize)> {
    let mut g = BUFFERS.lock().unwrap();
    let end = title.checked_add(bytes)?;
    let (span, users) = g
        .as_mut()?
        .mirrors
        .values_mut()
        .filter(|(b, _)| b.title <= title && end <= b.title + b.bytes)
        .max_by_key(|(b, _)| (b.bytes, std::cmp::Reverse(b.mirror)))?;
    *users += 1;
    Some((span.mirror + (title - span.title), span.mirror))
}

/// A hold given back: the mirror's address when nothing else used it any more (the caller gives
/// its video memory back).
pub(super) fn unhold(block: usize) -> Option<usize> {
    BUFFERS.lock().unwrap().as_mut()?.release(block)
}

/// The buffer `res` is placed at `mirror` in the mirror held as `block` (`hold`), over the
/// title's `[title, title + bytes)`: the hold becomes the buffer's.
pub(super) fn register_in(res: usize, title: usize, mirror: usize, bytes: usize, block: usize) {
    let mut g = BUFFERS.lock().unwrap();
    let t = g.get_or_insert_with(Table::default);
    let Some(&(span, _)) = t.mirrors.get(&block) else { return };
    let seen = if span.seen != 0 { span.seen + (title - span.title) } else { 0 };
    t.buffers.insert(res, Backing { title, mirror, bytes, seen, cursor: 0, block });
    GENERATION.fetch_add(1, Ordering::Release);
}

/// The buffer `res` with a mirror of its own at `mirror`, over the title's
/// `[title, title + bytes)` (the mirror gets what the title's memory holds).
pub(super) unsafe fn register(res: usize, title: usize, mirror: usize, bytes: usize) {
    let mut backing = Backing { title, mirror, bytes, seen: 0, cursor: 0, block: mirror };
    let mut g = BUFFERS.lock().unwrap();
    let t = g.get_or_insert_with(Table::default);
    // (read as a copy: the title can take the memory away between `unsupported` and here)
    let seen = super::title_textures::with_title_bytes(title, bytes, |now| {
        std::ptr::copy_nonoverlapping(now.as_ptr(), mirror as *mut u8, bytes);
        (!watched(title, title + bytes)).then(|| now.to_vec().into_boxed_slice())
    });
    if let Some(seen) = seen {
        backing.seen = Box::into_raw(seen) as *mut u8 as usize;
    }
    t.mirrors.insert(mirror, (backing, 1));
    t.buffers.insert(res, backing);
    GENERATION.fetch_add(1, Ordering::Release);
}

pub(super) fn view_owners(va:usize,len:usize)->Option<(usize, Vec<(usize,usize,usize,usize,usize,Option<usize>)>)> {
    let end=va.checked_add(len)?;
    let g=crate::alias::try_lock(&BUFFERS)?;
    let mut out=Vec::new();
    let mut total=0;
    if let Some(t)=g.as_ref() {
        for (&res,b) in &t.buffers {
            let Some(hi)=b.title.checked_add(b.bytes) else {continue};
            if b.title>=end || hi<=va {continue}
            total+=1;
            if out.len()<8 {out.push((res,b.title,b.bytes,b.mirror,b.block,(b.title<=va && end<=hi).then(|| b.mirror.checked_add(va-b.title)).flatten()));}
        }
    }
    Some((total,out))
}

/// (diagnostics, `alias::report`) The buffers the title placed over `[va, va + len)` outside unified
/// memory, the first 4: a Map with a read range copies each one's mirror over its memory.
pub(super) fn placed_over(va: usize, len: usize) -> Vec<String> {
    let Some(g) = crate::alias::try_lock(&BUFFERS) else { return Vec::new() };
    let Some(buffers) = g.as_ref().map(|t| &t.buffers) else { return Vec::new() };
    buffers
        .iter()
        .filter(|(_, b)| b.title < va.saturating_add(len) && va < b.title + b.bytes)
        .take(4)
        .map(|(res, b)| format!("a buffer is placed over this memory: PC buffer {res:#x} at {:#x} + {:#x}, mirrored at {:#x} (a Map for reading copies the mirror over it)", b.title, b.bytes, b.mirror))
        .collect()
}

/// The buffer `res` is gone: its mirror's address when no other buffer is placed in it (the
/// mirror is forgotten with it; the caller gives its video memory back).
pub(super) fn forget(res: usize) -> Option<usize> {
    let mut g = BUFFERS.lock().unwrap();
    let t = g.as_mut()?;
    let b = t.buffers.remove(&res)?;
    GENERATION.fetch_add(1, Ordering::Release);
    t.release(b.block)
}

/// Buffers outside write-watched memory: their pages whose bytes changed since the last look
/// are taken as written.
pub(super) unsafe fn unwatched_dirty(pages: &mut Vec<usize>) {
    let mut g = BUFFERS.lock().unwrap();
    let Some(t) = g.as_mut() else { return };
    for (b, _) in t.mirrors.values_mut() {
        b.changed_pages(LOOK_BYTES, pages);
    }
}

impl Backing {
    unsafe fn changed_pages(&mut self, budget: usize, out: &mut Vec<usize>) {
        let end = self.title + self.bytes;
        if self.seen == 0 || !writable(self.title, end) {
            return;
        }
        let mut at = self.title + self.cursor;
        let mut left = budget.min(self.bytes);
        while left > 0 {
            if at >= end {
                at = self.title;
            }
            // the pages of the look up to the end of the buffer, read at once and as a copy
            // (`with_title_bytes`: the title can unmap them while they are compared)
            let stop = (((at + left - 1) & !(PAGE - 1)) + PAGE).min(end);
            let (title, seen) = (self.title, self.seen);
            super::title_textures::with_title_bytes(at, stop - at, |now| {
                let mut p = at;
                while p < stop {
                    let next = ((p & !(PAGE - 1)) + PAGE).min(stop);
                    let now = &now[p - at..next - at];
                    let seen = std::slice::from_raw_parts_mut((seen + (p - title)) as *mut u8, next - p);
                    if now != seen {
                        seen.copy_from_slice(now);
                        out.push(p & !(PAGE - 1));
                    }
                    p = next;
                }
            });
            left = left.saturating_sub(stop - at);
            at = stop;
        }
        self.cursor = if at >= end { 0 } else { at - self.title };
    }

    unsafe fn sync_page(self, page: usize, granularity: usize) -> Option<Range<usize>> {
        let start = page.max(self.title);
        let end = (page + granularity).min(self.title + self.bytes);
        if start >= end || !writable(start, end) {
            return None;
        }
        let dst = self.mirror + (start - self.title);
        let len = end - start;
        // (as a copy: writable a moment ago is not writable now, `with_title_bytes`)
        super::title_textures::with_title_bytes(start, len, |now| std::ptr::copy_nonoverlapping(now.as_ptr(), dst as *mut u8, len));
        Some(dst..dst + len)
    }

    pub(super) unsafe fn read(mut self, begin: usize, end: usize) -> bool {
        let _refresh = REFRESH.lock().unwrap();
        let start = (self.title + begin) & !(PAGE - 1);
        let stop = self.title + end;
        if !writable(start, stop) {
            return false;
        }
        for (base, size) in crate::hle::kernelx::graphics_reservations() {
            let (lo, hi) = (start.max(base), stop.min(base + size));
            if lo >= hi {
                continue;
            }
            let mut pages = vec![std::ptr::null_mut::<c_void>(); (hi - lo).div_ceil(PAGE)];
            let mut count = pages.len();
            let mut granularity = 0u32;
            if GetWriteWatch(
                0,
                lo as *const c_void,
                hi - lo,
                pages.as_mut_ptr(),
                &mut count,
                &mut granularity,
            ) == 0
            {
                for &p in &pages[..count] {
                    self.sync_page(p as usize, granularity as usize);
                }
            }
        }
        let mut changed = Vec::new();
        self.changed_pages(self.bytes, &mut changed);
        for p in changed {
            self.sync_page(p, PAGE);
        }
        super::sparse::readback_now(self.mirror + begin, end - begin);
        // (XCL's write into the title's memory)
        crate::alias::mapping_changed("a placed buffer's mirror is copied over its memory", "ID3D12Resource::Map", self.title + begin, end - begin);
        crate::eh::note_guest_write("placed buffer read back (Map)", self.title + begin, end - begin);
        // (written by Windows, from a copy of the mirror: the title can unmap its memory between
        // `writable` and here, `memmap::write_title`)
        let mirrored = std::slice::from_raw_parts((self.mirror + begin) as *const u8, end - begin).to_vec();
        if !crate::memmap::write_title(self.title + begin, &mirrored) {
            return false;
        }
        if self.seen != 0 {
            std::ptr::copy_nonoverlapping((self.mirror + begin) as *const u8, (self.seen + begin) as *mut u8, end - begin);
        }
        true
    }
}

pub(super) unsafe fn sync_dirty(pages: &mut Vec<usize>, granularity: usize) {
    let g = BUFFERS.lock().unwrap();
    let Some(t) = g.as_ref() else { return };
    let original = pages.len();
    for (b, _) in t.mirrors.values() {
        let start = pages[..original].partition_point(|&p| p + granularity <= b.title);
        let end = pages[..original].partition_point(|&p| p < b.title + b.bytes);
        for i in start..end {
            if let Some(dst) = b.sync_page(pages[i], granularity) {
                pages.extend(((dst.start & !(PAGE - 1))..dst.end).step_by(PAGE));
            }
        }
    }
    if pages.len() != original {
        pages.sort_unstable();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::System::Memory::{VirtualAlloc, VirtualFree, MEM_COMMIT, MEM_DECOMMIT, MEM_RELEASE, MEM_RESERVE};

    /// Memory for a test – the title's, or a block of "video memory" – released with it.
    struct Mem(usize);
    impl Mem {
        unsafe fn new(bytes: usize) -> Mem {
            let p = VirtualAlloc(std::ptr::null(), bytes, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE) as usize;
            assert_ne!(p, 0);
            Mem(p)
        }
        unsafe fn byte(&self, at: usize) -> u8 {
            *((self.0 + at) as *const u8)
        }
    }
    impl Drop for Mem {
        fn drop(&mut self) {
            unsafe { VirtualFree(self.0 as *mut c_void, 0, MEM_RELEASE) };
        }
    }

    /// What a submit does for the mirrors: the pages the title wrote are found and copied.
    unsafe fn look(title: usize, at: usize) {
        let mut pages = Vec::new();
        unwatched_dirty(&mut pages);
        pages.push((title + at) & !(PAGE - 1));
        pages.sort_unstable();
        pages.dedup();
        sync_dirty(&mut pages, PAGE);
    }

    /// A buffer placed inside a mirrored one is placed in its mirror – the same video memory,
    /// as it is the same memory on the console – and the mirror lives until both are gone,
    /// whichever goes first. (The sizes of a retail game's two: a buffer over a whole pool,
    /// and minutes later one at the pool's start. It was refused, and the title went on with
    /// no buffer.)
    #[test]
    fn a_buffer_placed_inside_a_mirrored_one_shares_its_mirror() {
        const OUTER: usize = 0xae0_0000;
        const INNER: usize = 0x45_5d80;
        unsafe {
            let (title, mirror) = (Mem::new(OUTER), Mem::new(OUTER));
            let (outer, inner, third) = (title.0 + 1, title.0 + 2, title.0 + 3);
            *(title.0 as *mut u8) = 7;
            assert!(hold(title.0, INNER).is_none(), "nothing is mirrored there yet");
            register(outer, title.0, mirror.0, OUTER);
            assert_eq!(mirror.byte(0), 7);
            // the same base, inside: the outer's mirror, at its start
            let (at, block) = hold(title.0, INNER).expect("a buffer inside a mirrored one");
            assert_eq!((at, block), (mirror.0, mirror.0));
            register_in(inner, title.0, at, INNER, block);
            assert_eq!(get(inner).map(|b| (b.title, b.mirror, b.bytes)), Some((title.0, mirror.0, INNER)));
            // nested at an offset, identical ranges: in the same mirror, at their offsets
            let (at, block) = hold(title.0 + 0x12_3450, 0x1_0000).unwrap();
            assert_eq!(at, mirror.0 + 0x12_3450);
            register_in(third, title.0 + 0x12_3450, at, 0x1_0000, block);
            assert_eq!(hold(title.0, OUTER).map(|(at, _)| at), Some(mirror.0));
            assert_eq!(unhold(mirror.0), None, "a hold given back: the buffers still use the mirror");
            // one mirror for all: a write of the title's is in every buffer's view at once
            *((title.0 + 0x12_3456) as *mut u8) = 9;
            look(title.0, 0x12_3456);
            assert_eq!(mirror.byte(0x12_3456), 9);
            assert_eq!(*((get(third).unwrap().mirror + 6) as *const u8), 9);
            // the outer goes first: the mirror stays, and stays in step, for the buffers in it
            assert_eq!(forget(outer), None, "buffers are placed in its mirror");
            *((title.0 + 0x40) as *mut u8) = 11;
            look(title.0, 0x40);
            assert_eq!(*((get(inner).unwrap().mirror + 0x40) as *const u8), 11);
            assert!(hold(title.0 + OUTER - PAGE, PAGE + 1).is_none(), "reaches beyond the mirrored span");
            assert_eq!(forget(third), None);
            assert_eq!(forget(inner), Some(mirror.0), "the last buffer in the mirror: its video memory goes back");
            assert!(hold(title.0, INNER).is_none() && get(inner).is_none());
        }
    }

    /// The inner buffer goes first: the outer keeps its mirror, and gives it back when it goes.
    #[test]
    fn an_inner_buffer_released_first_leaves_the_outer_its_mirror() {
        const BYTES: usize = 64 * PAGE;
        unsafe {
            let (title, mirror) = (Mem::new(BYTES), Mem::new(BYTES));
            let (outer, inner) = (title.0 + 1, title.0 + 2);
            register(outer, title.0, mirror.0, BYTES);
            let (at, block) = hold(title.0 + 3 * PAGE + 0x80, 5 * PAGE).unwrap();
            register_in(inner, title.0 + 3 * PAGE + 0x80, at, 5 * PAGE, block);
            assert_eq!(forget(inner), None);
            *((title.0 + 4 * PAGE) as *mut u8) = 5;
            look(title.0, 4 * PAGE);
            assert_eq!(mirror.byte(4 * PAGE), 5);
            assert_eq!(forget(outer), Some(mirror.0));
        }
    }

    /// Buffers that overlap in part, and one that reaches around another, each have a mirror
    /// of their own: every one gets what the title writes where they overlap, and each mirror
    /// goes back with its buffer.
    #[test]
    fn buffers_overlapping_in_part_are_each_kept_in_step() {
        const BYTES: usize = 32 * PAGE;
        unsafe {
            let title = Mem::new(2 * BYTES);
            let (m1, m2, m3) = (Mem::new(BYTES), Mem::new(BYTES), Mem::new(2 * BYTES));
            let (first, second, around) = (title.0 + 1, title.0 + 2, title.0 + 3);
            register(first, title.0, m1.0, BYTES);
            // from the middle of the first to beyond its end
            assert!(hold(title.0 + BYTES / 2, BYTES).is_none());
            register(second, title.0 + BYTES / 2, m2.0, BYTES);
            // around both
            assert!(hold(title.0, 2 * BYTES).is_none());
            register(around, title.0, m3.0, 2 * BYTES);
            let at = BYTES / 2 + 3 * PAGE + 1;
            *((title.0 + at) as *mut u8) = 0xC3;
            look(title.0, at);
            assert_eq!((m1.byte(at), m2.byte(at - BYTES / 2), m3.byte(at)), (0xC3, 0xC3, 0xC3));
            assert_eq!(forget(first), Some(m1.0));
            assert_eq!(forget(around), Some(m3.0));
            *((title.0 + at) as *mut u8) = 0x3C;
            look(title.0, at);
            assert_eq!(m2.byte(at - BYTES / 2), 0x3C);
            assert_eq!(forget(second), Some(m2.0));
        }
    }

    /// A Map for reading of a buffer placed inside another copies its part of the shared mirror
    /// over its own memory, no more, and what the title's memory holds then is not taken for a
    /// write of the title's.
    #[test]
    fn a_read_of_an_inner_buffer_copies_its_part_of_the_shared_mirror() {
        const BYTES: usize = 16 * PAGE;
        unsafe {
            let (title, mirror) = (Mem::new(BYTES), Mem::new(BYTES));
            let (outer, inner) = (title.0 + 1, title.0 + 2);
            register(outer, title.0, mirror.0, BYTES);
            let at = 2 * PAGE + 0x10;
            let (place, block) = hold(title.0 + at, 3 * PAGE).unwrap();
            register_in(inner, title.0 + at, place, 3 * PAGE, block);
            // what the GPU wrote: in the mirror, in the inner buffer and before it
            *((mirror.0 + at + 8) as *mut u8) = 0x21;
            *((mirror.0 + at - 1) as *mut u8) = 0x22;
            assert!(get(inner).unwrap().read(0, 16));
            assert_eq!((title.byte(at + 8), title.byte(at - 1)), (0x21, 0));
            let mut pages = Vec::new();
            unwatched_dirty(&mut pages);
            assert!(!pages.iter().any(|&p| (title.0..title.0 + BYTES).contains(&p)), "XCL's own write is not a write of the title's");
            // the outer's view of the same bytes: the same mirror
            assert!(get(outer).unwrap().read(at, at + 16));
            assert_eq!(title.byte(at + 8), 0x21);
            assert_eq!(forget(outer), None);
            assert_eq!(forget(inner), Some(mirror.0));
        }
    }

    /// Inside several mirrored spans at once, a buffer is placed in the largest: the same one
    /// every time.
    #[test]
    fn a_buffer_inside_several_mirrored_spans_is_placed_in_the_largest() {
        const BYTES: usize = 8 * PAGE;
        unsafe {
            let title = Mem::new(4 * BYTES);
            let (small, large) = (Mem::new(BYTES), Mem::new(4 * BYTES));
            let (first, around) = (title.0 + 1, title.0 + 2);
            register(first, title.0 + BYTES, small.0, BYTES);
            register(around, title.0, large.0, 4 * BYTES);
            for _ in 0..8 {
                let (place, block) = hold(title.0 + BYTES + PAGE, PAGE).unwrap();
                assert_eq!((place, block), (large.0 + BYTES + PAGE, large.0));
                assert_eq!(unhold(block), None);
            }
            assert_eq!(forget(first), Some(small.0));
            assert_eq!(forget(around), Some(large.0));
        }
    }

    /// A placed buffer's memory is read as a copy: the title decommits and commits a page of it
    /// on another thread while its pages are compared and copied to the mirror (read where they
    /// are after "writable", this was an access violation), and what the mirror gets of a page
    /// is its bytes or zeros.
    #[test]
    fn a_buffer_the_title_decommits_meanwhile_is_read_without_a_fault() {
        const BYTES: usize = 16 * PAGE;
        unsafe {
            let title = VirtualAlloc(std::ptr::null(), BYTES, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE) as usize;
            assert_ne!(title, 0);
            std::ptr::write_bytes(title as *mut u8, 0x5A, BYTES);
            let mut mirror = vec![0u8; BYTES];
            let seen = Box::into_raw(vec![0u8; BYTES].into_boxed_slice()) as *mut u8 as usize;
            let mut b = Backing { title, mirror: mirror.as_mut_ptr() as usize, bytes: BYTES, seen, cursor: 0, block: mirror.as_ptr() as usize };
            let stop = std::sync::atomic::AtomicBool::new(false);
            std::thread::scope(|s| {
                s.spawn(|| {
                    let page = (title + 5 * PAGE) as *mut c_void;
                    while !stop.load(Ordering::Relaxed) {
                        VirtualFree(page, PAGE, MEM_DECOMMIT);
                        VirtualAlloc(page, PAGE, MEM_COMMIT, PAGE_READWRITE);
                        std::ptr::write_bytes(page as *mut u8, 0x5A, PAGE);
                    }
                });
                let mut dirty = Vec::new();
                for _ in 0..4000 {
                    dirty.clear();
                    b.changed_pages(BYTES, &mut dirty);
                    for at in (title..title + BYTES).step_by(PAGE) {
                        Backing { ..b }.sync_page(at, PAGE);
                    }
                    assert!(mirror.iter().all(|&x| x == 0x5A || x == 0), "the title's bytes or zeros");
                }
                stop.store(true, Ordering::Relaxed);
            });
            drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(seen as *mut u8, BYTES)));
            VirtualFree(title as *mut c_void, 0, MEM_RELEASE);
        }
    }
}
