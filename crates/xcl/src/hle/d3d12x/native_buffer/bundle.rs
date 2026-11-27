//! Managed BUNDLE recordings of console code, replayed into a private native BUNDLE at each
//! ExecuteBundle site (`recordings::bundle_site`).
//!
//! The `title_list` object of a BUNDLE has its own vtable (`title_list::bundle_vtable`): the typed
//! slots of `slots` log their arguments and run the canonical console method; every other slot
//! is forwarded as before and marks the recording incomplete. At Close a recording that
//! `recordings` tracks (private IO locator or native leases) installs an immutable recipe only
//! when every call since Reset was logged; each site then gets its own allocator, list, locator
//! and arena by running the same canonical methods again. An incomplete managed recording
//! installs nothing, so its sites fail instead of replaying part of it; an unmanaged one stays
//! the shared native bundle.

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

use windows::core::Interface;
use windows::Win32::Graphics::Direct3D12::*;

use crate::hle::d3d12x::physical_directory::recordings::{self, PrivateBundle};
use crate::hle::d3d12x::{bind, console_list, list_x, pso_async, real, shader_code};

#[derive(Clone, Debug, PartialEq)]
enum Cmd {
    Pipeline(usize),
    Signature { compute: bool, rs: usize },
    Table { compute: bool, param: u32, handle: u64 },
    Constant { compute: bool, param: u32, value: u32, offset: u32 },
    Constants { compute: bool, param: u32, count: u32, values: Option<Vec<u32>>, offset: u32 },
    Descriptor { compute: bool, param: u32, va: u64 },
    Topology(u32),
    VertexBuffers { start: u32, count: u32, views: Option<Vec<D3D12_VERTEX_BUFFER_VIEW>> },
    IndexBuffer(Option<D3D12_INDEX_BUFFER_VIEW>),
    BlendFactor(Option<[f32; 4]>),
    StencilRef(u32),
    Heaps { count: u32, heaps: Option<Vec<usize>> },
    Draw { vertices: u32, instances: u32, first_vertex: u32, first_instance: u32 },
    DrawIndexed { indices: u32, instances: u32, first_index: u32, base_vertex: i32, first_instance: u32 },
}

fn ptr<T>(v: &Option<Vec<T>>) -> *const T {
    v.as_ref().map_or(std::ptr::null(), |v| v.as_ptr())
}

impl Cmd {
    unsafe fn replay(&self, pc: *mut c_void) {
        match self {
            Cmd::Pipeline(pso) => bind::set_pipeline_state(pc, *pso as *mut c_void),
            Cmd::Signature { compute: false, rs } => bind::set_graphics_root_signature(pc, *rs as *mut c_void),
            Cmd::Signature { compute: true, rs } => bind::set_compute_root_signature(pc, *rs as *mut c_void),
            Cmd::Table { compute: false, param, handle } => bind::set_graphics_root_table(pc, *param, *handle),
            Cmd::Table { compute: true, param, handle } => bind::set_compute_root_table(pc, *param, *handle),
            Cmd::Constant { compute: false, param, value, offset } => bind::set_graphics_root_constant(pc, *param, *value, *offset),
            Cmd::Constant { compute: true, param, value, offset } => bind::set_compute_root_constant(pc, *param, *value, *offset),
            Cmd::Constants { compute: false, param, count, values, offset } => bind::set_graphics_root_constants(pc, *param, *count, ptr(values), *offset),
            Cmd::Constants { compute: true, param, count, values, offset } => bind::set_compute_root_constants(pc, *param, *count, ptr(values), *offset),
            Cmd::Descriptor { compute: false, param, va } => bind::set_graphics_root_descriptor(pc, *param, *va),
            Cmd::Descriptor { compute: true, param, va } => bind::set_compute_root_descriptor(pc, *param, *va),
            Cmd::Topology(t) => list_x::ia_set_topology_x(pc, *t),
            Cmd::VertexBuffers { start, count, views } => list_x::ia_set_vertex_buffers_x(pc, *start, *count, ptr(views)),
            Cmd::IndexBuffer(view) => list_x::ia_set_index_buffer_x(pc, view.as_ref().map_or(std::ptr::null(), |v| v)),
            Cmd::BlendFactor(f) => list_x::om_set_blend_factor_x(pc, f.as_ref().map_or(std::ptr::null(), |f| f)),
            Cmd::StencilRef(r) => list_x::om_set_stencil_ref_x(pc, *r),
            Cmd::Heaps { count, heaps } => list_x::set_descriptor_heaps_x(pc, *count, ptr(heaps)),
            Cmd::Draw { vertices, instances, first_vertex, first_instance } => bind::draw_instanced(pc, *vertices, *instances, *first_vertex, *first_instance),
            Cmd::DrawIndexed { indices, instances, first_index, base_vertex, first_instance } => {
                bind::draw_indexed_instanced(pc, *indices, *instances, *first_index, *base_vertex, *first_instance)
            }
        }
    }
}

/// Why a recording cannot be replayed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Gap {
    /// a console slot without a recipe command
    Slot(u32),
    /// recorded into a split segment instead of the bundle itself
    Split,
    /// QueryInterface handed out an object other than the wrapper: calls through it are not seen
    TearOff,
    /// no creation or Reset seen for the recording
    Unopened,
}

#[derive(Default)]
struct Log {
    pso: usize,
    cmds: Vec<Cmd>,
    gap: Option<Gap>,
    /// kept across Reset: the tear-off can record any later recording
    tainted: bool,
}

/// primary native BUNDLE -> its current recording
static LOGS: LazyLock<Mutex<HashMap<usize, Log>>> = LazyLock::new(Default::default);

/// The bundle `pc` was created or reset with the title's `pso`: a new recording.
pub(super) fn begin(pc: usize, pso: usize) {
    let mut logs = LOGS.lock().unwrap();
    let log = logs.entry(pc).or_default();
    let tainted = log.tainted;
    *log = Log { pso, tainted, ..Default::default() };
}

pub(super) fn taint(pc: usize) {
    LOGS.lock().unwrap().entry(pc).or_default().tainted = true;
}

pub(super) fn forget(pc: usize) {
    LOGS.lock().unwrap().remove(&pc);
}

/// A bundle wrapper slot without a recipe command, called before it is forwarded.
pub(super) unsafe extern "system" fn uncovered(this: *mut c_void, slot: u32) {
    let primary = *((this as usize + super::F_PRIMARY) as *const usize);
    if let Some(log) = LOGS.lock().unwrap().get_mut(&primary) {
        log.gap.get_or_insert(Gap::Slot(slot));
    }
}

/// `cmd` for the wrapper `this`; the list the canonical method runs on.
unsafe fn log(this: *mut c_void, cmd: Cmd) -> *mut c_void {
    let primary = *((this as usize + super::F_PRIMARY) as *const usize);
    let backing = *((this as usize + super::F_BACKING) as *const usize);
    if let Some(log) = LOGS.lock().unwrap().get_mut(&primary) {
        if backing == primary {
            log.cmds.push(cmd);
        } else {
            log.gap.get_or_insert(Gap::Split);
        }
    }
    backing as *mut c_void
}

/// Close of the bundle `pc` succeeded: its recipe, when the recording is managed and complete.
pub(super) unsafe fn closed(pc: *mut c_void) {
    let (pso, cmds, gap) = match LOGS.lock().unwrap().get_mut(&(pc as usize)) {
        Some(log) => (log.pso, std::mem::take(&mut log.cmds), log.gap.or(log.tainted.then_some(Gap::TearOff))),
        None => (0, Vec::new(), Some(Gap::Unopened)),
    };
    if recordings::capture(&[pc]).is_empty() {
        return;
    }
    if let Some(gap) = gap {
        static TOLD: AtomicU32 = AtomicU32::new(0);
        if TOLD.fetch_add(1, Ordering::Relaxed) < 8 {
            crate::xlog_warn!("D3D12.X: managed bundle {pc:p} has no complete site recipe ({gap:?}): its ExecuteBundle sites fail instead of replaying part of it");
        }
        return;
    }
    let recipe = Arc::new(Recipe { pso, cmds });
    if let Err(why) = recordings::install_bundle_recipe(pc as usize, Arc::new(move |parent| unsafe { recipe.instance(parent) })) {
        crate::xlog_warn!("D3D12.X: bundle {pc:p}: {why}");
    }
}

struct Recipe {
    pso: usize,
    cmds: Vec<Cmd>,
}

/// What a site's private bundle owns besides the list: its allocator, and through it the
/// list's arena (`bind`), freed once nothing executes the bundle any more.
struct Lease {
    pc: usize,
    allocator: ID3D12CommandAllocator,
}

impl Drop for Lease {
    fn drop(&mut self) {
        bind::forget(self.pc as *mut c_void);
        console_list::allocator_reset(self.allocator.as_raw());
    }
}

impl Recipe {
    unsafe fn instance(&self, parent: usize) -> Result<PrivateBundle, String> {
        let parent = parent as *mut c_void;
        let parent = ID3D12GraphicsCommandList::from_raw_borrowed(&parent).ok_or("no parent list")?;
        let mut dev: Option<ID3D12Device> = None;
        parent.GetDevice(&mut dev).map_err(|e| format!("parent device: {e}"))?;
        let dev = dev.ok_or("parent device: none")?;
        let dev = dev.as_raw();
        if !real::is_adopted(dev) {
            return Err("parent device is not adopted".into());
        }
        let mut raw = std::ptr::null_mut();
        let iid = &ID3D12CommandAllocator::IID as *const windows::core::GUID as *const windows_sys::core::GUID;
        let hr = real::dev_create_command_allocator(dev, D3D12_COMMAND_LIST_TYPE_BUNDLE.0 as usize, iid, &mut raw);
        if hr < 0 || raw.is_null() {
            return Err(format!("private bundle allocator: {:#x}", hr as u32));
        }
        let allocator = ID3D12CommandAllocator::from_raw(raw);
        let a = allocator.as_raw() as usize;
        // a native BUNDLE even where the title's own bundles are DIRECT lists (`single`)
        let mut pc = std::ptr::null_mut();
        let iid = &ID3D12GraphicsCommandList::IID as *const windows::core::GUID as *const windows_sys::core::GUID;
        let first = pso_async::resolve_for_bind(self.pso);
        shader_code::in_bundle(first);
        let hr = real::dev_create_command_list(dev, 0, D3D12_COMMAND_LIST_TYPE_BUNDLE.0 as usize, a, first, iid, &mut pc);
        if hr < 0 || pc.is_null() {
            return Err(format!("private BUNDLE creation: {:#x}", hr as u32));
        }
        let list = ID3D12GraphicsCommandList::from_raw(pc);
        let lease = Arc::new(Lease { pc: pc as usize, allocator });
        list_x::on_create(pc, a, self.pso, D3D12_COMMAND_LIST_TYPE_BUNDLE);
        bind::reset(pc, a, self.pso);
        for cmd in &self.cmds {
            cmd.replay(pc);
        }
        let close: unsafe extern "system" fn(*mut c_void) -> i32 = std::mem::transmute(super::pc_slot(pc, 9));
        let hr = close(pc);
        if hr < 0 {
            return Err(format!("private BUNDLE Close: {:#x}", hr as u32));
        }
        PrivateBundle::new(list, lease).map_err(str::to_owned)
    }
}

unsafe fn copied<T: Copy>(p: *const T, n: u32) -> Option<Vec<T>> {
    (!p.is_null()).then(|| std::slice::from_raw_parts(p, n as usize).to_vec())
}

unsafe extern "system" fn draw_instanced(this: *mut c_void, vertices: u32, instances: u32, first_vertex: u32, first_instance: u32) {
    let pc = log(this, Cmd::Draw { vertices, instances, first_vertex, first_instance });
    bind::draw_instanced(pc, vertices, instances, first_vertex, first_instance)
}

unsafe extern "system" fn draw_indexed_instanced(this: *mut c_void, indices: u32, instances: u32, first_index: u32, base_vertex: i32, first_instance: u32) {
    let pc = log(this, Cmd::DrawIndexed { indices, instances, first_index, base_vertex, first_instance });
    bind::draw_indexed_instanced(pc, indices, instances, first_index, base_vertex, first_instance)
}

unsafe extern "system" fn topology(this: *mut c_void, t: u32) {
    list_x::ia_set_topology_x(log(this, Cmd::Topology(t)), t)
}

unsafe extern "system" fn blend_factor(this: *mut c_void, f: *const [f32; 4]) {
    list_x::om_set_blend_factor_x(log(this, Cmd::BlendFactor(f.as_ref().copied())), f)
}

unsafe extern "system" fn stencil_ref(this: *mut c_void, r: u32) {
    list_x::om_set_stencil_ref_x(log(this, Cmd::StencilRef(r)), r)
}

unsafe extern "system" fn pipeline(this: *mut c_void, pso: *mut c_void) {
    bind::set_pipeline_state(log(this, Cmd::Pipeline(pso as usize)), pso)
}

unsafe extern "system" fn heaps(this: *mut c_void, count: u32, heaps: *const usize) {
    list_x::set_descriptor_heaps_x(log(this, Cmd::Heaps { count, heaps: copied(heaps, count) }), count, heaps)
}

unsafe extern "system" fn compute_signature(this: *mut c_void, rs: *mut c_void) {
    bind::set_compute_root_signature(log(this, Cmd::Signature { compute: true, rs: rs as usize }), rs)
}

unsafe extern "system" fn graphics_signature(this: *mut c_void, rs: *mut c_void) {
    bind::set_graphics_root_signature(log(this, Cmd::Signature { compute: false, rs: rs as usize }), rs)
}

unsafe extern "system" fn compute_table(this: *mut c_void, param: u32, handle: u64) {
    bind::set_compute_root_table(log(this, Cmd::Table { compute: true, param, handle }), param, handle)
}

unsafe extern "system" fn graphics_table(this: *mut c_void, param: u32, handle: u64) {
    bind::set_graphics_root_table(log(this, Cmd::Table { compute: false, param, handle }), param, handle)
}

unsafe extern "system" fn compute_constant(this: *mut c_void, param: u32, value: u32, offset: u32) {
    bind::set_compute_root_constant(log(this, Cmd::Constant { compute: true, param, value, offset }), param, value, offset)
}

unsafe extern "system" fn graphics_constant(this: *mut c_void, param: u32, value: u32, offset: u32) {
    bind::set_graphics_root_constant(log(this, Cmd::Constant { compute: false, param, value, offset }), param, value, offset)
}

unsafe extern "system" fn compute_constants(this: *mut c_void, param: u32, count: u32, data: *const u32, offset: u32) {
    let pc = log(this, Cmd::Constants { compute: true, param, count, values: copied(data, count), offset });
    bind::set_compute_root_constants(pc, param, count, data, offset)
}

unsafe extern "system" fn graphics_constants(this: *mut c_void, param: u32, count: u32, data: *const u32, offset: u32) {
    let pc = log(this, Cmd::Constants { compute: false, param, count, values: copied(data, count), offset });
    bind::set_graphics_root_constants(pc, param, count, data, offset)
}

unsafe extern "system" fn compute_descriptor(this: *mut c_void, param: u32, va: u64) {
    bind::set_compute_root_descriptor(log(this, Cmd::Descriptor { compute: true, param, va }), param, va)
}

unsafe extern "system" fn graphics_descriptor(this: *mut c_void, param: u32, va: u64) {
    bind::set_graphics_root_descriptor(log(this, Cmd::Descriptor { compute: false, param, va }), param, va)
}

unsafe extern "system" fn index_buffer(this: *mut c_void, view: *const D3D12_INDEX_BUFFER_VIEW) {
    list_x::ia_set_index_buffer_x(log(this, Cmd::IndexBuffer(view.as_ref().copied())), view)
}

unsafe extern "system" fn vertex_buffers(this: *mut c_void, start: u32, count: u32, views: *const D3D12_VERTEX_BUFFER_VIEW) {
    list_x::ia_set_vertex_buffers_x(log(this, Cmd::VertexBuffers { start, count, views: copied(views, count) }), start, count, views)
}

/// The bundle-legal console slots with a recipe command, each at the slot of its canonical
/// method in `real::console_slots` (all below #59, where the newer layout's shift starts, so
/// one table serves both layouts).
pub(super) fn slots() -> [(usize, usize); 23] {
    [
        (12, draw_instanced as usize),
        (13, draw_indexed_instanced as usize),
        (20, topology as usize),
        (23, blend_factor as usize),
        (24, stencil_ref as usize),
        (25, pipeline as usize),
        (32, heaps as usize),
        (33, compute_signature as usize),
        (34, graphics_signature as usize),
        (35, compute_table as usize),
        (36, graphics_table as usize),
        (37, compute_constant as usize),
        (38, graphics_constant as usize),
        (39, compute_constants as usize),
        (40, graphics_constants as usize),
        (41, compute_descriptor as usize),
        (42, graphics_descriptor as usize),
        (43, compute_descriptor as usize),
        (44, graphics_descriptor as usize),
        (45, compute_descriptor as usize),
        (46, graphics_descriptor as usize),
        (47, index_buffer as usize),
        (48, vertex_buffers as usize),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::{release, wrap};
    use windows_sys::core::GUID;

    /// A stand-in PC list: counts forwarded calls (slot 26), QueryInterface answers `qi_answer`.
    #[repr(C)]
    struct Fake {
        vt: *const usize,
        refs: u32,
        forwarded: u32,
        qi_answer: usize,
    }

    unsafe extern "system" fn fake_qi(this: *mut c_void, _riid: *const GUID, ppv: *mut *mut c_void) -> i32 {
        let f = &mut *(this as *mut Fake);
        f.refs += 1;
        *ppv = if f.qi_answer != 0 { f.qi_answer as *mut c_void } else { this };
        0
    }
    unsafe extern "system" fn fake_add_ref(this: *mut c_void) -> u32 {
        (*(this as *mut Fake)).refs += 1;
        (*(this as *mut Fake)).refs
    }
    unsafe extern "system" fn fake_release(this: *mut c_void) -> u32 {
        (*(this as *mut Fake)).refs -= 1;
        (*(this as *mut Fake)).refs
    }
    unsafe extern "system" fn fake_ok(_: *mut c_void) -> i32 {
        0
    }
    unsafe extern "system" fn fake_reset(_: *mut c_void, _: usize, _: usize) -> i32 {
        0
    }
    unsafe extern "system" fn fake_device(_: *mut c_void, _: *const GUID, _: *mut *mut c_void) -> i32 {
        0x8000_4005u32 as i32
    }
    unsafe extern "system" fn fake_barrier(this: *mut c_void, _: u32, _: *const c_void) {
        (*(this as *mut Fake)).forwarded += 1;
    }

    fn fake(qi_answer: usize) -> *mut c_void {
        let mut vt = vec![0usize; super::super::LIST_SLOTS];
        vt[0] = fake_qi as usize;
        vt[1] = fake_add_ref as usize;
        vt[2] = fake_release as usize;
        vt[7] = fake_device as usize;
        vt[9] = fake_ok as usize;
        vt[10] = fake_reset as usize;
        vt[26] = fake_barrier as usize;
        let vt = Box::leak(vt.into_boxed_slice()).as_ptr();
        Box::into_raw(Box::new(Fake { vt, refs: 1, forwarded: 0, qi_answer })).cast()
    }

    unsafe fn slot<F: Copy>(o: *mut c_void, s: usize) -> F {
        std::mem::transmute_copy(&*(*(o as *const *const usize)).add(s))
    }

    /// A console BUNDLE object over a fake PC list created with title pipeline 0x77.
    unsafe fn bundle(qi_answer: usize) -> (*mut c_void, *mut c_void) {
        let pc = fake(qi_answer);
        list_x::on_create(pc, 0, 0x77, D3D12_COMMAND_LIST_TYPE_BUNDLE);
        (pc, wrap(pc))
    }

    unsafe fn constant(w: *mut c_void) {
        slot::<unsafe extern "system" fn(*mut c_void, u32, u32, u32)>(w, 38)(w, 1, 0xAB, 2);
    }

    unsafe fn close(w: *mut c_void) -> i32 {
        slot::<unsafe extern "system" fn(*mut c_void) -> i32>(w, 9)(w)
    }

    fn logged(pc: *mut c_void) -> (usize, Vec<Cmd>, Option<Gap>, bool) {
        let logs = LOGS.lock().unwrap();
        let l = logs.get(&(pc as usize)).expect("bundle recording logged");
        (l.pso, l.cmds.clone(), l.gap, l.tainted)
    }

    #[test]
    fn a_complete_managed_bundle_installs_a_recipe_each_site_runs() {
        unsafe {
            let (pc, w) = bundle(0);
            assert!(super::super::is_bundle(w));
            assert_eq!(super::super::backing(w), Some(pc));
            constant(w);
            assert_eq!(logged(pc), (0x77, vec![Cmd::Constant { compute: false, param: 1, value: 0xAB, offset: 2 }], None, false));
            recordings::retain_native(pc as usize, Arc::new(())).unwrap();
            assert_eq!(close(w), 0);
            let failed = recordings::uncovered_count(recordings::Uncovered::BundleSite);
            assert!(recordings::bundle_site(fake(0) as usize, pc as usize).is_none(), "the fake parent has no device");
            assert!(recordings::uncovered_count(recordings::Uncovered::BundleSite) > failed, "the installed recipe ran and its failure is counted");
            assert_eq!(release(w), 0);
            assert!(!LOGS.lock().unwrap().contains_key(&(pc as usize)));
        }
    }

    #[test]
    fn an_uncovered_slot_is_forwarded_and_withholds_the_recipe() {
        unsafe {
            let (pc, w) = bundle(0);
            constant(w);
            slot::<unsafe extern "system" fn(*mut c_void, u32, *const c_void)>(w, 26)(w, 1, std::ptr::null());
            assert_eq!((*(pc as *mut Fake)).forwarded, 1);
            assert_eq!(logged(pc).2, Some(Gap::Slot(26)));
            recordings::retain_native(pc as usize, Arc::new(())).unwrap();
            assert_eq!(close(w), 0);
            let missing = recordings::uncovered_count(recordings::Uncovered::Bundle);
            assert!(recordings::bundle_site(fake(0) as usize, pc as usize).is_none());
            assert!(recordings::uncovered_count(recordings::Uncovered::Bundle) > missing, "no recipe is counted, not replayed in part");
            assert_eq!(release(w), 0);
        }
    }

    #[test]
    fn an_unmanaged_bundle_stays_on_the_native_shared_path() {
        unsafe {
            let (pc, w) = bundle(0);
            constant(w);
            slot::<unsafe extern "system" fn(*mut c_void, u32, *const c_void)>(w, 26)(w, 1, std::ptr::null());
            assert_eq!(close(w), 0);
            assert!(recordings::capture(&[pc]).is_empty());
            assert!(recordings::bundle_site(fake(0) as usize, pc as usize).is_none());
            assert_eq!(release(w), 0);
        }
    }

    #[test]
    fn a_foreign_tear_off_taints_later_recordings() {
        unsafe {
            let other = fake(0);
            let (pc, w) = bundle(other as usize);
            let mut out = std::ptr::null_mut();
            assert_eq!(slot::<unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> i32>(w, 0)(w, &std::mem::zeroed(), &mut out), 0);
            assert_eq!(out, other);
            fake_release(pc);
            assert_eq!(slot::<unsafe extern "system" fn(*mut c_void, usize, usize) -> i32>(w, 10)(w, 1, 0x99), 0);
            constant(w);
            let (pso, cmds, gap, tainted) = logged(pc);
            assert_eq!((pso, cmds.len(), gap, tainted), (0x99, 1, None, true));
            recordings::retain_native(pc as usize, Arc::new(())).unwrap();
            assert_eq!(close(w), 0);
            let missing = recordings::uncovered_count(recordings::Uncovered::Bundle);
            assert!(recordings::bundle_site(fake(0) as usize, pc as usize).is_none());
            assert!(recordings::uncovered_count(recordings::Uncovered::Bundle) > missing, "no recipe is counted, not replayed in part");
            assert_eq!(release(w), 0);
        }
    }

    #[test]
    fn recipe_slots_are_distinct_bundle_legal_console_slots() {
        let s = slots();
        let mut seen: Vec<usize> = s.iter().map(|e| e.0).collect();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), s.len());
        assert!(seen.iter().all(|&k| k > 10 && k < 59));
        assert!(!seen.iter().any(|k| [14, 15, 16, 17, 21, 22, 26, 27, 31].contains(k)));
    }
}
