//! Frame and input timing (plan §1 budgets), shared between the root view's
//! render (frame start) and a canvas painted last (frame end).

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use fgc_core::perf::{FrameRecorder, FrameStats, budgets};

/// CPU time consumed by the calling thread (excludes time the thread was
/// preempted, e.g. by a software rasterizer's worker threads).
pub fn thread_cpu_time() -> Duration {
    #[cfg(unix)]
    {
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: valid pointer to a timespec; CLOCK_THREAD_CPUTIME_ID is supported on Linux/macOS.
        if unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) } == 0 {
            return Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32);
        }
    }
    Duration::ZERO
}

#[derive(Debug)]
pub struct Perf {
    pub process_start: Instant,
    pub first_paint: Option<Duration>,
    frame_start: Option<Instant>,
    frame_start_cpu: Duration,
    /// UI-thread CPU time per frame, same window as `frames`.
    pub frames_cpu: FrameRecorder,
    pub phase_cpu: Option<FrameRecorder>,
    /// CPU time of every frame (render + layout + paint).
    pub frames: FrameRecorder,
    /// Input event (key/click, or a dispatched action) → end of the paint
    /// that shows its effect.
    pub input: FrameRecorder,
    input_at: Option<Instant>,
    /// Optional recorder for a benchmark phase.
    pub phase: Option<FrameRecorder>,
    pub last_frame: Duration,
    pub paints: u64,
    /// Diff-open measurement: command sent → first paint showing the diff.
    pub diff_open_at: Option<Instant>,
    pub diff_ready: bool,
    pub diff_open: Option<Duration>,
    /// Process start → first graph snapshot / complete layout received.
    pub graph_first: Option<Duration>,
    pub graph_complete: Option<Duration>,
}

pub type SharedPerf = Rc<RefCell<Perf>>;

impl Perf {
    pub fn new(process_start: Instant) -> SharedPerf {
        Rc::new(RefCell::new(Perf {
            process_start,
            first_paint: None,
            frame_start: None,
            frame_start_cpu: Duration::ZERO,
            frames_cpu: FrameRecorder::new(4096),
            phase_cpu: None,
            frames: FrameRecorder::new(4096),
            input: FrameRecorder::new(1024),
            input_at: None,
            phase: None,
            last_frame: Duration::ZERO,
            paints: 0,
            diff_open_at: None,
            diff_ready: false,
            diff_open: None,
            graph_first: None,
            graph_complete: None,
        }))
    }

    pub fn frame_started(&mut self) {
        self.frame_start = Some(Instant::now());
        self.frame_start_cpu = thread_cpu_time();
    }

    pub fn input_received(&mut self) {
        // Keep the earliest unpainted input: latency is measured from it.
        self.input_at.get_or_insert_with(Instant::now);
    }

    pub fn frame_painted(&mut self) {
        let now = Instant::now();
        self.paints += 1;
        self.first_paint.get_or_insert(now - self.process_start);
        if let Some(start) = self.frame_start.take() {
            self.last_frame = now - start;
            self.frames.record(self.last_frame);
            if let Some(p) = &mut self.phase {
                p.record(self.last_frame);
            }
            let cpu = thread_cpu_time().saturating_sub(self.frame_start_cpu);
            self.frames_cpu.record(cpu);
            if let Some(p) = &mut self.phase_cpu {
                p.record(cpu);
            }
        }
        if let Some(at) = self.input_at.take() {
            self.input.record(now - at);
        }
        if self.diff_ready {
            if let Some(at) = self.diff_open_at.take() {
                self.diff_open = Some(now - at);
            }
            self.diff_ready = false;
        }
    }

    pub fn frame_stats(&self) -> FrameStats {
        self.frames.stats(budgets::FRAME_TIME)
    }

    pub fn input_stats(&self) -> FrameStats {
        self.input.stats(budgets::INPUT_TO_PAINT)
    }
}
