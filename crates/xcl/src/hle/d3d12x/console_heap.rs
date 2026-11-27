//! Console descriptor heaps (CBV/SRV/UAV and samplers).
//!
//! On the console a descriptor is GPU-readable memory: 32 bytes per CBV/SRV/UAV slot (T# /
//! V#), 16 per sampler (S#), and a descriptor table in a root argument is a 32-bit GPU address
//! (shaders load it with the high half 0 – amd_fidelityfx_upscaler_xbox.dll, HANDOFF "FSR4").
//! XCL gives console code heaps whose slots live in unified memory (so the table addresses
//! console code computes from the heap start work as they are in the recompiler's `mem`
//! model) and keeps PC descriptors beside each: view slot k → an SRV and, next to it, a UAV;
//! sampler slot k → PC sampler k. A slot holds the descriptor in the XCL shader ABI
//! (`desc::xcl_image_words`: T# word 0 = the SRV's index in the shader-visible PC heap;
//! `desc::xcl_sampler_words`: S# word 3 [11:0] = k; V# with its unified-memory address).
//!
//! View descriptors are values on the console: titles create them in a CPU-only heap and copy
//! them into the shader-visible one as memory (the console header's inline
//! CopyDescriptorsSimple; XDefiant copies every texture that way), and a descriptor table may
//! be in any heap. So all console view heaps share one shader-visible PC heap (`Global`):
//! each gets a range there, T# word 0 is an index into it – a T# copied anywhere still names
//! its PC descriptor – and that heap is what lists bind. The title's own PC heap stays as a
//! CPU-only mirror (where views are made, copies come from and UAV clears take their CPU
//! handle). A heap gets its place in the shared heap a page of `PAGE_SLOTS` slots at a time,
//! when a slot of that page is first used: titles make large heaps and fill few slots
//! (Minecraft: eight view heaps of 73 727 slots – 147 454 PC descriptors each, more in all
//! than the 1 000 000 an NVIDIA shared heap holds – with a few hundred views in them). A heap
//! left out of the shared heap would be lost: lists bind only the shared heap for views.
//! Without the shared heap (it could not be made) a heap keeps its own PC heap: SRV at 2k,
//! UAV at 2k + 1.

pub(crate) mod vsharp;
use vsharp::Writer;
use std::sync::Mutex;

use windows::Win32::Graphics::Direct3D12::*;

use super::material_gate_views::{self, Key, NativeView};

/// bytes of a CBV/SRV/UAV slot (T# / V#) and of a sampler slot (S#)
pub const SLOT: u32 = 32;
pub const SAMPLER_SLOT: u32 = 16;

/// Whose memory a heap's slots are in.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Memory {
    /// a sparse VRAM allocation in window 0
    Sparse,
    /// committed system RAM
    Uma,
    /// the title's (its allocation hook gave it: `kernelx::runtime_alloc`)
    Title,
}

struct Heap {
    stamp: u64,
    obj: usize,
    shadow: usize,
    n: u32,
    pc_cpu: u64,
    /// 0: not shader visible
    pc_gpu: u64,
    inc: u32,
    sampler: bool,
    /// views in the shared heap: the index there of each page's first SRV (`UNMAPPED`: the
    /// page has no place yet); empty: the heap's views are not in the shared heap
    pages: Vec<u32>,
    /// shader visible as the title made it
    visible: bool,
    memory: Memory,
}

impl Heap {
    fn slot_bytes(&self) -> u32 {
        if self.sampler {
            SAMPLER_SLOT
        } else {
            SLOT
        }
    }
}

static HEAPS: Mutex<Vec<Heap>> = Mutex::new(Vec::new());

/// The shader-visible PC heap of all console view heaps.
struct Global {
    dev: ID3D12Device,
    heap: ID3D12DescriptorHeap,
    cpu: u64,
    gpu: u64,
    inc: u32,
    ranges: SharedRanges,
}

#[derive(Clone, Copy)]
pub(super) enum Owner {
    Title,
    DirectoryPage,
    DirectoryTable,
    Locator,
    Other,
}

const OWNER_COUNT: usize = Owner::Other as usize + 1;

struct SharedRanges {
    cap: u32,
    next: u32,
    /// Free ranges (first, count), ordered by start and separated by live descriptors.
    free: Vec<(u32, u32)>,
    live: [u32; OWNER_COUNT],
    refused: [u64; OWNER_COUNT],
}

impl SharedRanges {
    fn new(cap: u32) -> Self {
        Self { cap, next: NULL_IMAGE_VIEWS, free: Vec::new(), live: [0; OWNER_COUNT], refused: [0; OWNER_COUNT] }
    }

    fn reserve(&mut self, owner: Owner, count: u32) -> Option<u32> {
        let first = take_range(&mut self.free, &mut self.next, self.cap, count);
        if first.is_some() {
            self.live[owner as usize] += count;
        } else {
            if self.refused == [0; OWNER_COUNT] {
                crate::xlog_warn!("console descriptor heaps: the shared heap ({}) cannot fit {count} descriptors – shaders do not see the missing views", self.cap);
            }
            self.refused[owner as usize] += 1;
        }
        first
    }

    fn release(&mut self, owner: Owner, first: u32, count: u32) {
        return_range(&mut self.free, &mut self.next, first, count);
        self.live[owner as usize] -= count;
    }
}

// D3D12 descriptor heaps and devices are free-threaded
unsafe impl Send for Global {}

static GLOBAL: Mutex<Option<Global>> = Mutex::new(None);
static GLOBAL_FAILED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// descriptors of the shared heap at most (the GPU may take more: OPTIONS19)
const GLOBAL_MAX: u32 = 1 << 21;

/// view slots per page of a heap's place in the shared heap (2 PC descriptors each)
pub const PAGE_SLOTS: u32 = 256;
/// a page with no place in the shared heap yet
const UNMAPPED: u32 = u32::MAX;

/// PC descriptors of page `p` of a heap of `n` slots: 2 per slot, and the last page only as
/// long as the heap (a heap of a few slots takes a few descriptors, not a whole page's).
fn page_descriptors(n: u32, p: u32) -> u32 {
    2 * n.saturating_sub(p * PAGE_SLOTS).clamp(1, PAGE_SLOTS)
}

/// A first-fit range of `count` in `free` (released ranges), else at `next` below `cap`.
fn take_range(free: &mut Vec<(u32, u32)>, next: &mut u32, cap: u32, count: u32) -> Option<u32> {
    if let Some(i) = free.iter().position(|&(_, n)| n >= count) {
        let (f, n) = free[i];
        if n == count {
            free.remove(i);
        } else {
            free[i] = (f + count, n - count);
        }
        return Some(f);
    }
    if cap - *next < count {
        return None;
    }
    *next += count;
    Some(*next - count)
}

fn return_range(free: &mut Vec<(u32, u32)>, next: &mut u32, mut first: u32, count: u32) {
    if count == 0 {
        return;
    }
    let mut end = first + count;
    let mut i = free.partition_point(|&(start, _)| start < first);
    if i > 0 && free[i - 1].0 + free[i - 1].1 == first {
        i -= 1;
        first = free.remove(i).0;
    }
    if i < free.len() && free[i].0 == end {
        end += free.remove(i).1;
    }
    if end == *next {
        *next = first;
    } else {
        free.insert(i, (first, end - first));
    }
}

/// The first descriptors of the shared heap, which no view of a title's or of XCL's is given:
/// null image views (`null_views`). A T# of zeros – an empty console slot – has PC index 0 in
/// its word 0, so an image shader that reads one takes descriptor 0 (SRV) and 1 (UAV) of the
/// heap. Whatever was reserved first lay there: the memory windows' 4 GiB raw buffer UAVs, read
/// as a texture, which hangs NVIDIA GPUs. An even count: SRV / UAV pairs.
pub const NULL_IMAGE_VIEWS: u32 = 4;

/// Null views at descriptors `0..NULL_IMAGE_VIEWS` of the shared heap (an SRV / UAV pair per
/// console slot: even SRV, odd UAV): a shader that lands there for a T# of zeros reads zeros,
/// and its writes go nowhere.
unsafe fn null_views(dev: &ID3D12Device, cpu: u64, inc: u32) {
    for k in 0..NULL_IMAGE_VIEWS as u64 {
        let at = D3D12_CPU_DESCRIPTOR_HANDLE { ptr: (cpu + k * inc as u64) as usize };
        if k % 2 == 0 {
            let d = D3D12_SHADER_RESOURCE_VIEW_DESC {
                Format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_R8G8B8A8_UNORM,
                ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2D,
                Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
                Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 { Texture2D: D3D12_TEX2D_SRV { MipLevels: 1, ..Default::default() } },
            };
            dev.CreateShaderResourceView(None::<&ID3D12Resource>, Some(&d), at);
        } else {
            let d = D3D12_UNORDERED_ACCESS_VIEW_DESC {
                Format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_R32_UINT,
                ViewDimension: D3D12_UAV_DIMENSION_TEXTURE2D,
                Anonymous: D3D12_UNORDERED_ACCESS_VIEW_DESC_0 { Texture2D: D3D12_TEX2D_UAV::default() },
            };
            dev.CreateUnorderedAccessView(None::<&ID3D12Resource>, None::<&ID3D12Resource>, Some(&d), at);
        }
    }
}

/// The shared shader-visible heap, made on first use. False: there is none (it could not be
/// made) – view heaps then keep their own.
pub unsafe fn shared_heap(dev: &ID3D12Device) -> bool {
    use std::sync::atomic::Ordering::Relaxed;
    let mut g = GLOBAL.lock().unwrap();
    if g.is_none() {
        if GLOBAL_FAILED.load(Relaxed) {
            return false;
        }
        let mut o = D3D12_FEATURE_DATA_D3D12_OPTIONS19::default();
        let max = if dev.CheckFeatureSupport(D3D12_FEATURE_D3D12_OPTIONS19, &mut o as *mut _ as *mut std::ffi::c_void, std::mem::size_of_val(&o) as u32).is_ok() && o.MaxViewDescriptorHeapSize > 0 {
            o.MaxViewDescriptorHeapSize
        } else {
            1_000_000
        };
        let cap = max.min(GLOBAL_MAX);
        let d = D3D12_DESCRIPTOR_HEAP_DESC { Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV, NumDescriptors: cap, Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE, NodeMask: 0 };
        let heap: ID3D12DescriptorHeap = match dev.CreateDescriptorHeap(&d) {
            Ok(h) => h,
            Err(e) => {
                GLOBAL_FAILED.store(true, Relaxed);
                crate::xlog_warn!("console descriptor heaps: no shared shader-visible heap of {cap} ({e}) – each keeps its own");
                return false;
            }
        };
        crate::xlog_info!("console descriptor heaps: one shader-visible PC heap of {cap} view descriptors for all of them; its first {NULL_IMAGE_VIEWS} are null image views (an empty slot's)");
        null_views(dev, heap.GetCPUDescriptorHandleForHeapStart().ptr as u64, dev.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV));
        *g = Some(Global {
            dev: dev.clone(),
            cpu: heap.GetCPUDescriptorHandleForHeapStart().ptr as u64,
            gpu: heap.GetGPUDescriptorHandleForHeapStart().ptr,
            inc: dev.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV),
            heap,
            ranges: SharedRanges::new(cap),
        });
    }
    true
}

/// The place in the shared heap of the page slot `k` of `h` is in – (index of the page's
/// first SRV there, the page's first slot) – given now if the page has none yet. None: the
/// heap is not in the shared heap, or that is full.
fn page_of(h: &mut Heap, k: u32) -> Option<(u32, u32)> {
    let p = (k / PAGE_SLOTS) as usize;
    let mut first = *h.pages.get(p)?;
    if first == UNMAPPED {
        let mut g = GLOBAL.lock().unwrap();
        first = g.as_mut()?.ranges.reserve(Owner::Title, page_descriptors(h.n, p as u32))?;
        h.pages[p] = first;
    }
    Some((first, p as u32 * PAGE_SLOTS))
}

/// A shared range from `reserve_own` is free again.
pub fn release_global(first: u32, count: u32) {
    release_range(Owner::Other, first, count);
}

pub(super) fn release_range(owner: Owner, first: u32, count: u32) {
    if let Some(s) = GLOBAL.lock().unwrap().as_mut() {
        s.ranges.release(owner, first, count);
    }
}

/// `count` descriptors of the shared heap for XCL's own views, kept for the process lifetime:
/// the index of the first and its CPU handle (the next ones `inc` apart). None: no shared heap,
/// or no room left in it.
pub fn reserve_own(count: u32) -> Option<(u32, D3D12_CPU_DESCRIPTOR_HANDLE, u32)> {
    reserve_own_as(Owner::Other, count)
}

pub(super) fn reserve_own_as(owner: Owner, count: u32) -> Option<(u32, D3D12_CPU_DESCRIPTOR_HANDLE, u32)> {
    let mut g = GLOBAL.lock().unwrap();
    let s = g.as_mut()?;
    let first = s.ranges.reserve(owner, count)?;
    Some((first, D3D12_CPU_DESCRIPTOR_HANDLE { ptr: (s.cpu + first as u64 * s.inc as u64) as usize }, s.inc))
}

/// Sample the allocator state without reading the console slots that the title can write.
pub(crate) fn log_occupancy() {
    let (cap, high, largest, live, refused) = {
        let g = GLOBAL.lock().unwrap();
        let Some(g) = g.as_ref() else { return };
        let r = &g.ranges;
        let largest = r.free.iter().map(|&(_, count)| count).max().unwrap_or(0).max(r.cap - r.next);
        (r.cap, r.next, largest, r.live, r.refused)
    };
    let [title, pages, tables, locators, other] = live;
    let total = live.iter().sum::<u32>() + NULL_IMAGE_VIEWS;
    crate::xlog_info!(
        "console descriptor heaps: {total} of {cap} live, high water {high}, largest free range {largest}; title {title}, directory pages {pages}, directory tables {tables}, locators {locators}, other {other}; reservations refused by owner (same order) {refused:?}"
    );
}

/// The shader-visible PC heap lists bind for view descriptors (None: none shared).
pub fn global_heap() -> Option<ID3D12DescriptorHeap> {
    GLOBAL.lock().unwrap().as_ref().map(|s| s.heap.clone())
}

/// Whether `obj` (a title's view heap) has its descriptors in the shared heap.
pub fn in_global(obj: usize) -> bool {
    HEAPS.lock().unwrap().iter().any(|h| h.obj == obj && !h.pages.is_empty())
}

/// Memory for a heap of `bytes` from the title's allocation hook – where the title expects the
/// heap then. Only what descriptor tables can address (window 0) and is committed; anything else
/// goes back to the title.
unsafe fn title_memory(u: &crate::gpu::uma::Uma, bytes: usize) -> Option<usize> {
    use crate::hle::kernelx::{runtime_alloc, runtime_free, D3D_DESCRIPTOR_HEAP};
    let p = runtime_alloc(bytes, D3D_DESCRIPTOR_HEAP)?;
    // (physical pages the title mapped there are committed on first use)
    crate::hle::kernelx::lazy::ensure(p, bytes);
    let readable = crate::gpu::shader::readable(p, bytes);
    if readable && u.window_of(p) == Some(0) && u.window_of(p + bytes - 1) == Some(0) {
        return Some(p);
    }
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    if N.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 8 {
        crate::xlog_warn!("console descriptor heap: the title's memory {p:#x} ({bytes:#x} bytes) not used (window {:?}, committed {readable})", u.window_of(p));
    }
    runtime_free(p, D3D_DESCRIPTOR_HEAP);
    None
}

/// `obj` is a PC heap from `pc_cpu` / `pc_gpu` (increment `inc`; `pc_gpu` 0 when not shader
/// visible) with 2·`n` view descriptors or `n` samplers: give it `n` console slots in
/// unified memory. `shared`: its views go to the shared heap (`shared_heap`), a page at a
/// time as slots are used; `visible`: what the title asked for.
pub fn register(obj: usize, n: u32, pc_cpu: u64, pc_gpu: u64, inc: u32, sampler: bool, shared: bool, visible: bool) -> bool {
    let Some(u) = crate::gpu::uma::get() else { return false };
    let bytes = n.max(1) * if sampler { SAMPLER_SLOT } else { SLOT };
    // Descriptor tables are 32-bit console addresses (high dword 0): must live in window 0.
    // On the sparse-window path, `sparse::alloc_in(0, ...)` commits fresh zeroed pages so we
    // avoid touching unused pages with `write_bytes` (which would mark them dirty for write-watch).
    let (p, memory) = if let Some(p) = unsafe { title_memory(u, bytes as usize) } {
        (p as *mut u8, Memory::Title)
    } else if u.is_sparse_window(0) {
        let Some((addr, _)) = (unsafe { super::sparse::alloc_in(0, bytes as usize, 256) }) else {
            return false;
        };
        (addr as *mut u8, Memory::Sparse)
    } else {
        let Some(p) = u.alloc_in(0, bytes as usize, 256) else { return false };
        unsafe { std::ptr::write_bytes(p, 0, bytes as usize) };
        (p, Memory::Uma)
    };
    {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        if N.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 48 {
            crate::xlog_info!(
                "console descriptor heap {obj:#x}: {n} {} slots at {} {:#x}, {}",
                if sampler { "sampler" } else { "view" },
                match memory {
                    Memory::Sparse => "sparse VRAM",
                    Memory::Uma => "UMA",
                    Memory::Title => "the title's memory",
                },
                p as usize - u.base(),
                if visible { "shader visible" } else { "CPU only" }
            );
        }
    }
    let pages = if shared && !sampler && GLOBAL.lock().unwrap().is_some() { vec![UNMAPPED; n.div_ceil(PAGE_SLOTS).max(1) as usize] } else { Vec::new() };
    static STAMP: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let stamp = if vsharp::enabled() { STAMP.fetch_add(1,std::sync::atomic::Ordering::Relaxed)+1 } else {0};
    HEAPS.lock().unwrap().push(Heap { stamp, obj, shadow: p as usize, n, pc_cpu, pc_gpu, inc, sampler, pages, visible, memory });
    if !sampler && n != 0 && super::depth_lineage::enabled() {
        LINEAGE_UAVS.lock().unwrap().get_or_insert_with(Default::default)
            .heaps.insert(p as usize, p as usize + n as usize * SLOT as usize);
    }
    true
}

/// The heap object was released for good: its slots go back to unified memory.
pub fn unregister(obj: usize) {
    let gone = {
        let mut heaps = HEAPS.lock().unwrap();
        let gone = heaps.iter().position(|h| h.obj == obj).map(|i| heaps.swap_remove(i));
        if let Some(h)=gone.as_ref().filter(|h|!h.sampler) { vsharp::forget(h.shadow..h.shadow+h.n as usize*SLOT as usize); }
        if let Some(h) = gone.as_ref().filter(|h| !h.sampler && material_gate_views::enabled()) {
            material_gate_views::forget_heap(h.shadow..h.shadow + h.n as usize * SLOT as usize,
                h.pages.iter().enumerate().filter(|&(_, &f)| f != UNMAPPED).map(|(p, &f)| (f, page_descriptors(h.n, p as u32))));
        }
        gone
    };
    if let Some(h) = gone {
        if !h.sampler && super::depth_lineage::enabled() {
            if let Some(uavs) = LINEAGE_UAVS.lock().unwrap().as_mut() {
                if let Some(end) = uavs.heaps.remove(&h.shadow) {
                    uavs.forget(|at, _| (h.shadow..end).contains(&at));
                }
            }
        }
        if let Some(res) = RES.lock().unwrap().as_mut() {
            res.forget(h.shadow..h.shadow + h.n as usize * h.slot_bytes() as usize);
        }
        for (p, &f) in h.pages.iter().enumerate().filter(|&(_, &f)| f != UNMAPPED) {
            release_range(Owner::Title, f, page_descriptors(h.n, p as u32));
        }
        match h.memory {
            Memory::Sparse => {
                unsafe { super::sparse::free(h.shadow) };
            }
            Memory::Uma => {
                if let Some(u) = crate::gpu::uma::get() {
                    u.free(h.shadow);
                }
            }
            Memory::Title => unsafe { crate::hle::kernelx::runtime_free(h.shadow, crate::hle::kernelx::D3D_DESCRIPTOR_HEAP) },
        }
    }
}

/// Start of the console slots of heap `obj` (CPU handle = GPU handle on the console).
pub fn start(obj: usize) -> Option<u64> {
    HEAPS.lock().unwrap().iter().find(|h| h.obj == obj).map(|h| h.shadow as u64)
}

/// A console slot: where its descriptor goes and its PC descriptors.
#[derive(Clone, Copy, Debug)]
pub struct Slot {
    pub at: usize,
    /// views: PC heap index of the SRV (the UAV is at + 1); samplers: of the sampler
    pub index: u32,
    /// PC CPU handles of the SRV / UAV where views are made (the title's heap)
    pub pc_srv: u64,
    pub pc_uav: u64,
    /// PC GPU handle of the UAV (0: heap not shader visible)
    pub pc_gpu_uav: u64,
    pub sampler: bool,
    /// CPU handle of the SRV in the shared heap (0: the heap is not in it)
    pub shared_srv: u64,
    /// the slot's number in its heap
    pub k: u32,
}

/// The slot behind a console CPU handle (None: not one of XCL's console heaps). A view slot
/// of a heap in the shared heap gets its page's place there now if the page has none yet.
pub fn slot(cpu: u64) -> Option<Slot> {
    let mut heaps = HEAPS.lock().unwrap();
    let h = heaps.iter_mut().find(|h| cpu >= h.shadow as u64 && cpu < (h.shadow + (h.n * h.slot_bytes()) as usize) as u64)?;
    let k = ((cpu - h.shadow as u64) / h.slot_bytes() as u64) as u32;
    if h.sampler {
        return Some(Slot { at: h.shadow + (k * SAMPLER_SLOT) as usize, index: k, pc_srv: h.pc_cpu + (k * h.inc) as u64, pc_uav: 0, pc_gpu_uav: 0, sampler: true, shared_srv: 0, k });
    }
    let uav = ((2 * k + 1) * h.inc) as u64;
    let shared = page_of(h, k).and_then(|(first, k0)| {
        let g = GLOBAL.lock().unwrap();
        let g = g.as_ref()?;
        let i = first + 2 * (k - k0);
        Some((i, g.gpu + (i as u64 + 1) * g.inc as u64, g.cpu + i as u64 * g.inc as u64))
    });
    let (index, pc_gpu_uav, shared_srv) = shared.unwrap_or((2 * k, if h.pc_gpu == 0 { 0 } else { h.pc_gpu + uav }, 0));
    Some(Slot { at: h.shadow + (k * SLOT) as usize, index, pc_srv: h.pc_cpu + (2 * k * h.inc) as u64, pc_uav: h.pc_cpu + uav, pc_gpu_uav, sampler: false, shared_srv, k })
}

/// The view just made in slot `s` (its SRV, or with `uav` its UAV) goes to the shared heap.
pub unsafe fn publish(s: &Slot, uav: bool) {
    if s.shared_srv == 0 {
        return;
    }
    if let Some(g) = GLOBAL.lock().unwrap().as_ref() {
        let (dst, src) = if uav { (s.shared_srv + g.inc as u64, s.pc_uav) } else { (s.shared_srv, s.pc_srv) };
        g.dev.CopyDescriptorsSimple(1, D3D12_CPU_DESCRIPTOR_HANDLE { ptr: dst as usize }, D3D12_CPU_DESCRIPTOR_HANDLE { ptr: src as usize }, D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV);
    }
}

/// `n` view slots from `first` were copied (in the title's heap): they go to the shared heap,
/// a run per page (a heap's pages lie apart there).
pub unsafe fn publish_range(first: &Slot, n: u32) {
    let mut done = 0;
    while done < n {
        let s = if done == 0 { Some(*first) } else { slot((first.at + (done * SLOT) as usize) as u64) };
        let Some(s) = s else { return };
        let run = (PAGE_SLOTS - s.k % PAGE_SLOTS).min(n - done);
        if s.shared_srv != 0 {
            if let Some(g) = GLOBAL.lock().unwrap().as_ref() {
                g.dev.CopyDescriptorsSimple(2 * run, D3D12_CPU_DESCRIPTOR_HANDLE { ptr: s.shared_srv as usize }, D3D12_CPU_DESCRIPTOR_HANDLE { ptr: s.pc_srv as usize }, D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV);
            }
        }
        done += run;
    }
}

#[derive(Default)]
struct ViewOwners {
    by_slot: std::collections::HashMap<usize, usize>,
    by_shared_index: std::collections::HashMap<u32, (usize, usize)>,
    native_srvs: std::collections::HashMap<usize, NativeSrv>,
    native_uavs: std::collections::HashMap<usize, NativeUav>,
}

#[derive(Clone, Copy)]
pub(super) struct NativeUav {
    pub resource: usize,
    pub view: (i32, i32),
    pub backing: (u64, u32, u16, i32, i32, i32),
    pub volume: Option<(u32, u32, u32)>,
}

pub(super) type NativeSrv = (usize, i32, i32, i32, i32, i32);

impl ViewOwners {
    fn note(&mut self, s: &Slot, res: usize) {
        if res == 0 {
            self.by_slot.remove(&s.at);
            if s.shared_srv != 0 {
                self.by_shared_index.remove(&s.index);
            }
        } else {
            self.by_slot.insert(s.at, res);
            if s.shared_srv != 0 {
                self.by_shared_index.insert(s.index, (s.at, res));
            }
        }
    }

    fn forget(&mut self, slots: std::ops::Range<usize>) {
        self.by_slot.retain(|at, _| !slots.contains(at));
        self.by_shared_index.retain(|_, (at, _)| !slots.contains(at));
        self.native_srvs.retain(|at, _| !slots.contains(at));
        self.native_uavs.retain(|at, _| !slots.contains(at));
    }

    fn shared_owner(&self, index: u32) -> Option<usize> {
        self.by_shared_index.get(&index).map(|&(_, res)| res)
    }
}

static RES: Mutex<Option<ViewOwners>> = Mutex::new(None);

/// The view in slot `s` names `res` (0: none).
pub fn note_resource(s: &Slot, res: usize) {
    if s.sampler || !(super::present::trace_frame() || super::markers::enabled() || super::present::writer_trace_configured()) {
        return;
    }
    RES.lock().unwrap().get_or_insert_with(Default::default).note(s, res);
}

/// The resource of the view slot at UMA address `at`.
pub fn resource_at(at: usize) -> Option<usize> {
    RES.lock().unwrap().as_ref()?.by_slot.get(&at).copied()
}

pub(super) fn image_resource_at(addr32: u32, index: u32) -> Option<usize> {
    let shared = GLOBAL.lock().unwrap().is_some();
    if shared {
        return RES.lock().unwrap().as_ref()?.shared_owner(index);
    }
    let u = crate::gpu::uma::get()?;
    let s = slot((u.base() + addr32 as usize) as u64)?;
    if s.sampler || s.index != index {
        return None;
    }
    resource_at(s.at)
}

pub(super) fn note_native_srv(s: &Slot, srv: Option<NativeSrv>) {
    let mut guard = RES.lock().unwrap();
    let owners = guard.get_or_insert_with(Default::default);
    if let Some(srv) = srv {
        owners.native_srvs.insert(s.at, srv);
    } else {
        owners.native_srvs.remove(&s.at);
    }
}

pub(super) fn native_srv_at(addr32: u32, index: u32) -> Option<NativeSrv> {
    native_view_at(addr32, index, |owners, at| owners.native_srvs.get(&at).copied())
}

pub(super) fn note_native_uav(s: &Slot, resource: Option<NativeUav>) {
    let mut guard = RES.lock().unwrap();
    let owners = guard.get_or_insert_with(Default::default);
    if let Some(resource) = resource {
        owners.native_uavs.insert(s.at, resource);
    } else {
        owners.native_uavs.remove(&s.at);
    }
}

pub(super) fn native_uav_at(addr32: u32, index: u32) -> Option<usize> {
    native_uav_details_at(addr32, index).map(|v| v.resource)
}

pub(super) fn native_uav_details_at(addr32: u32, index: u32) -> Option<NativeUav> {
    native_view_at(addr32, index, |owners, at| owners.native_uavs.get(&at).copied())
}

fn native_view_at<T>(addr32: u32, index: u32, get: impl FnOnce(&ViewOwners, usize) -> Option<T>) -> Option<T> {
    let shared = GLOBAL.lock().unwrap().is_some();
    if shared {
        let guard = RES.lock().unwrap();
        let owners = guard.as_ref()?;
        let &(at, _) = owners.by_shared_index.get(&index)?;
        return get(owners, at);
    }
    let u = crate::gpu::uma::get()?;
    let s = slot((u.base() + addr32 as usize) as u64)?;
    if s.sampler || s.index != index {
        return None;
    }
    get(RES.lock().unwrap().as_ref()?, s.at)
}

type LineageUav = super::device_x::LineageUav;

#[derive(Default)]
struct LineageUavs {
    heaps: std::collections::BTreeMap<usize, usize>,
    by_slot: std::collections::HashMap<usize, ([u32; 8], LineageUav, Option<u32>)>,
    by_index: std::collections::HashMap<u32, usize>,
}

impl LineageUavs {
    fn forget(&mut self, mut forget: impl FnMut(usize, LineageUav) -> bool) {
        self.by_slot.retain(|&at, &mut (_, uav, _)| !forget(at, uav));
        let by_slot = &self.by_slot;
        self.by_index.retain(|_, at| by_slot.contains_key(at));
    }
}

static LINEAGE_UAVS: Mutex<Option<LineageUavs>> = Mutex::new(None);

pub(super) fn forget_lineage_uav_resource(resource: usize) {
    if let Some(uavs) = LINEAGE_UAVS.lock().unwrap().as_mut() {
        uavs.forget(|_, uav| uav.owner == resource || uav.shadow == Some(resource));
    }
}

pub(super) fn has_lineage_uavs() -> bool {
    LINEAGE_UAVS.lock().unwrap().as_ref().is_some_and(|uavs| !uavs.by_index.is_empty())
}

pub(super) fn lineage_uavs_in_table(table: u32, first: u32, count: u32) -> Vec<(u32, u32, LineageUav)> {
    let Some(u) = crate::gpu::uma::get() else { return Vec::new() };
    let Some(start) = first.checked_mul(SLOT).and_then(|off| table.checked_add(off)).and_then(|off| u.base().checked_add(off as usize)) else {
        return Vec::new();
    };
    let uavs = LINEAGE_UAVS.lock().unwrap();
    let Some(uavs) = uavs.as_ref() else { return Vec::new() };
    let Some((&base, &end)) = uavs.heaps.range(..=start).next_back() else {
        return Vec::new();
    };
    if start >= end || (start - base) % SLOT as usize != 0 {
        return Vec::new();
    }
    let count = count.min(((end - start) / SLOT as usize) as u32);
    if count == 0 || !crate::gpu::shader::readable(start, count as usize * SLOT as usize) {
        return Vec::new();
    }
    (0..count).filter_map(|i| {
        let at = start + i as usize * SLOT as usize;
        let words = unsafe { (at as *const [u32; 8]).read_unaligned() };
        if words[3] >> 28 < 8 {
            return None;
        }
        let source_at = uavs.by_index.get(&words[0])?;
        let &(expected, uav, index) = uavs.by_slot.get(source_at)?;
        (index == Some(words[0]) && expected == words).then_some((first + i, words[0], uav))
    }).collect()
}

pub fn write(s: &Slot, words: &[u32]) {
    write_view(s, words, None, None);
}

pub(super) fn write_view(s: &Slot, words: &[u32], view: Option<LineageUav>, native: Option<NativeView>) {
    write_observed(s,words,view,native,Writer::Other)
}

pub(super) fn write_observed(s: &Slot, words: &[u32], view: Option<LineageUav>, native: Option<NativeView>, writer: Writer) {
    if !s.sampler && vsharp::enabled() {vsharp::written(s.at,words,writer);}
    let n = words.len().min(if s.sampler { 4 } else { 8 });
    if !s.sampler && material_gate_views::enabled() {
        material_gate_views::note(s, words, native);
    }
    if !s.sampler && super::depth_lineage::enabled() {
        let mut g = LINEAGE_UAVS.lock().unwrap();
        if let Some(uavs) = g.as_mut() {
            if let Some((_, _, Some(index))) = uavs.by_slot.remove(&s.at) {
                uavs.by_index.remove(&index);
            }
        }
        if let Some((view, words)) = view.zip(<[u32; 8]>::try_from(words).ok()).filter(|(_, w)| w[3] >> 28 >= 8) {
            let uavs = g.get_or_insert_with(Default::default);
            let index = (s.shared_srv != 0).then_some(s.index);
            uavs.by_slot.insert(s.at, (words, view, index));
            if let Some(index) = index {
                uavs.by_index.insert(index, s.at);
            }
        }
        unsafe { std::ptr::copy_nonoverlapping(words.as_ptr(), s.at as *mut u32, n) };
        return;
    }
    unsafe { std::ptr::copy_nonoverlapping(words.as_ptr(), s.at as *mut u32, n) };
}

/// Copy a console slot's descriptor to another slot of the same kind, the PC heap index
/// in it (T# word 0, S# word 3 [11:0]) renumbered for the destination. Buffer V# carry
/// their address and stay as they are.
pub fn copy_words(src: &Slot, dst: &Slot) {
    let n = if src.sampler { 4 } else { 8 };
    let mut w = [0u32; 8];
    unsafe { std::ptr::copy_nonoverlapping(src.at as *const u32, w.as_mut_ptr(), n) };
    let lineage = if !src.sampler && super::depth_lineage::enabled() {
        LINEAGE_UAVS.lock().unwrap().as_ref().and_then(|uavs| uavs.by_slot.get(&src.at))
            .and_then(|&(words, uav, _)| (w == words).then_some(uav))
    } else {
        None
    };
    let native = if !src.sampler && material_gate_views::enabled() {
        material_gate_views::copied(src, &w)
    } else {
        None
    };
    if src.sampler {
        w[3] = (w[3] & !0xFFF) | (dst.index & 0xFFF);
    } else if w[3] >> 28 >= 8 {
        w[0] = dst.index;
    }
    write_observed(dst, &w[..n], lineage, native,Writer::Copy(src.at));
    if !src.sampler {
        note_resource(dst, resource_at(src.at).unwrap_or(0));
        if super::present::native_view_trace_configured() {
            let (srv, uav) = RES.lock().unwrap().as_ref().map_or((None, None), |owners| {
                (owners.native_srvs.get(&src.at).copied(), owners.native_uavs.get(&src.at).copied())
            });
            note_native_srv(dst, srv);
            note_native_uav(dst, uav);
        }
    }
}

/// `XCL_TRACE_FRAME`: what the slots of each shader-visible view heap hold – images whose
/// PC index is the slot's own (made or copied through the device) or another (copied by the
/// title itself, as memory), buffers, empty.
pub fn census() {
    let heaps = HEAPS.lock().unwrap();
    // samplers: S# word 3 [11:0] is the PC index in the sampler heap the slot belongs to
    for h in heaps.iter().filter(|h| h.sampler) {
        let (mut empty, mut own, mut other) = (0u32, 0u32, 0u32);
        let mut examples: Vec<(u32, u32)> = Vec::new();
        for k in 0..h.n {
            let w = unsafe { std::slice::from_raw_parts((h.shadow + (k * SAMPLER_SLOT) as usize) as *const u32, 4) };
            if w.iter().all(|&x| x == 0) {
                empty += 1;
            } else if w[3] & 0xFFF == k {
                own += 1;
            } else {
                other += 1;
                if examples.len() < 8 {
                    examples.push((k, w[3] & 0xFFF));
                }
            }
        }
        crate::xlog_info!(
            "frame trace: sampler heap {:#x} ({} slots, {}): {own} with their own index, {other} with another (slot, index: {examples:?}), {empty} empty",
            h.obj,
            h.n,
            if h.visible { "shader visible" } else { "CPU only" }
        );
    }
    for h in heaps.iter().filter(|h| !h.sampler) {
        let (mut empty, mut own, mut other, mut buffers) = (0u32, 0u32, 0u32, 0u32);
        let mut examples: Vec<(u32, u32)> = Vec::new();
        let own_index = |k: u32| match h.pages.get((k / PAGE_SLOTS) as usize) {
            Some(&f) if f != UNMAPPED => f + 2 * (k % PAGE_SLOTS),
            _ => 2 * k,
        };
        for k in 0..h.n {
            let w = unsafe { std::slice::from_raw_parts((h.shadow + (k * SLOT) as usize) as *const u32, 8) };
            if w.iter().all(|&x| x == 0) {
                empty += 1;
            } else if w[3] >> 28 >= 8 {
                if w[0] == own_index(k) {
                    own += 1;
                } else {
                    other += 1;
                    if examples.len() < 8 {
                        examples.push((k, w[0]));
                    }
                }
            } else {
                buffers += 1;
            }
        }
        crate::xlog_info!(
            "frame trace: view heap {:#x} ({} slots, {}): {own} images with their own PC index, {other} with another (slot, index: {examples:?}), {buffers} buffers, {empty} empty",
            h.obj,
            h.n,
            if h.visible { "shader visible" } else { "CPU only" }
        );
    }
}

/// The descriptor at a console GPU address (low 32 bits, as in a descriptor table), if it
/// is in one of the console heaps.
pub fn words_at(addr32: u32) -> Option<[u32; 8]> {
    let u = crate::gpu::uma::get()?;
    let s = slot((u.base() + addr32 as usize) as u64)?;
    let mut w = [0u32; 8];
    unsafe { std::ptr::copy_nonoverlapping(s.at as *const u32, w.as_mut_ptr(), if s.sampler { 4 } else { 8 }) };
    Some(w)
}

pub(super) fn material_input(table: u32, offset: u32) -> Option<(u32, [u32; 8], Option<Key>)> {
    let u = crate::gpu::uma::get()?;
    let addr = table.checked_add(offset)?;
    let base = u.base().checked_add(table as usize)?;
    let at = u.base().checked_add(addr as usize)?;
    let heaps = HEAPS.lock().unwrap();
    let h = heaps.iter().find(|h| !h.sampler && base >= h.shadow && at >= base
        && (base - h.shadow) % SLOT as usize == 0 && (at - h.shadow) / (SLOT as usize) < h.n as usize)?;
    if offset % SLOT != 0 || !crate::gpu::shader::readable(at, SLOT as usize) {
        return None;
    }
    let words = unsafe { (at as *const [u32; 8]).read_unaligned() };
    let key = if GLOBAL.lock().unwrap().is_some() {
        Some(Key::Shared(words[0]))
    } else {
        let k = ((at - h.shadow) / SLOT as usize) as u32;
        (h.visible && words[0] == 2 * k).then_some(Key::Local(at))
    };
    Some((addr, words, key))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_owner_follows_descriptor_index_across_raw_copies() {
        let source = Slot { at: 0x1000, index: 17, pc_srv: 0, pc_uav: 0, pc_gpu_uav: 0, sampler: false, shared_srv: 1, k: 0 };
        let copy = Slot { at: 0x2000, index: 19, ..source };
        let mut owners = ViewOwners::default();
        owners.note(&source, 101);
        owners.note(&copy, 102);
        let words = crate::gpu::desc::xcl_image_for_view(source.index, 22, crate::gpu::desc::ImageType::D3, 4, 4, 4, 1);
        assert_eq!(owners.shared_owner(words[0]), Some(101));
        assert_eq!(owners.by_slot.get(&copy.at), Some(&102));
        owners.forget(source.at..source.at + SLOT as usize);
        assert_eq!(owners.shared_owner(words[0]), None);
        assert_eq!(owners.shared_owner(copy.index), Some(102));
    }

    #[test]
    fn texture3d_uav_companion_reads_written_mip() {
        use super::super::real;
        use std::mem::ManuallyDrop;
        use windows::core::Interface;
        use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_R32_UINT, DXGI_SAMPLE_DESC};

        if let Err(e) = crate::gpu::recomp::dxc::location().and_then(|_| real::init()) {
            println!("SKIPPED texture3d_uav_companion_reads_written_mip: {e}");
            return;
        }
        unsafe {
            let raw = real::device_raw();
            let dev = ID3D12Device::from_raw_borrowed(&raw).unwrap();
            let ty = D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV;
            let heap: ID3D12DescriptorHeap = dev.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                Type: ty,
                NumDescriptors: 2,
                ..Default::default()
            }).unwrap();
            let shared = global_heap().expect("shared descriptor heap");
            let obj = heap.as_raw() as usize;
            assert!(register(obj, 1, heap.GetCPUDescriptorHandleForHeapStart().ptr as u64, 0, dev.GetDescriptorHandleIncrementSize(ty), false, true, true));
            let cpu = start(obj).unwrap();
            let s = slot(cpu).unwrap();
            assert_ne!(s.shared_srv, 0);
            let resource = |heap, state, desc: &D3D12_RESOURCE_DESC| {
                let mut r: Option<ID3D12Resource> = None;
                super::super::title_textures::own_work(|| dev.CreateCommittedResource(
                    &D3D12_HEAP_PROPERTIES { Type: heap, ..Default::default() },
                    D3D12_HEAP_FLAG_NONE, desc, state, None, &mut r,
                )).unwrap();
                r.unwrap()
            };
            let volume = resource(D3D12_HEAP_TYPE_DEFAULT, D3D12_RESOURCE_STATE_UNORDERED_ACCESS, &D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE3D,
                Width: 4, Height: 4, DepthOrArraySize: 4, MipLevels: 2,
                Format: DXGI_FORMAT_R32_UINT,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
                ..Default::default()
            });
            dev.CreateShaderResourceView(None::<&ID3D12Resource>, Some(&D3D12_SHADER_RESOURCE_VIEW_DESC {
                Format: DXGI_FORMAT_R32_UINT,
                ViewDimension: D3D12_SRV_DIMENSION_TEXTURE3D,
                Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
                Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                    Texture3D: D3D12_TEX3D_SRV { MipLevels: 1, ..Default::default() },
                },
            }), D3D12_CPU_DESCRIPTOR_HANDLE { ptr: s.pc_srv as usize });
            publish(&s, false);
            real::dev_create_uav_x(raw, volume.as_raw(), std::ptr::null_mut(), &D3D12_UNORDERED_ACCESS_VIEW_DESC {
                Format: DXGI_FORMAT_R32_UINT,
                ViewDimension: D3D12_UAV_DIMENSION_TEXTURE3D,
                Anonymous: D3D12_UNORDERED_ACCESS_VIEW_DESC_0 {
                    Texture3D: D3D12_TEX3D_UAV { MipSlice: 1, FirstWSlice: 0, WSize: u32::MAX },
                },
            }, cpu);
            let index = (s.at as *const u32).read();
            assert_eq!(index, s.index);

            let hlsl = format!("
                RWByteAddressBuffer res : register(u1);
                [numthreads(1, 1, 1)]
                void main() {{
                    Texture3D<uint> tex = ResourceDescriptorHeap[{index}];
                    uint w, h, d, mips;
                    tex.GetDimensions(0, w, h, d, mips);
                    res.Store4(0, uint4(tex.Load(int4(1, 1, 1, 0)), w, h, d));
                }}
            ");
            let dxil = crate::gpu::recomp::dxc::compile(&hlsl, "main", "cs_6_6", &[]).unwrap();
            let rs = crate::gpu::recomp::pso::abi_root_signature(dev, false).unwrap();
            let mut desc = D3D12_COMPUTE_PIPELINE_STATE_DESC {
                pRootSignature: ManuallyDrop::new(Some(rs.clone())),
                CS: D3D12_SHADER_BYTECODE { pShaderBytecode: dxil.as_ptr().cast(), BytecodeLength: dxil.len() },
                ..Default::default()
            };
            let pso: ID3D12PipelineState = dev.CreateComputePipelineState(&desc).unwrap();
            ManuallyDrop::drop(&mut desc.pRootSignature);
            let mut buffer = D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                Width: 256, Height: 1, DepthOrArraySize: 1, MipLevels: 1,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
                ..Default::default()
            };
            let out = resource(D3D12_HEAP_TYPE_DEFAULT, D3D12_RESOURCE_STATE_UNORDERED_ACCESS, &buffer);
            buffer.Flags = D3D12_RESOURCE_FLAG_NONE;
            let readback = resource(D3D12_HEAP_TYPE_READBACK, D3D12_RESOURCE_STATE_COPY_DEST, &buffer);
            let transition = |r, before, after| D3D12_RESOURCE_BARRIER {
                Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
                Anonymous: D3D12_RESOURCE_BARRIER_0 {
                    Transition: ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                        pResource: std::mem::transmute_copy(r),
                        Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                        StateBefore: before, StateAfter: after,
                    }),
                },
                ..Default::default()
            };
            let queue: ID3D12CommandQueue = dev.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC::default()).unwrap();
            let alloc: ID3D12CommandAllocator = dev.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT).unwrap();
            let list: ID3D12GraphicsCommandList = dev.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &alloc, &pso).unwrap();
            let fence: ID3D12Fence = dev.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap();
            const TEXEL: u32 = 0x1234_ABCD;
            list.SetDescriptorHeaps(&[Some(shared)]);
            list.ClearUnorderedAccessViewUint(
                D3D12_GPU_DESCRIPTOR_HANDLE { ptr: s.pc_gpu_uav },
                D3D12_CPU_DESCRIPTOR_HANDLE { ptr: s.pc_uav as usize },
                &volume, &[TEXEL, 0, 0, 0], &[],
            );
            list.ResourceBarrier(&[transition(&volume, D3D12_RESOURCE_STATE_UNORDERED_ACCESS, D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE)]);
            list.SetComputeRootSignature(&rs);
            list.SetComputeRootUnorderedAccessView(1, out.GetGPUVirtualAddress());
            list.Dispatch(1, 1, 1);
            list.ResourceBarrier(&[transition(&out, D3D12_RESOURCE_STATE_UNORDERED_ACCESS, D3D12_RESOURCE_STATE_COPY_SOURCE)]);
            list.CopyBufferRegion(&readback, 0, &out, 0, 16);
            list.Close().unwrap();
            queue.ExecuteCommandLists(&[Some(list.cast().unwrap())]);
            queue.Signal(&fence, 1).unwrap();
            crate::gpu::fence::wait(&fence, 1, dev, None, 10_000).unwrap();
            let mut p = std::ptr::null_mut();
            readback.Map(0, None, Some(&mut p)).unwrap();
            let got = p.cast::<[u32; 4]>().read();
            readback.Unmap(0, None);
            assert_eq!(got, [TEXEL, 2, 2, 2]);
        }
    }

    /// Ranges of the shared heap: released ones first (first fit, split), then new ones, and
    /// none past its size.
    #[test]
    fn shared_heap_ranges() {
        let (mut free, mut next) = (Vec::new(), 0u32);
        assert_eq!(take_range(&mut free, &mut next, 100, 40), Some(0));
        assert_eq!(take_range(&mut free, &mut next, 100, 40), Some(40));
        assert_eq!(take_range(&mut free, &mut next, 100, 40), None, "20 left");
        return_range(&mut free, &mut next, 0, 40);
        assert_eq!(take_range(&mut free, &mut next, 100, 30), Some(0));
        assert_eq!(free, vec![(30, 10)]);
        assert_eq!(take_range(&mut free, &mut next, 100, 20), Some(80));
        assert_eq!(take_range(&mut free, &mut next, 100, 10), Some(30));
        assert!(free.is_empty());
    }

    #[test]
    fn shared_ranges_count_live_and_refused_descriptors_by_owner() {
        let start = NULL_IMAGE_VIEWS;
        let mut ranges = SharedRanges::new(start + 6);
        let title = ranges.reserve(Owner::Title, 4).unwrap();
        let page = ranges.reserve(Owner::DirectoryPage, 1).unwrap();
        assert_eq!((title, page), (start, start + 4));
        assert_eq!(ranges.reserve(Owner::DirectoryTable, 2), None);
        assert_eq!(ranges.live, [4, 1, 0, 0, 0]);
        assert_eq!(ranges.refused, [0, 0, 1, 0, 0]);

        ranges.release(Owner::Title, title, 4);
        assert_eq!(ranges.reserve(Owner::Locator, 3), Some(title));
        assert_eq!(ranges.live, [0, 1, 0, 3, 0]);
        ranges.release(Owner::Locator, title, 3);
        ranges.release(Owner::DirectoryPage, page, 1);
        assert_eq!(ranges.live, [0; OWNER_COUNT]);
        assert_eq!(ranges.next, start);
        assert!(ranges.free.is_empty());
    }

    #[test]
    fn adjacent_shared_heap_ranges_are_reusable_together() {
        let (mut free, mut next) = (Vec::new(), 0u32);
        for first in [0, 32, 64, 96] {
            assert_eq!(take_range(&mut free, &mut next, 160, 32), Some(first));
        }
        for first in [64, 0, 32] {
            return_range(&mut free, &mut next, first, 32);
        }
        assert_eq!(free, [(0, 96)]);
        assert_eq!(take_range(&mut free, &mut next, 160, 96), Some(0));
        return_range(&mut free, &mut next, 96, 32);
        assert_eq!(take_range(&mut free, &mut next, 160, 64), Some(96));
    }

    /// A T# of zeros (an empty slot) names descriptors 0 (SRV) and 1 (UAV) of the shared heap:
    /// those are null views, written before anything is reserved, and nothing is ever reserved
    /// there – not the memory windows' raw buffer UAVs (a texture read of one hangs the GPU).
    #[test]
    fn a_tsharp_of_zeros_finds_null_views_and_never_a_memory_window() {
        use std::ffi::c_void;
        use windows::core::Interface;
        static SEEN: Mutex<Vec<(bool, usize, usize)>> = Mutex::new(Vec::new());
        #[repr(C)]
        struct Object {
            vt: *const usize,
        }
        unsafe extern "system" fn srv(_this: *mut c_void, r: *mut c_void, _d: *const D3D12_SHADER_RESOURCE_VIEW_DESC, at: D3D12_CPU_DESCRIPTOR_HANDLE) {
            SEEN.lock().unwrap().push((false, r as usize, at.ptr));
        }
        unsafe extern "system" fn uav(_this: *mut c_void, r: *mut c_void, _c: *mut c_void, _d: *const D3D12_UNORDERED_ACCESS_VIEW_DESC, at: D3D12_CPU_DESCRIPTOR_HANDLE) {
            SEEN.lock().unwrap().push((true, r as usize, at.ptr));
        }
        assert!(NULL_IMAGE_VIEWS >= 2, "the SRV / UAV pair of a T# of zeros is not kept");
        assert_eq!(NULL_IMAGE_VIEWS % 2, 0, "the kept descriptors split a console slot's SRV / UAV pair");
        // (ID3D12Device: CreateShaderResourceView and CreateUnorderedAccessView)
        let mut vt = [0usize; 20];
        vt[18] = srv as usize;
        vt[19] = uav as usize;
        let mut device = Object { vt: vt.as_ptr() };
        unsafe {
            let dev = std::mem::ManuallyDrop::new(ID3D12Device::from_raw(&mut device as *mut _ as *mut c_void));
            null_views(&dev, 0x1000, 32);
        }
        let seen = SEEN.lock().unwrap().clone();
        assert_eq!(seen.len(), NULL_IMAGE_VIEWS as usize);
        for (k, &(is_uav, r, at)) in seen.iter().enumerate() {
            assert_eq!((is_uav, r, at), (k % 2 == 1, 0, 0x1000 + 32 * k), "descriptor {k}");
        }
        // a heap as it is made: the first range given starts after the null views, and one
        // given back and taken again does not move below them
        let (mut free, mut next) = (Vec::new(), NULL_IMAGE_VIEWS);
        let first = take_range(&mut free, &mut next, 1 << 20, crate::gpu::recomp::MEM_NULL_VIEW + 1).unwrap();
        assert_eq!(first, NULL_IMAGE_VIEWS, "the memory windows' views, reserved first, start after the null views");
        return_range(&mut free, &mut next, first, crate::gpu::recomp::MEM_NULL_VIEW + 1);
        assert!(take_range(&mut free, &mut next, 1 << 20, 2).unwrap() >= NULL_IMAGE_VIEWS);
        // with a device: the views XCL reserved for the memory windows
        if super::super::real::init().is_ok() {
            if let Some(views) = super::super::real::mem_views() {
                assert!(views >= NULL_IMAGE_VIEWS, "the memory windows' views start at descriptor {views}");
            }
        }
    }

    #[test]
    fn slots_copy_and_release() {
        let Some(u) = crate::gpu::uma::get() else { return };
        let rel = |p: u64| (p as usize - u.base()) as u32;
        // views: 4 slots over a PC heap at 0x1000 (increment 32, GPU 0x5000); samplers: 2 at 0x9000
        assert!(register(0xA1, 4, 0x1000, 0x5000, 32, false, false, true));
        assert!(register(0xA2, 2, 0x9000, 0, 16, true, false, true));
        let (v, sm) = (start(0xA1).unwrap(), start(0xA2).unwrap());
        assert_eq!(u.window_of(v as usize), Some(0), "view heap must live in window 0 (32-bit table addressing)");
        assert_eq!(u.window_of(sm as usize), Some(0), "sampler heap must live in window 0");
        assert!(u.is_sparse(v as usize) && u.is_sparse(sm as usize), "descriptor heaps backed by sparse VRAM");
        let s1 = slot(v + 32).unwrap();
        assert_eq!((s1.index, s1.pc_srv, s1.pc_uav, s1.pc_gpu_uav), (2, 0x1040, 0x1060, 0x5060));
        // a 2D T# from slot 1 to slot 3: word 0 = the destination's SRV index
        write(&s1, &[2, 0x0470_0000, 0, 9 << 28, 0, 0, 0, 0]);
        let s3 = slot(v + 96).unwrap();
        copy_words(&s1, &s3);
        assert_eq!(words_at(rel(v + 96)).unwrap()[..4], [6, 0x0470_0000, 0, 9 << 28]);
        // a buffer V# keeps its address
        write(&s1, &[0x1234, 0, 64, 0x3101_6FAC, 0, 0, 0, 0]);
        copy_words(&s1, &s3);
        assert_eq!(words_at(rel(v + 96)).unwrap()[0], 0x1234);
        // a raytracing BVH SRSRC keeps its base address and box-sort flag verbatim
        let bvh = crate::gpu::desc::bvh_words(0x12_3456_7800, true);
        write(&s1, &[bvh[0], bvh[1], bvh[2], bvh[3], 0, 0, 0, 0]);
        copy_words(&s1, &s3);
        assert_eq!(words_at(rel(v + 96)).unwrap()[..4], bvh);
        // samplers: 16-byte slots, S# word 3 [11:0] = sampler index
        let (q0, q1) = (slot(sm).unwrap(), slot(sm + 16).unwrap());
        assert!(q1.sampler && q1.index == 1 && q1.pc_srv == 0x9010);
        write(&q0, &[1, 2, 3, 0xABC0_0000]);
        copy_words(&q0, &q1);
        assert_eq!(words_at(rel(sm + 16)).unwrap()[..4], [1, 2, 3, 0xABC0_0001]);
        assert!(slot(sm + 32).is_none(), "past the last sampler slot");
        unregister(0xA1);
        unregister(0xA2);
        assert!(start(0xA1).is_none() && slot(v).is_none() && slot(sm).is_none());
    }

    /// Minecraft's view heaps: eight of 73 727 slots, 147 454 PC descriptors each – more in all
    /// than an NVIDIA shared heap holds (1 000 000). Each gets a place there for the pages it
    /// uses; before, the seventh and eighth got none and their views were lost (flicker).
    #[test]
    fn large_heaps_get_shared_pages_as_used() {
        use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_11_0;
        use windows::Win32::Graphics::Dxgi::*;
        if crate::gpu::uma::get().is_none() {
            return;
        }
        let dev = unsafe {
            let mut dev: Option<ID3D12Device> = None;
            if D3D12CreateDevice(None, D3D_FEATURE_LEVEL_11_0, &mut dev).is_err() {
                let factory: IDXGIFactory4 = CreateDXGIFactory1().expect("DXGI factory");
                let warp: IDXGIAdapter = factory.EnumWarpAdapter().expect("WARP");
                D3D12CreateDevice(&warp, D3D_FEATURE_LEVEL_11_0, &mut dev).expect("WARP D3D12 device");
            }
            dev.expect("D3D12 device")
        };
        assert!(unsafe { shared_heap(&dev) });
        const N: u32 = 73_727;
        let objs: Vec<usize> = (0..8).map(|i| 0xB000 + i).collect();
        for &o in &objs {
            assert!(register(o, N, 0x10_0000, 0, 32, false, true, true));
            assert!(in_global(o));
        }
        let mut seen = Vec::new();
        for &o in &objs {
            let v = start(o).unwrap();
            // slot 0, the last of page 0, the first of page 1 and the heap's last slot
            let s: Vec<Slot> = [0, PAGE_SLOTS - 1, PAGE_SLOTS, N - 1].iter().map(|&k| slot(v + (k * SLOT) as u64).unwrap()).collect();
            assert!(s.iter().all(|x| x.shared_srv != 0), "heap {o:#x}: a slot without a place in the shared heap");
            assert_eq!(s[1].index, s[0].index + 2 * (PAGE_SLOTS - 1), "a page is contiguous");
            assert_eq!(s[0].pc_srv, 0x10_0000, "views are still made in the title's heap");
            assert_eq!(s[3].pc_srv, 0x10_0000 + (2 * (N - 1) * 32) as u64);
            for x in &s {
                assert!(!seen.contains(&x.index), "two slots share the PC index {}", x.index);
                seen.push(x.index);
            }
            // the same slot again: the same place
            assert_eq!(slot(v).unwrap().index, s[0].index);
        }
        for &o in &objs {
            unregister(o);
        }
        // a heap smaller than a page takes 2 descriptors per slot, as it did when a heap's
        // whole range was reserved (a title with many small heaps must not fill the shared heap)
        assert_eq!((page_descriptors(4, 0), page_descriptors(N, 0), page_descriptors(N, N / PAGE_SLOTS)), (8, 2 * PAGE_SLOTS, 2 * (N % PAGE_SLOTS)));
        assert!(register(0xB100, 4, 0x10_0000, 0, 32, false, true, true) && register(0xB101, 4, 0x20_0000, 0, 32, false, true, true));
        let (a, b) = (slot(start(0xB100).unwrap()).unwrap(), slot(start(0xB101).unwrap()).unwrap());
        assert!(a.shared_srv != 0 && b.shared_srv != 0);
        assert_eq!(a.index.abs_diff(b.index), 8, "two heaps of 4 slots lie 8 descriptors apart");
        unregister(0xB100);
        unregister(0xB101);
    }

    /// A list's heaps when the title names its own (`list_x::pc_heaps`): its console view heap
    /// and a shader-visible PC heap outside the shared one. Only the shared heap is bound, and a
    /// dispatch through exactly that finds the runtime's memory views (`real::mem_views`) at
    /// io[`IO_MEM_VIEWS`] + k: window 3 reads back what the CPU wrote, and the view
    /// after the four windows is the null view – a store through it lands nowhere and a load
    /// reads 0 – which translated shaders select for an address outside the windows.
    #[test]
    fn title_heaps_keep_the_memory_views_bound() {
        use crate::gpu::recomp::{IO_MEM_VIEWS, KERNEL_SGPRS, MEM_NULL_VIEW};
        use windows::core::Interface;
        if crate::gpu::recomp::dxc::location().is_err() || super::super::real::init().is_err() {
            return;
        }
        let (Some(u), Some(views)) = (crate::gpu::uma::get(), super::super::real::mem_views()) else { return };
        unsafe {
            let raw = super::super::real::device_raw();
            let dev = ID3D12Device::from_raw_borrowed(&raw).expect("device").clone();
            assert!(shared_heap(&dev));
            let title = 0xDEAD_4EA9usize;
            assert!(register(title, 4, 0x1000, 0, 32, false, true, true) && in_global(title));
            let foreign: ID3D12DescriptorHeap = dev
                .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC { Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV, NumDescriptors: 16, Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE, NodeMask: 0 })
                .unwrap();
            let named = [title, foreign.as_raw() as usize];
            let bound = super::super::list_x::pc_heaps(2, named.as_ptr());
            let shared = global_heap().unwrap();
            assert_eq!(bound.iter().map(|h| h.as_ref().map(|h| h.as_raw())).collect::<Vec<_>>(), [Some(shared.as_raw())], "only the shared heap is bound");

            let w3 = u.alloc(0x1000, 256).unwrap() as usize;
            *(w3 as *mut u32) = 0x89AB_CDEF;
            *((w3 + 4) as *mut u32) = 0xCAFE_BABE;
            let (io_buf, res_buf) = (u.alloc(0x1000, 256).unwrap() as usize, u.alloc(0x1000, 256).unwrap() as usize);
            let mut io = vec![0u32; KERNEL_SGPRS];
            io[IO_MEM_VIEWS] = views;
            io[1] = (w3 - u.window(3).0 as usize) as u32;
            io[2] = io[1] + 4;
            std::ptr::copy_nonoverlapping(io.as_ptr(), io_buf as *mut u32, io.len());
            std::ptr::write_bytes(res_buf as *mut u8, 0xFF, 16);
            let hlsl = format!(
                "RWByteAddressBuffer io : register(u0);\nRWByteAddressBuffer res : register(u1);\n[numthreads(1, 1, 1)]\nvoid main()\n{{\n    uint v = io.Load({} * 4);\n    RWByteAddressBuffer w3 = ResourceDescriptorHeap[v + 3];\n    RWByteAddressBuffer none = ResourceDescriptorHeap[v + {}];\n    none.Store(io.Load(8), 0xBADu);\n    res.Store(4, w3.Load(io.Load(4)));\n    res.Store(8, none.Load(io.Load(8)));\n    res.Store(12, none.Load(0));\n}}\n",
                IO_MEM_VIEWS, MEM_NULL_VIEW
            );
            let dxil = crate::gpu::recomp::dxc::compile(&hlsl, "main", "cs_6_6", &[]).unwrap();
            let rs = crate::gpu::recomp::pso::abi_root_signature(&dev, false).unwrap();
            let mut desc = D3D12_COMPUTE_PIPELINE_STATE_DESC {
                pRootSignature: std::mem::ManuallyDrop::new(Some(rs.clone())),
                CS: D3D12_SHADER_BYTECODE { pShaderBytecode: dxil.as_ptr() as *const _, BytecodeLength: dxil.len() },
                ..Default::default()
            };
            let pso: ID3D12PipelineState = dev.CreateComputePipelineState(&desc).unwrap();
            std::mem::ManuallyDrop::drop(&mut desc.pRootSignature);
            let queue: ID3D12CommandQueue = dev.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC::default()).unwrap();
            let alloc: ID3D12CommandAllocator = dev.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT).unwrap();
            let list: ID3D12GraphicsCommandList = dev.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &alloc, Some(&pso)).unwrap();
            let fence: ID3D12Fence = dev.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap();
            list.SetComputeRootSignature(&rs);
            list.SetComputeRootUnorderedAccessView(0, super::super::real::uma_va_of(io_buf).unwrap());
            list.SetComputeRootUnorderedAccessView(1, super::super::real::uma_va_of(res_buf).unwrap());
            list.SetDescriptorHeaps(&bound);
            list.Dispatch(1, 1, 1);
            list.Close().unwrap();
            queue.ExecuteCommandLists(&[Some(list.cast::<ID3D12CommandList>().unwrap())]);
            queue.Signal(&fence, 1).unwrap();
            if fence.GetCompletedValue() < 1 {
                let ev = windows::Win32::System::Threading::CreateEventW(None, false, false, None).unwrap();
                fence.SetEventOnCompletion(1, ev).unwrap();
                windows::Win32::System::Threading::WaitForSingleObject(ev, 5000);
                let _ = windows::Win32::Foundation::CloseHandle(ev);
            }
            let res = std::slice::from_raw_parts(res_buf as *const u32, 4);
            assert_eq!(res[1..], [0x89AB_CDEF, 0, 0], "window 3, the null view after a store, the null view at 0");
            assert_eq!(*((w3 + 4) as *const u32), 0xCAFE_BABE, "a store through the null view lands nowhere");
            unregister(title);
        }
    }
}
