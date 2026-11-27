//! Alias diagnostics for a crash record: what else shows the memory an address shows, and
//! whether that memory still holds what XCL last put there.
//!
//! A title's record can be right when it is written and wrong when it is read with no write to
//! its address in between: the address shows a physical page that is mapped elsewhere too (its
//! pool slot, a GPU view, XCL's own view of the allocation), and a write through any of those
//! lands in the same memory; or the address was unmapped and shows another page now. The write
//! ledger (`eh::note_guest_write`) is kept by address, so neither shows there. For each address
//! of a crash record (`report`) this names XCL's account of the memory – the physical page,
//! where its data is, every other address showing it, the GPU's views, the sparse window tile,
//! the resources placed over it, the latest maps and unmaps – with XCL's writes through each of
//! those addresses, and checks the payloads XCL wrote there against what the memory holds now.
//!
//! Kept for that while the title runs: a 64-bit hash of every payload piece XCL writes for a
//! DirectStorage request (`payload_written`: one piece per 64 KiB page, with the physical page
//! the address showed then), and the physical pages' maps and unmaps (`MapHistory`, filled by
//! `kernelx::physical`). `XCL_DS_TRIPWIRE=<seconds>` (off by default) logs, as it happens and
//! with the calls that led there, every unmap, remap, decommit or page copy over a page such a
//! payload was written to within that many seconds (`mapping_changed`).

use std::cell::Cell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Mutex, MutexGuard, OnceLock};

/// a console page: the unit physical pages, GPU page views and sparse tiles are mapped in
pub const PAGE: usize = 0x1_0000;
const SMALL: usize = 0x1000;

/// A 64-bit hash of `bytes` (not cryptographic: four multiply-rotate lanes, the length mixed in).
pub fn hash(bytes: &[u8]) -> u64 {
    const K: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut h = [0x243F_6A88_85A3_08D3u64, 0x1319_8A2E_0370_7344, 0xA409_3822_299F_31D0, 0x082E_FA98_EC4E_6C89];
    let mut mix = |block: &[u8]| {
        for (lane, word) in h.iter_mut().zip(block.chunks_exact(8)) {
            *lane = (*lane ^ u64::from_le_bytes(word.try_into().unwrap())).wrapping_mul(K).rotate_left(29);
        }
    };
    let mut blocks = bytes.chunks_exact(32);
    for block in &mut blocks {
        mix(block);
    }
    let mut tail = [0u8; 32];
    tail[..blocks.remainder().len()].copy_from_slice(blocks.remainder());
    mix(&tail);
    let x = h[0] ^ h[1].rotate_left(16) ^ h[2].rotate_left(32) ^ h[3].rotate_left(48) ^ (bytes.len() as u64).wrapping_mul(K);
    (x ^ (x >> 31)).wrapping_mul(0xBF58_476D_1CE4_E5B9)
}

/// The physical page an address showed (page 0: a slot of a reservation that showed none).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Backing {
    pub page: usize,
    pub kind: &'static str,
}

/// The DirectStorage request a payload was written for (all zero: not for one).
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Request {
    pub options: u64,
    pub offset: u64,
    pub source_size: u32,
    pub list: PageList,
}

/// What a request's destination page list was at the request's turn, against what it was at
/// EnqueueRequest (`hle::dstorage::page_list_at_turn`).
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub enum PageList {
    /// no page list, or no request
    #[default]
    None,
    /// the same at both (an entry that was 0 at EnqueueRequest: as it was at Submit)
    Same,
    /// on the enqueuing thread's stack: the list as enqueued, not read again
    OnStack,
    /// `entries` of it differed (255: or more) and it was taken as at the turn; `stale` of those
    /// named a page at enqueue, the others were 0 then
    Changed { entries: u8, stale: u8 },
    /// no allocated pages there at the turn: the list as enqueued
    NotPages,
}

/// One piece of a payload XCL wrote into the title's memory (within one 64 KiB page).
#[derive(Clone, Copy, Debug)]
pub struct Payload {
    pub t: f32,
    pub at: usize,
    pub len: u32,
    pub hash: u64,
    pub backing: Option<Backing>,
    pub request: Request,
}

impl Payload {
    fn overlaps(&self, lo: usize, hi: usize) -> bool {
        self.at < hi && lo < self.at + self.len as usize
    }
}

/// The latest payload pieces, oldest first.
pub struct Payloads {
    ring: VecDeque<Payload>,
    most: usize,
}

impl Payloads {
    pub const fn new(most: usize) -> Self {
        Payloads { ring: VecDeque::new(), most }
    }

    pub fn push(&mut self, p: Payload) {
        if self.ring.len() == self.most {
            self.ring.pop_front();
        }
        self.ring.push_back(p);
    }

    pub fn len(&self) -> usize {
        self.ring.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ring.is_empty()
    }

    /// The bytes its ring holds.
    pub fn bytes(&self) -> usize {
        self.ring.capacity() * std::mem::size_of::<Payload>()
    }

    /// The pieces over `[lo, hi)`, the latest first (up to `most`), each with whether a later
    /// piece was written over part of it (then the memory is not expected to hold it any more).
    pub fn over(&self, lo: usize, hi: usize, most: usize) -> Vec<(Payload, bool)> {
        let mut later: Vec<(usize, usize)> = Vec::new();
        let mut out = Vec::new();
        // (a piece lies within one 64 KiB page: the later pieces over it are in the pages of the range)
        let (near_lo, near_hi) = (lo & !(PAGE - 1), hi.saturating_add(PAGE - 1) & !(PAGE - 1));
        for p in self.ring.iter().rev().filter(|p| p.overlaps(near_lo, near_hi)) {
            if p.overlaps(lo, hi) {
                out.push((*p, later.iter().any(|&(a, b)| p.overlaps(a, b))));
                if out.len() == most {
                    break;
                }
            }
            later.push((p.at, p.at + p.len as usize));
        }
        out
    }
}

impl Payloads {
    /// The pieces written to physical page `page` over `[lo, hi)` of the page (offsets in it),
    /// the latest first (up to `most`): through whatever address showed the page then.
    pub fn to_page(&self, page: usize, lo: usize, hi: usize, most: usize) -> Vec<Payload> {
        let within = |p: &Payload| (p.at & (PAGE - 1)) < hi && lo < (p.at & (PAGE - 1)) + p.len as usize;
        self.ring.iter().rev().filter(|p| p.backing.is_some_and(|b| b.page == page) && within(p)).take(most).copied().collect()
    }
}

/// `[at, at + len)` cut at 64 KiB page boundaries: (address, bytes) of each piece.
pub fn pieces(at: usize, len: usize) -> impl Iterator<Item = (usize, usize)> {
    let end = at.saturating_add(len);
    let mut next = at;
    std::iter::from_fn(move || {
        if next >= end {
            return None;
        }
        let stop = ((next & !(PAGE - 1)).saturating_add(PAGE)).min(end);
        let piece = (next, stop - next);
        next = stop;
        Some(piece)
    })
}

/// One map, unmap, commit or free in the model of physical pages (`kernelx::physical`).
#[derive(Clone, Copy, Debug)]
pub struct MapEvent {
    pub t: f32,
    pub thread: u32,
    /// the address range it changed (`len` 0: none – a page freed)
    pub at: usize,
    pub len: usize,
    /// the physical page (0: none)
    pub page: usize,
    pub what: &'static str,
    /// the call it was made for
    pub site: &'static str,
}

impl std::fmt::Display for MapEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:.3} s {}", self.t, self.what)?;
        if self.page != 0 {
            write!(f, " page {:#x}", self.page)?;
        }
        if self.len != 0 {
            write!(f, " at {:#x}", self.at)?;
            if self.len != PAGE {
                write!(f, " + {:#x}", self.len)?;
            }
        }
        write!(f, " ({}, thread {})", self.site, self.thread)
    }
}

/// The latest map events, oldest first.
pub struct MapHistory {
    ring: VecDeque<MapEvent>,
    most: usize,
}

impl Default for MapHistory {
    fn default() -> Self {
        MapHistory::new(1 << 18)
    }
}

impl MapHistory {
    pub const fn new(most: usize) -> Self {
        MapHistory { ring: VecDeque::new(), most }
    }

    pub fn push(&mut self, e: MapEvent) {
        if self.ring.len() == self.most {
            self.ring.pop_front();
        }
        self.ring.push_back(e);
    }

    pub fn len(&self) -> usize {
        self.ring.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ring.is_empty()
    }

    /// The bytes its ring holds.
    pub fn bytes(&self) -> usize {
        self.ring.capacity() * std::mem::size_of::<MapEvent>()
    }

    /// The events that changed the 64 KiB page at `at`, the latest first (up to `most`).
    pub fn at(&self, at: usize, most: usize) -> Vec<MapEvent> {
        self.ring.iter().rev().filter(|e| e.len != 0 && e.at < at + PAGE && at < e.at + e.len).take(most).copied().collect()
    }

    /// The events of physical page `page` elsewhere than at `not_at`, the latest first.
    pub fn of_page(&self, page: usize, not_at: usize, most: usize) -> Vec<MapEvent> {
        self.ring.iter().rev().filter(|e| e.page == page && page != 0 && (e.len == 0 || e.at != not_at)).take(most).copied().collect()
    }
}

/// The pages payloads were written to lately (kept only under `XCL_DS_TRIPWIRE`): 64 KiB page
/// → (when, the piece's address and bytes).
#[derive(Default)]
pub struct Recent {
    pages: HashMap<usize, (f32, usize, u32)>,
}

impl Recent {
    pub fn note(&mut self, t: f32, at: usize, len: u32, window: f32) {
        if self.pages.len() >= 1 << 16 {
            self.pages.retain(|_, v| t - v.0 <= window);
        }
        self.pages.insert(at & !(PAGE - 1), (t, at, len));
    }

    /// The pieces written within `window` seconds before `now` into pages of `[lo, hi)`: (seconds
    /// ago, address, bytes), the first 4.
    pub fn hits(&self, lo: usize, hi: usize, now: f32, window: f32) -> Vec<(f32, usize, u32)> {
        let fresh = |v: &(f32, usize, u32)| (now - v.0 <= window).then_some((now - v.0, v.1, v.2));
        let first = lo & !(PAGE - 1);
        if hi.saturating_sub(first) / PAGE <= 64 {
            return (first..hi).step_by(PAGE).filter_map(|p| self.pages.get(&p).and_then(fresh)).take(4).collect();
        }
        self.pages.iter().filter(|(&p, _)| p >= first && p < hi).filter_map(|(_, v)| fresh(v)).take(4).collect()
    }
}

static PAYLOADS: Mutex<Payloads> = Mutex::new(Payloads::new(1 << 18));
static RECENT: Mutex<Option<Recent>> = Mutex::new(None);

/// (diagnostics, `memdiag`) The payload pieces kept: (pieces, bytes of one, bytes the ring holds).
pub(crate) fn diag_payloads() -> (usize, usize, usize) {
    let p = lock(&PAYLOADS);
    (p.len(), std::mem::size_of::<Payload>(), p.bytes())
}

thread_local! {
    /// the DirectStorage request the calling thread's payloads are written for (`with_request`)
    static REQUEST: Cell<Request> = const { Cell::new(Request { options: 0, offset: 0, source_size: 0, list: PageList::None }) };
}

/// `f`, with the payloads it writes kept as written for `request`.
pub fn with_request<R>(request: Request, f: impl FnOnce() -> R) -> R {
    let was = REQUEST.with(|r| r.replace(request));
    let r = f();
    REQUEST.with(|r| r.set(was));
    r
}

/// `XCL_DS_TRIPWIRE=<seconds>`: the tripwire's window (0: off).
fn tripwire() -> f32 {
    static WINDOW: OnceLock<f32> = OnceLock::new();
    *WINDOW.get_or_init(|| {
        let window = std::env::var("XCL_DS_TRIPWIRE").ok().and_then(|v| v.trim().parse::<f32>().ok()).filter(|w| *w > 0.0).unwrap_or(0.0);
        if window > 0.0 {
            crate::xlog_info!("XCL_DS_TRIPWIRE={window}: every unmap, remap, decommit or page copy over a page a DirectStorage payload was written to in the last {window} s is logged");
        }
        window
    })
}

/// Whether the tripwire is on (to skip working out what to tell it).
pub fn tripwire_on() -> bool {
    tripwire() > 0.0
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// (at a crash: a lock another thread holds is not waited for. In tests it is: another test
/// holds it for a moment, and the record must not depend on which tests run beside this one.)
pub(crate) fn try_lock<T>(m: &Mutex<T>) -> Option<MutexGuard<'_, T>> {
    #[cfg(test)]
    return Some(lock(m));
    #[cfg(not(test))]
    match m.try_lock() {
        Ok(g) => Some(g),
        Err(std::sync::TryLockError::Poisoned(e)) => Some(e.into_inner()),
        Err(std::sync::TryLockError::WouldBlock) => None,
    }
}

/// XCL wrote `bytes` of a payload at `at`, memory within one 64 KiB page that showed `backing`.
pub fn payload_written_as(at: usize, bytes: &[u8], hash: u64, backing: Option<Backing>) {
    if bytes.is_empty() {
        return;
    }
    let t = crate::log::uptime() as f32;
    let request = REQUEST.with(|r| r.get());
    lock(&PAYLOADS).push(Payload { t, at, len: bytes.len() as u32, hash, backing, request });
    let window = tripwire();
    if window > 0.0 {
        lock(&RECENT).get_or_insert_with(Recent::default).note(t, at, bytes.len() as u32, window);
    }
}

/// XCL wrote the payload `bytes` at the title's address `at` (a DirectStorage request's
/// destination): kept per 64 KiB page with its hash and the physical page the address shows.
pub fn payload_written(at: usize, bytes: &[u8]) {
    for (piece, len) in pieces(at, bytes.len()) {
        let part = &bytes[piece - at..piece - at + len];
        payload_written_as(piece, part, hash(part), crate::hle::kernelx::backing(piece));
    }
}

/// The mapping of `[at, at + len)` changes or XCL copies a page over it (`what`, for the call
/// `site`): under `XCL_DS_TRIPWIRE` logged when a payload was written there within the window.
pub fn mapping_changed(what: &'static str, site: &'static str, at: usize, len: usize) {
    let window = tripwire();
    if window <= 0.0 || len == 0 {
        return;
    }
    let now = crate::log::uptime() as f32;
    let hits = match lock(&RECENT).as_ref() {
        Some(r) => r.hits(at, at.saturating_add(len), now, window),
        None => return,
    };
    if hits.is_empty() {
        return;
    }
    static LINES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    if LINES.fetch_add(1, std::sync::atomic::Ordering::Relaxed) >= 512 {
        return;
    }
    let written: Vec<String> = hits.iter().map(|&(ago, a, n)| format!("{n:#x} bytes at {a:#x} {ago:.3} s ago")).collect();
    crate::xlog_warn!(
        "DS tripwire: {what} ({site}) over {at:#x} + {len:#x} (thread {}), where payloads were written: {}; on the stack: {}",
        unsafe { windows_sys::Win32::System::Threading::GetCurrentThreadId() },
        written.join(", "),
        crate::eh::callers_here(12)
    );
}

fn show_request(r: &Request) -> String {
    if *r == Request::default() {
        return "no DirectStorage request".into();
    }
    let list = match r.list {
        PageList::None => String::new(),
        PageList::Same => "; its page list at its turn as it was at EnqueueRequest (entries that were 0 then: as at Submit)".into(),
        PageList::OnStack => "; its page list on the enqueuing thread's stack, used as enqueued".into(),
        PageList::Changed { entries, stale } => format!("; {entries} entries of its page list CHANGED between EnqueueRequest and its turn ({stale} of them named a page at enqueue), used as at its turn"),
        PageList::NotPages => "; its page list held no allocated pages at its turn, used as enqueued".into(),
    };
    format!("request with options {:#x}: {} bytes at {:#x} of its source{list}", r.options, r.source_size, r.offset)
}

fn show_backing(b: Option<Backing>) -> String {
    match b {
        Some(b) if b.page != 0 => format!("physical page {:#x} ({})", b.page, b.kind),
        Some(b) => b.kind.to_string(),
        None => "no physical page".into(),
    }
}

/// The payload pieces written over the 4 KiB page of `a`, each checked against the memory now.
unsafe fn payloads_at(a: usize, now: Option<Backing>) -> Vec<String> {
    let lo = a & !(SMALL - 1);
    let Some(hits) = try_lock(&PAYLOADS).map(|g| g.over(lo, lo + SMALL, 4)) else { return vec!["(the payload ledger is locked by another thread)".into()] };
    hits.iter()
        .map(|(p, overwritten)| {
            let len = p.len as usize;
            let holds = if crate::gpu::shader::readable(p.at, len) {
                if hash(std::slice::from_raw_parts(p.at as *const u8, len)) == p.hash {
                    "the memory there STILL HOLDS it".to_string()
                } else if *overwritten {
                    "the memory there no longer holds it (a later payload was written over part of it)".to_string()
                } else {
                    "the memory there NO LONGER HOLDS it, and no later payload was written to that address".to_string()
                }
            } else {
                "the memory there is not readable now".to_string()
            };
            let same = p.backing.map(|b| b.page) == now.map(|b| b.page);
            let mut backing = if same { format!("showing {} then and now", show_backing(p.backing)) } else { format!("showing {} then, {} NOW", show_backing(p.backing), show_backing(now)) };
            // the address shows another page now: whether the page it showed then still has the payload
            if let Some(then) = p.backing.filter(|b| !same && b.page != 0) {
                backing += &match crate::hle::kernelx::page_data(then.page).map(|data| data + (p.at & (PAGE - 1))) {
                    Some(a) if crate::gpu::shader::readable(a, len) && hash(std::slice::from_raw_parts(a as *const u8, len)) == p.hash => format!("; physical page {:#x} itself STILL HOLDS it (at {a:#x})", then.page),
                    Some(a) => format!("; physical page {:#x} itself no longer holds it either (at {a:#x})", then.page),
                    None => format!("; physical page {:#x} is gone or was never used", then.page),
                };
            }
            format!("{:.3} s: {len:#x} bytes at {:#x}, hash {:016x} ({}); {holds}; the address {backing}", p.t, p.at, p.hash, show_request(&p.request))
        })
        .collect()
}

/// For an address of a crash record: XCL's account of the memory there and of every other address
/// showing it (see the module). None where XCL has none – plain memory nothing was written to –
/// and for a 4 KiB page already looked at (the first 128 pages XCL has an account of are
/// reported).
pub unsafe fn report(p: usize) -> Option<String> {
    report_inner(p, false)
}

pub(crate) fn crash_ownership(p: usize) -> Option<String> {
    unsafe { report_inner(p, true) }
}

unsafe fn report_inner(p: usize, compact: bool) -> Option<String> {
    if p < 0x1_0000 || p >> 47 != 0 {
        return None;
    }
    static SEEN: Mutex<Option<HashSet<usize>>> = Mutex::new(None);
    static REPORTED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    if !compact {
        let mut g = try_lock(&SEEN)?;
        let seen = g.get_or_insert_with(HashSet::new);
        if seen.len() >= 4096 || REPORTED.load(std::sync::atomic::Ordering::Relaxed) >= 128 || !seen.insert(p & !(SMALL - 1)) {
            return None;
        }
    }
    let (page, off) = (p & !(PAGE - 1), p & (PAGE - 1));
    let mut lines: Vec<String> = Vec::new();
    // (64 KiB address, what it is): the other addresses showing this memory
    let mut aliases: Vec<(usize, String)> = Vec::new();
    let physical = if compact {
        crate::hle::kernelx::crash_alias_facts(p)
    } else {
        crate::hle::kernelx::alias_facts(p)
    };
    let backing = physical.as_ref().and_then(|f| f.backing);
    if let Some(f) = physical {
        lines.push(f.what);
        aliases.extend(f.aliases);
        if !f.history_here.is_empty() {
            lines.push(format!("maps and unmaps of this address, the latest first: {}", f.history_here.join("; ")));
        }
        if !f.history_page.is_empty() {
            lines.push(format!("maps and unmaps of the page elsewhere, the latest first: {}", f.history_page.join("; ")));
        }
    }
    let views = crate::hle::d3d12x::page_mappings::alias_facts(p, backing.map(|b| b.page).filter(|&p| p != 0));
    lines.extend(views.here);
    lines.extend(crate::hle::d3d12x::sparse::alias_facts(p));
    if !compact || crate::gpu::uma::made().is_some() {
        lines.extend(crate::hle::d3d12x::real::console_buffers_at(p));
        lines.extend(crate::gpu::uma::history_at(p));
    }
    if compact {
        let lo = p & !(SMALL - 1);
        let offset = lo & (PAGE - 1);
        let recent = try_lock(&PAYLOADS).and_then(|ledger| {
            backing.filter(|b| b.page != 0)
                .and_then(|b| ledger.to_page(b.page, offset, offset + SMALL, 1).into_iter().next())
                .or_else(|| ledger.over(lo, lo + SMALL, 1).into_iter().next().map(|(payload, _)| payload))
        });
        if let Some(w) = recent {
            lines.push(format!(
                "Recorded DirectStorage write: {:.3} s, {:#x} bytes at {:#x}, through {} ({})",
                w.t, w.len, w.at, show_backing(w.backing), show_request(&w.request)
            ));
        }
        return (!lines.is_empty()).then(|| lines.join("\n"));
    }
    for (a, what) in views.aliases {
        match aliases.iter_mut().find(|x| x.0 == a) {
            Some(x) => x.1 = format!("{}, {what}", x.1),
            None => aliases.push((a, what.to_string())),
        }
    }
    for (a, what) in crate::hle::kernelx::mirrors::counterparts(page) {
        if !aliases.iter().any(|x| x.0 == a) {
            // (not the same memory unless the same page is mapped there: what XCL's models say of it)
            let there = crate::hle::kernelx::alias_facts(a).map(|f| f.what).unwrap_or_else(|| "no physical page".into());
            lines.push(format!("{what} {a:#x}: {there}"));
        }
    }
    for a in std::iter::once(page).chain(aliases.iter().map(|x| x.0)) {
        lines.extend(crate::hle::d3d12x::title_textures::placed_over(a, PAGE));
        lines.extend(crate::hle::d3d12x::placed_buffers_over(a, PAGE));
    }
    let own = payloads_at(p, backing);
    if lines.is_empty() && own.is_empty() {
        return None;
    }
    if !aliases.is_empty() {
        let shown: Vec<String> = aliases.iter().take(24).map(|(a, what)| format!("{a:#x} ({what})")).collect();
        lines.push(format!("the same memory at {} other address(es): {}{}", aliases.len(), shown.join(", "), if aliases.len() > 24 { " …" } else { "" }));
    }
    for line in own {
        lines.push(format!("payload written here: {line}"));
    }
    for (a, what) in aliases.iter().take(24) {
        let there = a + off;
        if let Some(tile) = crate::hle::d3d12x::sparse::alias_facts(there) {
            lines.push(format!("{there:#x} ({what}) is in a sparse window: {tile}"));
        }
        if let Some(w) = crate::eh::writes_over(there) {
            lines.push(format!("XCL's own writes through {there:#x} ({what}): {w}"));
        }
        // (the address is another one: the page it shows now is its own matter)
        for line in payloads_at(there, crate::hle::kernelx::alias_facts(there).and_then(|f| f.backing)) {
            lines.push(format!("payload written through {there:#x} ({what}): {line}"));
        }
    }
    // what was written to the page through addresses that show it no longer (where it was mapped
    // to be filled, a GPU view since moved): found by the page, not by an address
    if let Some(b) = backing.filter(|b| b.page != 0) {
        let lo = off & !(SMALL - 1);
        let hits = try_lock(&PAYLOADS).map_or(Vec::new(), |g| g.to_page(b.page, lo, lo + SMALL, 8));
        let data = crate::hle::kernelx::page_data(b.page);
        for h in hits.iter().filter(|h| h.at & !(PAGE - 1) != page && !aliases.iter().any(|x| x.0 == h.at & !(PAGE - 1))) {
            let len = h.len as usize;
            let holds = match data.map(|d| d + (h.at & (PAGE - 1))) {
                Some(a) if crate::gpu::shader::readable(a, len) && hash(std::slice::from_raw_parts(a as *const u8, len)) == h.hash => "the page STILL HOLDS it",
                Some(_) => "the page no longer holds it",
                None => "the page's data is not to be had now",
            };
            lines.push(format!(
                "payload written to physical page {:#x} through {:#x}, an address that shows the page no longer: {:.3} s: {len:#x} bytes at offset {:#x} of the page, hash {:016x} ({}); {holds}",
                b.page,
                h.at,
                h.t,
                h.at & (PAGE - 1),
                h.hash,
                show_request(&h.request)
            ));
        }
    }
    REPORTED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    Some(format!("      memory behind {p:#x}:\n        {}", lines.join("\n        ")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hash_tells_payloads_apart_by_any_byte_and_by_length() {
        let a: Vec<u8> = (0..0xC00u32).map(|i| (i * 7 + i / 256) as u8).collect();
        assert_eq!(hash(&a), hash(&a.clone()));
        for at in [0, 1, 31, 32, 33, 0x7FF, 0xBFF] {
            let mut b = a.clone();
            b[at] ^= 0x10;
            assert_ne!(hash(&a), hash(&b), "a changed byte at {at:#x}");
        }
        assert_ne!(hash(&a), hash(&a[..0xBFF]), "a shorter payload");
        assert_ne!(hash(&[]), hash(&[0]), "zeros of another length");
        assert_ne!(hash(&[0; 32]), hash(&[0; 64]));
    }

    #[test]
    fn a_payload_is_kept_per_64k_page() {
        assert_eq!(pieces(0x1_F000, 0xC00).collect::<Vec<_>>(), [(0x1_F000, 0xC00)]);
        assert_eq!(pieces(0x1_F800, 0x1_1000).collect::<Vec<_>>(), [(0x1_F800, 0x800), (0x2_0000, 0x1_0000), (0x3_0000, 0x800)]);
        assert_eq!(pieces(0x2_0000, 0).count(), 0);
        assert_eq!(pieces(usize::MAX - 0xFFF, 0x1000).map(|p| p.1).sum::<usize>(), 0xFFF, "no wrap at the end of the address space");
    }

    fn piece(t: f32, at: usize, len: u32) -> Payload {
        Payload { t, at, len, hash: t as u64, backing: None, request: Request::default() }
    }

    #[test]
    fn the_pieces_over_a_page_are_the_latest_first_and_know_what_was_written_over_them() {
        let mut l = Payloads::new(4);
        l.push(piece(1.0, 0x5_E000, 0xC00));
        l.push(piece(2.0, 0x5_F000, 0xC00));
        l.push(piece(3.0, 0x5_E800, 0x1000)); // over the end of the first and the start of the second
        let over = l.over(0x5_E000, 0x5_F000, 4);
        assert_eq!(over.iter().map(|x| (x.0.t, x.1)).collect::<Vec<_>>(), [(3.0, false), (1.0, true)]);
        let over = l.over(0x5_F000, 0x6_0000, 4);
        assert_eq!(over.iter().map(|x| (x.0.t, x.1)).collect::<Vec<_>>(), [(3.0, false), (2.0, true)]);
        assert!(l.over(0x6_0000, 0x6_1000, 4).is_empty());
        assert_eq!(l.over(0x5_0000, 0x6_0000, 1).len(), 1, "no more than asked for");
        // the ring keeps the latest
        l.push(piece(4.0, 0x9_0000, 0x10));
        l.push(piece(5.0, 0x9_0000, 0x10));
        assert_eq!(l.len(), 4);
        assert_eq!(l.over(0x5_E000, 0x5_F000, 4).iter().map(|x| x.0.t).collect::<Vec<_>>(), [3.0], "the oldest piece is gone");
    }

    fn event(t: f32, at: usize, len: usize, page: usize, what: &'static str) -> MapEvent {
        MapEvent { t, thread: 7, at, len, page, what, site: "a test" }
    }

    #[test]
    fn map_history_answers_by_address_and_by_page() {
        let mut h = MapHistory::new(8);
        h.push(event(1.0, 0x10_0000, PAGE, 0x20, "map view of"));
        h.push(event(2.0, 0x30_0000, PAGE, 0x20, "map copy of"));
        h.push(event(3.0, 0x10_0000, PAGE, 0x20, "unmap view of"));
        h.push(event(4.0, 0x10_0000, PAGE, 0x30, "map view of"));
        h.push(event(5.0, 0, 0, 0x20, "freed"));
        h.push(event(6.0, 0x0C_0000, 0x10 * PAGE, 0, "release"));
        let here: Vec<f32> = h.at(0x10_0000, 8).iter().map(|e| e.t).collect();
        assert_eq!(here, [6.0, 4.0, 3.0, 1.0], "a range event covers its pages");
        assert_eq!(h.at(0x10_0000, 2).len(), 2);
        let elsewhere: Vec<f32> = h.of_page(0x20, 0x10_0000, 8).iter().map(|e| e.t).collect();
        assert_eq!(elsewhere, [5.0, 2.0]);
        assert!(h.of_page(0, 0, 8).is_empty(), "no page is not a page");
        assert_eq!(event(3.0, 0x10_0000, PAGE, 0x20, "unmap view of").to_string(), "3.000 s unmap view of page 0x20 at 0x100000 (a test, thread 7)");
        assert_eq!(event(6.0, 0xC_0000, 0x10 * PAGE, 0, "release").to_string(), "6.000 s release at 0xc0000 + 0x100000 (a test, thread 7)");
        for i in 0..8 {
            h.push(event(10.0 + i as f32, 0x50_0000, PAGE, 0x40, "map view of"));
        }
        assert_eq!(h.len(), 8);
        assert!(h.at(0x10_0000, 8).is_empty(), "the ring keeps the latest");
    }

    #[test]
    fn the_tripwire_finds_pages_written_within_its_window() {
        let mut r = Recent::default();
        r.note(10.0, 0x5_F000, 0xC00, 30.0);
        r.note(12.0, 0x9_0100, 0x40, 30.0);
        assert_eq!(r.hits(0x5_0000, 0x6_0000, 16.5, 30.0), [(6.5, 0x5_F000, 0xC00)]);
        assert!(r.hits(0x5_0000, 0x6_0000, 41.0, 30.0).is_empty(), "written longer ago than the window");
        assert!(r.hits(0x6_0000, 0x9_0000, 16.5, 30.0).is_empty(), "other pages");
        // a wide range (a whole reservation released) is answered from the pages kept
        let wide = r.hits(0, 0x1_0000_0000, 13.0, 30.0);
        assert_eq!(wide.len(), 2);
        // a later piece on the page replaces the earlier
        r.note(20.0, 0x5_E000, 0x10, 30.0);
        assert_eq!(r.hits(0x5_F000, 0x5_F001, 21.0, 30.0), [(1.0, 0x5_E000, 0x10)]);
    }

    #[test]
    fn a_written_payload_is_checked_against_the_memory() {
        let mut buf = vec![0u8; 3 * PAGE];
        let base = (buf.as_ptr() as usize + PAGE - 1) & !(PAGE - 1);
        let at = base + PAGE - 0x800;
        let payload: Vec<u8> = (0..0x1000u32).map(|i| (i * 13) as u8).collect();
        unsafe {
            std::ptr::copy_nonoverlapping(payload.as_ptr(), at as *mut u8, payload.len());
            with_request(Request { options: 1, offset: 0x1234, source_size: 0x99, list: PageList::None }, || payload_written(at, &payload));
            let first = payloads_at(at, None);
            assert_eq!(first.len(), 1, "{first:?}");
            assert!(first[0].contains("0x800 bytes at") && first[0].contains("STILL HOLDS") && first[0].contains("options 0x1: 153 bytes at 0x1234") && first[0].contains("then and now"), "{first:?}");
            let snapshot = crash_ownership(at).unwrap();
            assert!(snapshot.contains("Recorded DirectStorage write:") && snapshot.contains("options 0x1: 153 bytes at 0x1234"), "{snapshot}");
            let second = payloads_at(base + PAGE + 0x10, None);
            assert!(second.len() == 1 && second[0].contains("STILL HOLDS"), "the piece on the next 64 KiB page: {second:?}");
            // something else changes the second piece: no payload was written over it
            *((base + PAGE + 0x7FF) as *mut u8) ^= 1;
            let second = payloads_at(base + PAGE + 0x10, Some(Backing { page: 0x40, kind: "view" }));
            assert!(second[0].contains("NO LONGER HOLDS it, and no later payload") && second[0].contains("no physical page then, physical page 0x40 (view) NOW"), "{second:?}");
            assert!(payloads_at(at, None)[0].contains("STILL HOLDS"), "the first piece is as it was");
            // a later payload over part of the first piece
            payload_written(at + 0x100, &[0xEE; 0x20]);
            std::ptr::write_bytes((at + 0x100) as *mut u8, 0xEE, 0x20);
            let first = payloads_at(at, None);
            assert!(first.len() == 2 && first[0].contains("0x20 bytes at") && first[0].contains("STILL HOLDS") && first[1].contains("a later payload was written over part of it"), "{first:?}");
            assert!(payloads_at(base + 2 * PAGE + 0x2000, None).is_empty());
            let r = report(at).expect("an address a payload was written to");
            assert!(r.contains("payload written here") && report(at + 8).is_none(), "a page is reported once: {r}");
        }
        buf[0] = 1;
    }
}
