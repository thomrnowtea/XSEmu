use super::*;
use std::mem::ManuallyDrop;
use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;

/// A skipped producer's ranges are unwritten until it runs again; each skip replaces its ranges.
#[test]
fn unwritten_ranges_follow_their_producer() {
    let mut u = Unwritten::default();
    assert!(u.is_empty() && u.overlapping(0x1000, 16).is_none());
    u.mark(0xA, &[(0x1000, 0x100), (0x8000, 0x40)]);
    u.mark(0xB, &[(0x2000, 0x10)]);
    assert_eq!(u.overlapping(0x10f0, 0x20), Some((0x1000, 0x1100, 0xA)));
    assert_eq!(u.overlapping(0x0ff0, 0x10), None, "a range ending where the unwritten one starts");
    assert_eq!(u.overlapping(0x1100, 4), None, "just past it");
    assert_eq!(u.overlapping(0x203c, 0), None);
    assert_eq!(u.overlapping(0x2008, 0), Some((0x2000, 0x2010, 0xB)), "an empty read still names its first byte");
    u.mark(0xA, &[(0x9000, 0x10)]);
    assert_eq!(u.overlapping(0x1000, 16), None, "a later skip replaces the producer's ranges");
    assert!(u.overlapping(0x9008, 4).is_some());
    u.written_by(0xA);
    assert_eq!(u.overlapping(0x9008, 4), None);
    assert!(u.overlapping(0x2000, 4).is_some());
    u.written_by(0xB);
    assert!(u.is_empty());
}

/// A dispatch that runs with a UAV over part of an unwritten range writes that part, whoever
/// skipped it; a view larger than MAX_TRACKED neither marks nor clears.
#[test]
fn a_built_dispatch_writes_what_its_uavs_cover() {
    let mut u = Unwritten::default();
    u.mark(0xA, &[(0x1000, 0x1000), (0, MAX_TRACKED + 1)]);
    assert_eq!(u.overlapping(0x1_0000, 4), None, "the view of all memory is not kept");
    u.written(&[(0x1400, 0x100)]);
    assert_eq!(u.overlapping(0x1400, 0x100), None);
    assert_eq!(u.overlapping(0x1000, 0x800), Some((0x1000, 0x1400, 0xA)));
    assert_eq!(u.overlapping(0x1500, 4), Some((0x1500, 0x2000, 0xA)));
    u.written(&[(0, MAX_TRACKED + 1)]);
    assert!(u.overlapping(0x1000, 4).is_some(), "a write through the view of all memory cannot be placed");
    u.written(&[(0x0800, 0x2000)]);
    assert!(u.is_empty());
}

/// An indirect call whose arguments or count lie in an unwritten range is refused, by its
/// signature's stride and most commands; it runs again once the producer has run.
#[test]
fn indirect_calls_reading_a_skipped_producers_output_are_refused() {
    let (sig, producer) = (0x7E57_1D00usize, 0x7E57_1D01usize);
    let base = 0x7E57_0000_0000u64;
    note_signature(sig, DISPATCH, 12, true);
    skipped_producer(producer, &[(base + 0x100, 0x40)]);
    assert!(check(sig, 4, Some(base + 0x40), None).is_ok(), "4 records of 12 B end at +0x70");
    assert!(check(sig, 16, Some(base + 0x40), None).is_ok(), "16 records end at +0x100, where the range starts");
    assert!(check(sig, 17, Some(base + 0x40), None).unwrap_err().contains(&format!("{producer:#x}")), "17 records reach into it");
    assert!(check(sig, 1, Some(base), Some(base + 0x13c)).is_err(), "the count in the range");
    assert!(check(sig, 1, None, None).is_ok());
    produced(producer);
    assert!(check(sig, 17, Some(base + 0x40), Some(base + 0x13c)).is_ok());
}

fn buffer(bytes: u64) -> D3D12_RESOURCE_DESC {
    D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: bytes,
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        ..Default::default()
    }
}

unsafe fn committed(dev: &ID3D12Device, bytes: u64, ty: D3D12_HEAP_TYPE, flags: D3D12_RESOURCE_FLAGS, state: D3D12_RESOURCE_STATES) -> ID3D12Resource {
    let props = D3D12_HEAP_PROPERTIES { Type: ty, ..Default::default() };
    let desc = D3D12_RESOURCE_DESC { Flags: flags, ..buffer(bytes) };
    let mut r = None;
    super::super::title_textures::own_work(|| dev.CreateCommittedResource(&props, D3D12_HEAP_FLAG_NONE, &desc, state, None, &mut r)).expect("test buffer");
    r.unwrap()
}

unsafe fn barrier(list: &ID3D12GraphicsCommandList, r: &ID3D12Resource, from: D3D12_RESOURCE_STATES, to: D3D12_RESOURCE_STATES) {
    list.ResourceBarrier(&[D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            Transition: ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                pResource: std::mem::transmute_copy(r),
                Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                StateBefore: from,
                StateAfter: to,
            }),
        },
        ..Default::default()
    }]);
}

/// The sanitiser's output for `records` (words) of `kind`, `stride` bytes each, with a GPU count
/// when given: the records it wrote, and the count of active ones after them.
fn sanitised(kind: u32, stride: u32, records: &[u32], count: Option<u32>) -> (Vec<u32>, u32) {
    let (out, n, _) = sanitised_keeping(kind, stride, records, count, 0);
    (out, n)
}

/// `sanitised`, also keeping the first `raw_bytes` of the title's words after the count.
fn sanitised_keeping(kind: u32, stride: u32, records: &[u32], count: Option<u32>, raw_bytes: u32) -> (Vec<u32>, u32, Vec<u32>) {
    sanitised_two_pass(kind, stride, records, count, raw_bytes, None)
}

/// `sanitised_keeping` for a mesh call run in two passes (`record`'s `two_pass`): the words kept
/// after the count are then the mesh pass's records.
fn sanitised_two_pass(kind: u32, stride: u32, records: &[u32], count: Option<u32>, raw_bytes: u32, two_pass: Option<(u32, u32)>) -> (Vec<u32>, u32, Vec<u32>) {
    assert_eq!(super::super::backend(), super::super::Backend::Real);
    unsafe {
        let raw = super::super::real::device_raw();
        let dev = ID3D12Device::from_raw_borrowed(&raw).expect("D3D12 device");
        let n = (records.len() * 4) as u32 / stride;
        let bytes = 4096u64;
        let upload = committed(dev, bytes, D3D12_HEAP_TYPE_UPLOAD, D3D12_RESOURCE_FLAG_NONE, D3D12_RESOURCE_STATE_GENERIC_READ);
        let readback = committed(dev, bytes, D3D12_HEAP_TYPE_READBACK, D3D12_RESOURCE_FLAG_NONE, D3D12_RESOURCE_STATE_COPY_DEST);
        let src = committed(dev, bytes, D3D12_HEAP_TYPE_DEFAULT, D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS, D3D12_RESOURCE_STATE_COPY_DEST);
        let dst = committed(dev, bytes, D3D12_HEAP_TYPE_DEFAULT, D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS, D3D12_RESOURCE_STATE_COMMON);
        let mut p = std::ptr::null_mut();
        upload.Map(0, None, Some(&mut p)).expect("map upload");
        let words = p as *mut u32;
        for (k, w) in records.iter().enumerate() {
            *words.add(k) = *w;
        }
        // the count at byte 2048
        *words.add(512) = count.unwrap_or(0);
        upload.Unmap(0, None);
        let queue: ID3D12CommandQueue = dev.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC { Type: D3D12_COMMAND_LIST_TYPE_DIRECT, ..Default::default() }).expect("queue");
        let alloc: ID3D12CommandAllocator = dev.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT).expect("allocator");
        let list: ID3D12GraphicsCommandList = dev.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &alloc, None).expect("list");
        list.CopyBufferRegion(&src, 0, &upload, 0, bytes);
        barrier(&list, &src, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_UNORDERED_ACCESS);
        let (s, d) = (src.GetGPUVirtualAddress(), dst.GetGPUVirtualAddress());
        // stale words where the pass writes: what it leaves must not depend on them
        let stale = committed(dev, bytes, D3D12_HEAP_TYPE_UPLOAD, D3D12_RESOURCE_FLAG_NONE, D3D12_RESOURCE_STATE_GENERIC_READ);
        let mut q = std::ptr::null_mut();
        stale.Map(0, None, Some(&mut q)).expect("map stale");
        std::ptr::write_bytes(q as *mut u8, 0xA5, bytes as usize);
        stale.Unmap(0, None);
        barrier(&list, &dst, D3D12_RESOURCE_STATE_COMMON, D3D12_RESOURCE_STATE_COPY_DEST);
        list.CopyBufferRegion(&dst, 0, &stale, 0, bytes);
        barrier(&list, &dst, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_UNORDERED_ACCESS);
        assert!(record(&list, kind, stride, n, s, count.map(|_| s + 2048), d, raw_bytes, two_pass), "the sanitiser was made");
        barrier(&list, &dst, D3D12_RESOURCE_STATE_UNORDERED_ACCESS, D3D12_RESOURCE_STATE_COPY_SOURCE);
        list.CopyBufferRegion(&readback, 0, &dst, 0, bytes);
        list.Close().expect("close");
        queue.ExecuteCommandLists(&[Some(list.cast::<ID3D12CommandList>().unwrap())]);
        let fence: ID3D12Fence = dev.CreateFence(0, D3D12_FENCE_FLAG_NONE).expect("fence");
        queue.Signal(&fence, 1).expect("signal");
        let t0 = std::time::Instant::now();
        while fence.GetCompletedValue() < 1 {
            assert!(t0.elapsed() < std::time::Duration::from_secs(10), "the sanitiser's pass did not finish");
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        readback.Map(0, None, Some(&mut p)).expect("map readback");
        let out = std::slice::from_raw_parts(p as *const u32, (bytes / 4) as usize);
        let at = (n * stride / 4) as usize;
        let kept = if two_pass.is_some() { raw_bytes as usize / 4 + records.len() } else { raw_bytes as usize / 4 };
        let got = (out[..records.len()].to_vec(), out[at], out[at + 1..at + 1 + kept].to_vec());
        readback.Unmap(0, None);
        got
    }
}

/// Valid dispatches pass unchanged; an axis over 65,535 is capped; garbage (all ones) and a
/// record over the call's group budget run empty, and the budget is not charged for them.
#[test]
fn the_sanitiser_empties_oversized_and_garbage_dispatches() {
    let records = [2, 3, 4, 70000, 1, 1, u32::MAX, u32::MAX, u32::MAX, 65535, 65535, 1, 1, 1, 1];
    let (out, n) = sanitised(DISPATCH, 12, &records, None);
    assert_eq!(n, 5);
    assert_eq!(out, [2, 3, 4, 65535, 1, 1, 0, 0, 0, 0, 0, 0, 1, 1, 1]);
}

/// A capture keeps the title's words as they were after the count, the sanitised records unchanged.
#[test]
fn the_sanitiser_keeps_the_raw_words_after_the_count_for_a_capture() {
    let records = [2, 3, 4, u32::MAX, u32::MAX, u32::MAX];
    let (out, n, raw) = sanitised_keeping(DISPATCH, 12, &records, None, 24);
    assert_eq!((out, n), (vec![2, 3, 4, 0, 0, 0], 2));
    assert_eq!(raw, records);
}

/// A GPU count below the most commands bounds the records copied and the count the call reads.
#[test]
fn the_sanitiser_copies_only_the_records_the_gpu_count_names() {
    let records = [1, 2, 3, 4, 5, 6, 7, 8, 9];
    let (out, n) = sanitised(DISPATCH, 12, &records, Some(2));
    assert_eq!(n, 2);
    assert_eq!(out[..6], [1, 2, 3, 4, 5, 6]);
    let (_, n) = sanitised(DISPATCH, 12, &records, Some(u32::MAX));
    assert_eq!(n, 3, "a count over the most commands is the most commands");
}

/// Indexed draws: a valid one passes whole (its other fields too); a garbage one draws nothing
/// and keeps its other fields; mesh dispatches over D3D12's 2^22 groups run empty.
#[test]
fn the_sanitiser_bounds_draws_and_mesh_dispatches() {
    let draws = [36, 2, 7, 5, 9, u32::MAX, u32::MAX, 1, 2, 3];
    let (out, n) = sanitised(DRAW_INDEXED, 20, &draws, None);
    assert_eq!(n, 2);
    assert_eq!(out, [36, 2, 7, 5, 9, 0, 0, 1, 2, 3]);
    let mesh = [64, 1, 1, 65535, 128, 1];
    let (out, n) = sanitised(DISPATCH_MESH, 12, &mesh, None);
    assert_eq!(n, 2);
    assert_eq!(out, [64, 1, 1, 0, 0, 0]);
}

/// Records past the GPU's count are zero whatever the scratch held, so a caller may run the
/// records one by one.
#[test]
fn the_sanitiser_empties_the_records_past_the_gpu_count() {
    let records = [1, 2, 3, 4, 5, 6, 7, 8, 9];
    let (out, n) = sanitised(DISPATCH_MESH, 12, &records, Some(1));
    assert_eq!(n, 1);
    assert_eq!(out, [1, 2, 3, 0, 0, 0, 0, 0, 0]);
}

/// A mesh call in two passes: each record's groups on x, cut to the command's record memory;
/// the mesh pass's copy after the count has x times the pass-through's split; empty and
/// inactive records are empty in both.
#[test]
fn the_sanitiser_plans_a_mesh_call_in_two_passes() {
    let records = [5, 1, 1, 40, 2, 1, 0, 1, 1, 9, 1, 1];
    let (out, n, mesh) = sanitised_two_pass(DISPATCH_MESH, 12, &records, Some(3), 0, Some((64, 4)));
    assert_eq!(n, 3);
    assert_eq!(out, [5, 1, 1, 64, 1, 1, 0, 0, 0, 0, 0, 0], "40 x 2 groups cut to 64; the fourth record is past the count");
    assert_eq!(mesh, [20, 1, 1, 256, 1, 1, 0, 0, 0, 0, 0, 0]);
    // other calls are untouched by the plan
    let (out, n, kept) = sanitised_two_pass(DISPATCH, 12, &[40, 2, 1], None, 0, Some((64, 4)));
    assert_eq!((out, n), (vec![40, 2, 1], 1));
    assert!(kept.is_empty() || kept == [0xA5A5_A5A5; 3], "no second copy for a compute call");
}

/// play-98's pattern: a pass binds, as its own UAV, a view that starts inside a shared pool and
/// runs to the pool's end. It is part of the pool: it is not kept as unwritten, and the pool's
/// views that were kept before it was pooled go too, while a buffer outside the pool is kept.
#[test]
fn a_view_inside_a_shared_pool_is_part_of_it() {
    const POOL: u64 = 0x4_20b1_0000;
    const POOL_END: u64 = 0x4_2b91_0000;
    const OUT: u64 = 0x7_94c5_4fb0;
    let mut u = Unwritten::default();
    u.mark(0xA0, &[(0x4_2356_aac4, POOL_END - 0x4_2356_aac4)]);
    assert!(u.overlapping(0x4_2356_b000, 16).is_some(), "kept while the pool is not known");
    for k in 0..SHARED_BINDERS {
        u.bound(0xB0 + k, &[(POOL, POOL_END - POOL)]);
    }
    assert!(u.overlapping(0x4_2356_b000, 16).is_none(), "a view inside the pool goes when it becomes one");
    u.mark(0xA1, &[(0x4_2356_ac00, 0x1654), (OUT, 0x14)]);
    assert!(u.overlapping(0x4_2356_ac10, 16).is_none(), "a later skip does not mark a view inside the pool");
    assert_eq!(u.overlapping(OUT, 0x14), Some((OUT, OUT + 0x14, 0xA1)), "its own buffer outside the pool is kept");
    u.written(&[(0x4_2356_ac00, 0x1654), (0x4_2000_0000, 0x10_0000_0000)]);
    assert!(u.overlapping(OUT, 0x14).is_some(), "a write through a pool or a view too large cannot be placed");
}

/// Refused calls are counted by the range they read and the producer it waits for, most first.
#[test]
fn refusals_are_counted_by_range_and_producer() {
    const A: u64 = 0x7_7d95_a4b0;
    const B: u64 = 0x7_5fcd_71d0;
    let (pa, pb, sig) = (0x7E57_9801usize, 0x7E57_9802usize, 0x7E57_9800usize);
    note_signature(sig, DISPATCH, 12, true);
    skipped_producer(pa, &[(A, 0x400)]);
    skipped_producer(pb, &[(B, 0x780)]);
    for at in [A + 0x280, A + 0x380, B + 0x10, A] {
        assert!(check(sig, 1, Some(at), None).is_err());
    }
    let mine: Vec<_> = take_refusals().into_iter().filter(|r| r.0 .2 == pa || r.0 .2 == pb).collect();
    assert_eq!(mine, [((A, A + 0x400, pa), 3), ((B, B + 0x780, pb), 1)]);
    produced(pa);
    produced(pb);
}

