//! `XCL_GPU_MARKERS=1`: what the op a queue does not leave was given, read when the first GPU wait
//! times out (`markers::dump`). For each op the GPU has reached and not completed: every word of
//! its root arguments (the user data its shaders' registers and argument table are made of), and
//! behind each argument what is there now – a root descriptor's memory, and each descriptor of a
//! table: where it is, its words as they are, what they decode to, the PC views of its slot and,
//! for a buffer, the start of its memory.
//!
//! The memory is the CPU's copy at the time of the dump. What a shader wrote on the GPU is not
//! in it, and the title may have written over it since the op was recorded.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use super::console_list::{Layout, Param};

/// descriptors read of each table: a root argument does not say how long its table is, and
/// which of its slots a shader loads is not known here (`XCL_HANG_SLOTS` for more)
const SLOTS: u32 = 256;
/// bytes shown of a root descriptor's memory and of a buffer no bigger than a constant buffer
/// can be (`CONSTANTS_MOST`)
const CONSTANT_BYTES: usize = 4096;
const CONSTANTS_MOST: u64 = 64 << 10;
/// bytes shown of any other buffer (`XCL_HANG_BUFFER_BYTES` for more)
const BUFFER_BYTES: usize = 256;
/// ops described in full at one dump
pub(super) const OPS: usize = 6;
/// draws and mesh calls whose root arguments are kept
const GRAPHICS_KEPT: usize = 16384;

fn env_or(name: &str, default: u32) -> u32 {
    std::env::var(name).ok().and_then(|v| v.trim().parse().ok()).unwrap_or(default)
}

/// A draw, mesh call or indirect call of them as it was recorded: its place (marker slot,
/// generation, op from 0) and its graphics root arguments.
struct GraphicsOp {
    at: (usize, u32, usize),
    layout: Arc<Layout>,
    args: Vec<u8>,
}

static GRAPHICS_OPS: Mutex<VecDeque<GraphicsOp>> = Mutex::new(VecDeque::new());

/// The op just marked on `list` (`markers::after_op`) is not a dispatch: its graphics root arguments.
pub(super) fn note_graphics_op(list: usize, layout: Arc<Layout>, args: Vec<u8>) {
    let Some((slot, generation, ops)) = super::markers::position(list) else { return };
    let mut g = GRAPHICS_OPS.lock().unwrap();
    if g.len() >= GRAPHICS_KEPT {
        g.pop_front();
    }
    g.push_back(GraphicsOp { at: (slot, generation, ops.saturating_sub(1)), layout, args });
}

/// What op `op` (from 0) of the recording at marker `slot` / `generation` was given, a
/// dispatch (`bind::note_dispatch_snapshot` kept its arguments) or not (`note_graphics_op`).
pub(super) fn describe(slot: usize, generation: u32, op: usize, dispatch: bool) -> Vec<String> {
    let kept = if dispatch {
        super::bind::dispatch_args(slot, generation, op)
    } else {
        GRAPHICS_OPS.lock().unwrap().iter().rev().find(|g| g.at == (slot, generation, op)).map(|g| (g.layout.clone(), g.args.clone()))
    };
    match kept {
        Some((layout, args)) => arguments(&layout, &args, !dispatch),
        None => vec!["hang capture: its root arguments were not kept".into()],
    }
}

fn hex(w: impl IntoIterator<Item = u32>) -> String {
    w.into_iter().map(|x| format!("{x:x}")).collect::<Vec<_>>().join(" ")
}

/// The root arguments `args` of `layout` (a draw's when `gfx`) and what is behind each now.
pub(super) fn arguments(layout: &Layout, args: &[u8], gfx: bool) -> Vec<String> {
    let word = |at: usize| args.get(at..at + 4).map_or(0, |w| u32::from_le_bytes(w.try_into().unwrap()));
    let (first, inline) = if gfx { (8, layout.inline_gfx) } else { (4, layout.inline) };
    let words: Vec<String> = (0..args.len().min(4096) / 4)
        .map(|k| if 4 * k as u32 + 4 <= inline { format!("s{}={:x}", first + k, word(4 * k)) } else { format!("+{:#x}={:x}", 4 * k, word(4 * k)) })
        .collect();
    let mut out = vec![format!(
        "hang capture: user data, {} bytes of root arguments; at most the first {inline} are in registers from s{first} (a shader with fewer user registers reads the rest from its argument table): {}",
        args.len(),
        words.join(" ")
    )];
    for (k, p) in layout.params.iter().enumerate() {
        match *p {
            Param::Constants { at, count } => out.push(format!("hang capture: p{k}: {count} constants: {}", hex((0..count.min(1024) as usize).map(|n| word(at as usize + 4 * n))))),
            Param::Descriptor { at } => {
                let va = word(at as usize) as u64 | (word(at as usize + 4) as u64) << 32;
                if va == 0 {
                    out.push(format!("hang capture: p{k}: root descriptor, not set"));
                } else {
                    out.push(format!("hang capture: p{k}: root descriptor at {va:#x}; its first {CONSTANT_BYTES} bytes in the CPU's copy now:"));
                    memory(&mut out, &format!("p{k}"), va, CONSTANT_BYTES);
                }
            }
            Param::Table { at } => table(&mut out, k, word(at as usize)),
        }
    }
    out
}

/// The descriptors of the table at console GPU address `table` (root argument `param`): each
/// one that is not empty, to the end of its heap or `SLOTS`.
fn table(out: &mut Vec<String>, param: usize, table: u32) {
    let Some(uma) = crate::gpu::uma::get() else {
        out.push(format!("hang capture: p{param}: table at {table:#x}; no unified memory to read it from"));
        return;
    };
    let most = env_or("XCL_HANG_SLOTS", SLOTS);
    let buffer_bytes = env_or("XCL_HANG_BUFFER_BYTES", BUFFER_BYTES as u32) as usize;
    let (mut read, mut empty) = (0u32, Vec::new());
    let mut addr = table;
    for k in 0..most {
        let Some(s) = super::console_heap::lookup((uma.base() + addr as usize) as u64) else { break };
        let n = if s.sampler { 4 } else { 8 };
        let mut w = [0u32; 8];
        unsafe { std::ptr::copy_nonoverlapping(s.at as *const u32, w.as_mut_ptr(), n) };
        read += 1;
        let next = addr.checked_add(4 * n as u32);
        let here = std::mem::replace(&mut addr, next.unwrap_or(u32::MAX));
        if w == [0; 8] {
            empty.push(k);
        } else {
            let label = format!("p{param} table {table:#x} slot {k}");
            let place = format!("descriptor at {here:#x} (heap slot {}, PC heap index {}): {}", s.k, s.index, hex(w[..n].iter().copied()));
            // (a texture's owner goes by the view its words name: a descriptor copied as raw
            // bytes has no owner noted for its own slot)
            let owner = |image: bool| match if image { super::console_heap::image_resource_at(here, w[0]) } else { super::console_heap::resource_at(s.at) } {
                Some(r) if r != 0 => format!("PC resource {r:#x}"),
                _ => "no PC resource noted behind it".into(),
            };
            if s.sampler {
                out.push(format!("hang capture: {label}: {place}: a sampler"));
            } else if w[3] >> 30 != 0 {
                let what = crate::gpu::desc::image(&w).map_or("a texture whose words do not decode".into(), |v| {
                    format!(
                        "texture {:?} {}x{}x{}, GFX10 format {}, mips {}..{} of {}, console address {:#x}, SRV index {}, companion UAV index {}",
                        v.ty,
                        v.width,
                        v.height,
                        v.depth,
                        v.format,
                        v.base_level,
                        v.last_level,
                        v.max_mip + 1,
                        v.address,
                        w[0],
                        w[0].saturating_add(1)
                    )
                });
                out.push(format!("hang capture: {label}: {place}: {what}; {}", owner(true)));
            } else {
                let b = crate::gpu::desc::buffer(w[..4].try_into().unwrap());
                let bytes = b.num_records as u64 * b.stride.max(1) as u64;
                if b.address == 0 || bytes == 0 {
                    out.push(format!("hang capture: {label}: {place}: a null buffer (address {:#x}, {} records of {} B)", b.address, b.num_records, b.stride));
                } else {
                    let shown = (if bytes <= CONSTANTS_MOST { CONSTANT_BYTES.max(buffer_bytes) } else { buffer_bytes }).min(bytes as usize);
                    out.push(format!(
                        "hang capture: {label}: {place}: buffer at {:#x}, {} records of {} B ({bytes} B), GFX10 format {}; {}; its first {shown} bytes in the CPU's copy now:",
                        b.address,
                        b.num_records,
                        b.stride,
                        b.format,
                        owner(false)
                    ));
                    memory(out, &label, b.address, shown);
                }
            }
        }
        if next.is_none() {
            break;
        }
    }
    out.push(format!(
        "hang capture: p{param}: table at {table:#x}: {read} descriptors read ({}), {} of them empty",
        if read == most { format!("the first {most}: XCL_HANG_SLOTS for more") } else { "to the end of its heap".into() },
        empty.len()
    ));
    if !empty.is_empty() {
        out.push(format!("hang capture: p{param}: empty slots {}: a shader that reads one as an image takes PC views 0 (SRV) and 1 (UAV), which are {}", runs(&empty), views_of_zeros(super::real::mem_views())));
    }
}

/// What a T# of zeros selects in the shared PC heap – its descriptors 0 and 1 –, given where the
/// memory windows' raw views start (`real::mem_views`).
fn views_of_zeros(mem_views: Option<u32>) -> String {
    match mem_views {
        Some(first) if first <= 1 => format!("THE MEMORY WINDOWS' RAW 4 GiB BUFFER VIEWS (they start at descriptor {first}): a texture read of one can hang the GPU"),
        _ if super::console_heap::NULL_IMAGE_VIEWS >= 2 => "null image views".into(),
        _ => "whatever was reserved first".into(),
    }
}

/// Slot numbers in ascending order as runs: `0-3, 7, 9-12`.
fn runs(slots: &[u32]) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < slots.len() {
        let mut j = i;
        while j + 1 < slots.len() && slots[j + 1] == slots[j] + 1 {
            j += 1;
        }
        out.push(if j == i { slots[i].to_string() } else { format!("{}-{}", slots[i], slots[j]) });
        i = j + 1;
    }
    out.join(", ")
}

/// `bytes` at console address `va` as the CPU has them now, 16 words a line (`rows`); what
/// cannot be read is said, never faulted on.
fn memory(out: &mut Vec<String>, what: &str, va: u64, bytes: usize) {
    use windows_sys::Win32::System::Diagnostics::Debug::ReadProcessMemory;
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let mut buf = vec![0u32; bytes.div_ceil(4)];
    let mut got = 0usize;
    // (page by page: a buffer may end before the bytes asked for)
    while got < bytes {
        let n = (4096 - ((va as usize + got) & 4095)).min(bytes - got);
        let mut done = 0usize;
        let ok = unsafe { ReadProcessMemory(GetCurrentProcess(), (va as usize + got) as *const _, (buf.as_mut_ptr() as *mut u8).add(got).cast(), n, &mut done) };
        got += done.min(n);
        if ok == 0 || done < n {
            break;
        }
    }
    if got < 4 {
        out.push(format!("hang capture: {what}: {va:#x} is not readable by the CPU"));
        return;
    }
    for line in rows(&buf[..got / 4]) {
        out.push(format!("hang capture: {what} {line}"));
    }
    if got < bytes {
        out.push(format!("hang capture: {what}: not readable by the CPU from +{got:#x}"));
    }
}

/// `words` 16 a line with their byte offset; a run of lines of zeros is one line.
fn rows(words: &[u32]) -> Vec<String> {
    let mut out = Vec::new();
    let mut zeros: Option<usize> = None;
    let flush = |out: &mut Vec<String>, zeros: &mut Option<usize>, end: usize| {
        if let Some(start) = zeros.take() {
            out.push(format!("+{start:#05x}..+{end:#05x}: all 0"));
        }
    };
    for (i, row) in words.chunks(16).enumerate() {
        if row.iter().all(|&w| w == 0) {
            zeros.get_or_insert(64 * i);
            continue;
        }
        flush(&mut out, &mut zeros, 64 * i);
        out.push(format!("+{:#05x}: {}", 64 * i, hex(row.iter().copied())));
    }
    flush(&mut out, &mut zeros, 4 * words.len());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_slots_are_listed_as_runs_with_what_a_zero_descriptor_selects() {
        assert_eq!(runs(&[0, 1, 2, 3, 7, 9, 10, 255]), "0-3, 7, 9-10, 255");
        assert_eq!(runs(&[5]), "5");
        assert!(views_of_zeros(Some(0)).contains("RAW 4 GiB BUFFER VIEWS"));
        assert_eq!(views_of_zeros(Some(super::super::console_heap::NULL_IMAGE_VIEWS)), "null image views");
        assert_eq!(views_of_zeros(None), "null image views");
    }

    #[test]
    fn rows_keep_their_offsets_and_fold_runs_of_zeros() {
        let mut w = vec![0u32; 80];
        w[0] = 0x900;
        w[70] = 7;
        assert_eq!(
            rows(&w),
            vec!["+0x000: 900 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0".to_string(), "+0x040..+0x100: all 0".into(), "+0x100: 0 0 0 0 0 0 7 0 0 0 0 0 0 0 0 0".into()]
        );
        assert_eq!(rows(&[0; 20]), vec!["+0x000..+0x050: all 0".to_string()]);
    }

    /// Constants are shown in full and by register; an argument nothing was set for is said so.
    #[test]
    fn root_arguments_are_shown_by_register_and_parameter() {
        let layout = Layout { params: vec![Param::Constants { at: 0, count: 2 }, Param::Descriptor { at: 8 }, Param::Constants { at: 16, count: 1 }], size: 20, inline: 8, inline_gfx: 16, ..Default::default() };
        let mut args = vec![0u8; 20];
        args[..4].copy_from_slice(&0x11u32.to_le_bytes());
        args[4..8].copy_from_slice(&0x22u32.to_le_bytes());
        args[16..].copy_from_slice(&0x33u32.to_le_bytes());
        let d = arguments(&layout, &args, false);
        assert!(d[0].contains("s4=11 s5=22 +0x8=0 +0xc=0 +0x10=33"), "{}", d[0]);
        assert_eq!(&d[1..], ["hang capture: p0: 2 constants: 11 22", "hang capture: p1: root descriptor, not set", "hang capture: p2: 1 constants: 33"]);
        let g = arguments(&layout, &args, true);
        assert!(g[0].contains("s8=11 s9=22 s10=0 s11=0 +0x10=33"), "{}", g[0]);
    }

    /// The memory behind an argument is read without faulting: what is there, then where it ends.
    #[test]
    fn memory_is_read_to_where_it_stops_being_readable() {
        let data: Vec<u32> = (1..=32).collect();
        let mut out = Vec::new();
        memory(&mut out, "p0", data.as_ptr() as u64, 128);
        assert_eq!(out.len(), 2, "{out:?}");
        assert!(out[0].ends_with("p0 +0x000: 1 2 3 4 5 6 7 8 9 a b c d e f 10"), "{}", out[0]);
        let mut none = Vec::new();
        memory(&mut none, "p1", 0x10, 64);
        assert_eq!(none, vec!["hang capture: p1: 0x10 is not readable by the CPU".to_string()]);
    }
}
