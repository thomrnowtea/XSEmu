//! One pool of workers for every DirectStorage queue (`xcrdapi.dll`).
//!
//! The console has one decompression engine and one storage stack for all of a title's queues,
//! and a queue's `DSTORAGE_PRIORITY` (low -1, normal 0, high 1, realtime 2) says how it shares
//! them. So the queues share a fixed set of worker threads, sized from the logical cores with room
//! left for the title's own threads, the renderer and the shader and pipeline compiles.
//!
//! Each queue keeps its own order. A queue's entries take their turn one at a time, in the order
//! they were enqueued: a request is written into the title's memory at its turn (its page lists
//! read then, `page_list_at_turn`), a status entry gets the first failure since the previous one,
//! a fence is signalled and an event set only once everything before them in that queue is done,
//! and `CancelRequestsWithTag` is checked at the turn. What may run early is the slow part of a
//! request from a file: reading the file and decompressing it (zlib, BCPack). That read-ahead
//! runs on any free worker, so one queue's requests decode in parallel, and the result waits in
//! the queue for the request's turn. Read-ahead holds at most the staging buffer size of payloads
//! over all queues (`SetStagingBufferSize`, 32 to 256 MiB); the next request to take its turn
//! never waits for that room. Requests from memory run whole at their turn, since an earlier
//! request of the queue may write their source.
//!
//! Which queue a free worker serves is a byte-weighted fair share between the four priorities
//! (`Share`): realtime 1000, high 100, normal 10, low 1, counted in the bytes each piece of work
//! moves. A priority with work always gets its share, so a burst of realtime or high streaming
//! slows a low queue down but never stops it. Queues of one priority take turns.
//!
//! Telemetry stays bounded: per priority, the entries queued now and at most, and the count, total
//! and longest of the time an entry waited for a worker and of the time decoding took ("decode":
//! reading the source and unpacking it). Every 10 s with activity one log line gives those for the
//! interval, and how that time splits into stages over all priorities (`Stage`: the source read,
//! zlib, BCPack, and the write into the title's memory), so a slow interval shows whether it waited
//! on the disk or on the CPU; `stats` gives them since the start for the run report.

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::{Entry, FileInner, QueueInner, Request, E_QUEUE_CLOSED, S_OK};
use crate::util::HRESULT;

/// `DSTORAGE_PRIORITY`, lowest first.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Priority {
    Low,
    Normal,
    High,
    Realtime,
}

impl Priority {
    /// A queue desc's priority, already checked to be -1 … 2.
    pub(super) fn of(p: i8) -> Priority {
        match p {
            -1 => Priority::Low,
            0 => Priority::Normal,
            1 => Priority::High,
            _ => Priority::Realtime,
        }
    }

    pub(super) fn name(self) -> &'static str {
        NAMES[self as usize]
    }
}

const NAMES: [&str; 4] = ["low", "normal", "high", "realtime"];
/// Share of the workers by bytes, per priority (lowest first).
const WEIGHTS: [u64; 4] = [1, 10, 100, 1000];
/// The least a piece of work counts for: a status, a fence, or a request already read ahead.
const MIN_COST: u64 = 4096;
/// Read-ahead room when the title sets no larger staging buffer (the PC DirectStorage default).
const MIN_STAGING: u64 = 32 << 20;
const MAX_STAGING: u64 = 256 << 20;
/// A wait for a worker this long is logged.
const LONG_WAIT: Duration = Duration::from_secs(2);
const LOG_EVERY: Duration = Duration::from_secs(10);

/// The stages of a request's work, timed apart for the interval line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Stage {
    /// the source: a file read, or a copy from the title's memory
    Read,
    /// zlib, counted in output bytes
    Inflate,
    /// BCPack, counted in destination bytes
    Bcpack,
    /// the payload into the title's memory (virtual or physical pages)
    Write,
}

const STAGE_NAMES: [&str; 4] = ["read", "zlib", "BCPack", "write"];

/// One stage's totals since the start.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct StageTotals {
    count: u64,
    us: u64,
    bytes: u64,
}

struct StageClock {
    count: AtomicU64,
    us: AtomicU64,
    bytes: AtomicU64,
    /// the longest since the interval line last took it
    max_us: AtomicU64,
}

static STAGES: [StageClock; 4] = [const { StageClock { count: AtomicU64::new(0), us: AtomicU64::new(0), bytes: AtomicU64::new(0), max_us: AtomicU64::new(0) } }; 4];

/// `f`, timed as `stage`, counted as moving `bytes` of its result.
pub(super) fn timed<R>(stage: Stage, f: impl FnOnce() -> R, bytes: impl FnOnce(&R) -> usize) -> R {
    let t = Instant::now();
    let r = f();
    let us = t.elapsed().as_micros().min(u128::from(u64::MAX)) as u64;
    let bytes = bytes(&r);
    let c = &STAGES[stage as usize];
    c.count.fetch_add(1, Ordering::Relaxed);
    c.us.fetch_add(us, Ordering::Relaxed);
    c.bytes.fetch_add(bytes as u64, Ordering::Relaxed);
    c.max_us.fetch_max(us, Ordering::Relaxed);
    r
}

fn stage_totals() -> [StageTotals; 4] {
    std::array::from_fn(|k| StageTotals { count: STAGES[k].count.load(Ordering::Relaxed), us: STAGES[k].us.load(Ordering::Relaxed), bytes: STAGES[k].bytes.load(Ordering::Relaxed) })
}

/// The stages of an interval (totals `now` less `before`, and each one's longest): count, MB,
/// average and longest time, and MB/s over the time spent in it. `None` when none ran.
fn stages_line(before: &[StageTotals; 4], now: &[StageTotals; 4], max_us: &[u64; 4]) -> Option<String> {
    let parts: Vec<String> = (0..4)
        .filter_map(|k| {
            let n = now[k].count.saturating_sub(before[k].count);
            let us = now[k].us.saturating_sub(before[k].us);
            let bytes = now[k].bytes.saturating_sub(before[k].bytes);
            (n > 0).then(|| {
                format!(
                    "{} {n} ({:.1} MB, avg {:.2} ms / max {:.1} ms, {:.0} MB/s)",
                    STAGE_NAMES[k],
                    bytes as f64 / f64::from(1 << 20),
                    us as f64 / n as f64 / 1000.0,
                    max_us[k] as f64 / 1000.0,
                    bytes as f64 / f64::from(1 << 20) / (us.max(1) as f64 / 1e6)
                )
            })
        })
        .collect();
    (!parts.is_empty()).then(|| parts.join(", "))
}

/// `IDStorageFactoryX::SetStagingBufferSize`, as the title last set it.
static STAGING: AtomicU64 = AtomicU64::new(MIN_STAGING);
static NEXT_LANE: AtomicU64 = AtomicU64::new(1);
static QUEUES_MADE: AtomicU32 = AtomicU32::new(0);
static POOL: OnceLock<&'static Pool> = OnceLock::new();

pub(super) fn set_staging(bytes: u32) {
    STAGING.store(u64::from(bytes), Ordering::Relaxed);
}

fn read_ahead_room() -> u64 {
    STAGING.load(Ordering::Relaxed).clamp(MIN_STAGING, MAX_STAGING)
}

/// `XCL_DSTORAGE_WORKERS` (1-64), else a quarter of the logical cores, 2 to 8: 6 on 24.
fn worker_count() -> (usize, usize) {
    let cores = std::thread::available_parallelism().map_or(4, |n| n.get());
    let set = std::env::var("XCL_DSTORAGE_WORKERS").ok().and_then(|v| v.trim().parse::<usize>().ok()).filter(|n| (1..=64).contains(n));
    (set.unwrap_or((cores / 4).clamp(2, 8)), cores)
}

/// Byte-weighted fair share between the priorities (stride scheduling). Each priority has a
/// pass that grows by the bytes it is served over its weight; the priority with work and the
/// lowest pass goes next, the higher one on a tie. A priority that had no work comes back at the
/// pass of the last one served, so idle time is no credit for a burst later.
#[derive(Default, Debug)]
struct Share {
    pass: [u64; 4],
    had_work: [bool; 4],
    clock: u64,
}

impl Share {
    fn pick(&mut self, ready: [bool; 4]) -> Option<usize> {
        for l in 0..4 {
            if ready[l] && !self.had_work[l] {
                self.pass[l] = self.pass[l].max(self.clock);
            }
            self.had_work[l] = ready[l];
        }
        (0..4).rev().filter(|&l| ready[l]).min_by_key(|&l| self.pass[l])
    }

    fn charge(&mut self, level: usize, bytes: u64) {
        self.clock = self.pass[level];
        self.pass[level] = self.pass[level].saturating_add(bytes.max(MIN_COST) * WEIGHTS[3] / WEIGHTS[level]);
    }
}

/// The work a request moves.
fn bytes(r: &Request) -> u64 {
    u64::from(r.destination_size.max(r.source_size))
}

fn reads_ahead(e: &Entry) -> bool {
    matches!(e, Entry::Request { req, .. } if !req.memory_source())
}

enum Ahead {
    No,
    /// cancelled when its read-ahead came: left to its turn, which skips it
    Skip,
    Running,
    Done(Result<Vec<u8>, HRESULT>),
}

struct Slot {
    entry: Entry,
    submitted: Instant,
    started: bool,
    ahead: Ahead,
    /// read-ahead bytes this slot holds of the shared room
    held: u64,
}

/// A queue's submitted entries in the pool.
struct Lane {
    id: u64,
    queue: Arc<QueueInner>,
    level: usize,
    slots: VecDeque<Slot>,
    /// ticket of `slots[0]`: a slot's ticket is `front` + its index
    front: u64,
    /// a worker runs the turn of the entry before `slots[0]`
    turn: bool,
    /// read-aheads running
    reading: usize,
    /// the first failure since the last status entry
    since_status: HRESULT,
    #[cfg(test)]
    hold: bool,
}

enum Choice {
    Turn,
    Ahead(usize),
}

impl Lane {
    fn choice(&self, room: u64, most_ahead: usize) -> Option<Choice> {
        #[cfg(test)]
        let held = self.hold;
        #[cfg(not(test))]
        let held = false;
        if !self.turn && !held && self.slots.front().is_some_and(|s| !matches!(s.ahead, Ahead::Running)) {
            return Some(Choice::Turn);
        }
        if self.reading >= most_ahead {
            return None;
        }
        let (k, s) = self.slots.iter().enumerate().find(|(_, s)| matches!(s.ahead, Ahead::No) && reads_ahead(&s.entry))?;
        let Entry::Request { req, .. } = &s.entry else { return None };
        (bytes(req) <= room).then_some(Choice::Ahead(k))
    }
}

enum Job {
    Turn { lane: u64, queue: Arc<QueueInner>, entry: Entry, read: Option<Result<Vec<u8>, HRESULT>>, since: HRESULT, level: usize, held: u64 },
    Ahead { lane: u64, ticket: u64, queue: Arc<QueueInner>, req: Request, seq: u64, file: Option<Arc<FileInner>>, level: usize },
}

#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct Times {
    pub count: u64,
    pub total_us: u64,
    pub max_us: u64,
}

impl Times {
    fn note(&mut self, d: Duration) {
        let us = d.as_micros().min(u64::MAX as u128) as u64;
        self.count += 1;
        self.total_us = self.total_us.saturating_add(us);
        self.max_us = self.max_us.max(us);
    }

    fn line(&self) -> String {
        let ms = |us: u64| us as f64 / 1000.0;
        format!("avg {:.1} ms / max {:.1} ms", ms(self.total_us / self.count.max(1)), ms(self.max_us))
    }
}

/// One priority's numbers.
#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct LevelStats {
    /// entries that took their turn
    pub done: u64,
    /// destination bytes of the requests among them
    pub bytes: u64,
    /// the most entries submitted and not yet through their turn at once
    pub depth_max: u64,
    /// from Submit to the first worker on the entry (its read-ahead or its turn)
    pub wait: Times,
    /// reading and decompressing a request's payload
    pub decode: Times,
    /// waits of 2 s or more
    pub long_waits: u64,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct PoolStats {
    pub workers: u64,
    pub most_busy: u64,
    /// by priority name
    pub priorities: BTreeMap<String, LevelStats>,
}

struct State {
    lanes: Vec<Lane>,
    /// per priority, the lane served last (queues of a priority take turns)
    last: [u64; 4],
    share: Share,
    held: u64,
    depth: [u64; 4],
    busy: usize,
    total: [LevelStats; 4],
    interval: [LevelStats; 4],
    most_busy: usize,
    interval_busy: usize,
    logged: Instant,
    /// the stage totals when the interval line was last written
    stages_at: [StageTotals; 4],
}

struct Pool {
    state: Mutex<State>,
    wake: Condvar,
    workers: usize,
}

fn pool() -> &'static Pool {
    POOL.get_or_init(|| {
        let (workers, cores) = worker_count();
        let p: &'static Pool = Box::leak(Box::new(Pool {
            state: Mutex::new(State {
                lanes: Vec::new(),
                last: [0; 4],
                share: Share::default(),
                held: 0,
                depth: [0; 4],
                busy: 0,
                total: Default::default(),
                interval: Default::default(),
                most_busy: 0,
                interval_busy: 0,
                logged: Instant::now(),
                stages_at: stage_totals(),
            }),
            wake: Condvar::new(),
            workers,
        }));
        let started = (0..workers).filter(|i| std::thread::Builder::new().name(format!("xcl-dstorage-{i}")).spawn(move || work(p)).is_ok()).count();
        crate::xlog_info!(
            "DirectStorage: one worker pool for every queue: {started} of {workers} workers started ({cores} logical cores; XCL_DSTORAGE_WORKERS sets the count), shared by priority in bytes realtime {} : high {} : normal {} : low {}, file reads and decompression read ahead into at most the staging buffer size ({} MB now)",
            WEIGHTS[3],
            WEIGHTS[2],
            WEIGHTS[1],
            WEIGHTS[0],
            read_ahead_room() >> 20
        );
        if started == 0 {
            crate::xlog_error!("DirectStorage: no worker thread could start: submitted requests will not run");
        }
        p
    })
}

fn lock(p: &Pool) -> MutexGuard<'_, State> {
    p.state.lock().unwrap_or_else(|e| e.into_inner())
}

pub(super) fn new_lane_id() -> u64 {
    NEXT_LANE.fetch_add(1, Ordering::Relaxed)
}

/// A new queue joins the pool.
pub(super) fn register(queue: &Arc<QueueInner>) {
    let p = pool();
    let n = QUEUES_MADE.fetch_add(1, Ordering::Relaxed) + 1;
    if n <= 32 || n.is_power_of_two() {
        crate::xlog_info!(
            "DirectStorage: queue {} (#{n}): {} source, priority {}, capacity {}{}",
            queue.lane,
            if queue.source_memory { "memory" } else { "file" },
            queue.priority.name(),
            queue.capacity,
            if queue.auto_submit { "" } else { ", submits only when told" }
        );
    }
    lock(p).lanes.push(Lane {
        id: queue.lane,
        queue: Arc::clone(queue),
        level: queue.priority as usize,
        slots: VecDeque::new(),
        front: 0,
        turn: false,
        reading: 0,
        since_status: S_OK,
        #[cfg(test)]
        hold: false,
    });
}

/// Submitted entries, in order, to the back of their queue's lane. Called with the queue's
/// pending lock held, so batches arrive in order and none after `close`; a lane already gone
/// hands the batch back for the caller to drop outside its locks.
pub(super) fn submit(lane: u64, batch: Vec<Entry>) -> Vec<Entry> {
    let p = pool();
    let mut s = lock(p);
    let Some(l) = s.lanes.iter_mut().find(|l| l.id == lane) else { return batch };
    let now = Instant::now();
    let level = l.level;
    l.slots.extend(batch.into_iter().map(|entry| Slot { entry, submitted: now, started: false, ahead: Ahead::No, held: 0 }));
    let depth: u64 = s.lanes.iter().filter(|l| l.level == level).map(|l| l.slots.len() as u64).sum();
    s.depth[level] = depth;
    s.noted(level, |st| st.depth_max = st.depth_max.max(depth));
    drop(s);
    p.wake.notify_all();
    Vec::new()
}

/// A queue leaves the pool (Close, or its last Release): what it had submitted and not yet
/// started is dropped (status entries complete with E_QUEUE_CLOSED), and this returns once a
/// turn of it running on a worker is done. Called after `closed` is set under the queue's pending
/// lock, so nothing more is submitted.
pub(super) fn close(lane: u64) {
    let p = pool();
    let mut s = lock(p);
    let Some(i) = s.lanes.iter().position(|l| l.id == lane) else { return };
    let drained: Vec<Slot> = s.lanes[i].slots.drain(..).collect();
    let level = s.lanes[i].level;
    s.held -= drained.iter().map(|d| d.held).sum::<u64>();
    s.depth[level] = s.depth[level].saturating_sub(drained.len() as u64);
    while s.lanes.iter().any(|l| l.id == lane && l.turn) {
        s = p.wake.wait(s).unwrap_or_else(|e| e.into_inner());
    }
    let gone = s.lanes.iter().position(|l| l.id == lane).map(|i| s.lanes.remove(i));
    drop(s);
    p.wake.notify_all();
    for d in drained {
        if let Entry::Status(st, i) = &d.entry {
            if let Some(slot) = st.slots.get(*i as usize) {
                slot.store(E_QUEUE_CLOSED, Ordering::Release);
            }
        }
    }
    drop(gone);
}

impl State {
    /// The next piece of work: the priority by `Share`, then the next of its queues with work.
    fn take(&mut self, workers: usize) -> Option<Job> {
        let room = read_ahead_room().saturating_sub(self.held);
        let mut ready = [false; 4];
        for l in &self.lanes {
            ready[l.level] |= l.choice(room, workers).is_some();
        }
        let level = self.share.pick(ready)?;
        let n = self.lanes.len();
        let start = self.lanes.iter().position(|l| l.id == self.last[level]).map_or(0, |i| i + 1);
        let (i, choice) = (0..n).map(|k| (start + k) % n).filter(|&i| self.lanes[i].level == level).find_map(|i| self.lanes[i].choice(room, workers).map(|c| (i, c)))?;
        self.last[level] = self.lanes[i].id;
        let now = Instant::now();
        let lane = &mut self.lanes[i];
        let (job, cost, waited) = match choice {
            Choice::Turn => {
                let slot = lane.slots.pop_front()?;
                lane.front += 1;
                lane.turn = true;
                let read = match slot.ahead {
                    Ahead::Done(r) => Some(r),
                    _ => None,
                };
                let cost = match &slot.entry {
                    Entry::Request { req, .. } if read.is_none() => bytes(req),
                    _ => MIN_COST,
                };
                let waited = (!slot.started).then(|| now - slot.submitted);
                let job = Job::Turn { lane: lane.id, queue: Arc::clone(&lane.queue), entry: slot.entry, read, since: lane.since_status, level, held: slot.held };
                self.depth[level] = self.depth[level].saturating_sub(1);
                (job, cost, waited)
            }
            Choice::Ahead(k) => {
                let ticket = lane.front + k as u64;
                let slot = &mut lane.slots[k];
                let Entry::Request { req, seq, file, .. } = &slot.entry else { return None };
                let cost = bytes(req);
                let job = Job::Ahead { lane: lane.id, ticket, queue: Arc::clone(&lane.queue), req: *req, seq: *seq, file: file.clone(), level };
                slot.ahead = Ahead::Running;
                slot.held = cost;
                let waited = (!slot.started).then(|| now - slot.submitted);
                slot.started = true;
                lane.reading += 1;
                self.held += cost;
                (job, cost, waited)
            }
        };
        self.share.charge(level, cost);
        if let Some(w) = waited {
            let long = w >= LONG_WAIT;
            self.noted(level, |st| {
                st.wait.note(w);
                st.long_waits += u64::from(long);
            });
            let k = self.total[level].long_waits;
            if long && (k <= 8 || k.is_power_of_two()) {
                let queued: Vec<String> = (0..4).rev().filter(|&l| self.depth[l] > 0).map(|l| format!("{} {}", NAMES[l], self.depth[l])).collect();
                crate::xlog_warn!(
                    "DirectStorage pool: a {} entry waited {} ms for a worker (#{k} such wait at {}); queued now: {}",
                    NAMES[level],
                    w.as_millis(),
                    NAMES[level],
                    if queued.is_empty() { "nothing else".to_string() } else { queued.join(", ") }
                );
            }
        }
        self.busy += 1;
        self.most_busy = self.most_busy.max(self.busy);
        self.interval_busy = self.interval_busy.max(self.busy);
        Some(job)
    }

    fn noted(&mut self, level: usize, f: impl Fn(&mut LevelStats)) {
        f(&mut self.total[level]);
        f(&mut self.interval[level]);
    }

    /// The interval's line, every `LOG_EVERY` with activity.
    fn interval_line(&mut self, workers: usize) -> Option<String> {
        if self.logged.elapsed() < LOG_EVERY {
            return None;
        }
        self.logged = Instant::now();
        if !self.interval.iter().any(|s| s.done > 0 || s.wait.count > 0) {
            return None;
        }
        let parts: Vec<String> = (0..4)
            .rev()
            .filter(|&l| self.interval[l].done > 0 || self.interval[l].wait.count > 0 || self.depth[l] > 0)
            .map(|l| {
                let s = &self.interval[l];
                format!(
                    "{}: {} done ({} MB), queued {} (most {}), wait {}, decode {}{}",
                    NAMES[l],
                    s.done,
                    s.bytes >> 20,
                    self.depth[l],
                    s.depth_max,
                    s.wait.line(),
                    s.decode.line(),
                    if s.long_waits > 0 { format!(", {} waits of 2 s or more", s.long_waits) } else { String::new() }
                )
            })
            .collect();
        let now = stage_totals();
        let max_us: [u64; 4] = std::array::from_fn(|k| STAGES[k].max_us.swap(0, Ordering::Relaxed));
        let stages = stages_line(&self.stages_at, &now, &max_us);
        self.stages_at = now;
        let line = format!(
            "DirectStorage pool (10 s): {workers} workers, up to {} busy, read-ahead holds {} of {} MB; {}{}",
            self.interval_busy,
            self.held >> 20,
            read_ahead_room() >> 20,
            parts.join("; "),
            stages.map_or(String::new(), |s| format!("; stages: {s}"))
        );
        self.interval = Default::default();
        self.interval_busy = self.busy;
        Some(line)
    }
}

fn work(p: &'static Pool) {
    loop {
        let job = {
            let mut s = lock(p);
            loop {
                if let Some(j) = s.take(p.workers) {
                    break j;
                }
                s = p.wake.wait(s).unwrap_or_else(|e| e.into_inner());
            }
        };
        if let Some(line) = run(p, job) {
            crate::xlog_info!("{line}");
        }
    }
}

/// One piece of work, then its result into the lane: the interval's log line when one is due.
fn run(p: &Pool, job: Job) -> Option<String> {
    match job {
        Job::Turn { lane, queue, entry, read, since, level, held } => {
            let moved = match &entry {
                Entry::Request { req, .. } => u64::from(req.destination_size),
                _ => 0,
            };
            let mut decode = None;
            // a panic ends neither the worker nor the queue's turns (`close` waits for them)
            let since = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| queue.turn(entry, read, since, &mut decode))).unwrap_or_else(|_| {
                crate::xlog_error!("DirectStorage pool: an entry's turn on queue {lane} panicked; the queue goes on");
                since
            });
            drop(queue);
            let mut s = lock(p);
            s.held -= held;
            s.busy -= 1;
            if let Some(l) = s.lanes.iter_mut().find(|l| l.id == lane) {
                l.turn = false;
                l.since_status = since;
            }
            s.noted(level, |st| {
                st.done += 1;
                st.bytes += moved;
                if let Some(d) = decode {
                    st.decode.note(d);
                }
            });
            let line = s.interval_line(p.workers);
            drop(s);
            p.wake.notify_all();
            line
        }
        Job::Ahead { lane, ticket, queue, req, seq, file, level } => {
            let (result, took) = if queue.cancels_ahead(seq, req.tag) {
                (Ahead::Skip, None)
            } else {
                let t = Instant::now();
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe { super::load(&req, file.as_deref(), None) })).unwrap_or_else(|_| {
                    crate::xlog_error!("DirectStorage pool: a read-ahead on queue {lane} panicked; the request fails at its turn");
                    Err(super::E_ABORT)
                });
                (Ahead::Done(r), Some(t.elapsed()))
            };
            drop((queue, file));
            let mut s = lock(p);
            s.busy -= 1;
            let mut freed = 0;
            if let Some(l) = s.lanes.iter_mut().find(|l| l.id == lane) {
                l.reading -= 1;
                if let Some(slot) = ticket.checked_sub(l.front).and_then(|k| l.slots.get_mut(k as usize)) {
                    if matches!(result, Ahead::Skip) {
                        freed = std::mem::take(&mut slot.held);
                    }
                    slot.ahead = result;
                }
            }
            s.held -= freed;
            if let Some(d) = took {
                s.noted(level, |st| st.decode.note(d));
            }
            let line = s.interval_line(p.workers);
            drop(s);
            p.wake.notify_all();
            line
        }
    }
}

/// Since the start, for the run report (empty when no queue was made).
pub fn stats() -> PoolStats {
    let Some(p) = POOL.get() else { return PoolStats::default() };
    let s = lock(p);
    PoolStats {
        workers: p.workers as u64,
        most_busy: s.most_busy as u64,
        priorities: (0..4).filter(|&l| s.total[l] != LevelStats::default()).map(|l| (NAMES[l].to_string(), s.total[l])).collect(),
    }
}

/// Keep a lane's entries from taking their turn; read-ahead goes on (tests that look at what was
/// submitted or read ahead).
#[cfg(test)]
pub(super) fn hold(lane: u64, on: bool) {
    let p = pool();
    if let Some(l) = lock(p).lanes.iter_mut().find(|l| l.id == lane) {
        l.hold = on;
    }
    p.wake.notify_all();
}

/// Submitted entries of a lane not yet through their turn (0 when the lane is gone).
#[cfg(test)]
pub(super) fn submitted(lane: u64) -> usize {
    lock(pool()).lanes.iter().find(|l| l.id == lane).map_or(0, |l| l.slots.len())
}

/// Submitted entries of a lane read ahead and waiting for their turn.
#[cfg(test)]
pub(super) fn read_ahead(lane: u64) -> usize {
    lock(pool()).lanes.iter().find(|l| l.id == lane).map_or(0, |l| l.slots.iter().filter(|s| matches!(s.ahead, Ahead::Done(_))).count())
}

/// A held lane's submitted entries, taken out.
#[cfg(test)]
pub(super) fn take_submitted(lane: u64) -> Vec<Entry> {
    let mut s = lock(pool());
    let Some(l) = s.lanes.iter_mut().find(|l| l.id == lane) else { return Vec::new() };
    let taken: Vec<Entry> = l.slots.drain(..).map(|s| s.entry).collect();
    l.front += taken.len() as u64;
    taken
}

#[cfg(test)]
mod tests {
    use super::*;

    fn serve(share: &mut Share, ready: [bool; 4], cost: [u64; 4], n: usize) -> [u64; 4] {
        let mut served = [0u64; 4];
        for _ in 0..n {
            let l = share.pick(ready).unwrap();
            share.charge(l, cost[l]);
            served[l] += 1;
        }
        served
    }

    #[test]
    fn priorities_share_by_bytes_in_their_weights() {
        let mut share = Share::default();
        let served = serve(&mut share, [true; 4], [64 << 10; 4], 11_110);
        assert_eq!(served, [10, 100, 1000, 10_000]);
    }

    #[test]
    fn a_realtime_burst_never_stops_a_low_queue() {
        let mut share = Share::default();
        let (mut gaps, mut since_low) = (Vec::new(), 0);
        for _ in 0..20_000 {
            let l = share.pick([true, false, false, true]).unwrap();
            share.charge(l, 64 << 10);
            if l == 0 {
                gaps.push(since_low);
                since_low = 0;
            } else {
                since_low += 1;
            }
        }
        assert!(gaps.len() >= 19, "low served {} times", gaps.len());
        assert!(gaps.iter().all(|&g| g <= 1001), "{gaps:?}");
    }

    #[test]
    fn small_low_requests_get_their_byte_share_beside_large_high_ones() {
        let mut share = Share::default();
        let cost = [64 << 10, 0, 8 << 20, 0];
        let served = serve(&mut share, [true, false, true, false], cost, 20_000);
        let ratio = (served[2] * cost[2]) as f64 / (served[0] * cost[0]) as f64;
        assert!((90.0..=110.0).contains(&ratio), "high/low bytes {ratio} ({served:?})");
    }

    #[test]
    fn idle_time_is_no_credit() {
        let mut share = Share::default();
        serve(&mut share, [false, false, false, true], [64 << 10; 4], 5_000);
        let served = serve(&mut share, [true, false, false, true], [64 << 10; 4], 1_000);
        assert!((1..=2).contains(&served[0]), "low after an idle stretch: {served:?}");
    }

    #[test]
    fn the_stage_line_gives_each_stage_of_the_interval() {
        let at = |count, us, bytes| StageTotals { count, us, bytes };
        let before = [at(10, 1_000, 4 << 20), at(5, 500, 1 << 20), at(0, 0, 0), at(10, 100, 4 << 20)];
        let now = [at(14, 201_000, 6 << 20), at(5, 500, 1 << 20), at(2, 8_000, 2 << 20), at(14, 300, 6 << 20)];
        let line = stages_line(&before, &now, &[90_000, 0, 5_000, 120]).unwrap();
        assert_eq!(
            line,
            "read 4 (2.0 MB, avg 50.00 ms / max 90.0 ms, 10 MB/s), BCPack 2 (2.0 MB, avg 4.00 ms / max 5.0 ms, 250 MB/s), write 4 (2.0 MB, avg 0.05 ms / max 0.1 ms, 10000 MB/s)"
        );
        assert_eq!(stages_line(&now, &now, &[0; 4]), None, "an interval with no stage has no part");
    }

    #[test]
    fn timed_adds_to_its_stage() {
        let before = stage_totals();
        assert_eq!(timed(Stage::Bcpack, || 7, |_| 4096), 7);
        let after = stage_totals();
        assert!(after[Stage::Bcpack as usize].count > before[Stage::Bcpack as usize].count);
        assert!(after[Stage::Bcpack as usize].bytes >= before[Stage::Bcpack as usize].bytes + 4096);
    }

    #[test]
    fn a_tie_goes_to_the_higher_priority() {
        let mut share = Share::default();
        assert_eq!(share.pick([true; 4]), Some(3));
        assert_eq!(share.pick([true, true, true, false]), Some(2));
        assert_eq!(share.pick([false; 4]), None);
    }
}
