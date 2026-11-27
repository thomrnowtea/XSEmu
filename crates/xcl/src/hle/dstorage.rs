//! `xcrdapi.dll` – the console's DirectStorage: `DStorageGetFactory` → IDStorageFactoryX/X2.
//!
//! The interfaces are the PC DirectStorage ones in the console's vtable order and structure
//! layouts (as observed on Fortnite, which reads all its data through them):
//! * factory: QueryInterface, AddRef, Release, CreateQueue(desc, riid, out), OpenFile(path,
//!   riid, out), CreateStatusArray(capacity, name, riid, out), SetCpuAffinity(mask),
//!   SetDebugFlags(flags), SetStagingBufferSize(bytes), GetStats (unsupported), CreateQueue1,
//! * queue desc (0x18): source type u32 @0 (0 file, 1 memory), capacity u16 @8, priority i8
//!   @10 (-1 … 2; memory queues 2), name @0x10. Desc1 adds AutoSubmitDisabled at 0x18.
//! * request (0x48): options u64 @0 (`DSTORAGE_REQUEST_OPTIONS`: bit 0 `ZlibDecompress`,
//!   bits 1–4 `BcpackMode`, bits 5–20 `SwizzleMode`, bit 21 `DestinationIsPhysicalPages`,
//!   bit 22 `SourceType` (0 file / 1 memory), bit 23 `SourceIsPhysicalPages`),
//!   destination @8 (`void*` virtual/UMA address or `PULONG_PTR DestinationPageArray`),
//!   `DestinationPageOffset` u16 @0x10, destination size u32 @0x18,
//!   source @0x20 (`IDStorageFileX*`, `const void*`, or `PULONG_PTR SourcePageArray`),
//!   file offset u64 / `SourcePageOffset` u16 @0x28, source size u32 @0x30,
//!   intermediate size u32 @0x34, cancellation tag @0x38, name @0x40,
//! * queue: QueryInterface, AddRef, Release, EnqueueRequest, EnqueueStatus(array, index),
//!   EnqueueSignal(fence, value), Submit, CancelRequestsWithTag(mask, value), Close,
//!   GetErrorEvent, RetrieveErrorRecord (0x290 bytes), Query (0x1C: the desc, then empty
//!   slots u16 @0x18 and requests until the automatic submit u16 @0x1A), EnqueueSetEvent, Query1,
//! * file: QueryInterface, AddRef, Release, Close, GetHandle, 2 unknown, GetId, then 4 more;
//!   status array: QueryInterface, AddRef, Release, IsComplete(index), GetHResult(index),
//!   then 5 more.
//!
//! Every queue's submitted entries run on one shared pool of workers (`pool`), shared by queue
//! priority. A queue's entries take their turn in order (file reads through the VFS like the
//! title's own, memory copies, zlib), a status entry gets the first failure of the requests since
//! the previous one, a signal sets the fence from the CPU; file reads and their decompression may
//! run ahead of their turn. Methods not listed are logged and answer E_NOTIMPL. BCPack decodes
//! after zlib when its tables are installed (see `hle::bcpack`); hardware swizzle modes are
//! refused.

#![allow(non_snake_case)]

use std::borrow::Cow;
use std::ffi::c_void;
use std::os::windows::fs::FileExt;
use std::collections::BTreeMap;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex, OnceLock};

use serde::{Deserialize, Serialize};
use windows_sys::core::GUID;
use windows_sys::Win32::Foundation::{CloseHandle, DuplicateHandle, DUPLICATE_SAME_ACCESS, HANDLE};
use windows_sys::Win32::System::Diagnostics::Debug::ReadProcessMemory;
use windows_sys::Win32::System::Memory::{VirtualQuery, MEMORY_BASIC_INFORMATION, MEM_COMMIT};
use windows_sys::Win32::System::Threading::{CreateEventW, GetCurrentProcess, ResetEvent, SetEvent};

use crate::util::{HRESULT, S_OK};

pub mod pool;

const DLL: &str = "xcrdapi.dll";

const E_NOTIMPL: HRESULT = 0x8000_4001u32 as i32;
const E_NOINTERFACE: HRESULT = 0x8000_4002u32 as i32;
const E_POINTER: HRESULT = 0x8000_4003u32 as i32;
const E_PENDING: HRESULT = 0x8000_000Au32 as i32;
const E_INVALIDARG: HRESULT = 0x8007_0057u32 as i32;
const E_ABORT: HRESULT = 0x8000_4004u32 as i32;
const E_QUEUE_CAPACITY: HRESULT = 0x8924_0003u32 as i32;
const E_END_OF_FILE: HRESULT = 0x8924_0007u32 as i32;
const E_ACCESS_VIOLATION: HRESULT = 0x8924_0009u32 as i32;
const E_DESTINATION_SIZE: HRESULT = 0x8924_000Fu32 as i32;
const E_QUEUE_CLOSED: HRESULT = 0x8924_0010u32 as i32;
const E_QUEUE_PRIORITY: HRESULT = 0x8924_0013u32 as i32;
const E_FILE_HANDLE: HRESULT = 0x8924_0017u32 as i32;
const E_SOURCE_TYPE: HRESULT = 0x8924_001Bu32 as i32;
const E_DECOMPRESSION: HRESULT = 0x8924_0030u32 as i32;
const E_ZLIB_DATA: HRESULT = 0x8924_0032u32 as i32;
const E_QUEUE_TYPE: HRESULT = 0x8924_00F5u32 as i32;

const IID_IUNKNOWN: GUID = GUID::from_u128(0x00000000_0000_0000_c000_000000000046);
const IID_FACTORY: GUID = GUID::from_u128(0xfb3b54ac_bf82_463a_b777_73195837864d);
const IID_FILE: GUID = GUID::from_u128(0x43704f9f_27c6_4a9f_8815_d16527894df4);
const IID_QUEUE: GUID = GUID::from_u128(0xa3f6649a_8c2c_4b3a_8b6e_10186a04b9bc);
/// The factory and queue of a newer SDK (a retail game): the same first methods; the factory's slot 10
/// creates a queue from a 0x20-byte desc (a u64 @0x18 more) with `IID_QUEUE_X1`.
const IID_FACTORY_X1: GUID = GUID::from_u128(0x62959d11_d45f_4884_a0a3_f7862ea243ae);
const IID_QUEUE_X1: GUID = GUID::from_u128(0x3c02cb0c_c050_4195_99a1_a1ce94ba33c5);
const IID_STATUS_ARRAY: GUID = GUID::from_u128(0xcd66d752_2786_44f7_9070_e2a6fdad3f1b);

/// Console `DSTORAGE_REQUEST_OPTIONS` bitfields (Xbox Series X|S `dstorage_xs.h`):
/// - bit  0      : `ZlibDecompress` (1 bit: RFC 1950 zlib)
/// - bits 1..=4  : `BcpackMode` (4 bits: 0 = none, 1..=7 = BC1..BC7)
/// - bits 5..=20 : `SwizzleMode` (16 bits: 0 = none, 1..=4 = hardware texture swizzle)
/// - bit  21     : `DestinationIsPhysicalPages` (1 bit)
/// - bit  22     : `SourceType` (1 bit: 0 = file, 1 = memory)
/// - bit  23     : `SourceIsPhysicalPages` (1 bit)
/// - bits 24..=63: `Reserved` (40 bits)
///
/// Unlike PC DirectStorage (`dstorage.h`), Xbox Series X|S is UMA: placed D3D12 buffers and
/// textures live at virtual addresses in UMA or graphics reservations (tracked by
/// `MEM_WRITE_WATCH`), or in 64 KiB physical page arrays from `AllocateTitlePhysicalPages`.
const ZLIB_BIT: u64 = 1 << 0;
const BCPACK_SHIFT: u32 = 1;
const BCPACK_MASK: u64 = 0xF << BCPACK_SHIFT;
const SWIZZLE_SHIFT: u32 = 5;
const SWIZZLE_MASK: u64 = 0xFFFF << SWIZZLE_SHIFT;
const DEST_PHYS_PAGES_BIT: u64 = 1 << 21;
const SOURCE_MEMORY_BIT: u64 = 1 << 22;
const SOURCE_PHYS_PAGES_BIT: u64 = 1 << 23;
const KNOWN_OPTIONS_MASK: u64 = (1u64 << 24) - 1;
const REQUEST_BYTES: usize = 0x48;
const ERROR_RECORD_BYTES: usize = 0x290;
const NO_COMMAND: u32 = 0xFFFF_FFFF;
const COMMAND_REQUEST: u32 = 0;
/// ID3D12Fence::Signal
const FENCE_SIGNAL: usize = 10;

pub fn exports() -> Vec<(&'static str, usize)> {
    crate::hle_exports![DStorageGetFactory]
}

fn same(a: &GUID, b: &GUID) -> bool {
    (a.data1, a.data2, a.data3, a.data4) == (b.data1, b.data2, b.data3, b.data4)
}

fn warn_once(what: &str, why: std::fmt::Arguments) {
    static SEEN: Mutex<Vec<String>> = Mutex::new(Vec::new());
    let mut s = SEEN.lock().unwrap();
    if s.len() < 64 && !s.iter().any(|x| x == what) {
        s.push(what.to_string());
        crate::xlog_warn!("DirectStorage: {what}: {why}");
    }
}

// ---------------------------------------------------------------------------------------
// BCPack refusals (report.json → dstorage)
// ---------------------------------------------------------------------------------------

/// `DSTORAGE_BCPACK_MODE` names (GDK `dstorage_xs.h`); 8..=15 are not in the enum.
const BCPACK_NAMES: [&str; 8] = ["none", "BC1", "BC2", "BC3", "BC4", "BC5", "BC6H", "BC7"];

fn bcpack_name(mode: u8) -> String {
    BCPACK_NAMES.get(mode as usize).map_or_else(|| format!("mode {mode}"), |n| n.to_string())
}

/// BCPack requests refused at enqueue (XCL has no BCPack decoder), by mode.
struct BcpackRefusals {
    requests: [AtomicU64; 16],
    with_zlib: [AtomicU64; 16],
    bytes: [AtomicU64; 16],
    filled_bytes: [AtomicU64; 16],
    total: AtomicU64,
}

impl BcpackRefusals {
    const fn new() -> Self {
        const Z: AtomicU64 = AtomicU64::new(0);
        Self { requests: [Z; 16], with_zlib: [Z; 16], bytes: [Z; 16], filled_bytes: [Z; 16], total: AtomicU64::new(0) }
    }

    /// Count a refused request; the number of BCPack refusals so far, this one included.
    fn note(&self, r: &Request) -> u64 {
        let m = r.bcpack() as usize & 0xF;
        self.requests[m].fetch_add(1, Ordering::Relaxed);
        if r.zlib() {
            self.with_zlib[m].fetch_add(1, Ordering::Relaxed);
        }
        self.bytes[m].fetch_add(r.destination_size as u64, Ordering::Relaxed);
        self.total.fetch_add(1, Ordering::Relaxed) + 1
    }

    fn stats(&self) -> BTreeMap<String, BcpackStats> {
        (1..16u8)
            .filter(|&m| self.requests[m as usize].load(Ordering::Relaxed) > 0)
            .map(|m| {
                let i = m as usize;
                let s = BcpackStats {
                    requests: self.requests[i].load(Ordering::Relaxed),
                    with_zlib: self.with_zlib[i].load(Ordering::Relaxed),
                    destination_bytes: self.bytes[i].load(Ordering::Relaxed),
                    filled_bytes: self.filled_bytes[i].load(Ordering::Relaxed),
                };
                (bcpack_name(m), s)
            })
            .collect()
    }

    fn summary(&self) -> String {
        let s = self.stats();
        let parts: Vec<String> = s.iter().map(|(n, v)| format!("{n} {} ({} bytes)", v.requests, v.destination_bytes)).collect();
        parts.join(", ")
    }
}

static BCPACK: BcpackRefusals = BcpackRefusals::new();

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct BcpackStats {
    pub requests: u64,
    /// of `requests`, zlib then BCPack (two-stage)
    pub with_zlib: u64,
    /// bytes the title's destinations were to receive
    pub destination_bytes: u64,
    /// bytes written by `XCL_DSTORAGE_BCPACK_FILL`
    pub filled_bytes: u64,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct DStorageStats {
    /// BCPack requests refused (no decoder), by `DSTORAGE_BCPACK_MODE`
    pub bcpack_refused: BTreeMap<String, BcpackStats>,
    /// the shared worker pool, by priority
    #[serde(default)]
    pub pool: pool::PoolStats,
}

pub fn stats() -> DStorageStats {
    DStorageStats { bcpack_refused: BCPACK.stats(), pool: pool::stats() }
}

/// Log a refused BCPack request: one warning per mode and zlib stage, then a running count on
/// the first 16 refusals and every power of two.
fn log_bcpack_refusal(r: &Request, k: u64) {
    let bc = r.bcpack();
    let key = format!("BCPack {}{}", bcpack_name(bc), if r.zlib() { " after zlib" } else { "" });
    warn_once(
        &key,
        format_args!(
            "mode {bc} unsupported (src={}, intermediate={}, dst={}, swizzle={}); the request fails with E_DECOMPRESSION",
            r.source_size,
            r.intermediate_size,
            r.destination_size,
            r.swizzle()
        ),
    );
    if k <= 16 || k.is_power_of_two() {
        crate::xlog_info!("DirectStorage: BCPack refusals so far: {k} requests: {}", BCPACK.summary());
    }
}

/// `XCL_DSTORAGE_BCPACK_FILL=1`: a refused BCPack destination gets solid blocks of its BCn
/// format (magenta; red for BC4/BC5) instead of staying as it was, so the title's textures
/// that wait on BCPack show where they are. The request still fails.
fn bcpack_fill_from_env() -> bool {
    std::env::var("XCL_DSTORAGE_BCPACK_FILL").is_ok_and(|v| !v.trim().is_empty() && v.trim() != "0")
}

/// `XCL_DSTORAGE_BCPACK_FILL=succeed`: the filled requests also succeed, so a title whose loading
/// waits on them goes on (with solid textures) until XCL decodes BCPack.
fn bcpack_fill_succeeds() -> bool {
    static S: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *S.get_or_init(|| std::env::var("XCL_DSTORAGE_BCPACK_FILL").is_ok_and(|v| v.trim().eq_ignore_ascii_case("succeed")))
}

/// BC1 colour block: both endpoints magenta (565 `0xF81F`), every index 0.
const BC1_MAGENTA: [u8; 8] = [0x1F, 0xF8, 0x1F, 0xF8, 0, 0, 0, 0];
/// BC4 block: both endpoints 255, every index 0.
const BC4_FULL: [u8; 8] = [0xFF, 0xFF, 0, 0, 0, 0, 0, 0];
/// BC4 block: both endpoints 0.
const BC4_ZERO: [u8; 8] = [0; 8];

/// One solid block of `mode`'s BCn format.
fn bcpack_fill_block(mode: u8) -> Option<Vec<u8>> {
    let cat = |a: &[u8], b: &[u8]| [a, b].concat();
    Some(match mode {
        1 => BC1_MAGENTA.to_vec(),
        2 => cat(&[0xFF; 8], &BC1_MAGENTA),
        3 => cat(&BC4_FULL, &BC1_MAGENTA),
        4 => BC4_FULL.to_vec(),
        5 => cat(&BC4_FULL, &BC4_ZERO),
        // mode 11 (5 bits 00011): one region, 10-bit endpoints r/g/b = 495/0/495 (1.0 in UF16), indices 0
        6 => (3u128 | 495 << 5 | 495 << 25 | 495 << 35 | 495 << 55).to_le_bytes().to_vec(),
        // mode 6 (bit 6): 7-bit endpoints r/g/b/a = 127/0/127/127, both p-bits 1, indices 0
        7 => (1u128 << 6 | 127 << 7 | 127 << 14 | 127 << 35 | 127 << 42 | 127 << 49 | 127 << 56 | 1 << 63 | 1 << 64)
            .to_le_bytes()
            .to_vec(),
        _ => return None,
    })
}

/// Where a refused BCPack request's fill goes: its virtual destination, or the physical pages
/// it names (its page list as at enqueue, read again at its turn as for a request that runs:
/// `page_list_at_turn`).
enum FillTarget {
    Virtual,
    Pages(Vec<usize>, ListPlace),
}

/// Fill the title's destination of `r` (exactly `destination_size` bytes) with solid blocks.
unsafe fn bcpack_fill(r: &Request, target: &FillTarget) -> bool {
    let Some(block) = bcpack_fill_block(r.bcpack()) else { return false };
    let n = r.destination_size as usize;
    let bytes: Vec<u8> = block.iter().copied().cycle().take(n).collect();
    let ok = match target {
        FillTarget::Pages(at_enqueue, place) => {
            let list = page_list_at_turn("destination", r, r.destination as usize, at_enqueue, *place).0;
            destination_pages(r.destination as usize, &list).is_ok_and(|pages| crate::hle::kernelx::write_physical_pages(&pages, r.destination_page_offset as usize, &bytes))
        }
        FillTarget::Virtual => writable(r.destination as usize, n) && write_virtual(r.destination as usize, &bytes),
    };
    if ok {
        BCPACK.filled_bytes[r.bcpack() as usize & 0xF].fetch_add(n as u64, Ordering::Relaxed);
    }
    ok
}

/// A vtable: the listed methods, then logged E_NOTIMPL stubs up to `total` entries.
fn vtable(iface: &str, methods: &[usize], total: usize) -> &'static [usize] {
    let mut v = methods.to_vec();
    for i in methods.len()..total {
        let (stub, _) = crate::stubs::make_stub_info(DLL, &format!("{iface}::#{i}"), E_NOTIMPL as u32 as u64);
        v.push(stub);
    }
    Box::leak(v.into_boxed_slice())
}

/// COM object header: vtable, reference count, the shared part.
#[repr(C)]
struct Object<T> {
    vtbl: *const usize,
    refs: AtomicU32,
    inner: Arc<T>,
}

unsafe fn new_object<T>(vtbl: &'static [usize], inner: Arc<T>) -> *mut c_void {
    Box::into_raw(Box::new(Object { vtbl: vtbl.as_ptr(), refs: AtomicU32::new(1), inner })) as *mut c_void
}

unsafe fn inner<'a, T>(p: *mut c_void) -> &'a Arc<T> {
    &(*(p as *const Object<T>)).inner
}

unsafe fn add_ref<T>(p: *mut c_void) -> u32 {
    (*(p as *const Object<T>)).refs.fetch_add(1, Ordering::Relaxed) + 1
}

unsafe fn release<T>(p: *mut c_void) -> u32 {
    let n = (*(p as *const Object<T>)).refs.fetch_sub(1, Ordering::AcqRel) - 1;
    if n == 0 {
        drop(Box::from_raw(p as *mut Object<T>));
    }
    n
}

unsafe fn no_ref(_p: *mut c_void) -> u32 {
    1
}

unsafe fn query(p: *mut c_void, own: &GUID, riid: *const GUID, out: *mut *mut c_void, add: unsafe fn(*mut c_void) -> u32) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    if !riid.is_null() && (same(&*riid, own) || same(&*riid, &IID_IUNKNOWN)) {
        add(p);
        *out = p;
        return S_OK;
    }
    *out = std::ptr::null_mut();
    // a newer DirectStorage interface version a title asks for: named, so its assert has a cause
    if !riid.is_null() {
        let (r, o) = (&*riid, own);
        crate::xlog_warn!(
            "DirectStorage: QueryInterface({:08x}-{:04x}-{:04x}-{:02x?}) on the object of {:08x}-{:04x}-{:04x} refused (E_NOINTERFACE)",
            r.data1, r.data2, r.data3, r.data4, o.data1, o.data2, o.data3
        );
    }
    E_NOINTERFACE
}

// ---------------------------------------------------------------------------------------
// factory
// ---------------------------------------------------------------------------------------

/// `HRESULT DStorageGetFactory(REFIID riid, void** factory)`
pub unsafe extern "system" fn DStorageGetFactory(riid: *const GUID, out: *mut *mut c_void) -> HRESULT {
    static FACTORY: OnceLock<usize> = OnceLock::new();
    let f = *FACTORY.get_or_init(|| {
        let v = vtable(
            "IDStorageFactoryX",
            &[
                factory_qi as usize,
                static_ref as usize,
                static_ref as usize,
                factory_create_queue as usize,
                factory_open_file as usize,
                factory_create_status_array as usize,
                factory_set_cpu_affinity as usize,
                factory_set_debug_flags as usize,
                factory_set_staging_buffer_size as usize,
                factory_unknown as usize,
                factory_create_queue1 as usize,
            ],
            16,
        );
        Box::into_raw(Box::new(v.as_ptr())) as usize
    });
    factory_qi(f as *mut c_void, riid, out)
}

unsafe extern "system" fn static_ref(_p: *mut c_void) -> u32 {
    1
}

unsafe extern "system" fn factory_qi(p: *mut c_void, riid: *const GUID, out: *mut *mut c_void) -> HRESULT {
    if !riid.is_null() && same(&*riid, &IID_FACTORY_X1) {
        return query(p, &IID_FACTORY_X1, riid, out, no_ref);
    }
    let hr = query(p, &IID_FACTORY, riid, out, no_ref);
    if hr == E_NOINTERFACE && !riid.is_null() {
        // A refused factory IID fails far away in the title (rax=0x80004002); name it here
        // so the run log shows the cause (#161).
        let g = &*riid;
        warn_once(
            "DStorageGetFactory",
            format_args!(
                "refused IID {:08x}-{:04x}-{:04x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x} (E_NOINTERFACE)",
                g.data1, g.data2, g.data3,
                g.data4[0], g.data4[1], g.data4[2], g.data4[3], g.data4[4], g.data4[5], g.data4[6], g.data4[7]
            ),
        );
    }
    hr
}

unsafe extern "system" fn factory_unknown(_p: *mut c_void) -> HRESULT {
    warn_once("IDStorageFactoryX::#9", format_args!("called – answering E_NOTIMPL"));
    E_NOTIMPL
}

unsafe extern "system" fn factory_set_cpu_affinity(_p: *mut c_void, _mask: u64) -> HRESULT {
    S_OK
}

unsafe extern "system" fn factory_set_debug_flags(_p: *mut c_void, _flags: u32) {}

unsafe extern "system" fn factory_set_staging_buffer_size(_p: *mut c_void, bytes: u32) -> HRESULT {
    pool::set_staging(bytes);
    S_OK
}

// ---------------------------------------------------------------------------------------
// files
// ---------------------------------------------------------------------------------------

struct FileInner {
    id: u32,
    path: Vec<u16>,
    file: Mutex<Option<Arc<std::fs::File>>>,
}

impl FileInner {
    fn handle(&self) -> Option<Arc<std::fs::File>> {
        self.file.lock().unwrap().clone()
    }
}

fn file_vtbl() -> &'static [usize] {
    static V: OnceLock<&'static [usize]> = OnceLock::new();
    V.get_or_init(|| {
        vtable(
            "IDStorageFileX",
            &[file_qi as usize, file_add_ref as usize, file_release as usize, file_close as usize, file_get_handle as usize, file_unknown as usize, file_unknown as usize, file_get_id as usize],
            12,
        )
    })
}

unsafe extern "system" fn factory_open_file(_p: *mut c_void, path: *const u16, riid: *const GUID, out: *mut *mut c_void) -> HRESULT {
    static NEXT_ID: AtomicU32 = AtomicU32::new(1);
    if out.is_null() {
        return E_POINTER;
    }
    *out = std::ptr::null_mut();
    let Some(name) = crate::util::from_wide_ptr(path) else { return E_INVALIDARG };
    // through the VFS hooks, as the title's own CreateFileW
    let file = match std::fs::File::open(&name) {
        Ok(f) => f,
        Err(e) => {
            crate::xlog_warn!("DirectStorage: OpenFile '{name}': {e}");
            return e.raw_os_error().map_or(E_INVALIDARG, |c| (0x8007_0000u32 | (c as u32 & 0xFFFF)) as i32);
        }
    };
    let inner = Arc::new(FileInner { id: NEXT_ID.fetch_add(1, Ordering::Relaxed), path: name.encode_utf16().collect(), file: Mutex::new(Some(Arc::new(file))) });
    let o = new_object(file_vtbl(), inner);
    let hr = file_qi(o, riid, out);
    file_release(o);
    hr
}

unsafe extern "system" fn file_qi(p: *mut c_void, riid: *const GUID, out: *mut *mut c_void) -> HRESULT {
    query(p, &IID_FILE, riid, out, add_ref::<FileInner>)
}

unsafe extern "system" fn file_add_ref(p: *mut c_void) -> u32 {
    add_ref::<FileInner>(p)
}

unsafe extern "system" fn file_release(p: *mut c_void) -> u32 {
    release::<FileInner>(p)
}

unsafe extern "system" fn file_close(p: *mut c_void) {
    inner::<FileInner>(p).file.lock().unwrap().take();
}

/// a handle of the title's own (it closes it)
unsafe extern "system" fn file_get_handle(p: *mut c_void) -> HANDLE {
    let Some(f) = inner::<FileInner>(p).handle() else { return std::ptr::null_mut() };
    let mut h: HANDLE = std::ptr::null_mut();
    let me = GetCurrentProcess();
    if DuplicateHandle(me, f.as_raw_handle() as HANDLE, me, &mut h, 0, 0, DUPLICATE_SAME_ACCESS) == 0 {
        return std::ptr::null_mut();
    }
    h
}

unsafe extern "system" fn file_unknown(_p: *mut c_void) -> HRESULT {
    warn_once("IDStorageFileX #5 / #6", format_args!("not modelled"));
    E_NOTIMPL
}

unsafe extern "system" fn file_get_id(p: *mut c_void) -> u32 {
    let f = inner::<FileInner>(p);
    if f.handle().is_some() {
        f.id
    } else {
        u32::MAX
    }
}

// ---------------------------------------------------------------------------------------
// status arrays
// ---------------------------------------------------------------------------------------

struct StatusInner {
    slots: Box<[AtomicI32]>,
}

fn status_vtbl() -> &'static [usize] {
    static V: OnceLock<&'static [usize]> = OnceLock::new();
    V.get_or_init(|| {
        vtable(
            "IDStorageStatusArrayX",
            &[status_qi as usize, status_add_ref as usize, status_release as usize, status_is_complete as usize, status_get_hresult as usize],
            10,
        )
    })
}

unsafe extern "system" fn factory_create_status_array(_p: *mut c_void, capacity: u32, _name: *const u8, riid: *const GUID, out: *mut *mut c_void) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    *out = std::ptr::null_mut();
    if capacity == 0 || capacity > 1 << 20 {
        return E_INVALIDARG;
    }
    let inner = Arc::new(StatusInner { slots: (0..capacity).map(|_| AtomicI32::new(E_PENDING)).collect() });
    let o = new_object(status_vtbl(), inner);
    let hr = status_qi(o, riid, out);
    status_release(o);
    hr
}

unsafe extern "system" fn status_qi(p: *mut c_void, riid: *const GUID, out: *mut *mut c_void) -> HRESULT {
    query(p, &IID_STATUS_ARRAY, riid, out, add_ref::<StatusInner>)
}

unsafe extern "system" fn status_add_ref(p: *mut c_void) -> u32 {
    add_ref::<StatusInner>(p)
}

unsafe extern "system" fn status_release(p: *mut c_void) -> u32 {
    release::<StatusInner>(p)
}

unsafe extern "system" fn status_is_complete(p: *mut c_void, index: u32) -> bool {
    inner::<StatusInner>(p).slots.get(index as usize).is_some_and(|s| s.load(Ordering::Acquire) != E_PENDING)
}

unsafe extern "system" fn status_get_hresult(p: *mut c_void, index: u32) -> HRESULT {
    inner::<StatusInner>(p).slots.get(index as usize).map_or(E_INVALIDARG, |s| s.load(Ordering::Acquire))
}

// ---------------------------------------------------------------------------------------
// requests
// ---------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Request {
    options: u64,
    destination: u64,
    destination_page_offset: u16,
    destination_size: u32,
    source: u64,
    offset: u64,
    source_page_offset: u16,
    source_size: u32,
    intermediate_size: u32,
    tag: u64,
}

impl Request {
    fn parse(b: &[u8; REQUEST_BYTES]) -> Request {
        let q = |o: usize| u64::from_le_bytes(b[o..o + 8].try_into().unwrap());
        let d = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
        let w = |o: usize| u16::from_le_bytes(b[o..o + 2].try_into().unwrap());
        let options = q(0);
        let memory = options & SOURCE_MEMORY_BIT != 0;
        let dest_phys = options & DEST_PHYS_PAGES_BIT != 0;
        let src_phys = memory && (options & SOURCE_PHYS_PAGES_BIT != 0);
        Request {
            options,
            destination: q(8),
            destination_page_offset: if dest_phys { w(0x10) } else { 0 },
            destination_size: d(0x18),
            source: q(0x20),
            offset: if memory { 0 } else { q(0x28) },
            source_page_offset: if src_phys { w(0x28) } else { 0 },
            source_size: d(0x30),
            intermediate_size: d(0x34),
            tag: q(0x38),
        }
    }

    fn zlib(&self) -> bool {
        self.options & ZLIB_BIT != 0
    }

    fn bcpack(&self) -> u8 {
        ((self.options & BCPACK_MASK) >> BCPACK_SHIFT) as u8
    }

    fn swizzle(&self) -> u16 {
        ((self.options & SWIZZLE_MASK) >> SWIZZLE_SHIFT) as u16
    }

    fn dest_physical_pages(&self) -> bool {
        self.options & DEST_PHYS_PAGES_BIT != 0
    }

    fn memory_source(&self) -> bool {
        self.options & SOURCE_MEMORY_BIT != 0
    }

    fn source_physical_pages(&self) -> bool {
        self.options & SOURCE_PHYS_PAGES_BIT != 0
    }

    /// What a queue of `source_memory` refuses at enqueue.
    fn check(&self, source_memory: bool) -> Result<(), HRESULT> {
        if self.memory_source() != source_memory || (self.source_physical_pages() && !self.memory_source()) {
            return Err(E_SOURCE_TYPE);
        }
        if self.destination == 0 || (self.memory_source() && self.source == 0) {
            return Err(E_ACCESS_VIOLATION);
        }
        if self.options & !KNOWN_OPTIONS_MASK != 0 {
            warn_once("DSTORAGE_REQUEST_OPTIONS", format_args!("unknown high option bits {:#x}", self.options & !KNOWN_OPTIONS_MASK));
        }
        if self.swizzle() != 0 {
            warn_once("SwizzleMode", format_args!("hardware swizzle mode {} unsupported", self.swizzle()));
            return Err(E_DECOMPRESSION);
        }
        match (self.zlib(), self.bcpack()) {
            // Uncompressed copy: source and destination sizes must match.
            (false, 0) => {
                if self.destination_size != self.source_size || (self.intermediate_size != 0 && self.intermediate_size != self.destination_size) {
                    return Err(E_DESTINATION_SIZE);
                }
            }
            // Zlib decompression: if `intermediate_size > 0`, inflate into an intermediate buffer
            // of that size (`source_size < intermediate_size <= destination_size`) before copying.
            (true, 0) => {
                if self.intermediate_size == 0 {
                    if self.destination_size <= self.source_size {
                        return Err(E_DESTINATION_SIZE);
                    }
                } else if self.intermediate_size <= self.source_size || self.intermediate_size > self.destination_size {
                    return Err(E_DESTINATION_SIZE);
                }
            }
            // Two-stage zlib -> BCPack: validate intermediate bounds when supplied, then BCPack
            // needs its tables (a refusal is counted and logged at enqueue, `BcpackRefusals`).
            (true, bc) => {
                if self.intermediate_size != 0 && (self.intermediate_size <= self.source_size || self.destination_size <= self.intermediate_size) {
                    return Err(E_DESTINATION_SIZE);
                }
                if bcpack_tables(bc).is_none() {
                    return Err(E_DECOMPRESSION);
                }
            }
            // Single-stage BCPack: `intermediate_size` must be 0, then BCPack needs its tables.
            (false, bc) => {
                if self.intermediate_size != 0 {
                    return Err(E_DESTINATION_SIZE);
                }
                if bcpack_tables(bc).is_none() {
                    return Err(E_DECOMPRESSION);
                }
            }
        }
        Ok(())
    }
}

/// Where a request's physical page list was when the request was enqueued.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ListPlace {
    /// on the stack of the thread that called EnqueueRequest: gone when the call returned
    CallersStack,
    /// anywhere else: memory the title keeps (a slot of its own table)
    Kept,
}

impl ListPlace {
    /// Of the list of `pages` entries at `addr`, asked on the thread that enqueues the request.
    fn of(addr: u64, pages: usize) -> ListPlace {
        let (mut lo, mut hi) = (0usize, 0usize);
        unsafe { windows_sys::Win32::System::Threading::GetCurrentThreadStackLimits(&mut lo, &mut hi) };
        let (start, len) = (addr as usize, (pages * std::mem::size_of::<usize>()).max(1));
        if start < hi && start.saturating_add(len) > lo {
            ListPlace::CallersStack
        } else {
            ListPlace::Kept
        }
    }
}

/// What a page list is at its request's turn, against what it was at EnqueueRequest.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ListChange {
    /// the same, not readable any more, or on the enqueuing thread's stack (not looked at)
    Same,
    /// other pages, all allocated: those are the request's
    Taken,
    /// something else that names no allocated pages: the list as enqueued stands
    NotPages,
}

/// A request's physical page list at the request's turn. The console reads the list when it runs
/// the request, not at EnqueueRequest: a retail title keeps its lists in slots of its own table
/// and writes a slot's pages after EnqueueRequest, so at enqueue a slot can still name the pages
/// of the request it held before – pages the title has given to something else by the time this
/// request runs. Written by the list as enqueued, a texture's data went into a page mapped
/// elsewhere as other data, and the request succeeded. (Upstream: xcl-dev/xcl#1405.)
///
/// So the list is read again, every entry of it, and taken as it is then when all it names are
/// allocated pages. The list as enqueued (`at_enqueue`) stands in two cases:
///  - it was on the enqueuing thread's stack (`ListPlace::CallersStack`). That memory is given to
///    the thread's next calls when EnqueueRequest returns: what is there at the request's turn is
///    not the list, and a number there may well be an allocated page. A caller that builds its
///    list on its stack means the list as it calls.
///  - what is there now does not name allocated pages (the memory was freed or reused, or an
///    entry is still 0 – those are read again one by one, `destination_pages`).
fn page_list_now<'a>(addr: usize, at_enqueue: &'a [usize], place: ListPlace) -> (Cow<'a, [usize]>, ListChange) {
    let mut now = vec![0usize; at_enqueue.len()];
    if place == ListPlace::CallersStack || at_enqueue.is_empty() || read_page_indices(addr, &mut now).is_err() || now[..] == at_enqueue[..] {
        return (Cow::Borrowed(at_enqueue), ListChange::Same);
    }
    if !crate::hle::kernelx::pages_allocated(&now) {
        return (Cow::Borrowed(at_enqueue), ListChange::NotPages);
    }
    (Cow::Owned(now), ListChange::Taken)
}

/// Page lists that were not at their request's turn what they were at EnqueueRequest, so far.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PageListCounts {
    /// taken as at the turn, with an entry that named a page at enqueue and another at the turn:
    /// as enqueued, the request would have gone to a page of the slot's previous list
    pub stale: u64,
    /// of `stale`: such a page at enqueue was mapped somewhere at the turn – in use
    pub stale_in_use: u64,
    /// taken as at the turn, with only entries filled in that were 0 at enqueue and still at
    /// Submit (those were always read at the turn, `destination_pages`; most are read at Submit)
    pub filled: u64,
    /// something that names no allocated pages at the turn: the list as enqueued stood
    pub not_pages: u64,
}

static LISTS_CHANGED: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];

pub fn page_list_counters() -> PageListCounts {
    let n = |i: usize| LISTS_CHANGED[i].load(Ordering::Relaxed);
    PageListCounts { stale: n(0), stale_in_use: n(1), filled: n(2), not_pages: n(3) }
}

/// `page_list_now` for the request `r` at its turn (`which` list: "destination" or "source"),
/// counted and logged: the first lists that changed are shown entry by entry (8 with an entry
/// that named a page at enqueue, 2 with only entries that were 0 until then), then the counts at each
/// power of two. With it, what the list was, for the record of the request's payloads (`alias`).
fn page_list_at_turn<'a>(which: &'static str, r: &Request, addr: usize, at_enqueue: &'a [usize], place: ListPlace) -> (Cow<'a, [usize]>, crate::alias::PageList) {
    use crate::alias::PageList;
    use crate::hle::kernelx::{page_places, pages_allocated};
    let (list, change) = page_list_now(addr, at_enqueue, place);
    let add = |i: usize, n: bool| LISTS_CHANGED[i].fetch_add(u64::from(n), Ordering::Relaxed) + u64::from(n);
    let read = match change {
        ListChange::Same if place == ListPlace::CallersStack => PageList::OnStack,
        ListChange::Same => PageList::Same,
        ListChange::NotPages => {
            let k = add(3, true);
            if k.is_power_of_two() {
                crate::xlog_info!("DirectStorage: {k} page lists held something else than allocated pages at their request's turn (the latest: the {which} list of a request with options {:#x}): the list as enqueued is used", r.options);
            }
            PageList::NotPages
        }
        ListChange::Taken => {
            // (entries that named a page at enqueue first: those are the ones that went wrong)
            let (stale, filled): (Vec<usize>, Vec<usize>) = (0..list.len()).filter(|&i| list[i] != at_enqueue[i]).partition(|&i| at_enqueue[i] != 0);
            let in_use = stale.iter().any(|&i| page_places(at_enqueue[i]) > 0);
            let (stales, _, filleds) = (add(0, !stale.is_empty()), add(1, in_use), add(2, stale.is_empty()));
            let nth = if stale.is_empty() { filleds } else { stales };
            if nth <= if stale.is_empty() { 2 } else { 8 } {
                let entries: Vec<String> = stale
                    .iter()
                    .chain(&filled)
                    .take(4)
                    .map(|&i| match at_enqueue[i] {
                        0 => format!("entry {i}: 0 -> page {:#x}", list[i]),
                        was => format!(
                            "entry {i}: page {was:#x} ({}, mapped at {} address(es) now) -> page {:#x}",
                            if pages_allocated(&[was]) { "allocated" } else { "not allocated" },
                            page_places(was),
                            list[i]
                        ),
                    })
                    .collect();
                crate::xlog_info!(
                    "DirectStorage: a {which} page list changed between EnqueueRequest and its request's turn ({}): request with options {:#x}, {} bytes at page offset {} of its destination, {} source bytes at {:#x}; list of {} at {addr:#x}, {} entries differ, {} of them named a page at enqueue: {}{}. The list as it is at the request's turn is used, as on the console",
                    if stale.is_empty() {
                        format!("#{nth} with only entries that were 0 at enqueue and at Submit, which were always read at the turn")
                    } else {
                        format!("#{nth} with an entry that named a page at enqueue: as enqueued, the request would have used that page")
                    },
                    r.options,
                    r.destination_size,
                    r.destination_page_offset,
                    r.source_size,
                    r.offset,
                    list.len(),
                    stale.len() + filled.len(),
                    stale.len(),
                    entries.join("; "),
                    if stale.len() + filled.len() > entries.len() { "; ..." } else { "" }
                );
            }
            if nth.is_power_of_two() {
                let c = page_list_counters();
                crate::xlog_info!(
                    "DirectStorage: page lists that changed between EnqueueRequest and their request's turn so far: {} with an entry that named a page at enqueue and another at the turn ({} of them a page mapped somewhere at the turn), {} with only entries that were 0 at enqueue and at Submit, {} that held no pages at the turn and stood as enqueued",
                    c.stale,
                    c.stale_in_use,
                    c.filled,
                    c.not_pages
                );
            }
            PageList::Changed { entries: (stale.len() + filled.len()).min(255) as u8, stale: stale.len().min(255) as u8 }
        }
    };
    (list, read)
}

/// A `PULONG_PTR` physical page array as it is at enqueue time (`page_list_now`: read again at
/// the request's turn, unless it is on the caller's stack).
unsafe fn snapshot_page_array(ptr: u64, offset: u16, byte_len: u32) -> Result<Vec<usize>, HRESULT> {
    if byte_len == 0 {
        return Ok(Vec::new());
    }
    let Some(span) = (offset as usize).checked_add(byte_len as usize) else {
        return Err(E_DESTINATION_SIZE);
    };
    let count = span.div_ceil(crate::hle::kernelx::PHYS_PAGE_SIZE);
    let mut pages = vec![0usize; count];
    read_page_indices(ptr as usize, &mut pages)?;
    Ok(pages)
}

fn read_page_indices(addr: usize, pages: &mut [usize]) -> Result<(), HRESULT> {
    if !unsafe { read_own(addr, pages.as_mut_ptr().cast(), std::mem::size_of_val(pages)) } {
        return Err(E_ACCESS_VIOLATION);
    }
    Ok(())
}

fn destination_pages(addr: usize, snapshot: &[usize]) -> Result<Cow<'_, [usize]>, HRESULT> {
    if !snapshot.contains(&0) {
        return Ok(Cow::Borrowed(snapshot));
    }
    let mut pages = snapshot.to_vec();
    let mut start = 0;
    while let Some(index) = snapshot[start..].iter().position(|&page| page == 0) {
        start += index;
        let end = start + snapshot[start..].iter().take_while(|&&page| page == 0).count();
        let at = addr.checked_add(start * std::mem::size_of::<usize>()).ok_or(E_ACCESS_VIOLATION)?;
        read_page_indices(at, &mut pages[start..end])?;
        if pages[start..end].contains(&0) {
            return Err(E_ACCESS_VIOLATION);
        }
        start = end;
    }
    Ok(Cow::Owned(pages))
}

unsafe fn read_own(at: usize, out: *mut u8, len: usize) -> bool {
    let mut done = 0usize;
    at != 0 && ReadProcessMemory(GetCurrentProcess(), at as *const c_void, out as *mut c_void, len, &mut done) != 0 && done == len
}

/// Whether `n` bytes at `p` are committed, writable memory.
fn writable(p: usize, n: usize) -> bool {
    let Some(end) = p.checked_add(n) else { return false };
    crate::hle::kernelx::lazy::ensure(p, n);
    if crate::gpu::uma::get().is_some_and(|u| u.in_committed_window(p, n)) {
        return true;
    }
    if let Some(a) = crate::memmap::access(p, n) {
        return a == crate::memmap::Access::Write;
    }
    let mut at = p;
    while at < end {
        let mut m: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        if unsafe { VirtualQuery(at as *const c_void, &mut m, std::mem::size_of_val(&m)) } == 0 {
            return false;
        }
        let rw = 0x04 | 0x08 | 0x40 | 0x80; // PAGE_READWRITE / WRITECOPY / EXECUTE_READWRITE / EXECUTE_WRITECOPY
        if m.State != MEM_COMMIT || m.Protect & rw == 0 || m.Protect & 0x100 != 0 {
            return false;
        }
        at = m.BaseAddress as usize + m.RegionSize;
    }
    true
}

#[link(name = "ntdll", kind = "raw-dylib")]
extern "system" {
    fn NtWriteVirtualMemory(process: HANDLE, base: *mut c_void, buffer: *const c_void, len: usize, written: *mut usize) -> i32;
}

unsafe fn write_virtual(destination: usize, payload: &[u8]) -> bool {
    if payload.is_empty() {
        return true;
    }
    let mut written = 0;
    let status = NtWriteVirtualMemory(
        GetCurrentProcess(), destination as *mut c_void, payload.as_ptr().cast(), payload.len(), &mut written,
    );
    crate::hle::kernelx::note_written(destination, written);
    crate::eh::note_guest_write("DirectStorage", destination, written);
    // (kept with its hash: at a crash the memory is checked against it, `alias::report`)
    crate::alias::payload_written(destination, &payload[..written.min(payload.len())]);
    status >= 0 && written == payload.len()
}

fn read_exact_at(f: &std::fs::File, offset: u64, buf: &mut [u8]) -> Result<(), HRESULT> {
    let mut done = 0;
    while done < buf.len() {
        match f.seek_read(&mut buf[done..], offset + done as u64) {
            Ok(0) => return Err(E_END_OF_FILE),
            Ok(n) => done += n,
            Err(e) if e.raw_os_error() == Some(38) => return Err(E_END_OF_FILE), // ERROR_HANDLE_EOF
            Err(e) => return Err(e.raw_os_error().map_or(E_ABORT, |c| (0x8007_0000u32 | (c as u32 & 0xFFFF)) as i32)),
        }
    }
    Ok(())
}

static REQUESTS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static BYTES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static FAILED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Every queue the title made, for `waiting_for_submit`.
static QUEUES: Mutex<Vec<std::sync::Weak<QueueInner>>> = Mutex::new(Vec::new());

/// Entries enqueued but not yet submitted, over all open queues: a title waiting on a status or
/// fence entry that sits here waits for a Submit (or the half-full auto-submit) that never comes.
pub fn waiting_for_submit() -> usize {
    let mut queues = QUEUES.lock().unwrap();
    queues.retain(|q| q.strong_count() > 0);
    queues.iter().filter_map(|q| q.upgrade()).map(|q| q.pending.lock().unwrap().entries.len()).sum()
}

/// (requests run, bytes delivered, requests failed) so far
pub fn counters() -> (u64, u64, u64) {
    (REQUESTS.load(Ordering::Relaxed), BYTES.load(Ordering::Relaxed), FAILED.load(Ordering::Relaxed))
}

/// Run a request at its turn and report any short write as a failure.
/// (`dest_pages`, `src_pages`: the request's page lists as at enqueue, and where they were;
/// `read`: its payload when a worker read it ahead, `pool`; `decode`: how long loading it took
/// here, when it was not.)
unsafe fn execute(
    r: &Request,
    file: Option<&FileInner>,
    dest_pages: Option<(&[usize], ListPlace)>,
    src_pages: Option<(&[usize], ListPlace)>,
    read: Option<Result<Vec<u8>, HRESULT>>,
    decode: &mut Option<std::time::Duration>,
) -> Result<(), HRESULT> {
    let n = r.destination_size as usize;
    {
        // the first requests: where a title's data goes and how it is packed
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let k = N.fetch_add(1, Ordering::Relaxed);
        REQUESTS.fetch_add(1, Ordering::Relaxed);
        BYTES.fetch_add(n as u64, Ordering::Relaxed);
        if k < 16 || k.is_power_of_two() {
            crate::xlog_info!(
                "DirectStorage: request #{k}: options {:#x} ({} source{}), destination {:#x}{} ({n} bytes, page offset {}), source size {}, intermediate {}",
                r.options,
                if r.memory_source() { "memory" } else { "file" },
                if r.zlib() { ", zlib" } else { "" },
                r.destination,
                if r.dest_physical_pages() { " (physical pages)" } else { "" },
                r.destination_page_offset,
                r.source_size,
                r.intermediate_size
            );
        }
    }
    if !r.dest_physical_pages() && !writable(r.destination as usize, n) {
        return Err(E_ACCESS_VIOLATION);
    }
    let payload = match read {
        Some(read) => {
            file.and_then(|f| f.handle()).ok_or(E_FILE_HANDLE)?;
            read?
        }
        None => {
            let t = std::time::Instant::now();
            let loaded = load(r, file, src_pages);
            *decode = Some(t.elapsed());
            loaded?
        }
    };

    let request = || {
        format!(
            "request with options {:#x}: {} bytes at {:#x} of {}",
            r.options,
            r.source_size,
            r.offset,
            if r.memory_source() { format!("memory at {:#x}", r.source) } else { file.map(|f| String::from_utf16_lossy(&f.path)).unwrap_or_default() }
        )
    };
    let (list, list_was) = match dest_pages {
        Some((at_enqueue, place)) if r.dest_physical_pages() => {
            let (list, was) = page_list_at_turn("destination", r, r.destination as usize, at_enqueue, place);
            (Some(list), was)
        }
        _ => (None, crate::alias::PageList::None),
    };
    let written_for = crate::alias::Request { options: r.options, offset: if r.memory_source() { r.source } else { r.offset }, source_size: r.source_size, list: list_was };
    crate::alias::with_request(written_for, || crate::eh::with_write_context(request, || {
        let written = if r.dest_physical_pages() {
            let pages = destination_pages(r.destination as usize, list.as_deref().ok_or(E_ACCESS_VIOLATION)?)?;
            pool::timed(pool::Stage::Write, || crate::hle::kernelx::write_physical_pages(&pages, r.destination_page_offset as usize, &payload[..n]), |_| n)
        } else {
            pool::timed(pool::Stage::Write, || write_virtual(r.destination as usize, &payload[..n]), |_| n)
        };
        if !written {
            return Err(E_ACCESS_VIOLATION);
        }
        Ok(())
    }))
}

/// A request's payload: its source read (a file through the VFS, or memory at its turn) and
/// decompressed, `destination_size` bytes or more. A worker may run this for a file request
/// ahead of the request's turn (`pool`); its result waits for the turn.
unsafe fn load(r: &Request, file: Option<&FileInner>, src_pages: Option<(&[usize], ListPlace)>) -> Result<Vec<u8>, HRESULT> {
    let n = r.destination_size as usize;
    let src = pool::timed(pool::Stage::Read, || read_source(r, file, src_pages), |_| r.source_size as usize)?;

    if r.swizzle() != 0 {
        return Err(E_DECOMPRESSION);
    }

    Ok(if r.bcpack() != 0 {
        unpack_bcpack(r, &src)?
    } else if r.zlib() {
        let stage1_len = if r.intermediate_size != 0 { r.intermediate_size as usize } else { n };
        let mut d = inflate(&src, stage1_len)?;
        if d.len() != stage1_len {
            return Err(E_ZLIB_DATA);
        }
        if d.len() < n {
            d.resize(n, 0);
        }
        d
    } else {
        src
    })
}

/// zlib (RFC 1950) into at most `limit` bytes, timed as its stage.
fn inflate(src: &[u8], limit: usize) -> Result<Vec<u8>, HRESULT> {
    pool::timed(pool::Stage::Inflate, || miniz_oxide::inflate::decompress_to_vec_zlib_with_limit(src, limit).map_err(|_| E_ZLIB_DATA), |d| d.as_ref().map_or(0, Vec::len))
}

/// A request's source bytes: from its file, or from the title's memory at its turn.
unsafe fn read_source(r: &Request, file: Option<&FileInner>, src_pages: Option<(&[usize], ListPlace)>) -> Result<Vec<u8>, HRESULT> {
    Ok(if r.memory_source() {
        let mut b = vec![0u8; r.source_size as usize];
        if r.source_physical_pages() {
            let (at_enqueue, place) = src_pages.ok_or(E_ACCESS_VIOLATION)?;
            let pages = page_list_at_turn("source", r, r.source as usize, at_enqueue, place).0;
            if !crate::hle::kernelx::read_physical_pages(&pages, r.source_page_offset as usize, &mut b) {
                return Err(E_ACCESS_VIOLATION);
            }
        } else {
            if !crate::gpu::shader::readable(r.source as usize, r.source_size as usize) {
                return Err(E_ACCESS_VIOLATION);
            }
            let mut read = 0;
            let ok = ReadProcessMemory(GetCurrentProcess(), r.source as *const c_void, b.as_mut_ptr().cast(), b.len(), &mut read);
            if ok == 0 || read != b.len() {
                return Err(E_ACCESS_VIOLATION);
            }
        }
        b
    } else {
        let f = file.and_then(|f| f.handle()).ok_or(E_FILE_HANDLE)?;
        let mut b = vec![0u8; r.source_size as usize];
        read_exact_at(&f, r.offset, &mut b)?;
        b
    })
}

/// `XCL_DS_VALIDATE=1`: after each request, whether it could have broken memory it does not own –
/// every process heap validated (`HeapValidate`), XCL's physical page bookkeeping checked, and the
/// destination still mapped where the request wrote it (`destination_size` bytes, never more: the
/// payload is cut to it). The first failure is logged with the request and a thread dump, then the
/// checks stop. The checks run on a thread of their own: one that takes over 5 s (a heap lock held
/// elsewhere) is reported the same way, naming the step it is on.
unsafe fn validate_after(r: &Request, dest_pages: Option<(&[usize], ListPlace)>, result: Result<(), HRESULT>) {
    static ON: OnceLock<bool> = OnceLock::new();
    static FAILED_ONCE: AtomicBool = AtomicBool::new(false);
    static STEP: Mutex<String> = Mutex::new(String::new());
    type Job = (Box<dyn FnOnce() -> Option<String> + Send>, mpsc::Sender<Option<String>>);
    static CHECKER: OnceLock<Mutex<mpsc::Sender<Job>>> = OnceLock::new();
    if !*ON.get_or_init(|| std::env::var("XCL_DS_VALIDATE").is_ok_and(|v| v != "0")) || FAILED_ONCE.load(Ordering::Acquire) {
        return;
    }
    let n = r.destination_size as usize;
    let destination = r.destination as usize;
    let physical = r.dest_physical_pages();
    let pages: Option<(Vec<usize>, ListPlace)> = dest_pages.map(|(s, place)| (s.to_vec(), place));
    let ok = result.is_ok();
    let check = move || -> Option<String> {
        use windows_sys::Win32::System::Memory::{GetProcessHeaps, HeapValidate};
        let step = |s: String| *STEP.lock().unwrap() = s;
        // the heaps at most every 250 ms (a walk holds each heap's lock: every allocation in the
        // process waits for it); a failure names the requests since the last walk
        static LAST_WALK: Mutex<Option<(std::time::Instant, u64)>> = Mutex::new(None);
        let request = REQUESTS.load(Ordering::Relaxed);
        let since = {
            let mut last = LAST_WALK.lock().unwrap();
            match *last {
                Some((t, _)) if t.elapsed() < std::time::Duration::from_millis(250) => None,
                prev => {
                    *last = Some((std::time::Instant::now(), request));
                    Some(prev.map_or(0, |p| p.1))
                }
            }
        };
        if let Some(since) = since {
            let mut heaps = vec![std::ptr::null_mut(); 256];
            let count = unsafe { GetProcessHeaps(heaps.len() as u32, heaps.as_mut_ptr()) } as usize;
            for (i, &h) in heaps[..count.min(heaps.len())].iter().enumerate() {
                step(format!("validating process heap {i} of {count} ({:#x})", h as usize));
                if unsafe { HeapValidate(h, 0, std::ptr::null()) } == 0 {
                    return Some(format!("process heap {i} of {count} ({:#x}) does not validate (requests #{since}..#{request} since the last good walk)", h as usize));
                }
            }
        }
        step("checking the physical page bookkeeping".into());
        if let Err(e) = crate::hle::kernelx::validate_physical() {
            return Some(format!("physical page bookkeeping: {e}"));
        }
        step("checking the destination".into());
        if !ok {
            return None;
        }
        if physical {
            return match pages.as_ref().map(|(s, place)| destination_pages(destination, &page_list_now(destination, s, *place).0).map(Cow::into_owned)) {
                Some(Ok(p)) if crate::hle::kernelx::pages_allocated(&p) => None,
                Some(Ok(_)) => Some("a destination physical page is no longer allocated".into()),
                _ => Some("the destination page array no longer resolves".into()),
            };
        }
        (n > 0 && !writable(destination, n)).then(|| format!("the destination {destination:#x}..{:#x} is no longer mapped", destination + n))
    };
    let tx = CHECKER.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<Job>();
        let _ = std::thread::Builder::new().name("xcl-ds-validate".into()).spawn(move || {
            for (job, reply) in rx {
                let _ = reply.send(job());
            }
        });
        Mutex::new(tx)
    });
    let (reply, answer) = mpsc::channel();
    if tx.lock().unwrap().send((Box::new(check), reply)).is_err() {
        return;
    }
    let why = match answer.recv_timeout(std::time::Duration::from_secs(5)) {
        Ok(None) => return,
        Ok(Some(why)) => why,
        Err(_) => format!("the checks did not finish in 5 s ({})", STEP.lock().unwrap()),
    };
    if FAILED_ONCE.swap(true, Ordering::AcqRel) {
        return;
    }
    crate::xlog_error!(
        "XCL_DS_VALIDATE: after request #{} ({:?}): {why}. Options {:#x} ({} source{}{}), destination {:#x}{} ({n} bytes written at most, page offset {}), source {:#x} offset {:#x} size {}, intermediate {}; checks stop",
        REQUESTS.load(Ordering::Relaxed),
        result.err().map(|hr| format!("{hr:#x}")),
        r.options,
        if r.memory_source() { "memory" } else { "file" },
        if r.zlib() { ", zlib" } else { "" },
        if r.bcpack() != 0 { format!(", BCPack {}", r.bcpack()) } else { String::new() },
        r.destination,
        if r.dest_physical_pages() { " (physical pages)" } else { "" },
        r.destination_page_offset,
        r.source,
        r.offset,
        r.source_size,
        r.intermediate_size
    );
    crate::eh::dump_threads();
}

/// The BCPack tables when the decoder handles mode `bc` and they are installed.
fn bcpack_tables(bc: u8) -> Option<&'static crate::hle::bcpack::Tables> {
    crate::hle::bcpack::tables().filter(|_| crate::hle::bcpack::supported(bc))
}

/// BCPack into a destination-sized buffer, after zlib for a two-stage request (the console's
/// order). The zlib stage inflates into at most `intermediate_size` bytes (the destination size
/// when 0); BCPack ignores input past what it needs, so a shorter stage is fine.
fn unpack_bcpack(r: &Request, src: &[u8]) -> Result<Vec<u8>, HRESULT> {
    let bc = r.bcpack();
    let tables = bcpack_tables(bc).ok_or(E_DECOMPRESSION)?;
    let inflated;
    let packed = if r.zlib() {
        let limit = if r.intermediate_size != 0 { r.intermediate_size } else { r.destination_size };
        inflated = inflate(src, limit as usize)?;
        &inflated[..]
    } else {
        src
    };
    let mut out = vec![0u8; r.destination_size as usize];
    let len = out.len();
    pool::timed(pool::Stage::Bcpack, || crate::hle::bcpack::decode(bc, packed, &mut out, tables), |_| len).map_err(|e| {
        warn_once(&format!("BCPack BC{bc}"), format_args!("{e} (packed {} bytes, destination {})", packed.len(), out.len()));
        E_DECOMPRESSION
    })?;
    Ok(out)
}

// ---------------------------------------------------------------------------------------
// queues
// ---------------------------------------------------------------------------------------

/// RAII wrapper holding one COM reference to an `ID3D12Fence` so it is never leaked on
/// queue close, cancellation, or drop.
struct FenceRef(usize);

impl FenceRef {
    unsafe fn signal(&self, value: u64) {
        let vt = *(self.0 as *const *const usize);
        let signal: unsafe extern "system" fn(usize, u64) -> HRESULT = std::mem::transmute(*vt.add(FENCE_SIGNAL));
        signal(self.0, value);
    }
}

impl Drop for FenceRef {
    fn drop(&mut self) {
        unsafe {
            let vt = *(self.0 as *const *const usize);
            let release: unsafe extern "system" fn(usize) -> u32 = std::mem::transmute(*vt.add(2));
            release(self.0);
        }
    }
}

enum Entry {
    Request {
        seq: u64,
        req: Request,
        file: Option<Arc<FileInner>>,
        dest_pages: Option<Vec<usize>>,
        src_pages: Option<Vec<usize>>,
        /// where the destination and the source page list were at enqueue
        places: (ListPlace, ListPlace),
    },
    /// refused at enqueue: reported in queue order by the worker
    Refused {
        hr: HRESULT,
        req: Request,
        file: Option<Arc<FileInner>>,
        /// a refused BCPack request under `XCL_DSTORAGE_BCPACK_FILL`: where its solid blocks go
        fill: Option<FillTarget>,
    },
    Status(Arc<StatusInner>, u32),
    /// a fence (one reference held) and the value it gets
    Signal(FenceRef, u64),
    SetEvent(OwnedHandle),
    #[cfg(test)]
    /// Test-only barrier to hold the worker deterministically while testing submitted-batch cancellation.
    Gate(Arc<std::sync::Barrier>),
}

struct Pending {
    entries: Vec<Entry>,
    next_seq: u64,
    late_destinations: usize,
}

impl Entry {
    fn has_late_destination(&self) -> bool {
        matches!(self, Self::Request { dest_pages: Some(pages), .. } if pages.contains(&0))
    }
}

struct CancelRule {
    /// Cancels requests with `seq <= up_to_seq` (enqueued before this `CancelRequestsWithTag` call).
    up_to_seq: u64,
    mask: u64,
    value: u64,
}

struct FirstError {
    hr: HRESULT,
    request: Request,
    file: Vec<u16>,
}

struct QueueInner {
    desc: [u8; 0x18],
    auto_submit: bool,
    source_memory: bool,
    capacity: usize,
    priority: pool::Priority,
    /// this queue's lane in the shared worker pool
    lane: u64,
    pending: Mutex<Pending>,
    space: Condvar,
    cancels: Mutex<Vec<CancelRule>>,
    closed: AtomicBool,
    error_event: usize,
    errors: Mutex<(u32, Option<FirstError>)>,
    /// `XCL_DSTORAGE_BCPACK_FILL` when the queue was made
    bcpack_fill: AtomicBool,
}

impl QueueInner {
    fn fail(&self, hr: HRESULT, request: Request, file: Option<&FileInner>) {
        FAILED.fetch_add(1, Ordering::Relaxed);
        let mut e = self.errors.lock().unwrap();
        e.0 += 1;
        if e.1.is_none() {
            crate::xlog_warn!("DirectStorage: a request failed ({hr:#x}): {} bytes from {}", request.destination_size, file.map(|f| String::from_utf16_lossy(&f.path)).unwrap_or_default());
            e.1 = Some(FirstError { hr, request, file: file.map(|f| f.path.clone()).unwrap_or_default() });
        }
        unsafe { SetEvent(self.error_event as HANDLE) };
    }

    fn is_cancelled(&self, seq: u64, tag: u64) -> bool {
        let mut rules = self.cancels.lock().unwrap();
        // Requests take their turn in strictly increasing `seq` order, so rules with
        // `up_to_seq < seq` can never match this or any later request.
        rules.retain(|r| r.up_to_seq >= seq);
        rules.iter().any(|r| tag & r.mask == r.value)
    }

    /// Whether a cancel already covers the request, asked by its read-ahead (out of turn, so the
    /// rules are left as they are for the turn).
    fn cancels_ahead(&self, seq: u64, tag: u64) -> bool {
        self.cancels.lock().unwrap().iter().any(|r| r.up_to_seq >= seq && tag & r.mask == r.value)
    }

    fn submit(self: &Arc<Self>, explicit: bool) {
        // The pending lock is held into the pool, so concurrent `Submit()` calls hand over their
        // batches in strict `seq` order and none follows `close`.
        let mut pending = self.pending.lock().unwrap();
        if self.closed.load(Ordering::Acquire) {
            return;
        }
        if !explicit && (pending.late_destinations != 0 || pending.entries.len() < self.capacity / 2) {
            return;
        }
        if pending.late_destinations != 0 {
            for entry in &mut pending.entries {
                let Entry::Request { req, dest_pages: Some(pages), .. } = entry else { continue };
                if !pages.contains(&0) {
                    continue;
                }
                let Ok(current) = (unsafe { snapshot_page_array(req.destination, req.destination_page_offset, req.destination_size) }) else { continue };
                for (saved, now) in pages.iter_mut().zip(current) {
                    if *saved == 0 {
                        *saved = now;
                    }
                }
            }
            pending.late_destinations = 0;
        }
        let batch = std::mem::take(&mut pending.entries);
        if batch.is_empty() {
            return;
        }
        self.space.notify_all();
        let refused = pool::submit(self.lane, batch);
        // Release `pending` before dropping a batch the pool did not take, so fence `Release` cannot
        // re-enter under the lock.
        drop(pending);
        drop(refused);
    }

    /// One entry's turn on a pool worker, in queue order; `since` is the first failure since the
    /// last status entry, and what it is after this entry is returned. `read` is the payload read
    /// ahead for a file request, `decode` gets the time loading it took at the turn otherwise.
    fn turn(&self, e: Entry, read: Option<Result<Vec<u8>, HRESULT>>, mut since: HRESULT, decode: &mut Option<std::time::Duration>) -> HRESULT {
        if self.closed.load(Ordering::Acquire) {
            if let Entry::Status(s, i) = e {
                if let Some(slot) = s.slots.get(i as usize) {
                    slot.store(E_QUEUE_CLOSED, Ordering::Release);
                }
            }
            return since;
        }
        match e {
            Entry::Request { seq, req, file, dest_pages, src_pages, places } => {
                if self.is_cancelled(seq, req.tag) {
                    // Cancelled requests are skipped without affecting status or error record.
                    return since;
                }
                let dest = dest_pages.as_deref().map(|p| (p, places.0));
                let result = unsafe { execute(&req, file.as_deref(), dest, src_pages.as_deref().map(|p| (p, places.1)), read, decode) };
                unsafe { validate_after(&req, dest, result) };
                if let Err(hr) = result {
                    self.fail(hr, req, file.as_deref());
                    if since >= 0 {
                        since = hr;
                    }
                }
            }
            Entry::Refused { hr, req, file, fill } => {
                if let Some(t) = &fill {
                    // the destination gets solid blocks; the request still fails, unless
                    // XCL_DSTORAGE_BCPACK_FILL=succeed
                    let filled = unsafe { bcpack_fill(&req, t) };
                    if filled && bcpack_fill_succeeds() {
                        return since;
                    }
                }
                self.fail(hr, req, file.as_deref());
                if since >= 0 {
                    since = hr;
                }
            }
            Entry::Status(s, i) => {
                if let Some(slot) = s.slots.get(i as usize) {
                    slot.store(since, Ordering::Release);
                }
                since = S_OK;
            }
            Entry::Signal(fence, value) => unsafe {
                fence.signal(value);
            },
            Entry::SetEvent(event) => unsafe {
                SetEvent(event.as_raw_handle());
            },
            #[cfg(test)]
            Entry::Gate(b) => {
                b.wait();
            }
        }
        since
    }

    fn enqueue(self: &Arc<Self>, mut e: Entry) {
        let auto_submit = {
            let mut p = self.pending.lock().unwrap();
            while !self.auto_submit && p.entries.len() >= self.capacity - 1 && !self.closed.load(Ordering::Acquire) {
                p = self.space.wait(p).unwrap();
            }
            if self.closed.load(Ordering::Acquire) {
                return;
            }
            if let Entry::Request { seq, .. } = &mut e {
                *seq = p.next_seq;
                p.next_seq += 1;
            } else if let Entry::Status(ref s, i) = e {
                if let Some(slot) = s.slots.get(i as usize) {
                    slot.store(E_PENDING, Ordering::Release);
                }
            }
            p.late_destinations += usize::from(e.has_late_destination());
            p.entries.push(e);
            self.auto_submit && p.late_destinations == 0 && p.entries.len() >= self.capacity / 2
        };
        // as on PC: a half-full queue submits by itself
        if auto_submit {
            self.submit(false);
        }
    }
    /// Close(), and the last Release() (DirectStorage closes a queue when its last reference
    /// goes): what is not through its turn yet ends as E_QUEUE_CLOSED, and no request writes into
    /// the caller's memory once this has returned.
    fn close(&self) {
        // `closed` is set under the pending lock, which `submit` holds into the pool, so no racing
        // enqueue or submit hands the pool new entries after this.
        let drained = {
            let mut pend = self.pending.lock().unwrap();
            if self.closed.swap(true, Ordering::AcqRel) {
                Vec::new()
            } else {
                self.space.notify_all();
                pend.late_destinations = 0;
                std::mem::take(&mut pend.entries)
            }
        };
        for e in drained {
            if let Entry::Status(s, i) = e {
                if let Some(slot) = s.slots.get(i as usize) {
                    slot.store(E_QUEUE_CLOSED, Ordering::Release);
                }
            }
            // `Entry::Signal` drops `FenceRef` (calling Release); `Entry::Request` drops `FileInner`.
        }
        // Outside the queue's locks: submitted entries are dropped, and a turn running on a worker
        // ends before this returns (for concurrent callers too).
        pool::close(self.lane);
    }
}

impl Drop for QueueInner {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.error_event as HANDLE) };
    }
}

fn queue_vtbl() -> &'static [usize] {
    static V: OnceLock<&'static [usize]> = OnceLock::new();
    V.get_or_init(|| {
        vtable(
            "IDStorageQueueX",
            &[
                queue_qi as usize,
                queue_add_ref as usize,
                queue_release as usize,
                queue_enqueue_request as usize,
                queue_enqueue_status as usize,
                queue_enqueue_signal as usize,
                queue_submit as usize,
                queue_cancel as usize,
                queue_close as usize,
                queue_get_error_event as usize,
                queue_retrieve_error_record as usize,
                queue_query as usize,
                queue_enqueue_set_event as usize,
                queue_query1 as usize,
            ],
            18,
        )
    })
}

unsafe extern "system" fn factory_create_queue(_p: *mut c_void, desc: *const u8, riid: *const GUID, out: *mut *mut c_void) -> HRESULT {
    create_queue(desc, false, riid, out)
}

unsafe extern "system" fn factory_create_queue1(_p: *mut c_void, desc: *const u8, riid: *const GUID, out: *mut *mut c_void) -> HRESULT {
    create_queue(desc, true, riid, out)
}

unsafe fn create_queue(desc: *const u8, extended: bool, riid: *const GUID, out: *mut *mut c_void) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    *out = std::ptr::null_mut();
    if !crate::gpu::shader::readable(desc as usize, if extended { 0x20 } else { 0x18 }) {
        return E_INVALIDARG;
    }
    let raw: [u8; 0x18] = (desc as *const [u8; 0x18]).read_unaligned();
    let options = if extended { desc.add(0x18).cast::<u64>().read_unaligned() } else { 0 };
    if options & !1 != 0 {
        return E_INVALIDARG;
    }
    let source = u32::from_le_bytes(raw[0..4].try_into().unwrap());
    let capacity = u16::from_le_bytes(raw[8..10].try_into().unwrap()) as usize;
    let priority = raw[10] as i8;
    if source > 1 {
        return E_QUEUE_TYPE;
    }
    if !(-1..=2).contains(&priority) || (source == 1 && priority != 2) {
        return E_QUEUE_PRIORITY;
    }
    if capacity < 2 {
        return E_QUEUE_CAPACITY;
    }
    let ev = CreateEventW(std::ptr::null(), 1, 0, std::ptr::null());
    if ev.is_null() {
        return E_ABORT;
    }
    let inner = Arc::new(QueueInner {
        desc: raw,
        auto_submit: options == 0,
        source_memory: source == 1,
        capacity,
        priority: pool::Priority::of(priority),
        lane: pool::new_lane_id(),
        pending: Mutex::new(Pending { entries: Vec::new(), next_seq: 1, late_destinations: 0 }),
        space: Condvar::new(),
        cancels: Mutex::new(Vec::new()),
        closed: AtomicBool::new(false),
        error_event: ev as usize,
        errors: Mutex::new((0, None)),
        bcpack_fill: AtomicBool::new(bcpack_fill_from_env()),
    });
    QUEUES.lock().unwrap().push(Arc::downgrade(&inner));
    pool::register(&inner);
    let o = new_object(queue_vtbl(), inner);
    let hr = queue_qi(o, riid, out);
    queue_release(o);
    hr
}

unsafe fn queue<'a>(p: *mut c_void) -> &'a Arc<QueueInner> {
    inner::<QueueInner>(p)
}

unsafe extern "system" fn queue_qi(p: *mut c_void, riid: *const GUID, out: *mut *mut c_void) -> HRESULT {
    if !riid.is_null() && same(&*riid, &IID_QUEUE_X1) {
        return query(p, &IID_QUEUE_X1, riid, out, add_ref::<QueueInner>);
    }
    query(p, &IID_QUEUE, riid, out, add_ref::<QueueInner>)
}

unsafe extern "system" fn queue_add_ref(p: *mut c_void) -> u32 {
    add_ref::<QueueInner>(p)
}

unsafe extern "system" fn queue_release(p: *mut c_void) -> u32 {
    let obj = p as *mut Object<QueueInner>;
    let n = (*obj).refs.fetch_sub(1, Ordering::AcqRel) - 1;
    if n == 0 {
        let obj = Box::from_raw(obj);
        // DirectStorage closes the queue when its last reference is released; that also takes
        // its lane out of the worker pool, which drops the pool's Arc<QueueInner>.
        obj.inner.close();
        drop(obj);
    }
    n
}

unsafe extern "system" fn queue_enqueue_request(p: *mut c_void, req: *const u8) {
    let q = queue(p);
    if q.closed.load(Ordering::Acquire) || req.is_null() {
        return;
    }
    let r = Request::parse(&*(req as *const [u8; REQUEST_BYTES]));
    if let Err(hr) = r.check(q.source_memory) {
        warn_once("EnqueueRequest", format_args!("refused ({hr:#x}): {r:x?}"));
        let mut fill = None;
        if hr == E_DECOMPRESSION && r.bcpack() != 0 {
            log_bcpack_refusal(&r, BCPACK.note(&r));
            if q.bcpack_fill.load(Ordering::Relaxed) {
                fill = if !r.dest_physical_pages() {
                    Some(FillTarget::Virtual)
                } else {
                    snapshot_page_array(r.destination, r.destination_page_offset, r.destination_size).ok().map(|pages| {
                        let place = ListPlace::of(r.destination, pages.len());
                        FillTarget::Pages(pages, place)
                    })
                };
            }
        }
        return q.enqueue(Entry::Refused { hr, req: r, file: None, fill });
    }
    let file = if r.memory_source() {
        None
    } else {
        // only a file this factory opened
        let f = r.source as *mut c_void;
        let mut vt = 0usize;
        if !read_own(f as usize, &mut vt as *mut usize as *mut u8, 8) || vt != file_vtbl().as_ptr() as usize {
            return q.enqueue(Entry::Refused { hr: E_FILE_HANDLE, req: r, file: None, fill: None });
        }
        Some(Arc::clone(inner::<FileInner>(f)))
    };
    let dest_pages = if r.dest_physical_pages() {
        match snapshot_page_array(r.destination, r.destination_page_offset, r.destination_size) {
            Ok(pages) => Some(pages),
            Err(hr) => {
                return q.enqueue(Entry::Refused { hr, req: r, file, fill: None });
            }
        }
    } else {
        None
    };
    let src_pages = if r.memory_source() && r.source_physical_pages() {
        match snapshot_page_array(r.source, r.source_page_offset, r.source_size) {
            Ok(pages) => Some(pages),
            Err(hr) => {
                return q.enqueue(Entry::Refused { hr, req: r, file, fill: None });
            }
        }
    } else {
        None
    };
    let place = |addr: u64, pages: &Option<Vec<usize>>| pages.as_ref().map_or(ListPlace::Kept, |p| ListPlace::of(addr, p.len()));
    let places = (place(r.destination, &dest_pages), place(r.source, &src_pages));
    q.enqueue(Entry::Request { seq: 0, req: r, file, dest_pages, src_pages, places });
}

unsafe extern "system" fn queue_enqueue_status(p: *mut c_void, array: *mut c_void, index: u32) {
    let q = queue(p);
    if q.closed.load(Ordering::Acquire)
        || array.is_null()
        || !crate::gpu::shader::readable(array as usize, std::mem::size_of::<usize>())
        || *(array as *const *const usize) != status_vtbl().as_ptr()
    {
        return;
    }
    let s = Arc::clone(inner::<StatusInner>(array));
    q.enqueue(Entry::Status(s, index));
}

unsafe extern "system" fn queue_enqueue_signal(p: *mut c_void, fence: *mut c_void, value: u64) {
    let q = queue(p);
    if q.closed.load(Ordering::Acquire) || fence.is_null() || !crate::gpu::shader::readable(fence as usize, std::mem::size_of::<usize>()) {
        return;
    }
    let vt = *(fence as *const *const usize);
    if vt.is_null() || !crate::gpu::shader::readable(vt as usize, (FENCE_SIGNAL + 1) * std::mem::size_of::<usize>()) {
        return;
    }
    let add: unsafe extern "system" fn(*mut c_void) -> u32 = std::mem::transmute(*vt.add(1));
    add(fence);
    q.enqueue(Entry::Signal(FenceRef(fence as usize), value));
}

unsafe extern "system" fn queue_enqueue_set_event(p: *mut c_void, event: HANDLE) {
    let mut h: HANDLE = std::ptr::null_mut();
    let me = GetCurrentProcess();
    if DuplicateHandle(me, event, me, &mut h, 0, 0, DUPLICATE_SAME_ACCESS) != 0 {
        queue(p).enqueue(Entry::SetEvent(OwnedHandle::from_raw_handle(h)));
    }
}

unsafe extern "system" fn queue_submit(p: *mut c_void) {
    let q = queue(p);
    if !q.closed.load(Ordering::Acquire) {
        q.submit(true);
    }
}

/// Requests enqueued before this call (both pending and already submitted but not yet
/// executed) whose tag matches (`tag & mask == value`) are skipped without error.
unsafe extern "system" fn queue_cancel(p: *mut c_void, mask: u64, value: u64) {
    let q = queue(p);
    let (cutoff, unblocked) = {
        let mut pend = q.pending.lock().unwrap();
        let blocked = pend.late_destinations != 0;
        pend.entries.retain(|e| !matches!(e, Entry::Request { req, .. } if req.tag & mask == value));
        if blocked {
            pend.late_destinations = pend.entries.iter().filter(|e| e.has_late_destination()).count();
        }
        q.space.notify_all();
        (pend.next_seq.saturating_sub(1), blocked && pend.late_destinations == 0)
    };
    if cutoff == 0 {
        return;
    }
    let mut rules = q.cancels.lock().unwrap();
    if let Some(existing) = rules.iter_mut().find(|r| r.mask == mask && r.value == value) {
        existing.up_to_seq = existing.up_to_seq.max(cutoff);
    } else {
        rules.push(CancelRule { up_to_seq: cutoff, mask, value });
    }
    drop(rules);
    if q.auto_submit && unblocked {
        q.submit(false);
    }
}

unsafe extern "system" fn queue_close(p: *mut c_void) {
    queue(p).close();
}

/// Returns a duplicated event handle owned by the caller (who closes it with `CloseHandle`).
unsafe extern "system" fn queue_get_error_event(p: *mut c_void) -> HANDLE {
    let mut h: HANDLE = std::ptr::null_mut();
    let me = GetCurrentProcess();
    if DuplicateHandle(me, queue(p).error_event as HANDLE, me, &mut h, 0, 0, DUPLICATE_SAME_ACCESS) == 0 {
        return std::ptr::null_mut();
    }
    h
}

/// DSTORAGE_ERROR_RECORD (0x290): failure count u32 @0, then the first failure: HRESULT @8,
/// command type @0xC (none: 0xFFFFFFFF), the request's options @0x10, destination @0x18 and
/// size @0x20, its file name @0x24 (0x104 UTF-16) or memory source @0x238
unsafe extern "system" fn queue_retrieve_error_record(p: *mut c_void, out: *mut u8) {
    if out.is_null() {
        return;
    }
    let q = queue(p);
    let mut rec = [0u8; ERROR_RECORD_BYTES];
    {
        let mut e = q.errors.lock().unwrap();
        match &e.1 {
            Some(f) => {
                rec[0..4].copy_from_slice(&e.0.to_le_bytes());
                rec[8..12].copy_from_slice(&f.hr.to_le_bytes());
                rec[0xC..0x10].copy_from_slice(&COMMAND_REQUEST.to_le_bytes());
                rec[0x10..0x18].copy_from_slice(&f.request.options.to_le_bytes());
                rec[0x18..0x20].copy_from_slice(&f.request.destination.to_le_bytes());
                rec[0x20..0x24].copy_from_slice(&f.request.destination_size.to_le_bytes());
                if f.request.memory_source() {
                    rec[0x238..0x240].copy_from_slice(&f.request.source.to_le_bytes());
                } else {
                    for (i, c) in f.file.iter().take(0x103).enumerate() {
                        rec[0x24 + 2 * i..0x26 + 2 * i].copy_from_slice(&c.to_le_bytes());
                    }
                }
            }
            None => rec[0xC..0x10].copy_from_slice(&NO_COMMAND.to_le_bytes()),
        }
        e.0 = 0;
        e.1 = None;
        ResetEvent(q.error_event as HANDLE);
    }
    std::ptr::copy_nonoverlapping(rec.as_ptr(), out, ERROR_RECORD_BYTES);
}

/// DSTORAGE_QUEUE_INFO (0x1C): the queue's desc, empty slots, requests until it submits by itself
unsafe extern "system" fn queue_query(p: *mut c_void, out: *mut u8) {
    if out.is_null() {
        return;
    }
    let q = queue(p);
    let p = q.pending.lock().unwrap();
    let pending = p.entries.len();
    let empty = q.capacity.saturating_sub(pending) as u16;
    let until = if q.auto_submit && p.late_destinations == 0 { (q.capacity / 2).saturating_sub(pending).max(1) as u16 } else { 0 };
    drop(p);
    let mut info = [0u8; 0x1C];
    info[..0x18].copy_from_slice(&q.desc);
    info[0x18..0x1A].copy_from_slice(&empty.to_le_bytes());
    info[0x1A..0x1C].copy_from_slice(&until.to_le_bytes());
    std::ptr::copy_nonoverlapping(info.as_ptr(), out, info.len());
}

unsafe extern "system" fn queue_query1(p: *mut c_void, out: *mut u8) {
    if out.is_null() {
        return;
    }
    let q = queue(p);
    let p = q.pending.lock().unwrap();
    let pending = p.entries.len();
    let empty = (q.capacity - 1).saturating_sub(pending) as u16;
    let until = if q.auto_submit && p.late_destinations == 0 { (q.capacity / 2).saturating_sub(pending).max(1) as u16 } else { 0 };
    drop(p);
    let mut info = [0u8; 0x24];
    info[..0x18].copy_from_slice(&q.desc);
    info[0x18] = u8::from(!q.auto_submit);
    info[0x20..0x22].copy_from_slice(&empty.to_le_bytes());
    info[0x22..0x24].copy_from_slice(&until.to_le_bytes());
    std::ptr::copy_nonoverlapping(info.as_ptr(), out, info.len());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn factory_x2_creates_a_queue_that_waits_for_submit() {
        use windows_sys::Win32::Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT};
        use windows_sys::Win32::System::Threading::WaitForSingleObject;

        let factory_iid = GUID::from_u128(0x62959d11_d45f_4884_a0a3_f7862ea243ae);
        let queue_iid = GUID::from_u128(0x3c02cb0c_c050_4195_99a1_a1ce94ba33c5);
        let name = crate::pe::ImportName::Name { hint: 0, name: "DStorageGetFactory".into() };
        assert_eq!(crate::resolver::classify(DLL, &name), crate::resolver::Source::Hle(DLL.into()));
        let f = crate::resolver::resolve_import(DLL, &name, "synthetic-storage-x2", false);
        let get_factory: unsafe extern "system" fn(*const GUID, *mut *mut c_void) -> HRESULT = unsafe { std::mem::transmute(f) };
        unsafe {
            let mut factory = std::ptr::null_mut();
            assert_eq!(get_factory(&factory_iid, &mut factory), S_OK);
            let fv = *(factory as *const *const usize);
            let staging_size: unsafe extern "system" fn(*mut c_void, u32) -> HRESULT = std::mem::transmute(*fv.add(8));
            assert_eq!(staging_size(factory, 32 << 20), S_OK);
            let create: unsafe extern "system" fn(*mut c_void, *const u8, *const GUID, *mut *mut c_void) -> HRESULT = std::mem::transmute(*fv.add(10));
            let mut desc = [0u8; 0x20];
            desc[0] = 1;
            desc[8] = 4;
            desc[10] = 2;
            desc[0x18] = 2;
            let mut q = std::ptr::null_mut();
            assert_eq!(create(factory, desc.as_ptr(), &queue_iid, &mut q), E_INVALIDARG);
            assert!(q.is_null());
            desc[0x18] = 1;
            assert_eq!(create(factory, desc.as_ptr(), &queue_iid, &mut q), S_OK);
            let qv = *(q as *const *const usize);
            let enqueue: unsafe extern "system" fn(*mut c_void, *const u8) = std::mem::transmute(*qv.add(3));
            let submit: unsafe extern "system" fn(*mut c_void) = std::mem::transmute(*qv.add(6));
            let enqueue_event: unsafe extern "system" fn(*mut c_void, HANDLE) = std::mem::transmute(*qv.add(12));
            let query1: unsafe extern "system" fn(*mut c_void, *mut u8) = std::mem::transmute(*qv.add(13));
            let signal = CreateEventW(std::ptr::null(), 0, 0, std::ptr::null());
            assert!(!signal.is_null());
            let payload = b"queue X2";
            let mut dest = [0u8; 8];
            let req = request(SOURCE_MEMORY_BIT, dest.as_mut_ptr() as u64, 8, payload.as_ptr() as u64, 0, 8);
            enqueue(q, req.as_ptr());
            enqueue_event(q, signal);
            let mut info = [0u8; 0x28];
            query1(q, info.as_mut_ptr());
            assert_eq!(info[..0x20], desc);
            assert_eq!(info[0x20..0x24], [1, 0, 0, 0]);
            assert_eq!(WaitForSingleObject(signal, 50), WAIT_TIMEOUT);
            submit(q);
            assert_eq!(WaitForSingleObject(signal, 5000), WAIT_OBJECT_0);
            assert_eq!(&dest, payload);
            queue_close(q);
            queue_release(q);
            CloseHandle(signal);
        }
    }

    fn request(options: u64, dst: u64, dst_size: u32, src: u64, offset: u64, src_size: u32) -> [u8; REQUEST_BYTES] {
        let mut b = [0u8; REQUEST_BYTES];
        b[0..8].copy_from_slice(&options.to_le_bytes());
        b[8..16].copy_from_slice(&dst.to_le_bytes());
        b[0x18..0x1C].copy_from_slice(&dst_size.to_le_bytes());
        b[0x20..0x28].copy_from_slice(&src.to_le_bytes());
        b[0x28..0x30].copy_from_slice(&offset.to_le_bytes());
        b[0x30..0x34].copy_from_slice(&src_size.to_le_bytes());
        b
    }

    #[test]
    fn a_linear_memory_source_requires_the_entire_range() {
        use windows_sys::Win32::System::Memory::{VirtualAlloc, VirtualFree, MEM_DECOMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE};
        unsafe {
            let src = VirtualAlloc(std::ptr::null(), 0x2000, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE).cast::<u8>();
            assert!(!src.is_null());
            let mut dest = [0x7E; 8];
            let data = [1u8, 2, 3, 4, 5, 6, 7, 8];
            src.add(0xFFC).copy_from_nonoverlapping(data.as_ptr(), data.len());
            let req = Request::parse(&request(SOURCE_MEMORY_BIT, dest.as_mut_ptr() as u64, 8, src.add(0xFFC) as u64, 0, 8));
            let whole = execute(&req, None, None, None, None, &mut None);
            let copied = dest;
            dest.fill(0x7E);
            let decommitted = VirtualFree(src.add(0x1000).cast(), 0x1000, MEM_DECOMMIT);
            let partial = execute(&req, None, None, None, None, &mut None);
            VirtualFree(src.cast(), 0, MEM_RELEASE);
            assert_eq!(whole, Ok(()));
            assert_eq!(copied, data);
            assert_ne!(decommitted, 0);
            assert_eq!(partial, Err(E_ACCESS_VIOLATION));
            assert_eq!(dest, [0x7E; 8]);
        }
    }

    #[test]
    fn a_refused_factory_iid_returns_e_nointerface_and_clears_out() {
        unsafe {
            // The two IIDs the factory answers.
            let mut f = std::ptr::null_mut();
            assert_eq!(DStorageGetFactory(&IID_FACTORY, &mut f), S_OK);
            let mut fx = std::ptr::null_mut();
            assert_eq!(DStorageGetFactory(&IID_FACTORY_X1, &mut fx), S_OK);
            // An IID the factory does not answer: refused with E_NOINTERFACE, out pointer
            // cleared, and (per #161) the refusal is named in the log by warn_once.
            let unknown = GUID::from_u128(0xdead_beef_0000_4000_8000_001122334455);
            let mut p = 1usize as *mut c_void; // non-null on entry, must be cleared
            assert_eq!(DStorageGetFactory(&unknown, &mut p), E_NOINTERFACE);
            assert!(p.is_null(), "a refused QueryInterface clears the out pointer");
        }
    }

    #[test]
    fn requests_are_checked_like_the_console() {
        let ok = Request::parse(&request(0, 0x1000, 64, 0x2000, 0, 64));
        assert_eq!(ok.check(false), Ok(()));
        assert_eq!(ok.check(true), Err(E_SOURCE_TYPE));
        let mem = Request::parse(&request(SOURCE_MEMORY_BIT | 1, 0x1000, 64, 0x2000, 0x99, 20));
        assert_eq!((mem.offset, mem.check(true)), (0, Ok(())), "a memory source has no offset");
        assert_eq!(Request::parse(&request(1, 0x1000, 20, 0x2000, 0, 20)).check(false), Err(E_DESTINATION_SIZE));
        assert_eq!(Request::parse(&request(3, 0x1000, 64, 0x2000, 0, 20)).check(false), Err(E_DECOMPRESSION), "BCPack");
        assert_eq!(Request::parse(&request(0, 0, 64, 0x2000, 0, 64)).check(false), Err(E_ACCESS_VIOLATION));
    }

    #[test]
    fn a_memory_queue_inflates_zlib_and_reports_through_status_and_errors() {
        unsafe {
            let mut f = std::ptr::null_mut();
            assert_eq!(DStorageGetFactory(&IID_FACTORY, &mut f), S_OK);
            let vt = *(f as *const *const usize);
            let call = |i: usize| *vt.add(i);
            let mut desc = [0u8; 0x18];
            desc[0..4].copy_from_slice(&1u32.to_le_bytes());
            desc[8..10].copy_from_slice(&8u16.to_le_bytes());
            desc[10] = 2;
            let create_queue: unsafe extern "system" fn(*mut c_void, *const u8, *const GUID, *mut *mut c_void) -> HRESULT = std::mem::transmute(call(3));
            let mut q = std::ptr::null_mut();
            assert_eq!(create_queue(f, desc.as_ptr(), &IID_QUEUE, &mut q), S_OK);
            desc[10] = 0;
            let mut bad = std::ptr::null_mut();
            assert_eq!(create_queue(f, desc.as_ptr(), &IID_QUEUE, &mut bad), E_QUEUE_PRIORITY, "a memory queue is realtime");
            let create_status: unsafe extern "system" fn(*mut c_void, u32, *const u8, *const GUID, *mut *mut c_void) -> HRESULT = std::mem::transmute(call(5));
            let mut s = std::ptr::null_mut();
            assert_eq!(create_status(f, 2, std::ptr::null(), &IID_STATUS_ARRAY, &mut s), S_OK);

            let plain = b"XCL DirectStorage zlib payload, XCL DirectStorage zlib payload".to_vec();
            let packed = miniz_oxide::deflate::compress_to_vec_zlib(&plain, 6);
            let mut dst = vec![0u8; plain.len()];
            let qv = *(q as *const *const usize);
            let enqueue: unsafe extern "system" fn(*mut c_void, *const u8) = std::mem::transmute(*qv.add(3));
            let status: unsafe extern "system" fn(*mut c_void, *mut c_void, u32) = std::mem::transmute(*qv.add(4));
            let submit: unsafe extern "system" fn(*mut c_void) = std::mem::transmute(*qv.add(6));
            enqueue(q, request(SOURCE_MEMORY_BIT | 1, dst.as_mut_ptr() as u64, dst.len() as u32, packed.as_ptr() as u64, 0, packed.len() as u32).as_ptr());
            status(q, s, 0);
            // a refused request: the next status reports it, the error record names it
            enqueue(q, request(SOURCE_MEMORY_BIT, dst.as_mut_ptr() as u64, 8, packed.as_ptr() as u64, 0, 4).as_ptr());
            status(q, s, 1);
            submit(q);
            let sv = *(s as *const *const usize);
            let complete: unsafe extern "system" fn(*mut c_void, u32) -> bool = std::mem::transmute(*sv.add(3));
            let hresult: unsafe extern "system" fn(*mut c_void, u32) -> HRESULT = std::mem::transmute(*sv.add(4));
            let t = std::time::Instant::now();
            while !complete(s, 1) && t.elapsed().as_secs() < 5 {
                std::thread::yield_now();
            }
            assert_eq!((hresult(s, 0), hresult(s, 1)), (S_OK, E_DESTINATION_SIZE));
            assert_eq!(dst, plain);
            let record: unsafe extern "system" fn(*mut c_void, *mut u8) = std::mem::transmute(*qv.add(10));
            let mut rec = [0u8; ERROR_RECORD_BYTES];
            record(q, rec.as_mut_ptr());
            assert_eq!((u32::from_le_bytes(rec[0..4].try_into().unwrap()), i32::from_le_bytes(rec[8..12].try_into().unwrap())), (1, E_DESTINATION_SIZE));
            let qinfo: unsafe extern "system" fn(*mut c_void, *mut u8) = std::mem::transmute(*qv.add(11));
            let mut info = [0u8; 0x1C];
            qinfo(q, info.as_mut_ptr());
            assert_eq!((info[10], u16::from_le_bytes([info[0x18], info[0x19]])), (2, 8));
            queue_close(q);
            queue_release(q);
            status_release(s);
        }
    }

    fn request_ex(
        options: u64,
        dst: u64,
        dst_page_offset: u16,
        dst_size: u32,
        src: u64,
        offset_or_page_off: u64,
        src_size: u32,
        intermediate_size: u32,
        tag: u64,
    ) -> [u8; REQUEST_BYTES] {
        let mut b = request(options, dst, dst_size, src, offset_or_page_off, src_size);
        b[0x10..0x12].copy_from_slice(&dst_page_offset.to_le_bytes());
        b[0x34..0x38].copy_from_slice(&intermediate_size.to_le_bytes());
        b[0x38..0x40].copy_from_slice(&tag.to_le_bytes());
        b
    }

    #[repr(C)]
    struct MockFence {
        vtbl: *const usize,
        refs: AtomicU32,
        signalled: std::sync::atomic::AtomicU64,
    }

    fn mock_fence_vtbl() -> &'static [usize] {
        unsafe extern "system" fn qi(_p: *mut c_void, _riid: *const GUID, _out: *mut *mut c_void) -> HRESULT {
            E_NOINTERFACE
        }
        unsafe extern "system" fn add(p: *mut c_void) -> u32 {
            (*(p as *const MockFence)).refs.fetch_add(1, Ordering::Relaxed) + 1
        }
        unsafe extern "system" fn rel(p: *mut c_void) -> u32 {
            (*(p as *const MockFence)).refs.fetch_sub(1, Ordering::AcqRel) - 1
        }
        unsafe extern "system" fn sig(p: *mut c_void, val: u64) -> HRESULT {
            (*(p as *const MockFence)).signalled.store(val, Ordering::Release);
            S_OK
        }
        static V: OnceLock<&'static [usize]> = OnceLock::new();
        V.get_or_init(|| {
            let mut v = vec![0usize; FENCE_SIGNAL + 1];
            v[0] = qi as usize;
            v[1] = add as usize;
            v[2] = rel as usize;
            v[FENCE_SIGNAL] = sig as usize;
            Box::leak(v.into_boxed_slice())
        })
    }

    #[test]
    fn cancel_pending_and_submitted_requests_by_tag() {
        unsafe {
            let mut f = std::ptr::null_mut();
            assert_eq!(DStorageGetFactory(&IID_FACTORY, &mut f), S_OK);
            let vt = *(f as *const *const usize);
            let create_queue: unsafe extern "system" fn(*mut c_void, *const u8, *const GUID, *mut *mut c_void) -> HRESULT = std::mem::transmute(*vt.add(3));
            let create_status: unsafe extern "system" fn(*mut c_void, u32, *const u8, *const GUID, *mut *mut c_void) -> HRESULT = std::mem::transmute(*vt.add(5));

            let mut desc = [0u8; 0x18];
            desc[0..4].copy_from_slice(&1u32.to_le_bytes()); // memory queue
            desc[8..10].copy_from_slice(&32u16.to_le_bytes());
            desc[10] = 2;
            let mut q = std::ptr::null_mut();
            assert_eq!(create_queue(f, desc.as_ptr(), &IID_QUEUE, &mut q), S_OK);
            let mut s = std::ptr::null_mut();
            assert_eq!(create_status(f, 3, std::ptr::null(), &IID_STATUS_ARRAY, &mut s), S_OK);

            let qv = *(q as *const *const usize);
            let enqueue: unsafe extern "system" fn(*mut c_void, *const u8) = std::mem::transmute(*qv.add(3));
            let status: unsafe extern "system" fn(*mut c_void, *mut c_void, u32) = std::mem::transmute(*qv.add(4));
            let signal: unsafe extern "system" fn(*mut c_void, *mut c_void, u64) = std::mem::transmute(*qv.add(5));
            let submit: unsafe extern "system" fn(*mut c_void) = std::mem::transmute(*qv.add(6));
            let cancel: unsafe extern "system" fn(*mut c_void, u64, u64) = std::mem::transmute(*qv.add(7));
            let record: unsafe extern "system" fn(*mut c_void, *mut u8) = std::mem::transmute(*qv.add(10));
            let sv = *(s as *const *const usize);
            let complete: unsafe extern "system" fn(*mut c_void, u32) -> bool = std::mem::transmute(*sv.add(3));
            let hresult: unsafe extern "system" fn(*mut c_void, u32) -> HRESULT = std::mem::transmute(*sv.add(4));

            let src_a = b"SUBMITTED_CANCELLED";
            let src_b = b"SUBMITTED_KEPT_____";
            let src_c = b"POST_CANCEL_MATCH__";
            let src_d = b"PENDING_CANCELLED__";
            let mut dst_a = vec![0u8; src_a.len()];
            let mut dst_b = vec![0u8; src_b.len()];
            let mut dst_c = vec![0u8; src_c.len()];
            let mut dst_d = vec![0u8; src_d.len()];

            let fence = MockFence {
                vtbl: mock_fence_vtbl().as_ptr(),
                refs: AtomicU32::new(1),
                signalled: std::sync::atomic::AtomicU64::new(0),
            };
            let fence_ptr = &fence as *const MockFence as *mut c_void;

            // Hold the queue's turn deterministically so the submitted batch waits in the pool.
            let gate = Arc::new(std::sync::Barrier::new(2));
            queue(q).enqueue(Entry::Gate(Arc::clone(&gate)));
            // Submitted + matching tag (0x1001 & 0xF000 == 0x1000) -> cancelled.
            enqueue(q, request_ex(SOURCE_MEMORY_BIT, dst_a.as_mut_ptr() as u64, 0, dst_a.len() as u32, src_a.as_ptr() as u64, 0, src_a.len() as u32, 0, 0x1001).as_ptr());
            // Submitted + matching tag pointing to unwritable address (0x10) -> cancelled before execute, so S_OK & 0 errors!
            enqueue(q, request_ex(SOURCE_MEMORY_BIT, 0x10, 0, 4, src_a.as_ptr() as u64, 0, 4, 0, 0x1002).as_ptr());
            // Submitted + non-matching tag (0x2001) -> executes.
            enqueue(q, request_ex(SOURCE_MEMORY_BIT, dst_b.as_mut_ptr() as u64, 0, dst_b.len() as u32, src_b.as_ptr() as u64, 0, src_b.len() as u32, 0, 0x2001).as_ptr());
            status(q, s, 0);
            signal(q, fence_ptr, 42);
            submit(q);
            assert_eq!(fence.refs.load(Ordering::Acquire), 2, "queue holds one fence reference");

            // Also enqueue an unsubmitted request with a matching tag before cancelling.
            enqueue(q, request_ex(SOURCE_MEMORY_BIT, dst_d.as_mut_ptr() as u64, 0, dst_d.len() as u32, src_d.as_ptr() as u64, 0, src_d.len() as u32, 0, 0x1003).as_ptr());
            cancel(q, 0xF000, 0x1000);

            // Enqueued AFTER CancelRequestsWithTag with a matching tag -> must NOT be cancelled.
            enqueue(q, request_ex(SOURCE_MEMORY_BIT, dst_c.as_mut_ptr() as u64, 0, dst_c.len() as u32, src_c.as_ptr() as u64, 0, src_c.len() as u32, 0, 0x1004).as_ptr());
            status(q, s, 1);
            submit(q);

            // Release the worker and wait for completion.
            gate.wait();
            let t = std::time::Instant::now();
            while !complete(s, 1) && t.elapsed().as_secs() < 5 {
                std::thread::yield_now();
            }
            assert!(complete(s, 0) && complete(s, 1));
            assert_eq!((hresult(s, 0), hresult(s, 1)), (S_OK, S_OK));
            assert!(dst_a.iter().all(|&b| b == 0), "submitted request should be cancelled");
            assert!(dst_d.iter().all(|&b| b == 0), "pending request should be cancelled");
            assert_eq!(dst_b.as_slice(), src_b);
            assert_eq!(dst_c.as_slice(), src_c, "request enqueued after cancel must run");
            assert_eq!(fence.signalled.load(Ordering::Acquire), 42);
            assert_eq!(fence.refs.load(Ordering::Acquire), 1, "fence reference released after signal");
            assert!(queue(q).cancels.lock().unwrap().is_empty(), "expired cancel rules are pruned");

            let mut rec = [0u8; ERROR_RECORD_BYTES];
            record(q, rec.as_mut_ptr());
            assert_eq!(u32::from_le_bytes(rec[0..4].try_into().unwrap()), 0, "cancelled requests do not record errors");

            // Closing a queue with an unsubmitted signal must release its fence ref cleanly.
            signal(q, fence_ptr, 99);
            status(q, s, 2);
            assert_eq!(fence.refs.load(Ordering::Acquire), 2);
            queue_close(q);
            assert_eq!(fence.refs.load(Ordering::Acquire), 1, "queue_close releases pending fence");
            assert_eq!(hresult(s, 2), E_QUEUE_CLOSED);

            queue_release(q);
            status_release(s);
        }
    }

    #[test]
    fn multi_stage_intermediate_size_validation_and_zlib() {
        // Validation matrix for Request::check
        let bc1 = 1u64 << BCPACK_SHIFT;
        let bc7 = 7u64 << BCPACK_SHIFT;
        let swiz = 1u64 << SWIZZLE_SHIFT;

        // Zlib only with valid intermediate_size (== dst and < dst)
        assert_eq!(Request::parse(&request_ex(ZLIB_BIT, 0x1000, 0, 64, 0x2000, 0, 20, 64, 0)).check(false), Ok(()));
        assert_eq!(Request::parse(&request_ex(ZLIB_BIT, 0x1000, 0, 128, 0x2000, 0, 20, 64, 0)).check(false), Ok(()));
        // Zlib only with invalid intermediate_size (<= src or > dst)
        assert_eq!(Request::parse(&request_ex(ZLIB_BIT, 0x1000, 0, 128, 0x2000, 0, 20, 20, 0)).check(false), Err(E_DESTINATION_SIZE));
        assert_eq!(Request::parse(&request_ex(ZLIB_BIT, 0x1000, 0, 64, 0x2000, 0, 20, 96, 0)).check(false), Err(E_DESTINATION_SIZE));
        // Two-stage zlib + BCPack: valid sizes -> E_DECOMPRESSION, invalid intermediate -> E_DESTINATION_SIZE
        assert_eq!(Request::parse(&request_ex(ZLIB_BIT | bc1, 0x1000, 0, 256, 0x2000, 0, 20, 64, 0)).check(false), Err(E_DECOMPRESSION));
        assert_eq!(Request::parse(&request_ex(ZLIB_BIT | bc7, 0x1000, 0, 256, 0x2000, 0, 20, 20, 0)).check(false), Err(E_DESTINATION_SIZE));
        assert_eq!(Request::parse(&request_ex(ZLIB_BIT | bc7, 0x1000, 0, 64, 0x2000, 0, 20, 64, 0)).check(false), Err(E_DESTINATION_SIZE));
        // Single-stage BCPack: intermediate must be 0, then E_DECOMPRESSION
        assert_eq!(Request::parse(&request_ex(bc1, 0x1000, 0, 256, 0x2000, 0, 20, 0, 0)).check(false), Err(E_DECOMPRESSION));
        assert_eq!(Request::parse(&request_ex(bc1, 0x1000, 0, 256, 0x2000, 0, 20, 64, 0)).check(false), Err(E_DESTINATION_SIZE));
        // Hardware swizzle mode unsupported
        assert_eq!(Request::parse(&request_ex(swiz, 0x1000, 0, 64, 0x2000, 0, 64, 0, 0)).check(false), Err(E_DECOMPRESSION));
        // SourceIsPhysicalPages requires memory source
        assert_eq!(Request::parse(&request_ex(SOURCE_PHYS_PAGES_BIT, 0x1000, 0, 64, 0x2000, 0, 64, 0, 0)).check(false), Err(E_SOURCE_TYPE));

        // End-to-end execution: zlib with intermediate_size < destination_size (padded destination)
        unsafe {
            let mut f = std::ptr::null_mut();
            assert_eq!(DStorageGetFactory(&IID_FACTORY, &mut f), S_OK);
            let vt = *(f as *const *const usize);
            let create_queue: unsafe extern "system" fn(*mut c_void, *const u8, *const GUID, *mut *mut c_void) -> HRESULT = std::mem::transmute(*vt.add(3));
            let create_status: unsafe extern "system" fn(*mut c_void, u32, *const u8, *const GUID, *mut *mut c_void) -> HRESULT = std::mem::transmute(*vt.add(5));

            let mut desc = [0u8; 0x18];
            desc[0..4].copy_from_slice(&1u32.to_le_bytes());
            desc[8..10].copy_from_slice(&8u16.to_le_bytes());
            desc[10] = 2;
            let mut q = std::ptr::null_mut();
            assert_eq!(create_queue(f, desc.as_ptr(), &IID_QUEUE, &mut q), S_OK);
            let mut s = std::ptr::null_mut();
            assert_eq!(create_status(f, 2, std::ptr::null(), &IID_STATUS_ARRAY, &mut s), S_OK);

            let plain = b"XCL intermediate-buffer zlib test payload! XCL intermediate-buffer zlib test payload!".to_vec();
            let packed = miniz_oxide::deflate::compress_to_vec_zlib(&plain, 6);
            assert!(packed.len() < plain.len());
            let mut dst = vec![0xCCu8; plain.len() + 32];

            let qv = *(q as *const *const usize);
            let enqueue: unsafe extern "system" fn(*mut c_void, *const u8) = std::mem::transmute(*qv.add(3));
            let status: unsafe extern "system" fn(*mut c_void, *mut c_void, u32) = std::mem::transmute(*qv.add(4));
            let submit: unsafe extern "system" fn(*mut c_void) = std::mem::transmute(*qv.add(6));
            let sv = *(s as *const *const usize);
            let complete: unsafe extern "system" fn(*mut c_void, u32) -> bool = std::mem::transmute(*sv.add(3));
            let hresult: unsafe extern "system" fn(*mut c_void, u32) -> HRESULT = std::mem::transmute(*sv.add(4));

            // Valid intermediate_size == plain.len() < dst.len()
            enqueue(
                q,
                request_ex(SOURCE_MEMORY_BIT | ZLIB_BIT, dst.as_mut_ptr() as u64, 0, dst.len() as u32, packed.as_ptr() as u64, 0, packed.len() as u32, plain.len() as u32, 0).as_ptr(),
            );
            status(q, s, 0);
            // Wrong intermediate_size (1 byte off) -> E_ZLIB_DATA
            let mut dst_bad = vec![0u8; plain.len() + 32];
            enqueue(
                q,
                request_ex(SOURCE_MEMORY_BIT | ZLIB_BIT, dst_bad.as_mut_ptr() as u64, 0, dst_bad.len() as u32, packed.as_ptr() as u64, 0, packed.len() as u32, (plain.len() + 1) as u32, 0).as_ptr(),
            );
            status(q, s, 1);
            submit(q);

            let t = std::time::Instant::now();
            while !complete(s, 1) && t.elapsed().as_secs() < 5 {
                std::thread::yield_now();
            }
            assert_eq!((hresult(s, 0), hresult(s, 1)), (S_OK, E_ZLIB_DATA));
            assert_eq!(&dst[..plain.len()], plain.as_slice());
            assert!(dst[plain.len()..].iter().all(|&b| b == 0), "trailing padding zeroed");

            queue_close(q);
            queue_release(q);
            status_release(s);
        }
    }

    #[test]
    fn error_record_resets_after_retrieval_and_event_is_duplicated() {
        use windows_sys::Win32::Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT};
        use windows_sys::Win32::System::Threading::WaitForSingleObject;

        unsafe {
            let mut f = std::ptr::null_mut();
            assert_eq!(DStorageGetFactory(&IID_FACTORY, &mut f), S_OK);
            let vt = *(f as *const *const usize);
            let create_queue: unsafe extern "system" fn(*mut c_void, *const u8, *const GUID, *mut *mut c_void) -> HRESULT = std::mem::transmute(*vt.add(3));
            let create_status: unsafe extern "system" fn(*mut c_void, u32, *const u8, *const GUID, *mut *mut c_void) -> HRESULT = std::mem::transmute(*vt.add(5));
            let mut desc = [0u8; 0x18];
            desc[0..4].copy_from_slice(&1u32.to_le_bytes());
            desc[8..10].copy_from_slice(&8u16.to_le_bytes());
            desc[10] = 2;
            let mut q = std::ptr::null_mut();
            assert_eq!(create_queue(f, desc.as_ptr(), &IID_QUEUE, &mut q), S_OK);
            let mut s = std::ptr::null_mut();
            assert_eq!(create_status(f, 1, std::ptr::null(), &IID_STATUS_ARRAY, &mut s), S_OK);

            // Caller owns and closes the returned error event handle without breaking the queue.
            let ev1 = queue_get_error_event(q);
            assert!(!ev1.is_null());
            assert_ne!(ev1 as usize, queue(q).error_event, "GetErrorEvent must return a duplicated handle");
            assert_eq!(WaitForSingleObject(ev1, 0), WAIT_TIMEOUT);
            assert_ne!(CloseHandle(ev1), 0);

            let ev2 = queue_get_error_event(q);
            assert!(!ev2.is_null());
            assert_eq!(WaitForSingleObject(ev2, 0), WAIT_TIMEOUT);

            // Trigger a refused request -> reported in queue order on Submit().
            let mut dst = [0u8; 8];
            let src = [0u8; 4];
            queue_enqueue_request(q, request(SOURCE_MEMORY_BIT, dst.as_mut_ptr() as u64, 8, src.as_ptr() as u64, 0, 4).as_ptr());
            queue_enqueue_status(q, s, 0);
            assert_eq!(WaitForSingleObject(ev2, 0), WAIT_TIMEOUT, "error event must not signal before Submit");
            queue_submit(q);
            let start = std::time::Instant::now();
            while !status_is_complete(s, 0) && start.elapsed().as_secs() < 5 {
                std::thread::yield_now();
            }
            assert_eq!(status_get_hresult(s, 0), E_DESTINATION_SIZE);
            assert_eq!(WaitForSingleObject(ev2, 0), WAIT_OBJECT_0, "error event must signal on failure");

            let mut rec = [0u8; ERROR_RECORD_BYTES];
            queue_retrieve_error_record(q, rec.as_mut_ptr());
            assert_eq!(u32::from_le_bytes(rec[0..4].try_into().unwrap()), 1);
            assert_eq!(i32::from_le_bytes(rec[8..12].try_into().unwrap()), E_DESTINATION_SIZE);
            assert_eq!(u32::from_le_bytes(rec[0xC..0x10].try_into().unwrap()), COMMAND_REQUEST);
            assert_eq!(WaitForSingleObject(ev2, 0), WAIT_TIMEOUT, "RetrieveErrorRecord must reset error event");

            // Second retrieval sees cleared state.
            rec.fill(0xAA);
            queue_retrieve_error_record(q, rec.as_mut_ptr());
            assert_eq!(u32::from_le_bytes(rec[0..4].try_into().unwrap()), 0);
            assert_eq!(u32::from_le_bytes(rec[0xC..0x10].try_into().unwrap()), NO_COMMAND);

            assert_ne!(CloseHandle(ev2), 0);
            queue_close(q);
            queue_release(q);
            status_release(s);
        }
    }

    #[test]
    fn queue_release_concurrent_breaks_worker_cycle() {
        unsafe {
            let mut f = std::ptr::null_mut();
            assert_eq!(DStorageGetFactory(&IID_FACTORY, &mut f), S_OK);
            let vt = *(f as *const *const usize);
            let create_queue: unsafe extern "system" fn(*mut c_void, *const u8, *const GUID, *mut *mut c_void) -> HRESULT = std::mem::transmute(*vt.add(3));
            let create_status: unsafe extern "system" fn(*mut c_void, u32, *const u8, *const GUID, *mut *mut c_void) -> HRESULT = std::mem::transmute(*vt.add(5));
            let mut desc = [0u8; 0x18];
            desc[0..4].copy_from_slice(&1u32.to_le_bytes());
            desc[8..10].copy_from_slice(&8u16.to_le_bytes());
            desc[10] = 2;

            for _ in 0..32 {
                let mut q = std::ptr::null_mut();
                assert_eq!(create_queue(f, desc.as_ptr(), &IID_QUEUE, &mut q), S_OK);
                let mut s = std::ptr::null_mut();
                assert_eq!(create_status(f, 1, std::ptr::null(), &IID_STATUS_ARRAY, &mut s), S_OK);
                let weak = Arc::downgrade(queue(q));

                // Hand the pool a buffer-free status entry.
                queue_enqueue_status(q, s, 0);
                queue_submit(q);

                // Race two Release() calls from refs == 2.
                assert_eq!(queue_add_ref(q), 2);
                let barrier = Arc::new(std::sync::Barrier::new(2));
                let q_addr = q as usize;
                let b2 = Arc::clone(&barrier);
                let t = std::thread::spawn(move || {
                    b2.wait();
                    queue_release(q_addr as *mut c_void);
                });
                barrier.wait();
                queue_release(q);
                t.join().unwrap();

                // Worker must exit and drop the final Arc<QueueInner>.
                let start = std::time::Instant::now();
                while weak.upgrade().is_some() && start.elapsed().as_secs() < 5 {
                    std::thread::yield_now();
                }
                assert!(weak.upgrade().is_none(), "concurrent Release leaked QueueInner in the pool");
                status_release(s);
            }
        }
    }

    #[test]
    fn queue_validates_status_and_fence_pointers_and_closed_rejects_work() {
        unsafe {
            let mut f = std::ptr::null_mut();
            assert_eq!(DStorageGetFactory(&IID_FACTORY, &mut f), S_OK);
            let vt = *(f as *const *const usize);
            let create_queue: unsafe extern "system" fn(*mut c_void, *const u8, *const GUID, *mut *mut c_void) -> HRESULT = std::mem::transmute(*vt.add(3));
            let create_status: unsafe extern "system" fn(*mut c_void, u32, *const u8, *const GUID, *mut *mut c_void) -> HRESULT = std::mem::transmute(*vt.add(5));
            let mut desc = [0u8; 0x18];
            desc[0..4].copy_from_slice(&1u32.to_le_bytes());
            desc[8..10].copy_from_slice(&8u16.to_le_bytes());
            desc[10] = 2;
            let mut q = std::ptr::null_mut();
            assert_eq!(create_queue(f, desc.as_ptr(), &IID_QUEUE, &mut q), S_OK);
            let mut s = std::ptr::null_mut();
            assert_eq!(create_status(f, 2, std::ptr::null(), &IID_STATUS_ARRAY, &mut s), S_OK);

            // Foreign object pointer (wrong vtable) must be ignored safely by EnqueueStatus.
            let bogus_vtbl: [usize; 4] = [0; 4];
            let mut bogus_obj: [*const usize; 2] = [bogus_vtbl.as_ptr(), std::ptr::null()];
            queue_enqueue_status(q, bogus_obj.as_mut_ptr() as *mut c_void, 0);
            // Null-vtable fence must be ignored safely by EnqueueSignal.
            let mut null_vtbl_fence: [*const usize; 1] = [std::ptr::null()];
            queue_enqueue_signal(q, null_vtbl_fence.as_mut_ptr() as *mut c_void, 1);
            assert!(queue(q).pending.lock().unwrap().entries.is_empty());

            // Pending status fails with E_QUEUE_CLOSED on Close().
            queue_enqueue_status(q, s, 0);
            queue_close(q);
            assert_eq!(status_get_hresult(s, 0), E_QUEUE_CLOSED);

            // Subsequent EnqueueStatus / Submit on closed queue do not mutate status or hand the pool work.
            inner::<StatusInner>(s).slots[1].store(S_OK, Ordering::Release);
            queue_enqueue_status(q, s, 1);
            queue_submit(q);
            assert_eq!(status_get_hresult(s, 1), S_OK);
            assert!(queue(q).pending.lock().unwrap().entries.is_empty());
            assert_eq!(pool::submitted(queue(q).lane), 0);

            queue_release(q);
            status_release(s);
        }
    }

    /// A destination page array whose entry is still 0 at EnqueueRequest and gets its page
    /// before the request's turn: the data arrives (a retail title's streaming).
    #[test]
    fn a_destination_page_filled_in_after_enqueue() {
        use crate::hle::kernelx::{AllocateTitlePhysicalPages, FreeTitlePhysicalPages, MapTitlePhysicalPages, PHYS_PAGE_SIZE};
        if crate::gpu::uma::get().is_none() {
            return;
        }
        unsafe {
            let mut count = 2usize;
            let mut pages = [0usize; 2];
            assert_eq!(AllocateTitlePhysicalPages(std::ptr::null_mut(), 0, &mut count, pages.as_mut_ptr()), 1);

            let mut f = std::ptr::null_mut();
            assert_eq!(DStorageGetFactory(&IID_FACTORY, &mut f), S_OK);
            let vt = *(f as *const *const usize);
            let create_queue: unsafe extern "system" fn(*mut c_void, *const u8, *const GUID, *mut *mut c_void) -> HRESULT = std::mem::transmute(*vt.add(3));
            let create_status: unsafe extern "system" fn(*mut c_void, u32, *const u8, *const GUID, *mut *mut c_void) -> HRESULT = std::mem::transmute(*vt.add(5));
            let mut desc = [0u8; 0x18];
            desc[0..4].copy_from_slice(&1u32.to_le_bytes());
            desc[8..10].copy_from_slice(&16u16.to_le_bytes());
            desc[10] = 2;
            let mut q = std::ptr::null_mut();
            assert_eq!(create_queue(f, desc.as_ptr(), &IID_QUEUE, &mut q), S_OK);
            let mut s = std::ptr::null_mut();
            assert_eq!(create_status(f, 2, std::ptr::null(), &IID_STATUS_ARRAY, &mut s), S_OK);
            let qv = *(q as *const *const usize);
            let enqueue: unsafe extern "system" fn(*mut c_void, *const u8) = std::mem::transmute(*qv.add(3));
            let status: unsafe extern "system" fn(*mut c_void, *mut c_void, u32) = std::mem::transmute(*qv.add(4));
            let submit: unsafe extern "system" fn(*mut c_void) = std::mem::transmute(*qv.add(6));
            let sv = *(s as *const *const usize);
            let complete: unsafe extern "system" fn(*mut c_void, u32) -> bool = std::mem::transmute(*sv.add(3));
            let hresult: unsafe extern "system" fn(*mut c_void, u32) -> HRESULT = std::mem::transmute(*sv.add(4));

            // where the pages can be read back: mapped into a reservation
            let reserved = windows_sys::Win32::System::Memory::VirtualAlloc(
                std::ptr::null(),
                2 * PHYS_PAGE_SIZE,
                windows_sys::Win32::System::Memory::MEM_RESERVE,
                windows_sys::Win32::System::Memory::PAGE_READWRITE,
            );
            assert!(!reserved.is_null());
            assert_eq!(MapTitlePhysicalPages(reserved, 2, 0, 4, pages.as_mut_ptr()), reserved);

            // 0. the title's table: the second entry has no page yet when the request is enqueued
            let data: Vec<u8> = (0..0x1800u32).map(|i| (i * 13 + 5) as u8).collect();
            let page_off = (PHYS_PAGE_SIZE - 0x800) as u16;
            let mut table = Box::new([pages[0], 0usize]);
            enqueue(q, request_ex(SOURCE_MEMORY_BIT | DEST_PHYS_PAGES_BIT, table.as_ptr() as u64, page_off, data.len() as u32, data.as_ptr() as u64, 0, data.len() as u32, 0, 0).as_ptr());
            std::ptr::write_volatile(&mut table[1], pages[1]);
            status(q, s, 0);

            // 1. an entry that is still 0 at the request's turn is refused as before
            let empty = Box::new([0usize]);
            enqueue(q, request_ex(SOURCE_MEMORY_BIT | DEST_PHYS_PAGES_BIT, empty.as_ptr() as u64, 0, 16, data.as_ptr() as u64, 0, 16, 0, 0).as_ptr());
            status(q, s, 1);

            submit(q);
            let t = std::time::Instant::now();
            while !complete(s, 1) && t.elapsed().as_secs() < 5 {
                std::thread::yield_now();
            }
            assert_eq!((hresult(s, 0), hresult(s, 1)), (S_OK, E_ACCESS_VIOLATION));
            let got = std::slice::from_raw_parts((reserved as *const u8).add(page_off as usize), data.len());
            assert_eq!(got, &data[..], "the bytes cross from the page that was there into the one filled in later");

            MapTitlePhysicalPages(reserved, 2, 0, 4, std::ptr::null_mut());
            windows_sys::Win32::System::Memory::VirtualFree(reserved, 0, windows_sys::Win32::System::Memory::MEM_RELEASE);
            assert_eq!(FreeTitlePhysicalPages(std::ptr::null_mut(), 2, pages.as_mut_ptr()), 1);
            queue_close(q);
            queue_release(q);
            status_release(s);
        }
    }

    /// A request's payload is kept with its hash (`alias`): the record of a crash tells whether the
    /// destination still holds what the request wrote, and what it was written for.
    #[test]
    fn a_requests_payload_is_checked_against_its_destination_in_a_crash_record() {
        use windows_sys::Win32::System::Memory::{VirtualAlloc, VirtualFree, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE};
        unsafe {
            let plain: Vec<u8> = (0..0x2800u32).map(|i| (i * 31 + i / 7) as u8).collect();
            let packed = miniz_oxide::deflate::compress_to_vec_zlib(&plain, 6);
            let dst = VirtualAlloc(std::ptr::null(), 0x1_0000, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE) as usize;
            assert_ne!(dst, 0);
            let req = Request::parse(&request(SOURCE_MEMORY_BIT | ZLIB_BIT, dst as u64 + 0x1000, plain.len() as u32, packed.as_ptr() as u64, 0, packed.len() as u32));
            assert_eq!(execute(&req, None, None, None, None, &mut None), Ok(()));
            assert_eq!(std::slice::from_raw_parts((dst + 0x1000) as *const u8, plain.len()), &plain[..]);
            let held = crate::alias::report(dst + 0x1010).expect("an address a request wrote to");
            assert!(held.contains(&format!("{:#x} bytes at {:#x}", plain.len(), dst + 0x1000)) && held.contains("STILL HOLDS it"), "{held}");
            assert!(held.contains(&format!("request with options {:#x}: {} bytes at {:#x} of its source", SOURCE_MEMORY_BIT | ZLIB_BIT, packed.len(), packed.as_ptr() as usize)), "{held}");
            // (the address may have been other memory before, with payloads of its own)
            assert!(crate::alias::report(dst + 0x8000).is_none_or(|r| !r.contains(&format!("{:#x} bytes at {:#x}", plain.len(), dst + 0x1000))), "memory of the page the request did not write");
            // other bytes land there with no request
            std::ptr::write_bytes((dst + 0x2000) as *mut u8, 0xB7, 0x40);
            let gone = crate::alias::report(dst + 0x3000).expect("the payload's last page");
            assert!(gone.contains("NO LONGER HOLDS it, and no later payload was written to that address"), "{gone}");
            assert_ne!(VirtualFree(dst as *mut c_void, 0, MEM_RELEASE), 0);
        }
    }

    #[test]
    fn virtual_destination_decommitted_after_validation_fails_the_write() {
        use windows_sys::Win32::System::Memory::{VirtualAlloc, VirtualFree, MEM_DECOMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE};

        unsafe {
            let bytes = [1u8, 2, 3, 4];
            let len = bytes.len();
            let destination = VirtualAlloc(std::ptr::null(), len, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE);
            assert!(!destination.is_null());
            assert!(writable(destination as usize, len));
            assert!(write_virtual(destination as usize, &bytes));
            assert_eq!(std::slice::from_raw_parts(destination.cast::<u8>(), len), &bytes);
            assert_ne!(VirtualFree(destination, len, MEM_DECOMMIT), 0);
            let wrote_after_decommit = write_virtual(destination as usize, &bytes);
            assert_ne!(VirtualFree(destination, 0, MEM_RELEASE), 0);
            assert!(!wrote_after_decommit);
        }
    }

    #[test]
    fn auto_submit_waits_for_late_destination_pages() {
        unsafe {
            let mut desc = [0u8; 0x18];
            desc[0..4].copy_from_slice(&1u32.to_le_bytes());
            desc[8..10].copy_from_slice(&4u16.to_le_bytes());
            desc[10] = 2;
            let mut q = std::ptr::null_mut();
            assert_eq!(create_queue(desc.as_ptr(), false, &IID_QUEUE, &mut q), S_OK);
            pool::hold(queue(q).lane, true);

            let mut table = [5usize, 0, 0];
            let source = [0u8; 3];
            let mut dest = [0u8; 3];
            let plain = request(SOURCE_MEMORY_BIT, dest.as_mut_ptr() as u64, 3, source.as_ptr() as u64, 0, 3);
            let scatter_source = vec![0u8; 3 * crate::hle::kernelx::PHYS_PAGE_SIZE];
            let late = request_ex(
                SOURCE_MEMORY_BIT | DEST_PHYS_PAGES_BIT,
                table.as_ptr() as u64,
                0,
                scatter_source.len() as u32,
                scatter_source.as_ptr() as u64,
                0,
                scatter_source.len() as u32,
                0,
                0,
            );
            queue_enqueue_request(q, late.as_ptr());
            queue_enqueue_request(q, plain.as_ptr());
            assert_eq!(pool::submitted(queue(q).lane), 0);

            table.as_mut_ptr().write_volatile(9);
            table.as_mut_ptr().add(1).write_volatile(7);
            queue_submit(q);
            let batch = pool::take_submitted(queue(q).lane);
            assert!(matches!(&batch[..], [
                Entry::Request { seq: 1, dest_pages: Some(pages), .. },
                Entry::Request { seq: 2, .. }
            ] if pages.as_slice() == [5, 7, 0]));

            queue_enqueue_request(q, plain.as_ptr());
            queue_enqueue_request(q, plain.as_ptr());
            assert_eq!(pool::take_submitted(queue(q).lane).len(), 2);
            queue_close(q);
            queue_release(q);
        }
    }

    #[test]
    fn destination_reads_only_entries_missing_from_its_snapshot() {
        let table = [11usize, 22, 33, 44, 55];
        let addr = table.as_ptr() as usize;
        let snapshot = [1, 0, 0, 4, 0];
        assert_eq!(destination_pages(addr, &snapshot).unwrap().as_ref(), &[1, 22, 33, 4, 55]);
        assert!(matches!(destination_pages(0, &[1, 2]), Ok(Cow::Borrowed(_))));
        assert_eq!(destination_pages(0, &[0]).unwrap_err(), E_ACCESS_VIOLATION);
        let vacant = 0usize;
        assert_eq!(destination_pages(&vacant as *const usize as usize, &[0]).unwrap_err(), E_ACCESS_VIOLATION);
    }

    /// A queue with a status array, and five physical pages mapped where the title reads them
    /// (views in its reservation of 64 KiB pages): for requests whose page list is a slot of the
    /// title's that it writes after EnqueueRequest.
    struct Slots {
        q: *mut c_void,
        s: *mut c_void,
        pool: usize,
        pages: [usize; 5],
    }

    impl Slots {
        unsafe fn new() -> Slots {
            use crate::hle::kernelx::{AllocateTitlePhysicalPages, MapTitlePhysicalPages, PHYS_PAGE_SIZE};
            let mut count = 5usize;
            let mut pages = [0usize; 5];
            assert_eq!(AllocateTitlePhysicalPages(std::ptr::null_mut(), 0, &mut count, pages.as_mut_ptr()), 1);
            let mut f = std::ptr::null_mut();
            assert_eq!(DStorageGetFactory(&IID_FACTORY, &mut f), S_OK);
            let vt = *(f as *const *const usize);
            let create_queue: unsafe extern "system" fn(*mut c_void, *const u8, *const GUID, *mut *mut c_void) -> HRESULT = std::mem::transmute(*vt.add(3));
            let create_status: unsafe extern "system" fn(*mut c_void, u32, *const u8, *const GUID, *mut *mut c_void) -> HRESULT = std::mem::transmute(*vt.add(5));
            let mut desc = [0u8; 0x18];
            desc[0..4].copy_from_slice(&1u32.to_le_bytes());
            desc[8..10].copy_from_slice(&16u16.to_le_bytes());
            desc[10] = 2;
            let mut q = std::ptr::null_mut();
            assert_eq!(create_queue(f, desc.as_ptr(), &IID_QUEUE, &mut q), S_OK);
            let mut s = std::ptr::null_mut();
            assert_eq!(create_status(f, 8, std::ptr::null(), &IID_STATUS_ARRAY, &mut s), S_OK);
            let pool = crate::hle::kernelx::reserve(5 * PHYS_PAGE_SIZE).unwrap();
            assert_eq!(MapTitlePhysicalPages(pool as *mut c_void, 5, 0, 4, pages.as_mut_ptr()) as usize, pool);
            Slots { q, s, pool, pages }
        }

        /// EnqueueRequest of `request` with status `index`, then `then` (the title writing its slot),
        /// then Submit; the request's result.
        unsafe fn run(&self, index: u32, request: &[u8], then: impl FnOnce()) -> HRESULT {
            let qv = *(self.q as *const *const usize);
            let enqueue: unsafe extern "system" fn(*mut c_void, *const u8) = std::mem::transmute(*qv.add(3));
            let status: unsafe extern "system" fn(*mut c_void, *mut c_void, u32) = std::mem::transmute(*qv.add(4));
            let submit: unsafe extern "system" fn(*mut c_void) = std::mem::transmute(*qv.add(6));
            let sv = *(self.s as *const *const usize);
            let complete: unsafe extern "system" fn(*mut c_void, u32) -> bool = std::mem::transmute(*sv.add(3));
            let hresult: unsafe extern "system" fn(*mut c_void, u32) -> HRESULT = std::mem::transmute(*sv.add(4));
            enqueue(self.q, request.as_ptr());
            then();
            status(self.q, self.s, index);
            submit(self.q);
            let t = std::time::Instant::now();
            while !complete(self.s, index) && t.elapsed().as_secs() < 5 {
                std::thread::yield_now();
            }
            hresult(self.s, index)
        }

        /// `len` bytes at `offset` of page `page`, where the title reads them.
        unsafe fn at(&self, page: usize, offset: usize, len: usize) -> &[u8] {
            std::slice::from_raw_parts((self.pool + page * crate::hle::kernelx::PHYS_PAGE_SIZE + offset) as *const u8, len)
        }

        /// A record chunk in page 0, written by a request of its own to where the page is mapped.
        unsafe fn record(&self, index: u32) -> Vec<u8> {
            let record: Vec<u8> = (0..0xF40u32).map(|i| (i * 13 + 5) as u8).collect();
            assert_eq!(self.run(index, &request(SOURCE_MEMORY_BIT, (self.pool + 0x8000) as u64, record.len() as u32, record.as_ptr() as u64, 0, record.len() as u32), || ()), S_OK);
            record
        }

        unsafe fn close(mut self) {
            use crate::hle::kernelx::{FreeTitlePhysicalPages, MapTitlePhysicalPages};
            assert_eq!(MapTitlePhysicalPages(self.pool as *mut c_void, 5, 0, 4, std::ptr::null_mut()) as usize, self.pool);
            assert_eq!(crate::hle::kernelx::free_in_reservation(self.pool, 0, windows_sys::Win32::System::Memory::MEM_RELEASE), Some(1));
            assert_eq!(FreeTitlePhysicalPages(std::ptr::null_mut(), 5, self.pages.as_mut_ptr()), 1);
            queue_close(self.q);
            queue_release(self.q);
            status_release(self.s);
        }
    }

    /// The retail sequence: a texture's table slot still names the page of the request it held
    /// before when its request is enqueued, and gets its own page before Submit. The old page is a
    /// record chunk by then, mapped in the title's reservation and filled by a request of its own:
    /// it keeps the record, and the texture's data goes to the page the slot names at the
    /// request's turn. A list on the enqueuing thread's stack is the list as it was at
    /// EnqueueRequest, whatever is on that stack later. A source list is read like a destination's.
    #[test]
    fn a_page_list_rewritten_after_enqueue_is_read_at_the_requests_turn() {
        use crate::hle::kernelx::PHYS_PAGE_SIZE;
        unsafe {
            let t = Slots::new();
            let pages = t.pages;
            // 0. page 0 is a record chunk now
            let record = t.record(0);
            // 1. a texture's request: its slot names page 0 at EnqueueRequest, page 1 before Submit
            let texels: Vec<u8> = (0..0x8000u32).map(|i| (i * 7 + i / 251) as u8 | 0x80).collect();
            let mut slot = Box::new([pages[0]]);
            let before = page_list_counters();
            let texture = request_ex(SOURCE_MEMORY_BIT | DEST_PHYS_PAGES_BIT, slot.as_ptr() as u64, 0x8000, texels.len() as u32, texels.as_ptr() as u64, 0, texels.len() as u32, 0, 0);
            assert_eq!(t.run(1, &texture, || std::ptr::write_volatile(&mut slot[0], pages[1])), S_OK);
            assert_eq!(t.at(0, 0x8000, record.len()), &record[..], "the record chunk in the slot's previous page is left alone");
            assert_eq!(t.at(1, 0x8000, texels.len()), &texels[..], "the data goes to the page the slot names at the request's turn");
            let after = page_list_counters();
            assert!(after.stale > before.stale && after.stale_in_use > before.stale_in_use, "counted, with its page at enqueue in use: {before:?} {after:?}");
            let written = crate::alias::report(t.pool + PHYS_PAGE_SIZE + 0x8010).expect("an address a request wrote to");
            assert!(written.contains("; 1 entries of its page list CHANGED between EnqueueRequest and its turn (1 of them named a page at enqueue), used as at its turn"), "{written}");

            // 2. a list on the enqueuing thread's stack, overwritten there with another allocated
            // page after EnqueueRequest: the list as it was
            let small: Vec<u8> = (0..0x100u32).map(|i| (i * 3 + 5) as u8).collect();
            let mut on_stack = [pages[0]];
            let stack = request_ex(SOURCE_MEMORY_BIT | DEST_PHYS_PAGES_BIT, on_stack.as_ptr() as u64, 0, small.len() as u32, small.as_ptr() as u64, 0, small.len() as u32, 0, 0);
            assert_eq!(t.run(2, &stack, || std::ptr::write_volatile(&mut on_stack[0], pages[2])), S_OK);
            assert_eq!(t.at(0, 0, small.len()), &small[..], "a list on the stack: the page it named at EnqueueRequest");
            assert!(t.at(2, 0, small.len()).iter().all(|&b| b == 0), "the page written on the stack since is left alone");
            assert!(crate::alias::report(t.pool + 0x10).is_some_and(|r| r.contains("; its page list on the enqueuing thread's stack, used as enqueued")));

            // 3. a source list in a slot, rewritten after EnqueueRequest: read from the page it names then
            let mut readback = vec![0u8; 0x100];
            let mut source = Box::new([pages[0]]);
            let read = request_ex(SOURCE_MEMORY_BIT | SOURCE_PHYS_PAGES_BIT, readback.as_mut_ptr() as u64, 0, readback.len() as u32, source.as_ptr() as u64, 0x8000, readback.len() as u32, 0, 0);
            assert_eq!(t.run(3, &read, || std::ptr::write_volatile(&mut source[0], pages[1])), S_OK);
            assert_eq!(&readback[..], &texels[..0x100], "read from the page the source slot names at the request's turn");
            t.close();
        }
    }

    /// A slot that held a shorter list before: at EnqueueRequest its first entry still names the
    /// page of that list (a record chunk now) and the rest are 0; the title writes all of them
    /// before Submit. Entries that were 0 were always read again (at Submit) – the first one was
    /// not, and that part of the data went into the record. Then a slot that is all 0 at
    /// EnqueueRequest: read at Submit as before, and no change at the request's turn.
    #[test]
    fn a_slot_that_held_a_shorter_list_is_read_whole_at_the_requests_turn() {
        use crate::hle::kernelx::PHYS_PAGE_SIZE;
        unsafe {
            let t = Slots::new();
            let pages = t.pages;
            let record = t.record(0);
            let wide: Vec<u8> = (0..2 * PHYS_PAGE_SIZE as u32).map(|i| (i * 11 + i / 127) as u8 | 1).collect();
            let mut slot = Box::new([pages[0], 0]);
            let before = page_list_counters();
            let texture = request_ex(SOURCE_MEMORY_BIT | DEST_PHYS_PAGES_BIT, slot.as_ptr() as u64, 0, wide.len() as u32, wide.as_ptr() as u64, 0, wide.len() as u32, 0, 0);
            let write = |slot: &mut [usize; 2]| {
                std::ptr::write_volatile(&mut slot[0], pages[3]);
                std::ptr::write_volatile(&mut slot[1], pages[4]);
            };
            assert_eq!(t.run(1, &texture, || write(&mut slot)), S_OK);
            assert_eq!(t.at(0, 0x8000, record.len()), &record[..], "the record chunk in the page the slot's first entry named before is left alone");
            assert_eq!(t.at(3, 0, wide.len()), &wide[..], "all of the data in the pages the slot names at the request's turn");
            let after = page_list_counters();
            assert!(after.stale > before.stale && after.stale_in_use > before.stale_in_use, "counted as a list with a stale entry: {before:?} {after:?}");
            let written = crate::alias::report(t.pool + 3 * PHYS_PAGE_SIZE + 0x10).expect("an address a request wrote to");
            assert!(written.contains("; 1 entries of its page list CHANGED between EnqueueRequest and its turn (1 of them named a page at enqueue), used as at its turn"), "{written}");

            // all 0 at EnqueueRequest
            let again: Vec<u8> = wide.iter().rev().copied().collect();
            let mut empty = Box::new([0usize, 0]);
            let texture = request_ex(SOURCE_MEMORY_BIT | DEST_PHYS_PAGES_BIT, empty.as_ptr() as u64, 0, again.len() as u32, again.as_ptr() as u64, 0, again.len() as u32, 0, 0);
            assert_eq!(t.run(2, &texture, || write(&mut empty)), S_OK);
            assert_eq!(t.at(3, 0, again.len()), &again[..]);
            let written = crate::alias::report(t.pool + 4 * PHYS_PAGE_SIZE + 0x4010).expect("an address a request wrote to");
            assert!(written.contains("; its page list at its turn as it was at EnqueueRequest (entries that were 0 then: as at Submit)"), "{written}");
            t.close();
        }
    }

    /// A list is taken as it is at its request's turn only when it was not on the enqueuing
    /// thread's stack and names allocated pages then.
    #[test]
    fn a_page_list_at_its_turn_is_taken_only_when_it_names_pages() {
        use crate::hle::kernelx::{AllocateTitlePhysicalPages, FreeTitlePhysicalPages};
        unsafe {
            let mut count = 2usize;
            let mut pages = [0usize; 2];
            assert_eq!(AllocateTitlePhysicalPages(std::ptr::null_mut(), 0, &mut count, pages.as_mut_ptr()), 1);
            let at_enqueue = [pages[0], pages[1]];
            let mut slot = Box::new(at_enqueue);
            let addr = slot.as_ptr() as usize;
            assert_eq!(ListPlace::of(addr as u64, 2), ListPlace::Kept);
            assert_eq!(ListPlace::of(at_enqueue.as_ptr() as u64, 2), ListPlace::CallersStack);
            assert!(matches!(page_list_now(addr, &at_enqueue, ListPlace::Kept), (Cow::Borrowed(_), ListChange::Same)));
            slot.as_mut_ptr().write_volatile(pages[1]);
            slot.as_mut_ptr().add(1).write_volatile(pages[0]);
            let (now, change) = page_list_now(addr, &at_enqueue, ListPlace::Kept);
            assert_eq!((now.as_ref(), change), (&[pages[1], pages[0]][..], ListChange::Taken));
            assert!(matches!(page_list_now(addr, &at_enqueue, ListPlace::CallersStack), (Cow::Borrowed(_), ListChange::Same)));
            // counted by what the entries were at enqueue: pages (stale), or 0 (filled in since)
            use crate::alias::PageList;
            let r = Request::parse(&request_ex(SOURCE_MEMORY_BIT | DEST_PHYS_PAGES_BIT, addr as u64, 0, 16, addr as u64, 0, 16, 0, 0));
            let before = page_list_counters();
            assert_eq!(page_list_at_turn("destination", &r, addr, &at_enqueue, ListPlace::Kept).1, PageList::Changed { entries: 2, stale: 2 });
            assert_eq!(page_list_at_turn("destination", &r, addr, &[pages[0], 0], ListPlace::Kept).1, PageList::Changed { entries: 2, stale: 1 });
            let between = page_list_counters();
            assert!(between.stale >= before.stale + 2 && between.stale_in_use == before.stale_in_use, "two with a stale entry, no page of theirs mapped: {before:?} {between:?}");
            assert_eq!(page_list_at_turn("destination", &r, addr, &[0, 0], ListPlace::Kept).1, PageList::Changed { entries: 2, stale: 0 });
            assert_eq!(page_list_at_turn("destination", &r, addr, &[0, pages[0]], ListPlace::Kept).1, PageList::Changed { entries: 1, stale: 0 });
            assert!(page_list_counters().filled >= between.filled + 2);
            assert_eq!(page_list_at_turn("destination", &r, addr, &[pages[1], pages[0]], ListPlace::Kept).1, PageList::Same);
            assert_eq!(page_list_at_turn("destination", &r, addr, &at_enqueue, ListPlace::CallersStack).1, PageList::OnStack);
            // an entry not filled in yet, and memory that is no list any more
            for other in [0, usize::MAX & !15, 7] {
                slot.as_mut_ptr().add(1).write_volatile(other);
                let (now, change) = page_list_now(addr, &at_enqueue, ListPlace::Kept);
                assert_eq!((now.as_ref(), change), (&at_enqueue[..], ListChange::NotPages), "{other:#x}");
            }
            assert_eq!(page_list_now(0, &at_enqueue, ListPlace::Kept).1, ListChange::Same, "not readable any more");
            // freed since: no pages, the list as enqueued stands (and the request fails on it)
            slot.as_mut_ptr().add(1).write_volatile(pages[0]);
            assert_eq!(FreeTitlePhysicalPages(std::ptr::null_mut(), 2, pages.as_mut_ptr()), 1);
            assert_eq!(page_list_now(addr, &at_enqueue, ListPlace::Kept).1, ListChange::NotPages);
        }
    }

    #[test]
    fn physical_page_destinations_and_sources() {
        use crate::hle::kernelx::{AllocateTitlePhysicalPages, FreeTitlePhysicalPages, MapTitlePhysicalPages, PHYS_PAGE_SIZE};
        if crate::gpu::uma::get().is_none() {
            return;
        }
        unsafe {
            let mut count = 3usize;
            let mut pages = [0usize; 3];
            assert_eq!(AllocateTitlePhysicalPages(std::ptr::null_mut(), 0, &mut count, pages.as_mut_ptr()), 1);

            let mut f = std::ptr::null_mut();
            assert_eq!(DStorageGetFactory(&IID_FACTORY, &mut f), S_OK);
            let vt = *(f as *const *const usize);
            let create_queue: unsafe extern "system" fn(*mut c_void, *const u8, *const GUID, *mut *mut c_void) -> HRESULT = std::mem::transmute(*vt.add(3));
            let create_status: unsafe extern "system" fn(*mut c_void, u32, *const u8, *const GUID, *mut *mut c_void) -> HRESULT = std::mem::transmute(*vt.add(5));

            let mut desc = [0u8; 0x18];
            desc[0..4].copy_from_slice(&1u32.to_le_bytes());
            desc[8..10].copy_from_slice(&16u16.to_le_bytes());
            desc[10] = 2;
            let mut q = std::ptr::null_mut();
            assert_eq!(create_queue(f, desc.as_ptr(), &IID_QUEUE, &mut q), S_OK);
            let mut s = std::ptr::null_mut();
            assert_eq!(create_status(f, 4, std::ptr::null(), &IID_STATUS_ARRAY, &mut s), S_OK);

            let qv = *(q as *const *const usize);
            let enqueue: unsafe extern "system" fn(*mut c_void, *const u8) = std::mem::transmute(*qv.add(3));
            let status: unsafe extern "system" fn(*mut c_void, *mut c_void, u32) = std::mem::transmute(*qv.add(4));
            let submit: unsafe extern "system" fn(*mut c_void) = std::mem::transmute(*qv.add(6));
            let sv = *(s as *const *const usize);
            let complete: unsafe extern "system" fn(*mut c_void, u32) -> bool = std::mem::transmute(*sv.add(3));
            let hresult: unsafe extern "system" fn(*mut c_void, u32) -> HRESULT = std::mem::transmute(*sv.add(4));

            // 1. Zlib inflate into scattered physical pages [pages[2], pages[0]] crossing a 64 KiB boundary.
            let plain: Vec<u8> = (0..0x1000u32).map(|i| ((i * 37) ^ (i >> 3)) as u8).collect();
            let packed = miniz_oxide::deflate::compress_to_vec_zlib(&plain, 6);
            let page_off = (PHYS_PAGE_SIZE - 0x800) as u16;
            let mut scattered = [pages[2], pages[0]];
            enqueue(
                q,
                request_ex(
                    SOURCE_MEMORY_BIT | ZLIB_BIT | DEST_PHYS_PAGES_BIT,
                    scattered.as_ptr() as u64,
                    page_off,
                    plain.len() as u32,
                    packed.as_ptr() as u64,
                    0,
                    packed.len() as u32,
                    0,
                    0,
                )
                .as_ptr(),
            );
            // Clobber the stack array before Submit() to verify the page list was snapshotted at enqueue.
            std::ptr::write_volatile(&mut scattered, [0, 0]);
            status(q, s, 0);

            // 2. Read back from those scattered physical pages via SourceIsPhysicalPages into plain memory.
            let mut readback = vec![0u8; plain.len()];
            let src_scattered = [pages[2], pages[0]];
            enqueue(
                q,
                request_ex(
                    SOURCE_MEMORY_BIT | SOURCE_PHYS_PAGES_BIT,
                    readback.as_mut_ptr() as u64,
                    0,
                    readback.len() as u32,
                    src_scattered.as_ptr() as u64,
                    page_off as u64,
                    readback.len() as u32,
                    0,
                    0,
                )
                .as_ptr(),
            );
            status(q, s, 1);

            // 3. Map pages[1] into a reserved virtual address (like a streamed texture) and write via physical page!
            let reserved = windows_sys::Win32::System::Memory::VirtualAlloc(
                std::ptr::null(),
                PHYS_PAGE_SIZE,
                windows_sys::Win32::System::Memory::MEM_RESERVE,
                windows_sys::Win32::System::Memory::PAGE_READWRITE,
            );
            assert!(!reserved.is_null());
            assert_eq!(MapTitlePhysicalPages(reserved, 1, 0, 4, &mut pages[1]), reserved);
            let streamed = b"DIRECTSTORAGE_TO_MAPPED_PAGE";
            let mapped_list = [pages[1]];
            enqueue(
                q,
                request_ex(
                    SOURCE_MEMORY_BIT | DEST_PHYS_PAGES_BIT,
                    mapped_list.as_ptr() as u64,
                    0x40,
                    streamed.len() as u32,
                    streamed.as_ptr() as u64,
                    0,
                    streamed.len() as u32,
                    0,
                    0,
                )
                .as_ptr(),
            );
            status(q, s, 2);

            // 4. Invalid physical page number -> E_ACCESS_VIOLATION.
            let bogus = [usize::MAX];
            enqueue(
                q,
                request_ex(
                    SOURCE_MEMORY_BIT | DEST_PHYS_PAGES_BIT,
                    bogus.as_ptr() as u64,
                    0,
                    streamed.len() as u32,
                    streamed.as_ptr() as u64,
                    0,
                    streamed.len() as u32,
                    0,
                    0,
                )
                .as_ptr(),
            );
            status(q, s, 3);

            submit(q);
            let t = std::time::Instant::now();
            while !complete(s, 3) && t.elapsed().as_secs() < 5 {
                std::thread::yield_now();
            }
            assert_eq!((hresult(s, 0), hresult(s, 1), hresult(s, 2), hresult(s, 3)), (S_OK, S_OK, S_OK, E_ACCESS_VIOLATION));
            assert_eq!(readback, plain);
            let mapped_slice = std::slice::from_raw_parts((reserved as *const u8).add(0x40), streamed.len());
            assert_eq!(mapped_slice, streamed, "DirectStorage physical-page write lands in mapped reservation");

            // Unmap and free cleanly.
            MapTitlePhysicalPages(reserved, 1, 0, 4, std::ptr::null_mut());
            windows_sys::Win32::System::Memory::VirtualFree(reserved, 0, windows_sys::Win32::System::Memory::MEM_RELEASE);
            assert_eq!(FreeTitlePhysicalPages(std::ptr::null_mut(), 3, pages.as_mut_ptr()), 1);
            queue_close(q);
            queue_release(q);
            status_release(s);
        }
    }
    // Small reference decoders for the fill blocks (from the BCn block layouts; only what the
    // solid blocks use is decoded).

    fn bits(v: u128, at: u32, n: u32) -> u32 {
        ((v >> at) & ((1u128 << n) - 1)) as u32
    }

    /// BC1 colour block → 16 RGBA texels (`opaque`: the BC2/BC3 colour half, always 4 colours).
    fn bc1_texels(b: &[u8], opaque: bool) -> [[u8; 4]; 16] {
        let c0 = u16::from_le_bytes([b[0], b[1]]);
        let c1 = u16::from_le_bytes([b[2], b[3]]);
        let rgb = |c: u16| {
            let (r, g, bl) = ((c >> 11) as u32, ((c >> 5) & 63) as u32, (c & 31) as u32);
            [(r * 255 + 15) / 31, (g * 255 + 31) / 63, (bl * 255 + 15) / 31]
        };
        let (e0, e1) = (rgb(c0), rgb(c1));
        let four = opaque || c0 > c1;
        let mut pal = [[0u8, 0, 0, 255]; 4];
        for c in 0..3 {
            pal[0][c] = e0[c] as u8;
            pal[1][c] = e1[c] as u8;
            if four {
                pal[2][c] = ((2 * e0[c] + e1[c]) / 3) as u8;
                pal[3][c] = ((e0[c] + 2 * e1[c]) / 3) as u8;
            } else {
                pal[2][c] = ((e0[c] + e1[c]) / 2) as u8;
            }
        }
        if !four {
            pal[3] = [0, 0, 0, 0];
        }
        let idx = u32::from_le_bytes([b[4], b[5], b[6], b[7]]);
        std::array::from_fn(|i| pal[((idx >> (2 * i)) & 3) as usize])
    }

    /// BC4 block (also the BC3 alpha half) → 16 values.
    fn bc4_values(b: &[u8]) -> [u8; 16] {
        let v = u64::from_le_bytes(b[..8].try_into().unwrap());
        let (a0, a1) = (b[0] as u32, b[1] as u32);
        std::array::from_fn(|i| {
            let k = ((v >> (16 + 3 * i)) & 7) as u32;
            match k {
                0 => a0 as u8,
                1 => a1 as u8,
                _ if a0 > a1 => (((8 - k) * a0 + (k - 1) * a1) / 7) as u8,
                6 => 0,
                7 => 255,
                _ => (((6 - k) * a0 + (k - 1) * a1) / 5) as u8,
            }
        })
    }

    /// 4-bit index weights of BC6H and BC7.
    const WEIGHTS4: [u32; 16] = [0, 4, 9, 13, 17, 21, 26, 30, 34, 38, 43, 47, 51, 55, 60, 64];

    /// Index `i` of a one-region block whose 4-bit indices start at bit 65 (index 0 has 3 bits).
    fn index4(v: u128, i: usize) -> usize {
        (if i == 0 { bits(v, 65, 3) } else { bits(v, 68 + 4 * (i as u32 - 1), 4) }) as usize
    }

    /// BC7 mode 6 block → 16 RGBA texels.
    fn bc7_mode6_texels(b: &[u8]) -> [[u8; 4]; 16] {
        let v = u128::from_le_bytes(b.try_into().unwrap());
        assert_eq!(v.trailing_zeros(), 6, "BC7 mode 6");
        let p = [bits(v, 63, 1), bits(v, 64, 1)];
        let ep = |c: usize, e: usize| bits(v, 7 + 14 * c as u32 + 7 * e as u32, 7) << 1 | p[e];
        std::array::from_fn(|i| {
            let w = WEIGHTS4[index4(v, i)];
            std::array::from_fn(|c| ((ep(c, 0) * (64 - w) + ep(c, 1) * w + 32) >> 6) as u8)
        })
    }

    /// BC6H (UF16) mode 11 block → 16 RGB half-float bit patterns.
    fn bc6h_mode11_texels(b: &[u8]) -> [[u16; 3]; 16] {
        let v = u128::from_le_bytes(b.try_into().unwrap());
        assert_eq!(bits(v, 0, 5), 3, "BC6H mode 11");
        let unq = |e: u32| match e {
            0 => 0,
            1023 => 0xFFFF,
            _ => ((e << 16) + 0x8000) >> 10,
        };
        std::array::from_fn(|i| {
            let w = WEIGHTS4[index4(v, i)];
            std::array::from_fn(|c| {
                let (a, z) = (unq(bits(v, 5 + 10 * c as u32, 10)), unq(bits(v, 35 + 10 * c as u32, 10)));
                let l = (a * (64 - w) + z * w + 32) >> 6;
                ((l * 31) >> 6) as u16
            })
        })
    }

    #[test]
    fn bcpack_fill_blocks_are_solid_in_their_bcn_format() {
        const MAGENTA: [u8; 4] = [255, 0, 255, 255];
        let b = |m| bcpack_fill_block(m).unwrap();
        assert_eq!(b(1).len(), 8);
        assert!(bc1_texels(&b(1), false).iter().all(|t| *t == MAGENTA), "BC1");
        assert_eq!((b(2).len(), &b(2)[..8]), (16, &[0xFF; 8][..]), "BC2 explicit alpha 15");
        assert!(bc1_texels(&b(2)[8..], true).iter().all(|t| *t == MAGENTA), "BC2 colour");
        assert!(bc4_values(&b(3)[..8]).iter().all(|&a| a == 255), "BC3 alpha");
        assert!(bc1_texels(&b(3)[8..], true).iter().all(|t| *t == MAGENTA), "BC3 colour");
        assert_eq!(b(4).len(), 8);
        assert!(bc4_values(&b(4)).iter().all(|&r| r == 255), "BC4");
        assert!(bc4_values(&b(5)[..8]).iter().all(|&r| r == 255) && bc4_values(&b(5)[8..]).iter().all(|&g| g == 0), "BC5");
        assert!(bc6h_mode11_texels(&b(6)).iter().all(|t| *t == [0x3C00, 0, 0x3C00]), "BC6H: 1.0, 0, 1.0");
        assert!(bc7_mode6_texels(&b(7)).iter().all(|t| *t == [255, 1, 255, 255]), "BC7");
        assert_eq!((bcpack_fill_block(0), bcpack_fill_block(8)), (None, None), "not a BCn mode");
    }

    #[test]
    fn bcpack_refusals_are_counted_per_mode_and_still_fail() {
        let counts = BcpackRefusals::new();
        let bc7 = Request::parse(&request(SOURCE_MEMORY_BIT | 7 << BCPACK_SHIFT, 0x1000, 0x10000, 0x2000, 0, 0x4000));
        let bc7_zlib = Request::parse(&request(SOURCE_MEMORY_BIT | ZLIB_BIT | 7 << BCPACK_SHIFT, 0x1000, 0x10000, 0x2000, 0, 0x4000));
        let bc1 = Request::parse(&request(SOURCE_MEMORY_BIT | 1 << BCPACK_SHIFT, 0x1000, 0x8000, 0x2000, 0, 0x1000));
        for (i, r) in [&bc7, &bc7_zlib, &bc1, &bc7].into_iter().enumerate() {
            assert_eq!(r.check(true), Err(E_DECOMPRESSION), "the request fails as before");
            assert_eq!(counts.note(r), i as u64 + 1);
        }
        let s = counts.stats();
        assert_eq!(s.keys().collect::<Vec<_>>(), ["BC1", "BC7"]);
        assert_eq!((s["BC7"].requests, s["BC7"].with_zlib, s["BC7"].destination_bytes), (3, 1, 0x30000));
        assert_eq!((s["BC1"].requests, s["BC1"].with_zlib, s["BC1"].destination_bytes), (1, 0, 0x8000));
        assert_eq!(counts.summary(), "BC1 1 (32768 bytes), BC7 3 (196608 bytes)");
        for m in 1..=7u64 {
            let r = Request::parse(&request(SOURCE_MEMORY_BIT | m << BCPACK_SHIFT, 0x1000, 64, 0x2000, 0, 20));
            assert_eq!(r.check(true), Err(E_DECOMPRESSION));
        }
    }

    #[test]
    fn a_refused_bcpack_request_is_filled_only_under_the_switch() {
        unsafe {
            let mut f = std::ptr::null_mut();
            assert_eq!(DStorageGetFactory(&IID_FACTORY, &mut f), S_OK);
            let vt = *(f as *const *const usize);
            let create_queue: unsafe extern "system" fn(*mut c_void, *const u8, *const GUID, *mut *mut c_void) -> HRESULT = std::mem::transmute(*vt.add(3));
            let create_status: unsafe extern "system" fn(*mut c_void, u32, *const u8, *const GUID, *mut *mut c_void) -> HRESULT = std::mem::transmute(*vt.add(5));
            let mut desc = [0u8; 0x18];
            desc[0..4].copy_from_slice(&1u32.to_le_bytes());
            desc[8..10].copy_from_slice(&8u16.to_le_bytes());
            desc[10] = 2;
            let mut q = std::ptr::null_mut();
            assert_eq!(create_queue(f, desc.as_ptr(), &IID_QUEUE, &mut q), S_OK);
            let mut s = std::ptr::null_mut();
            assert_eq!(create_status(f, 2, std::ptr::null(), &IID_STATUS_ARRAY, &mut s), S_OK);
            let qv = *(q as *const *const usize);
            let enqueue: unsafe extern "system" fn(*mut c_void, *const u8) = std::mem::transmute(*qv.add(3));
            let status: unsafe extern "system" fn(*mut c_void, *mut c_void, u32) = std::mem::transmute(*qv.add(4));
            let submit: unsafe extern "system" fn(*mut c_void) = std::mem::transmute(*qv.add(6));
            let sv = *(s as *const *const usize);
            let complete: unsafe extern "system" fn(*mut c_void, u32) -> bool = std::mem::transmute(*sv.add(3));
            let hresult: unsafe extern "system" fn(*mut c_void, u32) -> HRESULT = std::mem::transmute(*sv.add(4));
            let packed = [0x5Au8; 64];
            let run = |dst: &mut [u8], n: u32, index: u32| {
                let r = request(SOURCE_MEMORY_BIT | 7 << BCPACK_SHIFT, dst.as_mut_ptr() as u64, n, packed.as_ptr() as u64, 0, packed.len() as u32);
                enqueue(q, r.as_ptr());
                status(q, s, index);
                submit(q);
                let t = std::time::Instant::now();
                while !complete(s, index) && t.elapsed().as_secs() < 5 {
                    std::thread::yield_now();
                }
                hresult(s, index)
            };
            // a BC7 destination of 0x1008 bytes (a part block at the end), then guard bytes
            let n = 0x1008usize;
            let mut dst = vec![0xCDu8; n + 64];
            assert!(!queue(q).bcpack_fill.load(Ordering::Relaxed), "off unless XCL_DSTORAGE_BCPACK_FILL is set");
            assert_eq!(run(&mut dst, n as u32, 0), E_DECOMPRESSION);
            assert!(dst.iter().all(|&b| b == 0xCD), "without the switch the destination is left as it was");

            queue(q).bcpack_fill.store(true, Ordering::Relaxed);
            let before = BCPACK.filled_bytes[7].load(Ordering::Relaxed);
            assert_eq!(run(&mut dst, n as u32, 1), E_DECOMPRESSION, "the fill does not change the status");
            let block = bcpack_fill_block(7).unwrap();
            assert!(dst[..n].chunks(16).all(|c| c == &block[..c.len()]), "solid BC7 blocks up to the destination size");
            assert!(dst[n..].iter().all(|&b| b == 0xCD), "nothing past the destination size");
            assert!(BCPACK.filled_bytes[7].load(Ordering::Relaxed) >= before + n as u64);
            queue_close(q);
            queue_release(q);
            status_release(s);
        }
    }

    /// DirectStorage closes a queue automatically when its last reference is released: in-flight
    /// worker activity finishes before `Release` returns, remaining submitted requests do not write
    /// caller buffers afterwards, and both submitted and unsubmitted status entries complete with
    /// `E_QUEUE_CLOSED` rather than staying `E_PENDING` or succeeding with `S_OK`.
    #[test]
    fn last_queue_release_closes_pending_and_submitted_work_before_returning() {
        unsafe {
            let mut f = std::ptr::null_mut();
            assert_eq!(DStorageGetFactory(&IID_FACTORY, &mut f), S_OK);
            let vt = *(f as *const *const usize);
            let create_queue: unsafe extern "system" fn(*mut c_void, *const u8, *const GUID, *mut *mut c_void) -> HRESULT = std::mem::transmute(*vt.add(3));
            let create_status: unsafe extern "system" fn(*mut c_void, u32, *const u8, *const GUID, *mut *mut c_void) -> HRESULT = std::mem::transmute(*vt.add(5));
            let mut desc = [0u8; 0x18];
            desc[0..4].copy_from_slice(&1u32.to_le_bytes());
            desc[8..10].copy_from_slice(&16u16.to_le_bytes());
            desc[10] = 2;
            let mut q = std::ptr::null_mut();
            assert_eq!(create_queue(f, desc.as_ptr(), &IID_QUEUE, &mut q), S_OK);
            let mut s = std::ptr::null_mut();
            assert_eq!(create_status(f, 2, std::ptr::null(), &IID_STATUS_ARRAY, &mut s), S_OK);
            let weak = Arc::downgrade(queue(q));

            let entered = Arc::new(std::sync::Barrier::new(2));
            let hold = Arc::new(std::sync::Barrier::new(2));
            let src = [0x5Au8; 64];
            let mut dst_submitted = [0xCDu8; 64];
            queue(q).enqueue(Entry::Gate(Arc::clone(&entered)));
            queue(q).enqueue(Entry::Gate(Arc::clone(&hold)));
            let req = request(SOURCE_MEMORY_BIT, dst_submitted.as_mut_ptr() as u64, dst_submitted.len() as u32, src.as_ptr() as u64, 0, src.len() as u32);
            queue_enqueue_request(q, req.as_ptr());
            queue_enqueue_status(q, s, 0);
            queue_submit(q);
            entered.wait();

            // Unsubmitted status entry sitting in `pending.entries` (`E_PENDING`).
            queue_enqueue_status(q, s, 1);
            assert!(!status_is_complete(s, 1));

            let unblocked = Arc::new(AtomicBool::new(false));
            let u2 = Arc::clone(&unblocked);
            let h2 = Arc::clone(&hold);
            let t = std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(30));
                u2.store(true, Ordering::Release);
                h2.wait();
            });

            assert_eq!(queue_release(q), 0);
            let waited_for_worker = unblocked.load(Ordering::Acquire);
            t.join().unwrap();
            let t0 = std::time::Instant::now();
            while weak.upgrade().is_some() && t0.elapsed().as_secs() < 5 {
                std::thread::yield_now();
            }
            assert!(waited_for_worker, "Release returned before active worker item finished");
            assert!(weak.upgrade().is_none());
            assert!(dst_submitted.iter().all(|&b| b == 0xCD), "submitted request wrote to buffer after final Release");
            assert!(status_is_complete(s, 0) && status_get_hresult(s, 0) == E_QUEUE_CLOSED, "submitted status must get E_QUEUE_CLOSED");
            assert!(status_is_complete(s, 1) && status_get_hresult(s, 1) == E_QUEUE_CLOSED, "pending status must get E_QUEUE_CLOSED");
            status_release(s);
        }
    }

    // -----------------------------------------------------------------------------------
    // the shared worker pool: order, cancellation, errors and priorities across queues
    // -----------------------------------------------------------------------------------

    const BLOCK: usize = 4096;

    /// A fence that checks, at each signal, that the requests before it in its queue have written
    /// their blocks and none after it has: a queue's requests write at their turn, in order.
    #[repr(C)]
    struct OrderFence {
        vtbl: *const usize,
        refs: AtomicU32,
        blocks: *const u8,
        count: usize,
        seed: u8,
        last: AtomicU64,
        bad: Mutex<Option<String>>,
    }

    fn order_fence_vtbl() -> &'static [usize] {
        unsafe extern "system" fn qi(_p: *mut c_void, _riid: *const GUID, _out: *mut *mut c_void) -> HRESULT {
            E_NOINTERFACE
        }
        unsafe extern "system" fn add(p: *mut c_void) -> u32 {
            (*(p as *const OrderFence)).refs.fetch_add(1, Ordering::Relaxed) + 1
        }
        unsafe extern "system" fn rel(p: *mut c_void) -> u32 {
            (*(p as *const OrderFence)).refs.fetch_sub(1, Ordering::AcqRel) - 1
        }
        unsafe extern "system" fn sig(p: *mut c_void, v: u64) -> HRESULT {
            let f = &*(p as *const OrderFence);
            let last = f.last.swap(v, Ordering::AcqRel);
            let mut bad = f.bad.lock().unwrap();
            if v != last + 1 {
                bad.get_or_insert(format!("signal {v} after {last}"));
            }
            for k in 0..f.count {
                let at = std::ptr::read_volatile(f.blocks.add(k * BLOCK));
                let want = if (k as u64) < v { f.seed.wrapping_add(k as u8) } else { 0 };
                if at != want {
                    bad.get_or_insert(format!("at signal {v}: block {k} holds {at:#x}, not {want:#x}"));
                }
            }
            S_OK
        }
        static V: OnceLock<&'static [usize]> = OnceLock::new();
        V.get_or_init(|| {
            let mut v = vec![0usize; FENCE_SIGNAL + 1];
            v[0] = qi as usize;
            v[1] = add as usize;
            v[2] = rel as usize;
            v[FENCE_SIGNAL] = sig as usize;
            Box::leak(v.into_boxed_slice())
        })
    }

    struct PoolQueue {
        q: *mut c_void,
        file: *mut c_void,
        path: std::path::PathBuf,
        status: *mut c_void,
    }

    impl PoolQueue {
        /// A queue (a file queue when `data` is given, a memory queue otherwise) and a status array.
        unsafe fn new(priority: i8, capacity: u16, statuses: u32, data: Option<&[u8]>, name: &str) -> PoolQueue {
            let mut desc = [0u8; 0x18];
            desc[0..4].copy_from_slice(&u32::from(data.is_none()).to_le_bytes());
            desc[8..10].copy_from_slice(&capacity.to_le_bytes());
            desc[10] = priority as u8;
            let mut q = std::ptr::null_mut();
            assert_eq!(create_queue(desc.as_ptr(), false, &IID_QUEUE, &mut q), S_OK);
            let path = std::env::temp_dir().join(format!("xcl-dstorage-pool-{name}-{}.bin", std::process::id()));
            let mut file = std::ptr::null_mut();
            if let Some(data) = data {
                std::fs::write(&path, data).unwrap();
                let wide: Vec<u16> = path.to_string_lossy().encode_utf16().chain([0]).collect();
                assert_eq!(factory_open_file(std::ptr::null_mut(), wide.as_ptr(), &IID_FILE, &mut file), S_OK);
            }
            let mut status = std::ptr::null_mut();
            assert_eq!(factory_create_status_array(std::ptr::null_mut(), statuses, std::ptr::null(), &IID_STATUS_ARRAY, &mut status), S_OK);
            PoolQueue { q, file, path, status }
        }

        fn lane(&self) -> u64 {
            unsafe { queue(self.q).lane }
        }

        unsafe fn wait(&self, index: u32) -> HRESULT {
            let t = std::time::Instant::now();
            while !status_is_complete(self.status, index) && t.elapsed().as_secs() < 20 {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            assert!(status_is_complete(self.status, index), "status {index} still pending");
            status_get_hresult(self.status, index)
        }

        unsafe fn errors(&self) -> (u32, HRESULT) {
            let mut rec = [0u8; ERROR_RECORD_BYTES];
            queue_retrieve_error_record(self.q, rec.as_mut_ptr());
            (u32::from_le_bytes(rec[0..4].try_into().unwrap()), i32::from_le_bytes(rec[8..12].try_into().unwrap()))
        }

        unsafe fn end(self) {
            queue_close(self.q);
            queue_release(self.q);
            if !self.file.is_null() {
                file_release(self.file);
                let _ = std::fs::remove_file(&self.path);
            }
            status_release(self.status);
        }
    }

    unsafe fn wait_until(what: &str, done: impl Fn() -> bool) {
        let t = std::time::Instant::now();
        while !done() && t.elapsed().as_secs() < 20 {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!(done(), "{what}");
    }

    /// `count` blocks of `BLOCK` bytes, block k all `seed + k`, each zlib-compressed: the file and
    /// each block's (offset, packed size).
    fn packed_blocks(count: usize, seed: u8) -> (Vec<u8>, Vec<(u64, u32)>) {
        let (mut file, mut at) = (Vec::new(), Vec::new());
        for k in 0..count {
            let packed = miniz_oxide::deflate::compress_to_vec_zlib(&[seed.wrapping_add(k as u8); BLOCK], 6);
            at.push((file.len() as u64, packed.len() as u32));
            file.extend_from_slice(&packed);
        }
        (file, at)
    }

    /// Queues of every priority on the pool at once (file queues reading zlib blocks ahead, one
    /// memory queue): each one's requests write at their turn and in order, and each signal comes
    /// after exactly the requests before it.
    #[test]
    fn the_shared_pool_keeps_each_queues_order() {
        const N: usize = 48;
        unsafe {
            let mut runs = Vec::new();
            for (i, priority) in [-1i8, 0, 1, 2].into_iter().enumerate() {
                let seed = 0x10 + 0x40 * i as u8;
                let (file, at) = packed_blocks(N, seed);
                let memory = priority == 2;
                let pq = PoolQueue::new(priority, 256, 1, (!memory).then_some(&file[..]), &format!("order{i}"));
                let dst = vec![0u8; N * BLOCK].into_boxed_slice();
                let fence = Box::new(OrderFence {
                    vtbl: order_fence_vtbl().as_ptr(),
                    refs: AtomicU32::new(1),
                    blocks: dst.as_ptr(),
                    count: N,
                    seed,
                    last: AtomicU64::new(0),
                    bad: Mutex::new(None),
                });
                runs.push((pq, file, at, dst, fence, memory));
            }
            for (pq, file, at, dst, fence, memory) in &runs {
                for (k, &(offset, size)) in at.iter().enumerate() {
                    let d = dst.as_ptr().add(k * BLOCK) as u64;
                    let r = if *memory {
                        request(SOURCE_MEMORY_BIT | ZLIB_BIT, d, BLOCK as u32, file.as_ptr() as u64 + offset, 0, size)
                    } else {
                        request(ZLIB_BIT, d, BLOCK as u32, pq.file as u64, offset, size)
                    };
                    queue_enqueue_request(pq.q, r.as_ptr());
                    queue_enqueue_signal(pq.q, &**fence as *const OrderFence as *mut c_void, k as u64 + 1);
                }
                queue_enqueue_status(pq.q, pq.status, 0);
            }
            for (pq, ..) in &runs {
                queue_submit(pq.q);
            }
            for (pq, _, _, dst, fence, _) in runs {
                assert_eq!(pq.wait(0), S_OK);
                assert_eq!(fence.bad.lock().unwrap().take(), None);
                assert_eq!(fence.last.load(Ordering::Acquire), N as u64);
                assert!((0..N).all(|k| dst[k * BLOCK..(k + 1) * BLOCK].iter().all(|&b| b == fence.seed.wrapping_add(k as u8))));
                assert_eq!(pq.errors().0, 0);
                pq.end();
                assert_eq!(fence.refs.load(Ordering::Acquire), 1, "every fence reference released");
            }
        }
    }

    /// Requests read ahead and then cancelled before their turn write nothing and report nothing;
    /// one enqueued after the cancel with the same tag runs.
    #[test]
    fn a_cancel_covers_requests_already_read_ahead() {
        const N: usize = 16;
        unsafe {
            let (file, at) = packed_blocks(N + 1, 0x21);
            let pq = PoolQueue::new(0, 64, 1, Some(&file), "cancel");
            pool::hold(pq.lane(), true);
            let mut dst = vec![0u8; (N + 1) * BLOCK];
            for (k, &(offset, size)) in at.iter().take(N).enumerate() {
                let tag = if k % 2 == 0 { 0x10 } else { 0x20 };
                let d = dst.as_mut_ptr().add(k * BLOCK) as u64;
                queue_enqueue_request(pq.q, request_ex(ZLIB_BIT, d, 0, BLOCK as u32, pq.file as u64, offset, size, 0, tag).as_ptr());
            }
            queue_submit(pq.q);
            wait_until("every request read ahead", || pool::read_ahead(pq.lane()) == N);
            queue_cancel(pq.q, 0xF0, 0x10);
            let (offset, size) = at[N];
            let d = dst.as_mut_ptr().add(N * BLOCK) as u64;
            queue_enqueue_request(pq.q, request_ex(ZLIB_BIT, d, 0, BLOCK as u32, pq.file as u64, offset, size, 0, 0x10).as_ptr());
            queue_enqueue_status(pq.q, pq.status, 0);
            queue_submit(pq.q);
            pool::hold(pq.lane(), false);
            assert_eq!(pq.wait(0), S_OK);
            for k in 0..=N {
                let block = &dst[k * BLOCK..(k + 1) * BLOCK];
                let runs = k == N || k % 2 == 1;
                assert!(block.iter().all(|&b| b == if runs { 0x21 + k as u8 } else { 0 }), "block {k}");
            }
            assert_eq!(pq.errors().0, 0, "cancelled requests record no error");
            assert!(queue(pq.q).cancels.lock().unwrap().is_empty(), "expired cancel rules are pruned at the turn");
            pq.end();
        }
    }

    /// Failures found while reading ahead (out of order) still reach the statuses and the error
    /// record in queue order.
    #[test]
    fn read_ahead_failures_report_in_queue_order() {
        unsafe {
            let (file, at) = packed_blocks(1, 0x33);
            let pq = PoolQueue::new(1, 64, 3, Some(&file), "errors");
            pool::hold(pq.lane(), true);
            let mut dst = vec![0u8; BLOCK];
            let d = dst.as_mut_ptr() as u64;
            let len = file.len() as u64;
            queue_enqueue_request(pq.q, request(0, d, 16, pq.file as u64, len, 16).as_ptr());
            queue_enqueue_status(pq.q, pq.status, 0);
            queue_enqueue_request(pq.q, request(ZLIB_BIT, d, BLOCK as u32, pq.file as u64, at[0].0, at[0].1).as_ptr());
            queue_enqueue_status(pq.q, pq.status, 1);
            queue_enqueue_request(pq.q, request(ZLIB_BIT, d, BLOCK as u32, pq.file as u64, 2, at[0].1 - 2).as_ptr());
            queue_enqueue_request(pq.q, request(0, d, 16, pq.file as u64, len, 16).as_ptr());
            queue_enqueue_status(pq.q, pq.status, 2);
            queue_submit(pq.q);
            wait_until("every request read ahead", || pool::read_ahead(pq.lane()) == 4);
            pool::hold(pq.lane(), false);
            assert_eq!((pq.wait(0), pq.wait(1), pq.wait(2)), (E_END_OF_FILE, S_OK, E_ZLIB_DATA));
            assert!(dst.iter().all(|&b| b == 0x33));
            assert_eq!(pq.errors(), (3, E_END_OF_FILE), "the first failure in queue order");
            pq.end();
        }
    }

    /// A low queue submitted behind a long high burst finishes long before the burst does.
    #[test]
    fn a_high_burst_does_not_starve_a_low_queue() {
        const BURST: u32 = 1500;
        unsafe {
            let data = vec![0x5Cu8; 64 << 10];
            let high = PoolQueue::new(1, 4096, 1, Some(&data), "burst");
            let low = PoolQueue::new(-1, 8, 1, Some(&data), "starve");
            let mut sink = vec![0u8; data.len()];
            let mut mine = vec![0u8; data.len()];
            pool::hold(high.lane(), true);
            for _ in 0..BURST {
                queue_enqueue_request(high.q, request(0, sink.as_mut_ptr() as u64, data.len() as u32, high.file as u64, 0, data.len() as u32).as_ptr());
            }
            queue_enqueue_status(high.q, high.status, 0);
            queue_submit(high.q);
            queue_enqueue_request(low.q, request(0, mine.as_mut_ptr() as u64, data.len() as u32, low.file as u64, 0, data.len() as u32).as_ptr());
            queue_enqueue_status(low.q, low.status, 0);
            queue_submit(low.q);
            pool::hold(high.lane(), false);
            assert_eq!(low.wait(0), S_OK);
            let left = pool::submitted(high.lane());
            assert!(left > BURST as usize / 2, "the low queue waited for most of the burst: {left} of {BURST} high entries left");
            assert_eq!(mine, data);
            assert_eq!(high.wait(0), S_OK);
            let stats = pool::stats();
            assert!(stats.priorities["low"].done >= 2 && stats.priorities["high"].done > BURST as u64, "{stats:?}");
            high.end();
            low.end();
        }
    }
}
