//! kernelx.dll – the console flavour of kernel32/kernelbase.
//!
//! Only functions that differ from Windows are implemented here; everything else falls
//! back to the real KERNELBASE/KERNEL32 export of the same name (see resolver).
//! Signatures and constants: public GDK docs + WinDurango (LGPL-3.0) as reference only (no code taken).

#![allow(non_snake_case)]

use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use windows_sys::Win32::Foundation as fnd;
use windows_sys::Win32::System::LibraryLoader as ll;
use windows_sys::Win32::System::Memory as mem;
use windows_sys::Win32::System::ProcessStatus as ps;
use windows_sys::Win32::System::SystemInformation::GROUP_AFFINITY;
use windows_sys::Win32::System::Threading as thr;

use crate::modules;
use crate::pe::ImportName;
use crate::resolver;
use crate::util::{dll_key, from_ansi_ptr, from_wide_ptr, write_ansi_out, write_wide_out};

type HANDLE = *mut c_void;
type BOOL = i32;

/// The title's own unhandled-exception filter goes behind XCL's (`eh`): XCL's crash report
/// first, then the title's handler (its crash reporter) as on the console.
pub unsafe extern "system" fn SetUnhandledExceptionFilter(f: usize) -> usize {
    crate::eh::set_title_filter(f)
}

pub fn exports() -> Vec<(&'static str, usize)> {
    crate::hle_exports![
        // registry: the console mode the GDK thunks read (XSystemGetDeviceType)
        RegOpenKeyW,
        RegOpenKeyExW,
        RegQueryValueExW,
        RegCloseKey,
        // memory
        VirtualAlloc,
        VirtualAllocEx,
        VirtualFree,
        VirtualFreeEx,
        VirtualProtect,
        VirtualProtectEx,
        XMemAlloc,
        XMemFree,
        XMemAllocDefault,
        XMemFreeDefault,
        XMemSetAllocationHooks,
        XMemVirtualQuery,
        XMemVirtualAlloc,
        XMemIsToolsMemAvailable,
        XMemAllocatePhysicalPages,
        XMemMapPhysicalPages,
        XMemMapPhysicalPagesScatter,
        XMemFreePhysicalPages,
        XMemTransferMemory,
        XMemGetWorkingSetStatistics,
        TitleMemoryStatus,
        JobTitleMemoryStatus,
        ToolingMemoryStatus,
        AllocateTitlePhysicalPages,
        FreeTitlePhysicalPages,
        MapTitlePhysicalPages,
        // app state (minappmodelx): a title is never suspended or constrained here
        RegisterAppStateChangeNotification,
        UnregisterAppStateChangeNotification,
        RegisterAppConstrainedChangeNotification,
        UnregisterAppConstrainedChangeNotification,
        // threads / system
        SetUnhandledExceptionFilter,
        Sleep,
        SetThreadName,
        GetThreadName,
        SetThreadpoolAffinityMask,
        SetThreadAffinityMask,
        SetThreadGroupAffinity,
        QueryProcessorSchedulingStatistics,
        GetConsoleType,
        OutputDebugStringA,
        OutputDebugStringW,
        GetSystemOSVersion,
        // modules
        LoadLibraryW,
        LoadLibraryA,
        LoadLibraryExW,
        LoadLibraryExA,
        LoadPackagedLibrary,
        GetProcAddress,
        GetModuleHandleW,
        GetModuleHandleA,
        GetModuleHandleExW,
        GetModuleHandleExA,
        GetModuleFileNameW,
        GetModuleFileNameA,
        GetModuleInformation,
        K32GetModuleInformation,
        FreeLibrary,
        FreeLibraryWhenCallbackReturns,
        DisableThreadLibraryCalls,
    ]
}

// ---------------------------------------------------------------------------------------
// Memory
// ---------------------------------------------------------------------------------------

pub const MEM_GRAPHICS: u32 = 0x1000_0000;
mod placeholders;
pub mod lazy;
pub const MEM_TITLE: u32 = 0x4000_0000;
const STD_ALLOC_MASK: u32 = 0x1000 | 0x2000 | 0x8_0000 | 0x10_0000 | 0x20_0000 | 0x100_0000;
const STD_PROTECT_MASK: u32 = 0xFF | 0x100 | 0x200 | 0x400;

static WARNED_FLAGS: AtomicU32 = AtomicU32::new(0);

pub fn sanitize_alloc_type(t: u32) -> u32 {
    let s = t & STD_ALLOC_MASK;
    let dropped = t & !STD_ALLOC_MASK;
    if dropped != 0 && WARNED_FLAGS.fetch_or(dropped, Ordering::Relaxed) & dropped != dropped {
        crate::xlog_debug!("VirtualAlloc: dropping console allocation flags {dropped:#x}");
    }
    if s == 0 {
        0x1000 | 0x2000
    } else {
        s
    }
}

pub fn sanitize_protect(p: u32) -> u32 {
    let s = p & STD_PROTECT_MASK;
    if s & 0xFF == 0 {
        (s & !0xFF) | mem::PAGE_READWRITE
    } else {
        s
    }
}

/// Write-watched title graphics allocations outside unified memory (base → size): address space
/// the title reserved that unified memory had no room for (where it maps physical pages), its own
/// 64 KiB-page reservations and write-combined large-page commits. Placed resources use these
/// ranges to mirror CPU writes to the GPU (`d3d12x::title_textures`).
static GRAPHICS_RESERVATIONS: std::sync::Mutex<std::collections::BTreeMap<usize, usize>> = std::sync::Mutex::new(std::collections::BTreeMap::new());
static GRAPHICS_RESERVATIONS_GEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Generation counter incremented whenever `GRAPHICS_RESERVATIONS` changes.
pub fn graphics_reservations_gen() -> u64 {
    GRAPHICS_RESERVATIONS_GEN.load(Ordering::Acquire)
}

/// The title's graphics reservations (base, size).
pub fn graphics_reservations() -> Vec<(usize, usize)> {
    GRAPHICS_RESERVATIONS.lock().unwrap().iter().map(|(&b, &s)| (b, s)).collect()
}

/// Whether `p` lies in one of the title's graphics reservations.
pub fn in_graphics_reservation(p: usize) -> bool {
    GRAPHICS_RESERVATIONS.lock().unwrap().range(..=p).next_back().is_some_and(|(&b, &s)| p < b + s)
}

const MEM_WRITE_WATCH: u32 = 0x20_0000;

/// GPU views of the title's CPU memory. A retail game reserves 24 GB of 64 KiB pages twice
/// in a row: once readable by the CPU (PAGE_READWRITE), once with PAGE_NOACCESS |
/// PAGE_GRAPHICS_NOACCESS. It maps physical pages only into the first, yet places GPU
/// resources at the same offset in the second (its Bink video planes: the decoder writes the
/// first view, the video shader samples textures in the second). On the console the second
/// reservation is a GPU-only view of the first; here it is separate, empty memory – the
/// video was its colour of zero luma and chroma, a green screen. `source` names the CPU view
/// behind an address of such a GPU view, which `d3d12x::title_textures` reads and watches.
pub mod mirrors {
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    /// MEM_LARGE_PAGES | MEM_PHYSICAL: the console's 64 KiB pages
    const PAGES_64K: u32 = 0x2040_0000;
    /// PAGE_GRAPHICS_NOACCESS
    const GRAPHICS_NOACCESS: u32 = 0x800;

    struct State {
        /// CPU-readable reservations of 64 KiB pages, oldest first: (base, size)
        cpu: Vec<(usize, usize)>,
        /// GPU view base → (size, CPU view base)
        gpu: BTreeMap<usize, (usize, usize)>,
    }
    static STATE: Mutex<State> = Mutex::new(State { cpu: Vec::new(), gpu: BTreeMap::new() });

    /// A reservation of `size` bytes at `base` with allocation type `t` and protection `p`.
    pub fn reserved(base: usize, size: usize, t: u32, p: u32) {
        if t & PAGES_64K != PAGES_64K || t & 0x1000 != 0 {
            return;
        }
        let mut s = STATE.lock().unwrap();
        if p & GRAPHICS_NOACCESS != 0 && p & 0xFF == super::mem::PAGE_NOACCESS {
            // the most recent CPU view of the same size without a GPU view yet
            let taken: Vec<usize> = s.gpu.values().map(|&(_, c)| c).collect();
            if let Some(&(cpu, _)) = s.cpu.iter().rev().find(|&&(b, n)| n == size && !taken.contains(&b)) {
                s.gpu.insert(base, (size, cpu));
                crate::xlog_info!("kernelx: reservation {base:#x} ({size:#x} bytes, no CPU access) is the GPU view of {cpu:#x}");
            }
        } else if p & 0xFF != super::mem::PAGE_NOACCESS {
            s.cpu.push((base, size));
        }
    }

    /// The reservation at `base` was released.
    pub fn released(base: usize) {
        let mut s = STATE.lock().unwrap();
        s.cpu.retain(|&(b, _)| b != base);
        s.gpu.remove(&base);
        s.gpu.retain(|_, &mut (_, c)| c != base);
    }

    /// (diagnostics) The address at the same offset in the other reservation of a CPU view / GPU
    /// view pair holding `va`, and what that reservation is. Not waited for at a crash.
    pub fn counterparts(va: usize) -> Vec<(usize, &'static str)> {
        let Some(s) = crate::alias::try_lock(&STATE) else { return Vec::new() };
        let mut out = Vec::new();
        for (&gpu, &(size, cpu)) in s.gpu.iter() {
            if va >= gpu && va - gpu < size {
                out.push((cpu + (va - gpu), "this address is in a GPU-view reservation; the same offset in the CPU view it mirrors,"));
            }
            if va >= cpu && va - cpu < size {
                out.push((gpu + (va - cpu), "the same offset in the GPU-view reservation of this CPU view,"));
            }
        }
        out
    }

    /// The CPU-view address behind `va` when `va` lies in a GPU view.
    pub fn source(va: usize) -> Option<usize> {
        let s = STATE.lock().unwrap();
        let (&gpu, &(size, cpu)) = s.gpu.range(..=va).next_back()?;
        (va < gpu + size).then(|| cpu + (va - gpu))
    }
}

/// Graphics memory (MEM_GRAPHICS, committed, no fixed address) comes from the unified
/// memory region the GPU uses directly (`gpu::uma`); a graphics reservation (no commit) is
/// write-watched address space of Windows; everything else from Windows.
/// The console's MEM_64K_PAGES (MEM_LARGE_PAGES | MEM_PHYSICAL): title memory of 64 KiB
/// pages – GPU-visible on the console like all title memory.
const MEM_64K_PAGES: u32 = 0x2040_0000;
const MEM_CONSOLE_LARGE_PAGES: u32 = 0x2000_0000;

/// Commit `[a, a + n)` of a sparse window (the title's reservations there) and give it video
/// memory (`d3d12x::sparse`).
unsafe fn commit_sparse(a: usize, n: usize, p: u32) -> *mut c_void {
    let q = mem::VirtualAlloc(a as *const c_void, n, mem::MEM_COMMIT, sanitize_protect(p));
    if !q.is_null() {
        crate::hle::d3d12x::sparse::committed(q as usize, n);
        crate::memmap::committed(a, n, sanitize_protect(p));
    }
    q
}

pub unsafe extern "system" fn VirtualAlloc(addr: *const c_void, size: usize, t: u32, p: u32) -> *mut c_void {
    virtual_alloc(addr, size, t, p, None)
}

unsafe fn virtual_alloc(addr: *const c_void, size: usize, t: u32, p: u32, gdk: Option<physical::RawGdk64>) -> *mut c_void {
    if let Some(q) = placeholders::alloc(addr, size, t, sanitize_protect(p)) {
        trace_mem("VirtualAlloc (placeholder)", addr as usize, size, t as u64, p, q as usize);
        return q;
    }
    // committing inside one of the title's reservations of 64 KiB pages made of placeholders
    if !addr.is_null() && t & mem::MEM_COMMIT != 0 {
        if let Some(q) = physical::commit_in_reservation(addr as usize, size, sanitize_protect(p)) {
            trace_mem("VirtualAlloc (commit among physical pages)", addr as usize, size, t as u64, p, q as usize);
            return q;
        }
    }
    let uma = crate::gpu::uma::get();
    let title_64k_reservation = addr.is_null() && t & mem::MEM_RESERVE != 0 && t & MEM_64K_PAGES == MEM_64K_PAGES;
    // committing inside one of the title's reservations in a sparse window
    if !addr.is_null() && t & 0x1000 != 0 && uma.is_some_and(|u| u.is_sparse(addr as usize)) {
        let q = commit_sparse(addr as usize, size, p);
        trace_mem("VirtualAlloc (sparse commit)", addr as usize, size, t as u64, p, q as usize);
        return q;
    }
    // the title's own reservations of 64 KiB pages: in a sparse window, so the GPU sees them
    if title_64k_reservation && crate::hle::d3d12x::sparse::accepts_allocations() {
        if let Some(q) = uma.and_then(|u| u.sparse_reserve(size)) {
            if t & 0x1000 != 0 && commit_sparse(q, size, p).is_null() {
                uma.map(|u| u.sparse_release(q));
                return std::ptr::null_mut();
            }
            mirrors::reserved(q, size, t, p);
            trace_mem("VirtualAlloc (64K pages, sparse)", 0, size, t as u64, p, q);
            return q as *mut c_void;
        }
    }
    // the title's other reservations of 64 KiB pages: placeholders, so the physical pages it maps
    // there are views of the pages themselves (`physical`; off with XCL_PAGE_VIEWS=0: copies)
    if title_64k_reservation && t & mem::MEM_COMMIT == 0 && page_views() {
        if let Some(q) = physical::reserve_compat(size, gdk) {
            mirrors::reserved(q, size, t, p);
            trace_mem("VirtualAlloc (64K pages, placeholders)", 0, size, t as u64, p, q);
            return q as *mut c_void;
        }
    }
    if addr.is_null() && t & MEM_GRAPHICS != 0 && t & 0x1000 != 0 {
        let align = if t & 0x8000_0000 != 0 {
            4 << 20
        } else if t & 0x2000_0000 != 0 && t & 0x0040_0000 == 0 {
            2 << 20
        } else {
            crate::gpu::uma::GRAIN
        };
        let rounded = size.max(1).div_ceil(align) * align;
        if let Some((q, _)) = crate::hle::d3d12x::sparse::alloc(rounded, align) {
            trace_mem("VirtualAlloc (graphics, sparse VRAM)", 0, rounded, t as u64, p, q);
            return q as *mut c_void;
        }
        if let Some(q) = uma.and_then(|u| u.alloc(rounded, align)) {
            trace_mem("VirtualAlloc (graphics, UMA)", 0, rounded, t as u64, p, q as usize);
            return q as *mut c_void;
        }
    }
    if addr.is_null() && t & MEM_GRAPHICS != 0 && t & (mem::MEM_RESERVE | mem::MEM_COMMIT) == mem::MEM_RESERVE {
        // on the console all graphics memory is GPU-visible, reserved address space too (titles
        // reserve a range, then commit or map pages and place resources in it): prefer the
        // sparse VRAM windows first so reservations consume zero system RAM until committed
        let reserved = if !crate::hle::d3d12x::sparse::accepts_allocations() {
            None
        } else if t & 0x8000_0000 != 0 {
            let rounded = size.max(1).div_ceil(4 << 20) * (4 << 20);
            uma.and_then(|u| u.sparse_alloc(rounded, 4 << 20)).map(|(q, _)| q)
        } else {
            uma.and_then(|u| u.sparse_reserve(size))
        };
        if let Some(q) = reserved {
            trace_mem("VirtualAlloc (graphics reserve, sparse)", 0, size, t as u64, p, q);
            return q as *mut c_void;
        }
        if let Some(q) = uma.and_then(|u| u.reserve(size, 0x1_0000)) {
            trace_mem("VirtualAlloc (graphics reserve, UMA)", 0, size, t as u64, p, q as usize);
            return q as *mut c_void;
        }
    }
    // (opt-in, `XCL_COHERENT_HEAPS_MAX_MB`; nothing changes unless it is set) A GPU heap of the
    // title's larger than that many MB goes to the sparse windows instead of the coherent window.
    // The window is a heap D3D12 opened, so every page of it is pinned RAM whether the title ever
    // writes it (play-97: 2,075 of its 3,360 MB were zero pages in the world); a sparse block's
    // CPU pages are resident only once written. The price is video memory for the tiles of the
    // block that are written, placed on or written by a GPU command (`sparse::alloc_diverted`; for
    // the whole block with `XCL_SPARSE_LAZY_TILES=0`) and the copy of what the CPU wrote there before
    // each submit. Meant for the heaps `memdiag`'s coherent-window line shows as large and mostly
    // zeros; `XCL_COHERENT_MB` then sizes the window for what stays in it.
    if addr.is_null() && t & 0x1000 != 0 && p & PAGE_GRAPHICS_ANY != 0 && coherent_heap_max().is_some_and(|max| size > max) {
        if let Some((q, rounded)) = sparse_heap(size, t) {
            crate::xlog_info!("UMA: a title GPU heap of {} MB goes to the sparse windows at {q:#x} (XCL_COHERENT_HEAPS_MAX_MB)", size >> 20);
            trace_mem("VirtualAlloc (graphics, sparse VRAM by XCL_COHERENT_HEAPS_MAX_MB)", 0, rounded, t as u64, p, q);
            return q as *mut c_void;
        }
    }
    // the title's GPU heaps (committed with a PAGE_GRAPHICS_* protection, write-combined: memory
    // the CPU writes and the GPU reads, as a PC game's upload heaps): in the coherent window, which
    // the GPU reads where it is – one copy (a retail game's 4.4 GB; as plain memory its shaders
    // read zeros, in the sparse windows video memory would have doubled them). The window is as
    // large as what was in use in it at once in earlier runs (`uma::coherent_hint`).
    if addr.is_null() && t & 0x1000 != 0 && p & PAGE_GRAPHICS_ANY != 0 && size <= crate::gpu::uma::MAX_SIZE && std::env::var("XCL_COHERENT_HEAPS").map_or(true, |v| v != "0") {
        if let Some(u) = uma {
            let align = if t & 0x2000_0000 != 0 && t & 0x0040_0000 == 0 { 2 << 20 } else { crate::gpu::uma::GRAIN };
            // what XCL keeps of the window for itself (readback heaps; its per-draw arguments have
            // a pool of their own)
            const KEEP: usize = 96 << 20;
            let fits = u.size().saturating_sub(u.used()) >= size + KEEP;
            let got = u.committed_window().filter(|_| fits).and_then(|k| u.alloc_in(k, size, align));
            // what the window had to hold at its fullest – with this allocation, when it had no
            // room for it (the sum of all that was asked for counted what the title freed again)
            crate::gpu::uma::note_coherent_need(u.peak().max(if got.is_some() { 0 } else { u.used().saturating_add(size) }));
            if let Some(q) = got {
                trace_mem("VirtualAlloc (graphics, coherent window)", 0, size, t as u64, p, q as usize);
                return q as *mut c_void;
            }
            if let Some((q, rounded)) = coherent_overflow(u, size, t) {
                trace_mem("VirtualAlloc (graphics, sparse VRAM: no room in the coherent window)", 0, rounded, t as u64, p, q);
                return q as *mut c_void;
            }
        }
    }
    // GPU-visible title memory the windows above had no room for (a retail game: a 24 GB
    // reservation of 64 KiB pages, 2.2 GB of large pages with a graphics protection) is plain
    // memory here. Textures the title places in it are kept in step with it
    // (`d3d12x::title_textures`), which needs its CPU writes to be seen: it is write-watched
    // like a graphics reservation. Without that such a texture was uploaded once, empty, and
    // never again (the game's glyph atlas: its menus lost their x-height letters).
    let watched = if gpu_visible_fallback(addr, t, p) { Some(reserve_watched(size, t, p)).filter(|q| !q.is_null()) } else { None };
    let q = watched.unwrap_or_else(|| mem::VirtualAlloc(addr, size, sanitize_alloc_type(t), sanitize_protect(p)));
    if !q.is_null() {
        let flags = sanitize_alloc_type(t);
        // (no address chosen: a new allocation whatever the flags – a commit alone reserves too.
        // Recorded as a reservation so its release takes what was committed in it along.)
        if addr.is_null() || flags & mem::MEM_RESERVE != 0 {
            crate::memmap::reserved(q as usize, if addr.is_null() { size } else { (addr as usize + size).saturating_sub(q as usize) });
        }
        if flags & mem::MEM_COMMIT != 0 {
            crate::memmap::committed(if addr.is_null() { q as usize } else { addr as usize }, size, sanitize_protect(p));
        }
    }
    if watched.is_some() {
        mirrors::reserved(q as usize, size, t, p);
        trace_mem("VirtualAlloc (GPU-visible, write-watched)", addr as usize, size, t as u64, p, q as usize);
        return q;
    }
    trace_mem("VirtualAlloc", addr as usize, size, t as u64, p, q as usize);
    q
}

/// The title's reservations of 64 KiB pages are placeholders holding views of its pages
/// (`XCL_PAGE_VIEWS=0`: off, write-watched memory holding copies).
fn page_views() -> bool {
    #[cfg(test)]
    if NO_PAGE_VIEWS.with(|n| n.get()) {
        return false;
    }
    std::env::var("XCL_PAGE_VIEWS").map_or(true, |v| v != "0")
}

#[cfg(test)]
thread_local! {
    static NO_PAGE_VIEWS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// (tests) This thread's reservations of 64 KiB pages are write-watched memory holding copies
/// of the pages, as with `XCL_PAGE_VIEWS=0`, until the guard goes.
#[cfg(test)]
pub(crate) fn without_page_views() -> impl Drop {
    struct Guard;
    impl Drop for Guard {
        fn drop(&mut self) {
            NO_PAGE_VIEWS.with(|n| n.set(false));
        }
    }
    NO_PAGE_VIEWS.with(|n| n.set(true));
    Guard
}

/// (tests) `check`, for memory a test has just released: run with `[base, base + len)` reserved
/// again behind XCL, so that no other test's allocation is there. Tests run side by side, and
/// Windows gives a released address to the next allocation of any of them – whose records are
/// then rightly at that address ("nothing of XCL's is here any more" failed a run in a few).
/// Not run when another thread has the address already: there is nothing of this test's to see.
/// That is counted – a check that is never run would pass for ever: once six looks have been
/// asked for, more than two skipped beyond those that were run fail the test (not before: the
/// first few skipped in a run side by side are chance, and failed whichever test came by).
#[cfg(test)]
pub(crate) unsafe fn in_released_memory(base: usize, len: usize, check: impl FnOnce()) {
    use std::sync::atomic::AtomicUsize;
    static RUN: AtomicUsize = AtomicUsize::new(0);
    static SKIPPED: AtomicUsize = AtomicUsize::new(0);
    let held = mem::VirtualAlloc(base as *const c_void, len, mem::MEM_RESERVE, mem::PAGE_NOACCESS);
    if held as usize == base {
        check();
        RUN.fetch_add(1, Ordering::Relaxed);
    } else {
        SKIPPED.fetch_add(1, Ordering::Relaxed);
    }
    if !held.is_null() {
        mem::VirtualFree(held, 0, mem::MEM_RELEASE);
    }
    let (run, skipped) = (RUN.load(Ordering::Relaxed), SKIPPED.load(Ordering::Relaxed));
    assert!(run + skipped < 6 || skipped <= run + 2, "{skipped} looks at released memory were skipped (the address was taken again), {run} run");
}

fn note_graphics_reservation(base: usize, size: usize) {
    GRAPHICS_RESERVATIONS.lock().unwrap().insert(base, size);
    GRAPHICS_RESERVATIONS_GEN.fetch_add(1, Ordering::Release);
}

/// Whether `[base, base + bytes)` is committed read-write memory (write-combined or not).
pub(crate) fn writable_span(base: usize, bytes: usize) -> bool {
    if let Some(a) = crate::memmap::access(base, bytes) {
        return a == crate::memmap::Access::Write;
    }
    let Some(end) = base.checked_add(bytes) else { return false };
    let mut at = base;
    while at < end {
        let mut info = mem::MEMORY_BASIC_INFORMATION::default();
        if unsafe { mem::VirtualQuery(at as *const _, &mut info, std::mem::size_of_val(&info)) } == 0
            || info.State != mem::MEM_COMMIT
            || info.Protect & !(mem::PAGE_NOCACHE | mem::PAGE_WRITECOMBINE) != mem::PAGE_READWRITE
        {
            return false;
        }
        at = info.BaseAddress as usize + info.RegionSize;
    }
    true
}

/// `XCL_COHERENT_HEAPS_MAX_MB` in bytes: the largest GPU heap of the title's that the coherent
/// window takes (None, when it is not set: any).
fn coherent_heap_max() -> Option<usize> {
    static MAX: std::sync::OnceLock<Option<usize>> = std::sync::OnceLock::new();
    *MAX.get_or_init(|| std::env::var("XCL_COHERENT_HEAPS_MAX_MB").ok().and_then(|v| v.trim().parse::<usize>().ok()).map(|mb| mb << 20))
}

/// A GPU heap of the title's of `size` bytes in the sparse windows, for the heaps the coherent
/// window does not take (`XCL_COHERENT_HEAPS_MAX_MB`, `coherent_overflow`): (address, bytes).
unsafe fn sparse_heap(size: usize, t: u32) -> Option<(usize, usize)> {
    let align = if t & 0x2000_0000 != 0 && t & 0x0040_0000 == 0 { 2 << 20 } else { crate::gpu::uma::GRAIN };
    let rounded = size.max(1).div_ceil(align) * align;
    crate::hle::d3d12x::sparse::alloc_diverted(rounded, align).map(|(q, _)| (q, rounded))
}

/// A GPU heap of the title's that has no room in the coherent window `u` – the window is full,
/// or was made smaller than the title needs (a first run, `XCL_COHERENT_MB`, a short commit in
/// `uma::coherent_hint`) – goes to the sparse windows, where the GPU sees it too: as plain memory
/// its shaders read zeros. (address, bytes); None: the sparse windows have no room either, or
/// `XCL_COHERENT_OVERFLOW_SPARSE=0` (off: plain memory, as before). Each heap is logged with its
/// size – the first 32, then when their number doubles.
unsafe fn coherent_overflow(u: &crate::gpu::uma::Uma, size: usize, t: u32) -> Option<(usize, usize)> {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    static HEAPS: AtomicUsize = AtomicUsize::new(0);
    static MOVED: AtomicUsize = AtomicUsize::new(0);
    let on = *ON.get_or_init(|| std::env::var("XCL_COHERENT_OVERFLOW_SPARSE").map_or(true, |v| v != "0"));
    let got = if on { sparse_heap(size, t) } else { None };
    let n = HEAPS.fetch_add(1, Ordering::Relaxed) + 1;
    let moved = MOVED.fetch_add(got.map_or(0, |(_, bytes)| bytes), Ordering::Relaxed) + got.map_or(0, |(_, bytes)| bytes);
    if n <= 32 || n.is_power_of_two() {
        let (mb, used, window) = (size as f64 / 1048576.0, u.used() >> 20, u.size() >> 20);
        match got {
            Some((q, _)) => crate::xlog_info!(
                "UMA: a title GPU heap of {mb:.1} MB has no room in the coherent window ({used} of {window} MB in use): it goes to the sparse windows at {q:#x} (heap {n} without room, {} MB of them in the sparse windows; XCL_COHERENT_OVERFLOW_SPARSE=0: plain memory)",
                moved >> 20
            ),
            None if on => crate::xlog_warn!("UMA: a title GPU heap of {mb:.1} MB has no room in the coherent window ({used} of {window} MB in use) nor in the sparse windows: plain memory, its shaders read zeros (heap {n} without room)"),
            None => crate::xlog_warn!("UMA: a title GPU heap of {mb:.1} MB has no room in the coherent window ({used} of {window} MB in use): plain memory, its shaders read zeros (heap {n} without room; XCL_COHERENT_OVERFLOW_SPARSE=0)"),
        }
    }
    got
}

/// Whether a VirtualAlloc that reached the plain-memory fallback is GPU-visible title memory
/// (a reservation with MEM_GRAPHICS, MEM_LARGE_PAGES / MEM_64K_PAGES or a PAGE_GRAPHICS_*
/// protection, at no fixed address), which is reserved write-watched (`reserve_watched`).
fn gpu_visible_fallback(addr: *const c_void, t: u32, p: u32) -> bool {
    let graphics = t & (MEM_GRAPHICS | MEM_CONSOLE_LARGE_PAGES) != 0 || p & PAGE_GRAPHICS_ANY != 0;
    // (a commit with no address reserves too: graphics memory the pools above could not take is
    // still memory the title places GPU resources in, `external_buffers` mirrors them)
    let committed = t & mem::MEM_COMMIT != 0 && t & MEM_CONSOLE_LARGE_PAGES == 0;
    addr.is_null() && graphics && (t & mem::MEM_RESERVE != 0 || committed)
}

/// Plain memory for GPU-visible title memory, write-watched and recorded with the graphics
/// reservations (whose ranges `title_textures` watches; `VirtualFree` forgets it). Null if
/// Windows refuses it.
unsafe fn reserve_watched(size: usize, t: u32, p: u32) -> *mut c_void {
    let q = mem::VirtualAlloc(std::ptr::null(), size, sanitize_alloc_type(t) | mem::MEM_RESERVE | MEM_WRITE_WATCH, sanitize_protect(p));
    if !q.is_null() {
        note_graphics_reservation(q as usize, size);
    }
    q
}

/// PAGE_GRAPHICS_NOACCESS … PAGE_GRAPHICS_NOCACHE (0x800 – 0x40000): the console's protection of
/// GPU-visible memory
const PAGE_GRAPHICS_ANY: u32 = 0x7_F800;

/// `XCL_TRACE_MEM=1`: the title's allocations of 64 KiB or more (the first 60; `XCL_TRACE_MEM=n`:
/// the first n when n > 1)
fn trace_mem(what: &str, addr: usize, size: usize, flags: u64, protect: u32, got: usize) {
    static LIMIT: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    static N: AtomicUsize = AtomicUsize::new(0);
    // the call being traced may have just set the title's last error: reading XCL_TRACE_MEM
    // (the first trace; 203 ERROR_ENVVAR_NOT_FOUND when unset) or logging must not change it
    keep_last_error(|| {
        let limit = *LIMIT.get_or_init(|| match std::env::var("XCL_TRACE_MEM") {
            Ok(v) => v.parse().ok().filter(|&n| n > 1).unwrap_or(60),
            Err(_) => 0,
        });
        if size >= 0x1_0000 && N.fetch_add(1, Ordering::Relaxed) < limit {
            crate::xlog_info!("mem: {what}({addr:#x}, {size:#x}, flags {flags:#x}, protect {protect:#x}) → {got:#x}");
        }
    })
}

/// `f` with the thread's last error as it was before.
fn keep_last_error<R>(f: impl FnOnce() -> R) -> R {
    let e = unsafe { fnd::GetLastError() };
    let r = f();
    unsafe { fnd::SetLastError(e) };
    r
}

/// Blocks of the unified memory region go back to it (MEM_RELEASE) or stay (MEM_DECOMMIT
/// zeroes them); others to Windows.
pub unsafe extern "system" fn VirtualFree(addr: *mut c_void, size: usize, t: u32) -> BOOL {
    // (`XCL_DS_TRIPWIRE`; size 0: to the end of the reservation, as far as XCL knows it)
    if crate::alias::tripwire_on() {
        let len = if size != 0 { size } else { crate::memmap::reservation(addr as usize).map_or(crate::alias::PAGE, |(_, end)| end - addr as usize) };
        crate::alias::mapping_changed(if t & mem::MEM_RELEASE != 0 { "the title releases memory" } else { "the title decommits memory" }, "VirtualFree", addr as usize, len);
    }
    if t & mem::MEM_RELEASE != 0 {
        mirrors::released(addr as usize);
    }
    if let Some(ok) = placeholders::free(addr, size, t) {
        return ok;
    }
    if let Some(ok) = physical::free_in_reservation(addr as usize, size, t) {
        return ok;
    }
    // the title's reservations in a sparse window: decommit (and on release give the address
    // space back); their video memory goes with it
    if let Some(u) = crate::gpu::uma::get().filter(|u| u.is_sparse(addr as usize)) {
        let a = addr as usize;
        let release = t & mem::MEM_RELEASE != 0;
        let decommit = t & mem::MEM_DECOMMIT != 0;
        if release == decommit || (release && size != 0) {
            fnd::SetLastError(fnd::ERROR_INVALID_PARAMETER);
            return 0;
        }
        let n = if release || size == 0 {
            match u.sparse_reservation(a) {
                Some((start, len)) if start == a => len,
                _ => {
                    fnd::SetLastError(fnd::ERROR_INVALID_PARAMETER);
                    return 0;
                }
            }
        } else {
            match u.sparse_reservation(a) {
                Some((start, len)) if a.checked_add(size).is_some_and(|end| end <= start + len) => size,
                _ => {
                    fnd::SetLastError(fnd::ERROR_INVALID_PARAMETER);
                    return 0;
                }
            }
        };
        if n > 0 {
            physical::title_freed(a, n, "VirtualFree");
            // sparse first: under its lock, so no copy of CPU writes to video memory reads the
            // pages while they go (the submit thread's copy faulted on them)
            crate::hle::d3d12x::sparse::decommitted(a, n);
            if mem::VirtualFree(addr, n, mem::MEM_DECOMMIT) == 0 {
                return 0;
            }
        }
        if release {
            u.sparse_release(a);
        }
        return 1;
    }
    if let Some(u) = crate::gpu::uma::get().filter(|u| u.contains(addr as usize)) {
        if t & 0x8000 != 0 {
            physical::title_freed(addr as usize, u.block_size(addr as usize).unwrap_or(0), "VirtualFree");
            return u.free(addr as usize) as BOOL;
        }
        if t & 0x4000 != 0 && size > 0 {
            physical::title_freed(addr as usize, size, "VirtualFree");
            std::ptr::write_bytes(addr as *mut u8, 0, size);
            crate::eh::note_guest_write("unified memory zeroed (decommit)", addr as usize, size);
        }
        return 1;
    }
    // (a call Windows takes: a release names the whole reservation, a decommit a range or – size
    // 0 – the reservation from its start; the size of a reservation is what XCL recorded of it)
    let (release, decommit) = (t & mem::MEM_RELEASE != 0, t & mem::MEM_DECOMMIT != 0);
    if release != decommit && !(release && size != 0) {
        let len = if size != 0 { size } else { crate::memmap::reservation(addr as usize).filter(|&(start, _)| start == addr as usize).map_or(0, |(start, end)| end - start) };
        physical::title_freed(addr as usize, len, "VirtualFree");
    }
    // A release gives the address back to Windows, and the next allocation of any thread may be
    // there at once: XCL's records of the memory go first. Taken out after the call, they were
    // those of an allocation another thread had made there in between – its reservation was
    // forgotten, and with it that its memory is the GPU's to see. Only for the release Windows
    // takes of a reservation (no size, no other flag): a call it refuses for its form changes
    // nothing, and nothing of XCL's either. (Should it refuse this one, all is put back as it
    // was, what is committed in the reservation included: nothing was released then.)
    let records = (t == mem::MEM_RELEASE && size == 0).then(|| {
        let graphics = GRAPHICS_RESERVATIONS.lock().unwrap().remove(&(addr as usize));
        if graphics.is_some() {
            GRAPHICS_RESERVATIONS_GEN.fetch_add(1, Ordering::Release);
        }
        (graphics, crate::memmap::take(addr as usize))
    });
    let ok = mem::VirtualFree(addr, size, t);
    match records {
        Some((graphics, reservation)) if ok == 0 => {
            let e = fnd::GetLastError();
            if let Some(bytes) = graphics {
                note_graphics_reservation(addr as usize, bytes);
            }
            if let Some(taken) = reservation {
                crate::memmap::restore(taken);
            }
            fnd::SetLastError(e);
        }
        Some(_) => {}
        // (a release in another form that Windows took – of a placeholder, whose address stays
        // the title's: forgotten after it, as ever)
        None if ok != 0 && release => {
            if GRAPHICS_RESERVATIONS.lock().unwrap().remove(&(addr as usize)).is_some() {
                GRAPHICS_RESERVATIONS_GEN.fetch_add(1, Ordering::Release);
            }
            crate::memmap::released(addr as usize);
        }
        None if ok != 0 && size == 0 => {
            if let Some((_, end)) = crate::memmap::reservation(addr as usize) {
                crate::memmap::decommitted(addr as usize, end - addr as usize);
            }
        }
        None if ok != 0 => crate::memmap::decommitted(addr as usize, size),
        None => {}
    }
    ok
}

pub unsafe extern "system" fn VirtualFreeEx(proc_: HANDLE, addr: *mut c_void, size: usize, t: u32) -> BOOL {
    // (the title's own memory whatever it is: XCL's records of it go with it)
    if proc_ == thr::GetCurrentProcess() {
        return VirtualFree(addr, size, t);
    }
    mem::VirtualFreeEx(proc_, addr, size, t)
}

pub unsafe extern "system" fn VirtualAllocEx(proc_: HANDLE, addr: *const c_void, size: usize, t: u32, p: u32) -> *mut c_void {
    mem::VirtualAllocEx(proc_, addr, size, sanitize_alloc_type(t), sanitize_protect(p))
}

pub unsafe extern "system" fn VirtualProtect(addr: *const c_void, size: usize, p: u32, old: *mut u32) -> BOOL {
    let ok = mem::VirtualProtect(addr, size, sanitize_protect(p), old);
    if ok != 0 {
        crate::memmap::protected(addr as usize, size, sanitize_protect(p));
    }
    ok
}

pub unsafe extern "system" fn VirtualProtectEx(proc_: HANDLE, addr: *const c_void, size: usize, p: u32, old: *mut u32) -> BOOL {
    let ok = mem::VirtualProtectEx(proc_, addr, size, sanitize_protect(p), old);
    if ok != 0 && proc_ == thr::GetCurrentProcess() {
        crate::memmap::protected(addr as usize, size, sanitize_protect(p));
    }
    ok
}

/// XALLOC_ATTRIBUTES bit fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct XAllocAttrs {
    pub object_type: u32,
    pub page_size: u32,
    pub allocator_id: u32,
    pub alignment: u32,
    pub memory_type: u32,
}

impl XAllocAttrs {
    pub fn decode(a: u64) -> Self {
        XAllocAttrs {
            object_type: (a & 0x3FFF) as u32,
            page_size: ((a >> 14) & 0x3) as u32,
            allocator_id: ((a >> 16) & 0xFF) as u32,
            alignment: ((a >> 24) & 0x1F) as u32,
            memory_type: ((a >> 29) & 0xF) as u32,
        }
    }
}

/// Bytes the title holds of the process heap through XMemAlloc (its CPU memory types)
static XMEM_HEAP_BYTES: AtomicUsize = AtomicUsize::new(0);

pub fn xmem_heap_bytes() -> usize {
    XMEM_HEAP_BYTES.load(Ordering::Relaxed)
}

unsafe fn aligned_heap_alloc(size: usize, align: usize) -> *mut c_void {
    let Some(total) = size.checked_add(align).and_then(|n| n.checked_add(16)) else {
        fnd::SetLastError(fnd::ERROR_NOT_ENOUGH_MEMORY);
        return std::ptr::null_mut();
    };
    let raw = mem::HeapAlloc(mem::GetProcessHeap(), mem::HEAP_ZERO_MEMORY, total) as usize;
    if raw == 0 {
        fnd::SetLastError(fnd::ERROR_NOT_ENOUGH_MEMORY);
        return std::ptr::null_mut();
    }
    XMEM_HEAP_BYTES.fetch_add(total, Ordering::Relaxed);
    let aligned = (raw + 16 + align - 1) & !(align - 1);
    *((aligned - 8) as *mut usize) = raw;
    aligned as *mut c_void
}

unsafe fn aligned_heap_free(p: *mut c_void) {
    if p.is_null() {
        return;
    }
    let raw = *((p as usize - 8) as *const usize);
    XMEM_HEAP_BYTES.fetch_sub(mem::HeapSize(mem::GetProcessHeap(), 0, raw as *const c_void), Ordering::Relaxed);
    mem::HeapFree(mem::GetProcessHeap(), 0, raw as *const c_void);
}

/// Rounded byte size and alignment for an XMem graphics allocation honoring `page_size`
/// (`2` = `XALLOC_PAGESIZE_4MB`, otherwise 64 KiB sparse tile grain) and `alignment`.
fn xmem_graphics_layout(size: usize, a: XAllocAttrs) -> Option<(usize, usize)> {
    let page_grain = match a.page_size {
        2 => 4 << 20, // XALLOC_PAGESIZE_4MB
        _ => crate::gpu::uma::GRAIN,
    };
    let align = (1usize << a.alignment.min(usize::BITS - 2)).max(page_grain);
    let rounded = size.max(1).checked_next_multiple_of(page_grain)?;
    Some((rounded, align))
}

pub unsafe extern "system" fn XMemAllocDefault(size: usize, attrs: u64) -> *mut c_void {
    let a = XAllocAttrs::decode(attrs);
    if a.memory_type == 0 {
        let q = aligned_heap_alloc(size, 1usize << a.alignment.clamp(4, usize::BITS - 2));
        trace_mem("XMemAlloc", 0, size, attrs, 0, q as usize);
        return q;
    }
    let Some((rounded, align)) = xmem_graphics_layout(size, a) else {
        fnd::SetLastError(fnd::ERROR_NOT_ENOUGH_MEMORY);
        return std::ptr::null_mut();
    };
    // Graphics memory types live in the sparse VRAM pool (fallback to coherent UMA, then Windows).
    if let Some((q, _)) = crate::hle::d3d12x::sparse::alloc(rounded, align) {
        trace_mem("XMemAlloc (graphics, sparse VRAM)", 0, rounded, attrs, 0, q);
        return q as *mut c_void;
    }
    if let Some(q) = crate::gpu::uma::get().and_then(|u| u.alloc(rounded, align)) {
        trace_mem("XMemAlloc (graphics, UMA)", 0, rounded, attrs, 0, q as usize);
        return q as *mut c_void;
    }
    let q = mem::VirtualAlloc(std::ptr::null(), rounded, mem::MEM_COMMIT | mem::MEM_RESERVE | MEM_WRITE_WATCH, mem::PAGE_READWRITE);
    if !q.is_null() {
        note_graphics_reservation(q as usize, rounded);
        crate::memmap::reserved(q as usize, rounded);
        crate::memmap::committed(q as usize, rounded, mem::PAGE_READWRITE);
    }
    q
}

pub unsafe extern "system" fn XMemFreeDefault(p: *mut c_void, attrs: u64) {
    if p.is_null() {
        return;
    }
    if XAllocAttrs::decode(attrs).memory_type == 0 {
        aligned_heap_free(p);
        return;
    }
    let a = p as usize;
    let uma = crate::gpu::uma::get();
    let len = uma
        .and_then(|u| u.sparse_reservation(a).filter(|&(start, _)| start == a).map(|(_, len)| len).or_else(|| u.block_size(a)))
        .or_else(|| crate::memmap::reservation(a).filter(|&(start, _)| start == a).map(|(start, end)| end - start));
    physical::title_freed(a, len.unwrap_or(0), "XMemFree");
    if !crate::hle::d3d12x::sparse::free(p as usize) && !crate::gpu::uma::get().is_some_and(|u| u.free(p as usize)) {
        // (XCL's records first: once released, the address may be another thread's, `VirtualFree`)
        crate::memmap::released(p as usize);
        if GRAPHICS_RESERVATIONS.lock().unwrap().remove(&(p as usize)).is_some() {
            GRAPHICS_RESERVATIONS_GEN.fetch_add(1, Ordering::Release);
        }
        mem::VirtualFree(p, 0, mem::MEM_RELEASE);
    }
}

type XMemAllocFn = unsafe extern "system" fn(usize, u64) -> *mut c_void;
type XMemFreeFn = unsafe extern "system" fn(*mut c_void, u64);
static XMEM_ALLOC_HOOK: AtomicUsize = AtomicUsize::new(0);
static XMEM_FREE_HOOK: AtomicUsize = AtomicUsize::new(0);

pub unsafe extern "system" fn XMemAlloc(size: usize, attrs: u64) -> *mut c_void {
    match XMEM_ALLOC_HOOK.load(Ordering::Acquire) {
        0 => XMemAllocDefault(size, attrs),
        f => std::mem::transmute::<usize, XMemAllocFn>(f)(size, attrs),
    }
}

pub unsafe extern "system" fn XMemFree(p: *mut c_void, attrs: u64) {
    match XMEM_FREE_HOOK.load(Ordering::Acquire) {
        0 => XMemFreeDefault(p, attrs),
        f => std::mem::transmute::<usize, XMemFreeFn>(f)(p, attrs),
    }
}

/// The exe's exported data `__XMemAllocationHooks__` {alloc, free, …}: the allocator functions
/// of a title that replaces XMem's, which the console's runtime libraries call for the memory
/// they need (`runtime_alloc`). Titles see there what the runtime allocates – and may supply the
/// memory themselves.
static TITLE_HOOKS: [AtomicUsize; 2] = [const { AtomicUsize::new(0) }; 2];

pub unsafe fn adopt_title_hooks(main: &crate::modules::MappedModule) {
    let Some(crate::pe::ExportTarget::Rva(rva)) = main.exports.by_name.get("__XMemAllocationHooks__") else { return };
    let p = (main.base + *rva as usize) as *const usize;
    let (alloc, free) = (p.read_unaligned(), p.add(1).read_unaligned());
    let inside = |f: usize| f > main.base && f < main.base + main.size;
    if inside(alloc) && inside(free) {
        TITLE_HOOKS[1].store(free, Ordering::Release);
        TITLE_HOOKS[0].store(alloc, Ordering::Release);
        crate::xlog_info!("XMem: the title has allocation hooks ({}, {})", crate::modules::describe_address(alloc), crate::modules::describe_address(free));
    }
}

/// XALLOC_ATTRIBUTES of a descriptor heap's memory: object type 4, 64 KiB pages and alignment,
/// allocator 0x80 (the D3D runtime), graphics memory. The retail game's heap class (its alloc hook for
/// allocator 0x80 compares object type and size) gives the memory of its heaps this way and
/// asserts that they start there.
pub const D3D_DESCRIPTOR_HEAP: u64 = 4 | 1 << 14 | 0x80 << 16 | 16 << 24 | 1 << 29;

/// XALLOC_ATTRIBUTES of a display target's memory: 4 MiB pages (the retail game's hook for its back buffers
/// takes every request with them), allocator 0x80, graphics memory.
pub const D3D_DISPLAY_TARGET: u64 = 2 << 14 | 0x80 << 16 | 16 << 24 | 1 << 29;

/// What the title's allocation hook gives a console runtime library (None: no hook, or null).
pub unsafe fn runtime_alloc(size: usize, attrs: u64) -> Option<usize> {
    let f = TITLE_HOOKS[0].load(Ordering::Acquire);
    if f == 0 {
        return None;
    }
    let p = std::mem::transmute::<usize, XMemAllocFn>(f)(size, attrs) as usize;
    (p != 0).then_some(p)
}

pub unsafe fn runtime_free(p: usize, attrs: u64) {
    let f = TITLE_HOOKS[1].load(Ordering::Acquire);
    if f != 0 {
        std::mem::transmute::<usize, XMemFreeFn>(f)(p as *mut c_void, attrs);
    }
}

/// `void* XMemVirtualAlloc(void* address, SIZE_T size, DWORD type, XALLOC_ATTRIBUTES attributes,
/// DWORD protect)`: VirtualAlloc with an allocator's attributes – a graphics memory type is
/// GPU-visible memory (as MEM_GRAPHICS: sparse VRAM pool).
pub unsafe extern "system" fn XMemVirtualAlloc(addr: *const c_void, size: usize, t: u32, attrs: u64, p: u32) -> *mut c_void {
    if let Some(q) = physical::gdk_reserve(addr, size, t, attrs, p) {
        if !q.is_null() {
            mirrors::reserved(q as usize, size, t, p);
        }
        return q;
    }
    let a = XAllocAttrs::decode(attrs);
    let gdk = (t & MEM_64K_PAGES == MEM_64K_PAGES).then_some(physical::RawGdk64 {
        requested: addr as usize, requested_size: size, allocation: t, flags: attrs, protection: p,
    });
    let mut t = if a.memory_type != 0 { t | MEM_GRAPHICS } else { t };
    if a.memory_type != 0 && a.page_size == 2 {
        t |= 0x8000_0000;
    }
    virtual_alloc(addr, size, t, p, gdk)
}

/// `BOOL XMemVirtualQuery(HANDLE process, const void* address, XMEM_BASIC_INFORMATION* info)`
/// (signature from XboxBvhBuilder.dll's call sites): MEMORY_BASIC_INFORMATION plus a field
/// at +48 whose bits 0-1 the caller requires for "CPU-readable memory" – set to 1 (CPU
/// cached) for committed readable memory. The caller zeroes the rest.
pub unsafe extern "system" fn XMemVirtualQuery(process: HANDLE, address: *const c_void, info: *mut c_void) -> BOOL {
    if info.is_null() {
        fnd::SetLastError(fnd::ERROR_INVALID_PARAMETER);
        return 0;
    }
    let mbi = info as *mut mem::MEMORY_BASIC_INFORMATION;
    if mem::VirtualQueryEx(process, address, mbi, std::mem::size_of::<mem::MEMORY_BASIC_INFORMATION>()) == 0 {
        return 0;
    }
    const READABLE: u32 = mem::PAGE_READONLY
        | mem::PAGE_READWRITE
        | mem::PAGE_WRITECOPY
        | mem::PAGE_EXECUTE_READ
        | mem::PAGE_EXECUTE_READWRITE
        | mem::PAGE_EXECUTE_WRITECOPY;
    let protect = (*mbi).Protect;
    let cpu = (*mbi).State == mem::MEM_COMMIT && (protect & mem::PAGE_GUARD) == 0 && (protect & READABLE) != 0;
    *(info as *mut u8).add(48).cast::<u32>() = cpu as u32;
    1
}

/// `BOOL XMemGetWorkingSetStatistics(UINT32 allocator, void* stats)` (as FSR's upscaler calls
/// it: allocator 0x20000001, 128-byte buffer, reads +8 only on success). No statistics:
/// FALSE, which the caller reports as 0 bytes.
/// `BOOL XMemGetWorkingSetStatistics(DWORD flags, XMEM_WORKING_SET_STATISTICS* stats)`: +0 the
/// title's memory limit, +8 what it uses – engines size their pools and texture streaming from
/// these (Fortnite: flags 0x10000001). Only those two are written: the structure's size
/// differs between GDK versions (Fortnite's exe passes 0x78 bytes, its EOSSDK – older – has
/// at most 0x50 bytes before its stack cookie, which zeroing 0x78 bytes destroyed: the DLL's
/// static initializer failed its cookie check, 0xC0000409).
pub unsafe extern "system" fn XMemGetWorkingSetStatistics(_flags: u32, stats: *mut u8) -> BOOL {
    if stats.is_null() {
        fnd::SetLastError(fnd::ERROR_INVALID_PARAMETER);
        return 0;
    }
    let limit = crate::state::config().title_memory_budget_mb() * 1024 * 1024;
    (stats as *mut u64).write_unaligned(limit);
    (stats.add(8) as *mut u64).write_unaligned(crate::util::process_commit_bytes().min(limit));
    1
}

/// `BOOL XMemTransferMemory(direction, size)`: moving budget between the title and the tools
/// pools – there is one budget here
pub unsafe extern "system" fn XMemTransferMemory(_direction: u32, _size: usize) -> BOOL {
    1
}

/// The title's debug output (console builds print a lot there) goes to XCL's log as well.
pub unsafe extern "system" fn OutputDebugStringA(s: *const u8) {
    if let Some(m) = from_ansi_ptr(s) {
        crate::xlog_info!("[debug] {}", m.trim_end());
    }
}

pub unsafe extern "system" fn OutputDebugStringW(s: *const u16) {
    if let Some(m) = from_wide_ptr(s) {
        crate::xlog_info!("[debug] {}", m.trim_end());
    }
}

pub unsafe extern "system" fn XMemSetAllocationHooks(alloc: usize, free: usize) {
    if alloc == 0 {
        XMEM_ALLOC_HOOK.store(0, Ordering::Release);
        XMEM_FREE_HOOK.store(0, Ordering::Release);
    } else {
        XMEM_ALLOC_HOOK.store(alloc, Ordering::Release);
        XMEM_FREE_HOOK.store(free, Ordering::Release);
    }
}

#[repr(C)]
pub struct TitleMemoryStatusT {
    pub dw_length: u32,
    pub dw_reserved: u32,
    pub total: u64,
    pub avail: u64,
    pub legacy_used: u64,
    pub legacy_peak: u64,
    pub legacy_avail: u64,
    pub title_used: u64,
    pub title_avail: u64,
    pub legacy_page_table_used: u64,
    pub title_page_table_used: u64,
}

pub unsafe extern "system" fn TitleMemoryStatus(buf: *mut TitleMemoryStatusT) -> BOOL {
    if buf.is_null() || (*buf).dw_length < std::mem::size_of::<TitleMemoryStatusT>() as u32 {
        fnd::SetLastError(fnd::ERROR_INVALID_PARAMETER);
        return 0;
    }
    let total = crate::state::config().title_memory_budget_mb() * 1024 * 1024;
    let used = crate::util::process_commit_bytes().min(total);
    let b = &mut *buf;
    b.total = total;
    b.avail = total - used;
    b.legacy_used = 0;
    b.legacy_peak = 0;
    b.legacy_avail = 0;
    b.title_used = used;
    b.title_avail = total - used;
    b.legacy_page_table_used = 0;
    b.title_page_table_used = 0;
    1
}

pub unsafe extern "system" fn JobTitleMemoryStatus(buf: *mut TitleMemoryStatusT) -> BOOL {
    TitleMemoryStatus(buf)
}

pub unsafe extern "system" fn ToolingMemoryStatus(buf: *mut u32) -> BOOL {
    if buf.is_null() || *buf < 40 {
        fnd::SetLastError(fnd::ERROR_INVALID_PARAMETER);
        return 0;
    }
    std::ptr::write_bytes((buf as *mut u8).add(8), 0, 32);
    1
}

pub extern "system" fn XMemIsToolsMemAvailable() -> BOOL {
    0
}

mod physical;
pub use physical::*;

// ---------------------------------------------------------------------------------------
// App state (minappmodelx.dll, appnotify.h)
// ---------------------------------------------------------------------------------------

/// The registration handed out: the callbacks are never called (XCL does not suspend titles
/// or constrain them for the system UI).
static APP_NOTIFICATION: u8 = 0;

/// `ULONG RegisterAppStateChangeNotification(routine, context, PAPPSTATE_REGISTRATION* out)`
pub unsafe extern "system" fn RegisterAppStateChangeNotification(_routine: usize, _ctx: *mut c_void, out: *mut *const u8) -> u32 {
    if out.is_null() {
        return fnd::ERROR_INVALID_PARAMETER;
    }
    *out = &APP_NOTIFICATION;
    0
}

pub unsafe extern "system" fn UnregisterAppStateChangeNotification(_registration: *const u8) {}

/// `ULONG RegisterAppConstrainedChangeNotification(routine, context, PAPPCONSTRAIN_REGISTRATION* out)`
pub unsafe extern "system" fn RegisterAppConstrainedChangeNotification(routine: usize, ctx: *mut c_void, out: *mut *const u8) -> u32 {
    RegisterAppStateChangeNotification(routine, ctx, out)
}

pub unsafe extern "system" fn UnregisterAppConstrainedChangeNotification(_registration: *const u8) {}

// ---------------------------------------------------------------------------------------
// Threads / system info
// ---------------------------------------------------------------------------------------

fn hresult_to_win32(hr: i32) -> u32 {
    if (hr as u32 & 0xFFFF_0000) == 0x8007_0000 {
        hr as u32 & 0xFFFF
    } else {
        fnd::ERROR_INVALID_PARAMETER
    }
}

/// a sleep is taken in steps of at most this, so that time held from then on is not counted
const SLEEP_STEP_MS: u32 = 20;
/// how often a sleep looks whether the title is still held
const SLEEP_HELD_POLL_MS: u32 = 5;
/// the shortest sleep that is a wait for time: a shorter one is a poll and is passed on as it is
const SLEEP_HOLD_MIN_MS: u32 = 100;

/// Sleep(ms): `ms` of the title's time. The time XCL holds one of the title's threads in a bind
/// for a pipeline still being built (`pso_async::title_held`) is not the title's: a console
/// title never waits there. A retail title's own stall check – a thread that ticks once a second
/// with Sleep and ends the process after 30 ticks without a sign of its main thread – ended it
/// while the driver built the pipelines of its world (#505, #232). Only a sleep of
/// `SLEEP_HOLD_MIN_MS` or more is taken so: a thread that polls with short sleeps (streaming,
/// sound) goes on while a bind waits. `XCL_SLEEP_HOLD=<ms>` sets that length, `0`: the PC's
/// Sleep as it is.
pub unsafe extern "system" fn Sleep(ms: u32) {
    static MIN: std::sync::OnceLock<Option<u32>> = std::sync::OnceLock::new();
    let Some(min) = *MIN.get_or_init(|| {
        let min = sleep_hold_min(std::env::var("XCL_SLEEP_HOLD").ok().as_deref());
        match min {
            Some(min) => crate::xlog_info!("kernelx: a Sleep of {min} ms or more does not count the time a bind waits for a pipeline build (XCL_SLEEP_HOLD)"),
            None => crate::xlog_info!("kernelx: Sleep is the PC's as it is (XCL_SLEEP_HOLD=0)"),
        }
        min
    }) else {
        return thr::Sleep(ms);
    };
    title_sleep(ms, min, |ms| thr::Sleep(ms), crate::hle::d3d12x::pso_async::title_held)
}

/// `XCL_SLEEP_HOLD`: the shortest sleep whose held time is not counted (`None`: no sleep).
fn sleep_hold_min(v: Option<&str>) -> Option<u32> {
    match v.map(str::trim) {
        None => Some(SLEEP_HOLD_MIN_MS),
        Some("0") => None,
        Some(v) => Some(v.parse().unwrap_or(SLEEP_HOLD_MIN_MS)),
    }
}

/// `ms` of time in which the title was not `held`, slept through `sleep` (a yield, an endless
/// sleep and one shorter than `min` as they are).
fn title_sleep(ms: u32, min: u32, sleep: impl Fn(u32), held: impl Fn() -> bool) {
    if ms == 0 || ms == thr::INFINITE || ms < min {
        return sleep(ms);
    }
    let mut left = std::time::Duration::from_millis(ms as u64);
    while !left.is_zero() {
        if held() {
            sleep(SLEEP_HELD_POLL_MS);
            continue;
        }
        let t = std::time::Instant::now();
        sleep((left.as_millis() as u32).clamp(1, SLEEP_STEP_MS));
        left = left.saturating_sub(t.elapsed());
    }
}

pub unsafe extern "system" fn SetThreadName(h: HANDLE, name: *const u16) -> BOOL {
    if let Some(n) = from_wide_ptr(name) {
        crate::xlog_debug!("SetThreadName({n})");
    }
    let hr = thr::SetThreadDescription(h, name);
    if hr < 0 {
        fnd::SetLastError(hresult_to_win32(hr));
        return 0;
    }
    1
}

pub unsafe extern "system" fn GetThreadName(h: HANDLE, buf: *mut u16, len: usize, ret_len: *mut usize) -> BOOL {
    if buf.is_null() && len > 0 {
        fnd::SetLastError(fnd::ERROR_INVALID_PARAMETER);
        return 0;
    }
    let mut p: *mut u16 = std::ptr::null_mut();
    let hr = thr::GetThreadDescription(h, &mut p);
    if hr < 0 || p.is_null() {
        fnd::SetLastError(hresult_to_win32(hr));
        return 0;
    }
    let s = from_wide_ptr(p).unwrap_or_default();
    fnd::LocalFree(p as *mut c_void);
    let n = s.encode_utf16().count();
    if !ret_len.is_null() {
        *ret_len = n;
    }
    if buf.is_null() || len <= n {
        fnd::SetLastError(fnd::ERROR_INSUFFICIENT_BUFFER);
        return 0;
    }
    write_wide_out(&s, buf, len.min(u32::MAX as usize) as u32);
    1
}

pub unsafe extern "system" fn SetThreadpoolAffinityMask(_pool: *mut c_void, _mask: usize) -> BOOL {
    1
}

/// An affinity mask the PC takes. The title sees the PC's processors, so a mask made from them
/// passes unchanged. A console mask (its 7 cores / 14 threads) that names processors outside the
/// process's affinity – a PC with fewer processors, or a process limited to some – makes Windows
/// fail the call (ERROR_INVALID_PARAMETER) when none of its processors is there or any is outside
/// the process's mask: it is cut to the processors there are, and when none is left, each console
/// processor goes to the PC's processor at its index modulo their count.
pub fn fit_affinity_mask(mask: usize, allowed: usize) -> usize {
    if mask == 0 || allowed == 0 || mask & !allowed == 0 {
        return mask;
    }
    if mask & allowed != 0 {
        return mask & allowed;
    }
    let pcs: Vec<u32> = (0..usize::BITS).filter(|b| allowed >> b & 1 != 0).collect();
    (0..usize::BITS).filter(|b| mask >> b & 1 != 0).fold(0, |m, b| m | 1 << pcs[b as usize % pcs.len()])
}

/// The process's affinity mask; 0 (masks pass unchanged) with more than one processor group.
fn process_affinity() -> usize {
    unsafe {
        if thr::GetActiveProcessorGroupCount() != 1 {
            return 0;
        }
        let (mut process, mut system) = (0usize, 0usize);
        if thr::GetProcessAffinityMask(thr::GetCurrentProcess(), &mut process, &mut system) == 0 {
            return 0;
        }
        process
    }
}

fn fitted_affinity(mask: usize, what: &str) -> usize {
    static LOGGED: std::sync::Mutex<std::collections::BTreeSet<usize>> = std::sync::Mutex::new(std::collections::BTreeSet::new());
    let allowed = process_affinity();
    let fit = fit_affinity_mask(mask, allowed);
    if fit != mask && LOGGED.lock().unwrap_or_else(|e| e.into_inner()).insert(mask) {
        crate::xlog_info!("{what}: mask {mask:#x} names processors outside the process's {allowed:#x} – {fit:#x}");
    }
    fit
}

pub unsafe extern "system" fn SetThreadAffinityMask(h: HANDLE, mask: usize) -> usize {
    thr::SetThreadAffinityMask(h, fitted_affinity(mask, "SetThreadAffinityMask"))
}

pub unsafe extern "system" fn SetThreadGroupAffinity(h: HANDLE, a: *const GROUP_AFFINITY, prev: *mut GROUP_AFFINITY) -> BOOL {
    if a.is_null() || (*a).Group != 0 {
        return thr::SetThreadGroupAffinity(h, a, prev);
    }
    let mut fit = *a;
    fit.Mask = fitted_affinity(fit.Mask, "SetThreadGroupAffinity");
    thr::SetThreadGroupAffinity(h, &fit, prev)
}

pub unsafe extern "system" fn QueryProcessorSchedulingStatistics(out: *mut [u64; 3]) {
    if out.is_null() {
        return;
    }
    let mut idle = std::mem::zeroed::<fnd::FILETIME>();
    let mut kern = std::mem::zeroed::<fnd::FILETIME>();
    let mut user = std::mem::zeroed::<fnd::FILETIME>();
    thr::GetSystemTimes(&mut idle, &mut kern, &mut user);
    let ft = |f: fnd::FILETIME| ((f.dwHighDateTime as u64) << 32) | f.dwLowDateTime as u64;
    let (i, k, u) = (ft(idle), ft(kern), ft(user));
    *out = [(k + u).saturating_sub(i), i, k + u];
}

pub unsafe extern "system" fn GetConsoleType() -> u32 {
    crate::state::config().console_type_raw
}

#[repr(C)]
pub struct SystemOsVersionInfo {
    pub major: u8,
    pub minor: u8,
    pub build: u16,
    pub revision: u16,
}

#[repr(C)]
struct OsVersionInfoW {
    size: u32,
    major: u32,
    minor: u32,
    build: u32,
    platform: u32,
    csd: [u16; 128],
}

#[link(name = "ntdll", kind = "raw-dylib")]
extern "system" {
    fn RtlGetVersion(info: *mut OsVersionInfoW) -> i32;
    fn NtQueryTimerResolution(coarsest: *mut u32, finest: *mut u32, current: *mut u32) -> i32;
    fn NtSetTimerResolution(desired: u32, set: u8, current: *mut u32) -> i32;
}

pub fn host_os_version() -> (u32, u32, u32) {
    unsafe {
        let mut v: OsVersionInfoW = std::mem::zeroed();
        v.size = std::mem::size_of::<OsVersionInfoW>() as u32;
        RtlGetVersion(&mut v);
        (v.major, v.minor, v.build)
    }
}

/// The finest timer Windows offers (0.5 ms) for the title: with the default 15.6 ms tick every
/// sleep rounds up (a retail game's audio mixer ran at a third of real time). Windows 11 keeps
/// it while the window is hidden only when told to.
pub fn fine_timer() {
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, ProcessPowerThrottling, SetProcessInformation, PROCESS_POWER_THROTTLING_CURRENT_VERSION,
        PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION, PROCESS_POWER_THROTTLING_STATE,
    };
    let state = PROCESS_POWER_THROTTLING_STATE {
        Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
        ControlMask: PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION,
        StateMask: 0,
    };
    let size = std::mem::size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32;
    let kept = unsafe { SetProcessInformation(GetCurrentProcess(), ProcessPowerThrottling, &state as *const _ as *const c_void, size) } != 0;
    // in 100 ns units; "finest" is the smallest period the system supports
    let (mut coarsest, mut finest, mut was) = (0u32, 0u32, 0u32);
    if unsafe { NtQueryTimerResolution(&mut coarsest, &mut finest, &mut was) } < 0 || finest == 0 {
        crate::xlog_warn!("timer: NtQueryTimerResolution failed – Windows' default resolution");
        return;
    }
    let mut now = 0u32;
    let status = unsafe { NtSetTimerResolution(finest, 1, &mut now) };
    crate::xlog_info!(
        "timer: {} (was {:.2} ms, now {:.2} ms; kept while the window is hidden: {})",
        if status >= 0 { "finest resolution for the title" } else { "raising the resolution failed" },
        was as f64 / 10_000.0,
        now as f64 / 10_000.0,
        if kept { "yes" } else { "no" }
    );
}

pub unsafe extern "system" fn GetSystemOSVersion(out: *mut SystemOsVersionInfo) {
    if out.is_null() {
        return;
    }
    let (major, minor, build) = host_os_version();
    *out = SystemOsVersionInfo { major: major as u8, minor: minor as u8, build: build as u16, revision: 0 };
}

// ---------------------------------------------------------------------------------------
// Registry: the console mode
// ---------------------------------------------------------------------------------------
//
// The GDK thunks a title links compute XSystemGetDeviceType from
// HKLM\Software\Microsoft\Windows NT\CurrentVersion\OEM, value ConsoleMode (REG_DWORD):
// 0x64 Series X (XboxScarlettAnaconda), 0x60 Series S (Lockhart), 0x68 devkit, 0x40 / 0x50 /
// 0x58 Xbox One S / X / devkit, no key = PC – read from the thunk in the game's XCurl.dll.
// Only that key is answered here; everything else goes to the real registry.

const OEM_KEY: &str = "software\\microsoft\\windows nt\\currentversion\\oem";
/// handle of the answered key (never reaches the real registry)
const OEM_HANDLE: usize = 0x5843_4C4F; // "XCLO"

fn console_mode() -> u32 {
    if crate::state::config().is_series_s() {
        0x60
    } else {
        0x64
    }
}

unsafe fn is_oem_key(root: usize, path: *const u16) -> bool {
    let hklm = windows_sys::Win32::System::Registry::HKEY_LOCAL_MACHINE as usize;
    root == hklm && from_wide_ptr(path).is_some_and(|p| p.trim_start_matches('\\').eq_ignore_ascii_case(OEM_KEY))
}

pub unsafe extern "system" fn RegOpenKeyExW(root: usize, path: *const u16, options: u32, sam: u32, out: *mut usize) -> u32 {
    if is_oem_key(root, path) && !out.is_null() {
        crate::xlog_debug!("registry: OEM key → console mode {:#x}", console_mode());
        *out = OEM_HANDLE;
        return 0;
    }
    windows_sys::Win32::System::Registry::RegOpenKeyExW(root as _, path, options, sam, out as _)
}

pub unsafe extern "system" fn RegOpenKeyW(root: usize, path: *const u16, out: *mut usize) -> u32 {
    if is_oem_key(root, path) && !out.is_null() {
        *out = OEM_HANDLE;
        return 0;
    }
    windows_sys::Win32::System::Registry::RegOpenKeyW(root as _, path, out as _)
}

pub unsafe extern "system" fn RegQueryValueExW(key: usize, name: *const u16, reserved: *mut u32, ty: *mut u32, data: *mut u8, size: *mut u32) -> u32 {
    if key != OEM_HANDLE {
        return windows_sys::Win32::System::Registry::RegQueryValueExW(key as _, name, reserved, ty, data, size);
    }
    const ERROR_FILE_NOT_FOUND: u32 = 2;
    const ERROR_MORE_DATA: u32 = 234;
    if !from_wide_ptr(name).is_some_and(|n| n.eq_ignore_ascii_case("ConsoleMode")) {
        return ERROR_FILE_NOT_FOUND;
    }
    if !ty.is_null() {
        *ty = 4; // REG_DWORD
    }
    if !data.is_null() {
        if size.is_null() || *size < 4 {
            if !size.is_null() {
                *size = 4;
            }
            return ERROR_MORE_DATA;
        }
        std::ptr::copy_nonoverlapping(console_mode().to_le_bytes().as_ptr(), data, 4);
    }
    if !size.is_null() {
        *size = 4;
    }
    0
}

pub unsafe extern "system" fn RegCloseKey(key: usize) -> u32 {
    if key == OEM_HANDLE {
        0
    } else {
        windows_sys::Win32::System::Registry::RegCloseKey(key as _)
    }
}

// ---------------------------------------------------------------------------------------
// Modules
// ---------------------------------------------------------------------------------------

const LOAD_AS_DATA: u32 = 0x2 | 0x20 | 0x40; // DATAFILE | IMAGE_RESOURCE | DATAFILE_EXCLUSIVE

fn load_library_impl(name: Option<String>) -> usize {
    let Some(name) = name else {
        unsafe { fnd::SetLastError(fnd::ERROR_INVALID_PARAMETER) };
        return 0;
    };
    match resolver::load_library(&name) {
        Some(h) => {
            crate::xlog_debug!("LoadLibrary({name}) → {h:#x}");
            h
        }
        None => {
            crate::xlog_warn!("LoadLibrary({name}) → NULL");
            unsafe { fnd::SetLastError(fnd::ERROR_MOD_NOT_FOUND) };
            0
        }
    }
}

pub unsafe extern "system" fn LoadLibraryW(name: *const u16) -> usize {
    load_library_impl(from_wide_ptr(name))
}

pub unsafe extern "system" fn LoadLibraryA(name: *const u8) -> usize {
    load_library_impl(from_ansi_ptr(name))
}

unsafe fn load_as_data(name: &str, file: HANDLE, flags: u32) -> usize {
    let real = crate::vfs::get().and_then(|v| v.real_path_for_read(name)).unwrap_or_else(|| name.to_string());
    ll::LoadLibraryExW(crate::util::wide(&real).as_ptr(), file, flags) as usize
}

pub unsafe extern "system" fn LoadLibraryExW(name: *const u16, file: HANDLE, flags: u32) -> usize {
    if flags & LOAD_AS_DATA != 0 {
        let Some(name) = from_wide_ptr(name) else {
            fnd::SetLastError(fnd::ERROR_INVALID_PARAMETER);
            return 0;
        };
        return load_as_data(&name, file, flags);
    }
    load_library_impl(from_wide_ptr(name))
}

pub unsafe extern "system" fn LoadLibraryExA(name: *const u8, file: HANDLE, flags: u32) -> usize {
    if flags & LOAD_AS_DATA != 0 {
        let Some(name) = from_ansi_ptr(name) else {
            fnd::SetLastError(fnd::ERROR_INVALID_PARAMETER);
            return 0;
        };
        return load_as_data(&name, file, flags);
    }
    load_library_impl(from_ansi_ptr(name))
}

pub unsafe extern "system" fn LoadPackagedLibrary(name: *const u16, _reserved: u32) -> usize {
    load_library_impl(from_wide_ptr(name))
}

pub unsafe extern "system" fn GetProcAddress(h: usize, name: *const u8) -> usize {
    if name.is_null() {
        fnd::SetLastError(fnd::ERROR_INVALID_PARAMETER);
        return 0;
    }
    let what = if (name as usize) >> 16 == 0 {
        ImportName::Ordinal(name as usize as u16)
    } else {
        ImportName::Name { hint: 0, name: crate::util::from_cstr_ptr(name).unwrap_or_default() }
    };
    let h = if h == 0 { modules::main_module().map(|m| m.base).unwrap_or(0) } else { h };
    match resolver::get_proc_address(h, &what) {
        Some(a) => a,
        None => {
            fnd::SetLastError(fnd::ERROR_PROC_NOT_FOUND);
            0
        }
    }
}

fn module_handle_impl(name: &str) -> usize {
    module_handle_in(crate::hle::registry(), name)
}

fn module_handle_in(reg: &crate::hle::Registry, name: &str) -> usize {
    let key = dll_key(name);
    match reg.route(&key) {
        Some(crate::hle::Route::Hle(i)) => {
            let m = &reg.modules[*i];
            if m.aliases_are_system() && key != m.name {
                // Real Windows DLLs (kernel32 / kernelbase / advapi32 / Windows API sets) keep
                // their genuine HMODULE; console-only aliases (advapi32legacy, kernel32legacy,
                // minappmodelx) fall back to the HLE pseudo handle, matching LoadLibrary.
                // SAFETY: `wide(name)` produces a valid NUL-terminated UTF-16 string.
                let real = unsafe { ll::GetModuleHandleW(crate::util::wide(name).as_ptr()) as usize };
                if real != 0 {
                    return real;
                }
            }
            return modules::hle_handle(m.name);
        }
        Some(crate::hle::Route::Stub) => {
            // Return the pseudo handle only if the @stub DLL has already been loaded or imported,
            // preserving GetModuleHandle's non-loading probe semantics for optional DLLs.
            return modules::find_hle_handle(&key).unwrap_or(0);
        }
        Some(crate::hle::Route::System) => {
            // SAFETY: `wide(name)` produces a valid NUL-terminated UTF-16 string.
            return unsafe { ll::GetModuleHandleW(crate::util::wide(name).as_ptr()) as usize };
        }
        None => {}
    }
    if let Some(m) = modules::find_mapped_by_name(&key) {
        return m.base;
    }
    // SAFETY: `wide(name)` produces a valid NUL-terminated UTF-16 string.
    unsafe { ll::GetModuleHandleW(crate::util::wide(name).as_ptr()) as usize }
}

fn main_or_real_base() -> usize {
    modules::main_module()
        .map(|m| m.base)
        .unwrap_or_else(|| unsafe { ll::GetModuleHandleW(std::ptr::null()) as usize })
}

pub unsafe extern "system" fn GetModuleHandleW(name: *const u16) -> usize {
    match from_wide_ptr(name) {
        None => main_or_real_base(),
        Some(n) => {
            let h = module_handle_impl(&n);
            if h == 0 {
                fnd::SetLastError(fnd::ERROR_MOD_NOT_FOUND);
            }
            h
        }
    }
}

pub unsafe extern "system" fn GetModuleHandleA(name: *const u8) -> usize {
    match from_ansi_ptr(name) {
        None => main_or_real_base(),
        Some(n) => {
            let h = module_handle_impl(&n);
            if h == 0 {
                fnd::SetLastError(fnd::ERROR_MOD_NOT_FOUND);
            }
            h
        }
    }
}

const FROM_ADDRESS: u32 = 0x4;

unsafe fn module_handle_ex(flags: u32, name: Option<String>, addr: usize, out: *mut usize) -> BOOL {
    if out.is_null() {
        fnd::SetLastError(fnd::ERROR_INVALID_PARAMETER);
        return 0;
    }
    let h = if flags & FROM_ADDRESS != 0 {
        match modules::image_base_for_addr(addr) {
            Some(b) => b,
            None => {
                let mut r = std::ptr::null_mut();
                if ll::GetModuleHandleExW(flags, addr as *const u16, &mut r) != 0 {
                    r as usize
                } else {
                    0
                }
            }
        }
    } else {
        match name {
            None => main_or_real_base(),
            Some(n) => module_handle_impl(&n),
        }
    };
    *out = h;
    if h == 0 {
        fnd::SetLastError(fnd::ERROR_MOD_NOT_FOUND);
        0
    } else {
        1
    }
}

pub unsafe extern "system" fn GetModuleHandleExW(flags: u32, name: *const u16, out: *mut usize) -> BOOL {
    let n = if flags & FROM_ADDRESS != 0 { None } else { from_wide_ptr(name) };
    module_handle_ex(flags, n, name as usize, out)
}

pub unsafe extern "system" fn GetModuleHandleExA(flags: u32, name: *const u8, out: *mut usize) -> BOOL {
    let n = if flags & FROM_ADDRESS != 0 { None } else { from_ansi_ptr(name) };
    module_handle_ex(flags, n, name as usize, out)
}

fn module_path_for(h: usize) -> Option<String> {
    let main = modules::main_module();
    if h == 0 || main.as_ref().map(|m| m.base) == Some(h) {
        return main.map(|m| m.path.to_string_lossy().into_owned());
    }
    if let Some(m) = modules::find_mapped_by_base(h) {
        return Some(m.path.to_string_lossy().into_owned());
    }
    modules::hle_name_by_handle(h).map(|n| format!("C:\\Windows\\System32\\{n}"))
}

pub unsafe extern "system" fn GetModuleFileNameW(h: usize, buf: *mut u16, size: u32) -> u32 {
    match module_path_for(h) {
        Some(p) => write_wide_out(&p, buf, size),
        None => ll::GetModuleFileNameW(h as _, buf, size),
    }
}

pub unsafe extern "system" fn GetModuleFileNameA(h: usize, buf: *mut u8, size: u32) -> u32 {
    match module_path_for(h) {
        Some(p) => write_ansi_out(&p, buf, size),
        None => ll::GetModuleFileNameA(h as _, buf, size),
    }
}

pub unsafe extern "system" fn GetModuleInformation(process: HANDLE, module: usize, info: *mut ps::MODULEINFO, size: u32) -> BOOL {
    K32GetModuleInformation(process, module, info, size)
}

pub unsafe extern "system" fn K32GetModuleInformation(process: HANDLE, module: usize, info: *mut ps::MODULEINFO, size: u32) -> BOOL {
    if process == thr::GetCurrentProcess() || thr::GetProcessId(process) == thr::GetCurrentProcessId() {
        let image = if module == 0 { modules::main_module() } else { modules::find_mapped_by_base(module) };
        if let Some(image) = image {
            if info.is_null() || (size as usize) < std::mem::size_of::<ps::MODULEINFO>() {
                fnd::SetLastError(fnd::ERROR_INSUFFICIENT_BUFFER);
                return 0;
            }
            info.write_unaligned(ps::MODULEINFO {
                lpBaseOfDll: image.base as *mut c_void,
                SizeOfImage: image.size as u32,
                EntryPoint: if image.entry_rva == 0 { std::ptr::null_mut() } else { (image.base + image.entry_rva as usize) as *mut c_void },
            });
            return 1;
        }
    }
    ps::K32GetModuleInformation(process, module as _, info, size)
}

pub unsafe extern "system" fn FreeLibrary(h: usize) -> BOOL {
    if modules::find_mapped_by_base(h).is_some() || modules::hle_name_by_handle(h).is_some() {
        return 1;
    }
    fnd::FreeLibrary(h as _)
}

/// A thread-pool callback asks for its module to be freed when it returns (the title's
/// msvcp140 does, for the reference it took on itself). A module XCL mapped is not in Windows'
/// loader – the pool's FreeLibrary fails and raises 0xC000070E in the worker (Minecraft: 20
/// workers at once, then a fail-fast). XCL's modules are never unloaded: nothing to do for them.
pub unsafe extern "system" fn FreeLibraryWhenCallbackReturns(instance: *mut c_void, h: usize) {
    let main = modules::main_module().is_some_and(|m| m.base == h);
    if main || modules::find_mapped_by_base(h).is_some() || modules::hle_name_by_handle(h).is_some() {
        return;
    }
    thr::FreeLibraryWhenCallbackReturns(instance as _, h as _)
}

pub unsafe extern "system" fn DisableThreadLibraryCalls(h: usize) -> BOOL {
    if let Some(m) = modules::find_mapped_by_base(h) {
        m.no_thread_calls.store(true, Ordering::Relaxed);
        return 1;
    }
    ll::DisableThreadLibraryCalls(h as _)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn real_sleep(ms: u32) {
        std::thread::sleep(std::time::Duration::from_millis(ms as u64));
    }

    /// A title's Sleep does not count the time the title is held (a bind waiting for a pipeline
    /// still being built): it returns once that is over and its own time has gone by after it.
    #[test]
    fn a_sleep_does_not_count_the_time_the_title_is_held() {
        use std::sync::atomic::AtomicBool;
        let held = AtomicBool::new(true);
        let start = std::time::Instant::now();
        std::thread::scope(|s| {
            s.spawn(|| {
                real_sleep(300);
                held.store(false, Ordering::Release);
            });
            title_sleep(60, 60, real_sleep, || held.load(Ordering::Acquire));
        });
        let took = start.elapsed();
        assert!(took >= std::time::Duration::from_millis(355), "back after {took:?}: the 300 ms held were counted");
    }

    /// Time held from the middle of a sleep on is not counted either (but for the step under way).
    #[test]
    fn a_sleep_stops_counting_when_the_title_gets_held() {
        use std::sync::atomic::AtomicBool;
        let held = AtomicBool::new(false);
        let start = std::time::Instant::now();
        std::thread::scope(|s| {
            s.spawn(|| {
                real_sleep(50);
                held.store(true, Ordering::Release);
                real_sleep(300);
                held.store(false, Ordering::Release);
            });
            title_sleep(200, SLEEP_HOLD_MIN_MS, real_sleep, || held.load(Ordering::Acquire));
        });
        let took = start.elapsed();
        // 200 ms of its own, less at most one step counted while held, and the 300 ms held
        assert!(took >= std::time::Duration::from_millis(200 + 300 - SLEEP_STEP_MS as u64 - 15), "back after {took:?}");
    }

    /// With the title not held a sleep takes its time, in steps; a yield and an endless sleep
    /// are passed on as they are.
    #[test]
    fn a_sleep_of_a_title_not_held_takes_its_time() {
        let calls = std::cell::RefCell::new(Vec::new());
        let sleep = |ms: u32| {
            calls.borrow_mut().push(ms);
            if ms != 0 && ms != thr::INFINITE {
                real_sleep(ms);
            }
        };
        title_sleep(0, 1, sleep, || false);
        title_sleep(thr::INFINITE, 1, sleep, || false);
        assert_eq!(*calls.borrow(), [0, thr::INFINITE]);
        calls.borrow_mut().clear();
        let t = std::time::Instant::now();
        title_sleep(50, 1, sleep, || false);
        let took = t.elapsed();
        assert!(took >= std::time::Duration::from_millis(50) && took < std::time::Duration::from_millis(400), "took {took:?}");
        assert!(calls.borrow().iter().all(|&ms| (1..=SLEEP_STEP_MS).contains(&ms)), "{:?}", calls.borrow());
        // a sleep shorter than a step is one sleep, as before
        calls.borrow_mut().clear();
        title_sleep(1, 1, sleep, || false);
        assert_eq!(*calls.borrow(), [1]);
    }

    /// A sleep shorter than the shortest one held is a poll: it is passed on as it is, also
    /// while the title is held. `XCL_SLEEP_HOLD` sets that length, `0` takes no sleep.
    #[test]
    fn a_short_sleep_is_passed_on_while_the_title_is_held() {
        let calls = std::cell::RefCell::new(Vec::new());
        let sleep = |ms: u32| calls.borrow_mut().push(ms);
        title_sleep(SLEEP_HOLD_MIN_MS - 1, SLEEP_HOLD_MIN_MS, sleep, || true);
        assert_eq!(*calls.borrow(), [SLEEP_HOLD_MIN_MS - 1]);
        assert_eq!(sleep_hold_min(None), Some(SLEEP_HOLD_MIN_MS));
        assert_eq!(sleep_hold_min(Some("0")), None);
        assert_eq!(sleep_hold_min(Some(" 250 ")), Some(250));
        assert_eq!(sleep_hold_min(Some("on")), Some(SLEEP_HOLD_MIN_MS));
    }

    #[test]
    fn fine_timer_makes_short_sleeps_short() {
        // Windows' Sleep (std::thread::sleep has its own high-resolution timer)
        fine_timer();
        let t = std::time::Instant::now();
        for _ in 0..20 {
            unsafe { windows_sys::Win32::System::Threading::Sleep(1) };
        }
        let per = t.elapsed() / 20;
        assert!(per < std::time::Duration::from_millis(5), "Sleep(1) took {per:?}: the 15.6 ms tick");
    }

    #[test]
    fn affinity_masks_outside_the_process_are_fitted() {
        // a mask from the PC's processors is the title's choice
        assert_eq!(fit_affinity_mask(0x3F, 0xFF), 0x3F);
        assert_eq!(fit_affinity_mask(0, 0xF), 0);
        // the console's 7 cores on a 4-processor PC: the ones there are
        assert_eq!(fit_affinity_mask(0x7F, 0xF), 0xF);
        // only processors the PC lacks: dealt onto the PC's by index
        assert_eq!(fit_affinity_mask(1 << 6, 0xF), 1 << 2);
        assert_eq!(fit_affinity_mask(0x3000, 0xF), 0x3);
        assert_eq!(fit_affinity_mask(1 << 20, 0b1010_1010), 0b10);
    }

    /// Windows refuses a mask none of whose processors the process has (ERROR_INVALID_PARAMETER);
    /// the title's call gets the processors there are. (Only XCL's answer is checked: Wine's own
    /// may differ.)
    #[test]
    fn set_thread_affinity_mask_takes_console_masks() {
        let allowed = process_affinity();
        if allowed == 0 || allowed == usize::MAX {
            println!("SKIPPED set_thread_affinity_mask_takes_console_masks: every processor bit is taken");
            return;
        }
        let missing = !allowed & allowed.wrapping_add(1);
        let first = allowed & allowed.wrapping_neg();
        std::thread::spawn(move || unsafe {
            let me = thr::GetCurrentThread();
            assert_ne!(SetThreadAffinityMask(me, missing), 0, "{missing:#x} in a {allowed:#x} process");
            assert_eq!(thr::SetThreadAffinityMask(me, allowed), fit_affinity_mask(missing, allowed));
            let mut a: GROUP_AFFINITY = std::mem::zeroed();
            a.Mask = missing | first;
            assert_eq!(SetThreadGroupAffinity(me, &a, std::ptr::null_mut()), 1);
        })
        .join()
        .unwrap();
    }

    /// Tracing keeps the last error of the call it traces: a lookup of an unset environment
    /// variable (what trace_mem's first call does) sets 203, which reached the title instead of
    /// the call's own error (xmem_and_physical_page_overflows_fail_cleanly, when it ran first).
    #[test]
    fn tracing_keeps_the_last_error() {
        unsafe {
            fnd::SetLastError(fnd::ERROR_NOT_ENOUGH_MEMORY);
            keep_last_error(|| assert!(std::env::var_os("XCL_TEST_UNSET_8C1F").is_none()));
            assert_eq!(fnd::GetLastError(), fnd::ERROR_NOT_ENOUGH_MEMORY);
            fnd::SetLastError(fnd::ERROR_NOT_ENOUGH_MEMORY);
            trace_mem("test", 0, 0x1_0000, 0, 0, 0);
            assert_eq!(fnd::GetLastError(), fnd::ERROR_NOT_ENOUGH_MEMORY);
        }
    }

    /// The GDK thunks' device type lookup: RegOpenKeyW(HKLM\…\OEM) + RegQueryValueExW(ConsoleMode).
    #[test]
    fn console_mode_registry_value() {
        unsafe {
            let hklm = windows_sys::Win32::System::Registry::HKEY_LOCAL_MACHINE as usize;
            let path = crate::util::wide("Software\\Microsoft\\Windows NT\\CurrentVersion\\OEM");
            let mut k = 0usize;
            assert_eq!(RegOpenKeyW(hklm, path.as_ptr(), &mut k), 0);
            let (mut ty, mut v, mut n) = (0u32, 0u32, 4u32);
            let name = crate::util::wide("ConsoleMode");
            assert_eq!(RegQueryValueExW(k, name.as_ptr(), std::ptr::null_mut(), &mut ty, &mut v as *mut u32 as *mut u8, &mut n), 0);
            assert_eq!((ty, n), (4, 4));
            assert_eq!(v, console_mode());
            let other = crate::util::wide("Other");
            assert_eq!(RegQueryValueExW(k, other.as_ptr(), std::ptr::null_mut(), &mut ty, &mut v as *mut u32 as *mut u8, &mut n), 2);
            assert_eq!(RegCloseKey(k), 0);
            // any other key: the real registry
            let real = crate::util::wide("Software\\Microsoft\\Windows NT\\CurrentVersion");
            let mut r = 0usize;
            assert_eq!(RegOpenKeyExW(hklm, real.as_ptr(), 0, 0x20019, &mut r), 0);
            assert_ne!(r, OEM_HANDLE);
            assert_eq!(RegCloseKey(r), 0);
        }
    }

    #[test]
    fn console_alloc_flags_are_stripped() {
        assert_eq!(sanitize_alloc_type(MEM_GRAPHICS | 0x1000 | 0x2000), 0x3000);
        assert_eq!(sanitize_alloc_type(MEM_TITLE), 0x3000);
        assert_eq!(sanitize_protect(0x1000_0004), 0x04);
        assert_eq!(sanitize_protect(0x2000_0000), mem::PAGE_READWRITE);
        unsafe {
            let p = VirtualAlloc(std::ptr::null(), 0x10000, MEM_GRAPHICS | MEM_TITLE | 0x3000, 0x0400_0004);
            assert!(!p.is_null());
            *(p as *mut u64) = 0xABCD;
            // graphics memory: from the GPU-visible region (when it could be created)
            if let Some(u) = crate::gpu::uma::get() {
                assert!(u.contains(p as usize));
            }
            assert_eq!(VirtualFree(p, 0, mem::MEM_RELEASE), 1);
            let q = VirtualAlloc(std::ptr::null(), 0x10000, 0x3000, 0x04); // ordinary memory: Windows
            assert!(crate::gpu::uma::get().is_none_or(|u| !u.contains(q as usize)));
            assert_eq!(VirtualFree(q, 0, mem::MEM_RELEASE), 1);
        }
    }

    #[test]
    fn xmem_placeholder_split_replace_and_coalesce() {
        unsafe {
            // The title splits a placeholder, replaces each piece with a physical-page
            // reservation, then maps pages at those exact GPU-visible addresses.
            let grain = PHYS_PAGE;
            let base = XMemVirtualAlloc(std::ptr::null(), 3 * grain, 0x42000, 7, mem::PAGE_NOACCESS);
            assert!(!base.is_null());
            let split = VirtualFree(base, grain, 0x8002);
            if split == 0 {
                VirtualFree(base, 0, mem::MEM_RELEASE);
            }
            assert_eq!(split, 1, "split a placeholder without releasing its address space");
            let rest = base.byte_add(grain);
            assert_eq!(VirtualFree(rest, grain, 0x8002), 1);
            assert_eq!(XMemVirtualAlloc(base, grain, 0x20406000, 0x27, 0x1004), base);
            assert!(XMemVirtualAlloc(rest, 2 * grain, 0x20406000, 0x27, 0x1004).is_null(), "replacement must match one placeholder");
            if let Some(u) = crate::gpu::uma::get() {
                assert!(u.is_title_address(base as usize), "replacement stays GPU-visible");
            }
            assert_eq!(VirtualAlloc(base, grain, mem::MEM_COMMIT, mem::PAGE_READWRITE), base);
            *(base as *mut u32) = 0x12345678;
            assert_eq!(VirtualFree(base, grain, 0x8002), 1, "return a replacement to a placeholder");
            assert_eq!(VirtualFree(base, 3 * grain, 0x8001), 1, "coalesce adjacent placeholders");
            assert_eq!(VirtualFree(base, 0, mem::MEM_RELEASE), 1);
        }
    }

    #[test]
    fn xmem_alignment_and_zeroing() {
        let attrs = |align_log2: u64, mtype: u64| (align_log2 << 24) | (mtype << 29);
        assert_eq!(XAllocAttrs::decode(attrs(8, 0)).alignment, 8);
        unsafe {
            let p = XMemAlloc(1000, attrs(8, 0));
            assert!(!p.is_null());
            assert_eq!(p as usize % 256, 0);
            assert!(std::slice::from_raw_parts(p as *const u8, 1000).iter().all(|&b| b == 0));
            XMemFree(p, attrs(8, 0));
            let g = XMemAlloc(0x20000, attrs(0, 1)); // graphics memory → VirtualAlloc
            assert!(!g.is_null());
            assert_eq!(g as usize % 0x10000, 0);
            XMemFree(g, attrs(0, 1));
        }
    }

    #[test]
    fn gpu_visible_fallback_tracks_writes() {
        unsafe {
            for (t, p) in [
                (mem::MEM_RESERVE, mem::PAGE_READWRITE | 0x800),
                (mem::MEM_RESERVE | mem::MEM_COMMIT | MEM_CONSOLE_LARGE_PAGES, mem::PAGE_READWRITE),
            ] {
                let p = VirtualAlloc(std::ptr::null(), PHYS_PAGE_SIZE, t, p);
                assert!(!p.is_null());
                assert!(in_graphics_reservation(p as usize));
                if t & mem::MEM_COMMIT == 0 {
                    assert_eq!(VirtualAlloc(p, PHYS_PAGE_SIZE, mem::MEM_COMMIT, mem::PAGE_READWRITE), p);
                }
                (p as *mut u8).write_volatile(1);
                let mut dirty = [std::ptr::null_mut(); 1];
                let (mut count, mut granularity) = (dirty.len(), 0);
                assert_eq!(mem::GetWriteWatch(0, p, PHYS_PAGE_SIZE, dirty.as_mut_ptr(), &mut count, &mut granularity), 0);
                assert_eq!((count, dirty[0]), (1, p));
                assert_eq!(VirtualFree(p, 0, mem::MEM_RELEASE), 1);
                in_released_memory(p as usize, PHYS_PAGE_SIZE, || assert!(!in_graphics_reservation(p as usize)));
            }
        }
    }

    #[test]
    fn xmem_virtual_query_marks_cpu_readable_memory() {
        unsafe {
            let mut info = [0u8; 64];
            let x = 5u32;
            let proc_ = thr::GetCurrentProcess();
            assert_eq!(XMemVirtualQuery(proc_, &x as *const u32 as _, info.as_mut_ptr() as _), 1);
            let mbi = &*(info.as_ptr() as *const mem::MEMORY_BASIC_INFORMATION);
            assert_eq!(mbi.State, mem::MEM_COMMIT);
            assert_eq!(u32::from_le_bytes(info[48..52].try_into().unwrap()) & 3, 1);
            // reserved, not committed: not CPU-readable
            let r = mem::VirtualAlloc(std::ptr::null(), 0x10000, mem::MEM_RESERVE, mem::PAGE_NOACCESS);
            let mut info = [0u8; 64];
            assert_eq!(XMemVirtualQuery(proc_, r, info.as_mut_ptr() as _), 1);
            assert_eq!(info[48] & 3, 0);
            mem::VirtualFree(r, 0, mem::MEM_RELEASE);
        }
    }

    #[test]
    fn xmem_tools_memory_is_unavailable() {
        assert_eq!(XMemIsToolsMemAvailable(), 0);
    }

    /// Timing (`--ignored --nocapture`; `XCL_SCATTER_PAGES=4096,8192`): pages written at their
    /// home, scattered from there into a reservation in one call, scattered again in reverse,
    /// and mapped back home.
    #[test]
    #[ignore]
    fn scatter_timing() {
        let sizes: Vec<usize> = std::env::var("XCL_SCATTER_PAGES").ok().map_or(vec![4096, 8192, 16384], |v| v.split(',').map(|s| s.parse().unwrap()).collect());
        let ms = |t: std::time::Instant| t.elapsed().as_secs_f64() * 1e3;
        for n in sizes {
            unsafe {
                let mut cnt = n;
                let mut pages = vec![0usize; n];
                assert_eq!(AllocateTitlePhysicalPages(std::ptr::null_mut(), 0, &mut cnt, pages.as_mut_ptr()), 1);
                let h = MapTitlePhysicalPages(std::ptr::null_mut(), n, 0, 4, pages.as_mut_ptr()) as usize;
                assert!(h != 0);
                for i in 0..n {
                    *((h + i * PHYS_PAGE) as *mut u32) = i as u32 + 1;
                }
                let r = mem::VirtualAlloc(std::ptr::null(), n * PHYS_PAGE, mem::MEM_RESERVE, mem::PAGE_READWRITE) as usize;
                let addrs: Vec<*mut c_void> = (0..n).map(|i| (r + i * PHYS_PAGE) as *mut c_void).collect();
                let mut ids: Vec<u64> = pages.iter().map(|&p| p as u64).collect();
                let t = std::time::Instant::now();
                assert_eq!(XMemMapPhysicalPagesScatter(addrs.as_ptr(), n, ids.as_mut_ptr()), 1);
                let from_home = ms(t);
                ids.reverse();
                let t = std::time::Instant::now();
                assert_eq!(XMemMapPhysicalPagesScatter(addrs.as_ptr(), n, ids.as_mut_ptr()), 1);
                let reverse = ms(t);
                for i in (0..n).step_by(97) {
                    assert_eq!(*((r + i * PHYS_PAGE) as *const u32), (n - i) as u32);
                }
                let t = std::time::Instant::now();
                assert_eq!(MapTitlePhysicalPages(std::ptr::null_mut(), n, 0, 4, pages.as_mut_ptr()) as usize, h);
                let back = ms(t);
                for i in (0..n).step_by(97) {
                    assert_eq!(*((h + i * PHYS_PAGE) as *const u32), i as u32 + 1);
                }
                eprintln!("scatter_timing pages={n} from_home={from_home:.1}ms reverse={reverse:.1}ms back_home={back:.1}ms");
                FreeTitlePhysicalPages(std::ptr::null_mut(), n, pages.as_mut_ptr());
                mem::VirtualFree(r as *mut c_void, 0, mem::MEM_RELEASE);
            }
        }
    }

    #[test]
    fn title_memory_status_reports_budget() {
        let mut s: TitleMemoryStatusT = unsafe { std::mem::zeroed() };
        s.dw_length = std::mem::size_of::<TitleMemoryStatusT>() as u32;
        assert_eq!(std::mem::size_of::<TitleMemoryStatusT>(), 80);
        assert_eq!(unsafe { TitleMemoryStatus(&mut s) }, 1);
        assert_eq!(s.total, 13_824 * 1024 * 1024);
        assert!(s.avail <= s.total);
        // the process's own memory counts as used (Wine leaves PrivateUsage 0)
        assert!(crate::util::process_commit_bytes() > 0 && s.avail < s.total);
        s.dw_length = 8;
        assert_eq!(unsafe { TitleMemoryStatus(&mut s) }, 0);
    }

    #[test]
    fn get_proc_address_and_load_library_ex_reject_null_name() {
        unsafe {
            let k32 = ll::GetModuleHandleW(crate::util::wide("kernel32.dll").as_ptr()) as usize;
            assert_ne!(k32, 0);
            fnd::SetLastError(0);
            assert!(ll::GetProcAddress(k32 as _, std::ptr::null()).is_none());
            let win32_err = fnd::GetLastError();

            fnd::SetLastError(0);
            assert_eq!(GetProcAddress(k32, std::ptr::null()), 0);
            assert_eq!(fnd::GetLastError(), fnd::ERROR_INVALID_PARAMETER);
            // Windows' own answer; Wine's GetProcAddress says ERROR_PROC_NOT_FOUND instead
            if !crate::util::under_wine() {
                assert_eq!(win32_err, fnd::ERROR_INVALID_PARAMETER);
            }

            // Non-zero ordinal lookup still resolves when valid (ordinal #1 on kernel32).
            assert_ne!(GetProcAddress(k32, 1usize as *const u8), 0);

            fnd::SetLastError(0);
            assert_eq!(LoadLibraryExW(std::ptr::null(), std::ptr::null_mut(), LOAD_AS_DATA), 0);
            assert_eq!(fnd::GetLastError(), fnd::ERROR_INVALID_PARAMETER);

            fnd::SetLastError(0);
            assert_eq!(LoadLibraryExA(std::ptr::null(), std::ptr::null_mut(), LOAD_AS_DATA), 0);
            assert_eq!(fnd::GetLastError(), fnd::ERROR_INVALID_PARAMETER);
        }
    }

    #[test]
    fn get_thread_name_sizing_truncation_and_errors() {
        std::thread::spawn(|| unsafe {
            let cur = thr::GetCurrentThread();

            // Empty thread name: capacity 0 fails with ERROR_INSUFFICIENT_BUFFER, capacity 1 writes NUL.
            let empty = crate::util::wide("");
            assert_eq!(SetThreadName(cur, empty.as_ptr()), 1);
            let mut ret = 99usize;
            fnd::SetLastError(0);
            assert_eq!(GetThreadName(cur, std::ptr::null_mut(), 0, &mut ret), 0);
            assert_eq!(fnd::GetLastError(), fnd::ERROR_INSUFFICIENT_BUFFER);
            assert_eq!(ret, 0);
            let mut nul_only = [0xBEEFu16; 1];
            assert_eq!(GetThreadName(cur, nul_only.as_mut_ptr(), 1, &mut ret), 1);
            assert_eq!((ret, nul_only[0]), (0, 0));

            let want = crate::util::wide("xcl-worker-1");
            assert_eq!(SetThreadName(cur, want.as_ptr()), 1);

            // Null buffer with non-zero length -> ERROR_INVALID_PARAMETER.
            fnd::SetLastError(0);
            assert_eq!(GetThreadName(cur, std::ptr::null_mut(), 4, &mut ret), 0);
            assert_eq!(fnd::GetLastError(), fnd::ERROR_INVALID_PARAMETER);

            // Size query (null buffer, len == 0) -> FALSE + ERROR_INSUFFICIENT_BUFFER with ret_len = 12.
            fnd::SetLastError(0);
            assert_eq!(GetThreadName(cur, std::ptr::null_mut(), 0, &mut ret), 0);
            assert_eq!(fnd::GetLastError(), fnd::ERROR_INSUFFICIENT_BUFFER);
            assert_eq!(ret, 12);

            // Buffer equal to char count (no room for NUL terminator) -> FALSE + ERROR_INSUFFICIENT_BUFFER.
            let mut exact = [0xBEEFu16; 12];
            fnd::SetLastError(0);
            assert_eq!(GetThreadName(cur, exact.as_mut_ptr(), exact.len(), &mut ret), 0);
            assert_eq!(fnd::GetLastError(), fnd::ERROR_INSUFFICIENT_BUFFER);
            assert_eq!(ret, 12);

            // Buffer with room for chars + NUL -> TRUE and exact match (both with and without ret_len).
            let mut full = [0u16; 13];
            assert_eq!(GetThreadName(cur, full.as_mut_ptr(), full.len(), &mut ret), 1);
            assert_eq!(ret, 12);
            assert_eq!(&full[..], &want[..]);
            full.fill(0);
            assert_eq!(GetThreadName(cur, full.as_mut_ptr(), full.len(), std::ptr::null_mut()), 1);
            assert_eq!(&full[..], &want[..]);

            // Invalid thread handle -> FALSE + Win32 error set.
            fnd::SetLastError(0);
            assert_eq!(GetThreadName(std::ptr::null_mut(), full.as_mut_ptr(), full.len(), &mut ret), 0);
            assert_ne!(fnd::GetLastError(), 0);
        })
        .join()
        .unwrap();
    }

    #[test]
    fn xmem_virtual_query_excludes_guard_pages() {
        unsafe {
            let proc_ = thr::GetCurrentProcess();
            let page = mem::VirtualAlloc(std::ptr::null(), 0x1000, mem::MEM_COMMIT | mem::MEM_RESERVE, mem::PAGE_READWRITE | mem::PAGE_GUARD);
            assert!(!page.is_null());
            let mut info = [0u64; 8];
            assert_eq!(XMemVirtualQuery(proc_, page, info.as_mut_ptr() as _), 1);
            assert_eq!((info[6] as u32) & 3, 0, "PAGE_GUARD page must not be marked CPU-readable");
            mem::VirtualFree(page, 0, mem::MEM_RELEASE);
        }
    }

    #[test]
    fn xmem_and_physical_page_overflows_fail_cleanly() {
        unsafe {
            // CPU heap allocation overflow -> null + ERROR_NOT_ENOUGH_MEMORY.
            fnd::SetLastError(0);
            assert!(XMemAllocDefault(usize::MAX, 0).is_null());
            assert_eq!(fnd::GetLastError(), fnd::ERROR_NOT_ENOUGH_MEMORY);

            // Graphics layout rounding overflow (usize::MAX) and largest 64 KiB multiple
            // (which previously overflowed `size + GRAIN - 1` inside Uma::sparse_alloc / alloc_where).
            let gfx_attrs = 1u64 << 29;
            assert!(xmem_graphics_layout(usize::MAX, XAllocAttrs::decode(gfx_attrs)).is_none());
            fnd::SetLastError(0);
            assert!(XMemAllocDefault(usize::MAX, gfx_attrs).is_null());
            assert_eq!(fnd::GetLastError(), fnd::ERROR_NOT_ENOUGH_MEMORY);

            let max_aligned = usize::MAX & !(crate::gpu::uma::GRAIN - 1);
            assert!(XMemAllocDefault(max_aligned, gfx_attrs).is_null());

            // Physical page byte-count overflow -> FALSE + ERROR_NOT_ENOUGH_MEMORY.
            let mut huge = usize::MAX;
            let mut page = 0usize;
            fnd::SetLastError(0);
            assert_eq!(AllocateTitlePhysicalPages(std::ptr::null_mut(), 0, &mut huge, &mut page), 0);
            assert_eq!(fnd::GetLastError(), fnd::ERROR_NOT_ENOUGH_MEMORY);
        }
    }

    /// A no-access graphics reservation of 64 KiB pages is the GPU view of the CPU-readable one
    /// of its size before it: textures there read the CPU view (`title_textures`).
    #[test]
    fn gpu_view_reservations_name_their_cpu_view() {
        // (addresses far above anything Windows hands out: only the bookkeeping is tested)
        let (a, b, size) = (0x7000_0000_0000usize, 0x7006_0000_0000usize, 0x6_0000_0000usize);
        let other = 0x7010_0000_0000usize;
        mirrors::reserved(a, size, 0x2040_2000, mem::PAGE_READWRITE);
        mirrors::reserved(other, 0x1000_0000, 0x2040_2000, mem::PAGE_READWRITE); // another size
        assert_eq!(mirrors::source(b + 0x1234_0000), None, "no GPU view yet");
        mirrors::reserved(b, size, 0x2040_2000, 0x801);
        assert_eq!(mirrors::source(b), Some(a));
        assert_eq!(mirrors::source(b + 0x4524_0000), Some(a + 0x4524_0000));
        assert_eq!(mirrors::source(b + size), None, "past its end");
        assert_eq!(mirrors::source(a + 0x10), None, "the CPU view is no GPU view");
        // committed memory or other protections are no GPU views
        let c = 0x7020_0000_0000usize;
        mirrors::reserved(c, size, 0x2040_3000, 0x801);
        assert_eq!(mirrors::source(c), None);
        mirrors::released(a);
        assert_eq!(mirrors::source(b), None, "released with its CPU view");
        mirrors::released(other);
    }

    /// GPU-visible title memory that reaches the plain-memory fallback (64 KiB pages, large
    /// pages, a graphics protection) is write-watched: a texture placed there sees the CPU's
    /// writes. The fallback is called directly: through `VirtualAlloc` these sizes land in a
    /// unified-memory window whenever one has room (once any test has made the windows).
    #[test]
    fn gpu_visible_fallback_memory_is_write_watched() {
        let null = std::ptr::null();
        // (MEM_64K_PAGES | MEM_RESERVE; MEM_LARGE_PAGES | MEM_RESERVE | MEM_COMMIT with a
        // PAGE_GRAPHICS_* protection; MEM_GRAPHICS | MEM_RESERVE | MEM_COMMIT)
        // (and a commit with no reservation and a PAGE_GRAPHICS_* protection)
        let gpu = [(0x2040_2000u32, mem::PAGE_READWRITE), (0x2000_3000, 0x2404), (MEM_GRAPHICS | 0x3000, mem::PAGE_READWRITE), (0x1000, 0x2404)];
        for &(t, p) in &gpu {
            assert!(gpu_visible_fallback(null, t, p), "flags {t:#x} protect {p:#x}");
        }
        // plain memory stays plain: CPU-only reservations, commits at a fixed address
        assert!(!gpu_visible_fallback(null, 0x3000, mem::PAGE_READWRITE));
        assert!(!gpu_visible_fallback(0x1_0000 as *const c_void, 0x2040_2000, mem::PAGE_READWRITE), "a fixed address");
        unsafe {
            for (t, p) in gpu {
                let size = 0x40_0000;
                let q = reserve_watched(size, t, p);
                assert!(!q.is_null(), "flags {t:#x}");
                let a = q as usize;
                assert!(in_graphics_reservation(a), "flags {t:#x}: a fallback allocation is watched");
                if t & 0x1000 == 0 {
                    assert!(!VirtualAlloc((a + 0x2_0000) as *const c_void, 0x1_0000, 0x1000, mem::PAGE_READWRITE).is_null());
                }
                let mut buf = [std::ptr::null_mut::<c_void>(); 16];
                let (mut n, mut gran) = (buf.len(), 0u32);
                assert_eq!(mem::GetWriteWatch(1, q, size, buf.as_mut_ptr(), &mut n, &mut gran), 0);
                ((a + 0x2_1234) as *mut u8).write_volatile(7);
                n = buf.len();
                assert_eq!(mem::GetWriteWatch(1, q, size, buf.as_mut_ptr(), &mut n, &mut gran), 0);
                assert_eq!(&buf[..n], &[(a + 0x2_1000) as *mut c_void], "flags {t:#x}: the written page is reported");
                assert_eq!(VirtualFree(q, 0, mem::MEM_RELEASE), 1);
                in_released_memory(q as usize, size, || assert!(!in_graphics_reservation(a)));
            }
        }
    }

    /// A commit with no address reserves too: released, nothing of it stays in XCL's record of
    /// title memory (the span left there answered "committed" for the next owner of the address).
    #[test]
    fn a_commit_with_no_address_is_forgotten_when_released() {
        unsafe {
            let p = VirtualAlloc(std::ptr::null(), 0x2800, mem::MEM_COMMIT, mem::PAGE_READWRITE);
            assert!(!p.is_null());
            assert_eq!(crate::memmap::access(p as usize, 0x2800), Some(crate::memmap::Access::Write));
            assert_eq!(crate::memmap::reservation(p as usize), Some((p as usize, p as usize + 0x3000)));
            assert_eq!(VirtualFree(p, 0, mem::MEM_RELEASE), 1);
            // (the address reserved again behind XCL, if no other thread took it meanwhile)
            if mem::VirtualAlloc(p, 0x3000, mem::MEM_RESERVE, mem::PAGE_NOACCESS) == p {
                assert_eq!(crate::memmap::access(p as usize, 4), None);
                assert_eq!(crate::memmap::access(p as usize + 0x2800, 4), None);
                mem::VirtualFree(p, 0, mem::MEM_RELEASE);
            }
        }
    }

    /// A release forgets XCL's records of the memory before the address goes back to Windows.
    /// Forgotten after, they were the records of whoever had the address next: two threads take
    /// turns at one address here (and at whatever Windows gives a watched reservation), and each
    /// sees its own allocation recorded for as long as it has it.
    #[test]
    fn a_release_leaves_the_records_of_the_next_allocation_at_its_address() {
        unsafe {
            let at = mem::VirtualAlloc(std::ptr::null(), 0x1_0000, mem::MEM_RESERVE, mem::PAGE_NOACCESS) as usize;
            assert_ne!(at, 0);
            mem::VirtualFree(at as *mut c_void, 0, mem::MEM_RELEASE);
            let turns = || {
                for _ in 0..20_000 {
                    let p = VirtualAlloc(at as *const c_void, 0x1_0000, mem::MEM_RESERVE, mem::PAGE_READWRITE);
                    if !p.is_null() {
                        for _ in 0..16 {
                            assert_eq!(crate::memmap::reservation(at), Some((at, at + 0x1_0000)), "the reservation of the thread that has the address");
                        }
                        assert_eq!(VirtualFree(p, 0, mem::MEM_RELEASE), 1);
                    }
                    let q = reserve_watched(0x1_0000, mem::MEM_RESERVE, mem::PAGE_READWRITE);
                    assert!(!q.is_null());
                    crate::memmap::reserved(q as usize, 0x1_0000);
                    for _ in 0..16 {
                        assert!(in_graphics_reservation(q as usize), "the graphics reservation of the thread that has the address");
                        assert_eq!(crate::memmap::reservation(q as usize), Some((q as usize, q as usize + 0x1_0000)));
                    }
                    assert_eq!(VirtualFree(q, 0, mem::MEM_RELEASE), 1);
                }
            };
            std::thread::scope(|s| {
                s.spawn(turns);
                turns();
            });
        }
    }

    /// A release Windows refuses (a size with it) leaves XCL's records as they were: the
    /// reservation, what is committed in it, and that it is the GPU's to see.
    #[test]
    fn a_refused_release_changes_none_of_the_records() {
        unsafe {
            let p = VirtualAlloc(std::ptr::null(), 0x1_0000, mem::MEM_RESERVE | mem::MEM_COMMIT, mem::PAGE_READWRITE);
            assert!(!p.is_null());
            note_graphics_reservation(p as usize, 0x1_0000);
            for t in [mem::MEM_RELEASE, mem::MEM_RELEASE | mem::MEM_DECOMMIT] {
                let size = if t == mem::MEM_RELEASE { 0x1000 } else { 0 };
                assert_eq!(VirtualFree(p, size, t), 0, "flags {t:#x}");
                assert_eq!(crate::memmap::reservation(p as usize), Some((p as usize, p as usize + 0x1_0000)));
                assert_eq!(crate::memmap::access(p as usize, 0x1_0000), Some(crate::memmap::Access::Write), "what is committed stays known");
                assert!(in_graphics_reservation(p as usize));
            }
            assert_eq!(VirtualFree(p, 0, mem::MEM_RELEASE), 1);
            in_released_memory(p as usize, 0x1_0000, || assert!(!in_graphics_reservation(p as usize) && crate::memmap::reservation(p as usize).is_none()));
        }
    }

    /// The title's own memory freed through VirtualFreeEx is forgotten as through VirtualFree.
    #[test]
    fn virtual_free_ex_of_the_title_itself_is_virtual_free() {
        unsafe {
            let p = VirtualAlloc(std::ptr::null(), 0x1_0000, mem::MEM_RESERVE | mem::MEM_COMMIT, mem::PAGE_READWRITE);
            assert_eq!(crate::memmap::access(p as usize, 4), Some(crate::memmap::Access::Write));
            assert_eq!(VirtualFreeEx(thr::GetCurrentProcess(), p, 0, mem::MEM_RELEASE), 1);
            if mem::VirtualAlloc(p, 0x1_0000, mem::MEM_RESERVE, mem::PAGE_NOACCESS) == p {
                assert_eq!(crate::memmap::access(p as usize, 4), None);
                mem::VirtualFree(p, 0, mem::MEM_RELEASE);
            }
        }
    }

    #[test]
    fn virtual_free_ex_and_sparse_validation() {
        unsafe {
            let cur = thr::GetCurrentProcess();

            // Failed MEM_RELEASE (non-zero size) must keep a tracked graphics reservation registered,
            // and VirtualFreeEx(GetCurrentProcess(), ...) must release & untrack it.
            let host = mem::VirtualAlloc(std::ptr::null(), 0x10000, mem::MEM_RESERVE, mem::PAGE_NOACCESS);
            assert!(!host.is_null());
            GRAPHICS_RESERVATIONS.lock().unwrap().insert(host as usize, 0x10000);
            assert!(in_graphics_reservation(host as usize));
            assert_eq!(VirtualFree(host, 0x1000, mem::MEM_RELEASE), 0);
            assert!(in_graphics_reservation(host as usize));
            assert_eq!(VirtualFreeEx(cur, host, 0, mem::MEM_RELEASE), 1);
            in_released_memory(host as usize, 0x10000, || assert!(!in_graphics_reservation(host as usize)));

            // Sparse UMA reservation: invalid flag combos / non-zero MEM_RELEASE fail without freeing;
            // VirtualFreeEx(GetCurrentProcess(), ...) releases cleanly.
            let sp = VirtualAlloc(std::ptr::null(), 0x10000, mem::MEM_RESERVE | MEM_GRAPHICS, mem::PAGE_READWRITE);
            if !sp.is_null() && crate::gpu::uma::get().is_some_and(|u| u.is_sparse(sp as usize)) {
                fnd::SetLastError(0);
                assert_eq!(VirtualFree(sp, 0x1000, mem::MEM_RELEASE), 0);
                assert_eq!(fnd::GetLastError(), fnd::ERROR_INVALID_PARAMETER);

                fnd::SetLastError(0);
                assert_eq!(VirtualFree(sp, 0, mem::MEM_RELEASE | mem::MEM_DECOMMIT), 0);
                assert_eq!(fnd::GetLastError(), fnd::ERROR_INVALID_PARAMETER);

                fnd::SetLastError(0);
                assert_eq!(VirtualFree(sp, 0, 0), 0);
                assert_eq!(fnd::GetLastError(), fnd::ERROR_INVALID_PARAMETER);

                assert_eq!(VirtualFreeEx(cur, sp, 0, mem::MEM_RELEASE), 1);
                assert!(crate::gpu::uma::get().unwrap().sparse_reservation(sp as usize).is_none());
            } else if !sp.is_null() {
                assert_eq!(VirtualFreeEx(cur, sp, 0, mem::MEM_RELEASE), 1);
            }
        }
    }

    #[test]
    fn get_module_handle_matches_load_library_for_console_aliases_and_stubs() {
        unsafe {
            // Real Windows system DLL keeps its genuine Windows HMODULE.
            let real_k32 = ll::GetModuleHandleW(crate::util::wide("kernel32.dll").as_ptr()) as usize;
            assert_ne!(real_k32, 0);
            assert_eq!(GetModuleHandleW(crate::util::wide("kernel32.dll").as_ptr()), real_k32);
            assert_eq!(GetModuleHandleA(c"kernel32.dll".as_ptr() as *const u8), real_k32);

            // Console-only kernelx aliases return the kernelx HLE pseudo handle (matching LoadLibrary).
            let kx = modules::hle_handle(crate::hle::KERNELX);
            for dll in ["advapi32legacy.dll", "kernel32legacy.dll", "minappmodelx.dll"] {
                let w = crate::util::wide(dll);
                let h_load = LoadLibraryW(w.as_ptr());
                assert_eq!(h_load, kx, "LoadLibraryW({dll})");
                assert_eq!(GetModuleHandleW(w.as_ptr()), kx, "GetModuleHandleW({dll})");

                let c = std::ffi::CString::new(dll).unwrap();
                assert_eq!(GetModuleHandleA(c.as_ptr() as *const u8), kx, "GetModuleHandleA({dll})");

                let mut out = 0usize;
                assert_eq!(GetModuleHandleExW(0, w.as_ptr(), &mut out), 1, "GetModuleHandleExW({dll})");
                assert_eq!(out, kx);
            }

            // Unknown DLL still returns NULL + ERROR_MOD_NOT_FOUND.
            fnd::SetLastError(0);
            assert_eq!(GetModuleHandleW(crate::util::wide("no_such_optional_dll_xyz.dll").as_ptr()), 0);
            assert_eq!(fnd::GetLastError(), fnd::ERROR_MOD_NOT_FOUND);
        }

        // @stub DLLs: unloaded probe returns 0; once loaded (hle_handle created), returns that handle.
        let mut cfg = crate::config::Config::default();
        cfg.aliases.insert("unit_stub_probe.dll".into(), "@stub".into());
        let reg = crate::hle::build(&cfg);
        assert_eq!(module_handle_in(&reg, "unit_stub_probe.dll"), 0);
        let stub_h = modules::hle_handle("unit_stub_probe.dll");
        assert_ne!(stub_h, 0);
        assert_eq!(module_handle_in(&reg, "unit_stub_probe.dll"), stub_h);
    }
}
