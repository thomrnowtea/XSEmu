use std::cell::Cell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum Stage {
    PipelineWait,
    PipelineBuild,
    ShaderCompile,
    DriverSlot,
    DriverBuild,
    FenceWait,
    GpuMemoryWait,
    QueueFenceWait,
    Queued,
}

impl Stage {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::PipelineWait => "pending pipeline wait",
            Self::PipelineBuild => "pipeline build",
            Self::ShaderCompile => "shader translation / compiler",
            Self::DriverSlot => "driver slot / RAM wait",
            Self::DriverBuild => "driver build (slot held)",
            Self::FenceWait => "CPU waiting on GPU fence",
            Self::GpuMemoryWait => "CPU resolving GPU memory wait",
            Self::QueueFenceWait => "GPU queue fence wait",
            Self::Queued => "urgent pipeline queued",
        }
    }

    pub(crate) fn blocking(self) -> bool {
        matches!(
            self,
            Self::PipelineWait | Self::FenceWait | Self::GpuMemoryWait | Self::QueueFenceWait
        )
    }
}

#[derive(Clone, Copy)]
pub(crate) struct Sample {
    pub stage: Stage,
    pub since: Instant,
    pub tid: u32,
    pub pipeline: u64,
    pub shader: u64,
    pub target: Option<u64>,
    pub queue: Option<usize>,
}

impl Sample {
    pub(crate) fn text(self, now: Instant) -> String {
        use std::fmt::Write;
        let mut s = format!(
            "{} {:.1}s",
            self.stage.label(),
            now.saturating_duration_since(self.since).as_secs_f32()
        );
        if self.tid != 0 {
            let _ = write!(s, " | TID {}", self.tid);
        }
        if self.pipeline != 0 {
            let _ = write!(s, " | PSO {:016x}", self.pipeline);
        }
        if self.shader != 0 {
            let _ = write!(s, " | shader key {:016x}", self.shader);
        }
        if let Some(queue) = self.queue {
            let _ = write!(s, " | q{queue}");
        }
        if let Some(target) = self.target {
            let _ = write!(s, " | target {target:#x}");
        }
        s
    }
}

struct Slot {
    active: AtomicBool,
    data: Mutex<Option<Sample>>,
}

static SLOTS: [Slot; 64] = [const {
    Slot {
        active: AtomicBool::new(false),
        data: Mutex::new(None),
    }
}; 64];

pub(crate) fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| overlay() || logs())
}

pub(crate) fn overlay() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("XCL_STALL_OVERLAY").as_deref() == Ok("1"))
}

pub(crate) fn logs() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("XCL_STALL_LOG").as_deref() == Ok("1"))
}

thread_local! {
    static JOB: Cell<(u64, u64)> = const { Cell::new((0, 0)) };
}

/// The pipeline and shader key this thread works on (`with_job`; 0s: none).
pub(crate) fn job() -> (u64, u64) {
    JOB.with(Cell::get)
}

/// `f` builds `pipeline` of `shader`: named in the stall samples, and by `job` (live stall off too).
pub(crate) fn with_job<R>(pipeline: u64, shader: u64, f: impl FnOnce() -> R) -> R {
    struct Restore((u64, u64));
    impl Drop for Restore {
        fn drop(&mut self) {
            JOB.with(|key| key.set(self.0));
        }
    }
    let _restore = Restore(JOB.with(|key| key.replace((pipeline, shader))));
    let _work = stage(Stage::PipelineBuild, || 0);
    f()
}

pub(crate) struct Span(Option<&'static Slot>);

impl Drop for Span {
    fn drop(&mut self) {
        if let Some(slot) = self.0 {
            slot.active.store(false, Ordering::Release);
        }
    }
}

pub(crate) fn stage(stage: Stage, shader: impl FnOnce() -> u64) -> Span {
    if !enabled() {
        return Span(None);
    }
    let (pipeline, key) = JOB.with(Cell::get);
    begin(
        stage,
        pipeline,
        if key == 0 { shader() } else { key },
        None,
        None,
    )
}

pub(crate) fn begin(
    stage: Stage,
    pipeline: u64,
    shader: u64,
    target: Option<u64>,
    queue: Option<usize>,
) -> Span {
    if !enabled() {
        return Span(None);
    }
    for slot in &SLOTS {
        if slot.active.load(Ordering::Relaxed) {
            continue;
        }
        let Ok(mut data) = slot.data.try_lock() else {
            continue;
        };
        if slot.active.load(Ordering::Acquire) {
            continue;
        }
        let tid = unsafe { windows_sys::Win32::System::Threading::GetCurrentThreadId() };
        *data = Some(Sample {
            stage,
            since: Instant::now(),
            tid,
            pipeline,
            shader,
            target,
            queue,
        });
        slot.active.store(true, Ordering::Release);
        return Span(Some(slot));
    }
    Span(None)
}

pub(crate) fn snapshot() -> Vec<Sample> {
    let mut samples: Vec<_> = SLOTS
        .iter()
        .filter_map(|slot| {
            let data = slot.data.try_lock().ok()?;
            if slot.active.load(Ordering::Acquire) {
                *data
            } else {
                None
            }
        })
        .collect();
    samples.sort_unstable_by_key(|s| s.since);
    samples
}
