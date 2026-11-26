//! Performance budget helpers shared by the probe, benches and (from M3) the UI.
//!
//! [`FrameRecorder`] collects per-frame durations; the GPUI shell will feed it
//! from its frame callback, and CI asserts the resulting [`FrameStats`]
//! against the scroll/resize budget (8.3 ms at 120 fps).

use std::time::{Duration, Instant};

/// Plan §1 budgets, in one place so code and CI agree.
pub mod budgets {
    use std::time::Duration;

    pub const COLD_START_FIRST_FRAME: Duration = Duration::from_millis(300);
    pub const OPEN_REPO_FIRST_COMMITS: Duration = Duration::from_millis(400);
    pub const FRAME_TIME: Duration = Duration::from_micros(8_333);
    pub const INPUT_TO_PAINT: Duration = Duration::from_millis(16);
    pub const STATUS_AFTER_SAVE: Duration = Duration::from_millis(150);
    pub const DIFF_FIRST_SCREEN: Duration = Duration::from_millis(100);
    /// M1 acceptance: linux.git, first 500 commits.
    pub const FIRST_500_COMMITS: Duration = Duration::from_millis(100);
    pub const IDLE_MEMORY_BYTES: u64 = 150 * 1024 * 1024;
}

/// Records frame durations with bounded memory.
#[derive(Debug, Clone)]
pub struct FrameRecorder {
    frames_us: Vec<u32>,
    capacity: usize,
    last: Option<Instant>,
}

impl FrameRecorder {
    pub fn new(capacity: usize) -> Self {
        FrameRecorder {
            frames_us: Vec::with_capacity(capacity.min(1 << 16)),
            capacity: capacity.max(1),
            last: None,
        }
    }

    /// Call once per presented frame. The first call only sets the baseline.
    pub fn tick(&mut self) {
        let now = Instant::now();
        if let Some(prev) = self.last.replace(now) {
            self.record(now - prev);
        }
    }

    /// Record an explicitly measured frame (e.g. CPU time of layout + paint).
    pub fn record(&mut self, frame: Duration) {
        if self.frames_us.len() == self.capacity {
            // Keep the most recent window; drop the oldest half in one go so
            // the amortized cost stays O(1).
            let half = self.capacity / 2;
            self.frames_us.drain(..half.max(1));
        }
        let us = frame.as_micros().min(u32::MAX as u128) as u32;
        self.frames_us.push(us);
    }

    /// Forget the baseline, e.g. after the window was hidden.
    pub fn pause(&mut self) {
        self.last = None;
    }

    pub fn clear(&mut self) {
        self.frames_us.clear();
        self.last = None;
    }

    pub fn len(&self) -> usize {
        self.frames_us.len()
    }

    pub fn is_empty(&self) -> bool {
        self.frames_us.is_empty()
    }

    pub fn stats(&self, budget: Duration) -> FrameStats {
        FrameStats::from_micros(&self.frames_us, budget)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
pub struct FrameStats {
    pub frames: usize,
    pub mean_us: f64,
    pub p50_us: u32,
    pub p95_us: u32,
    pub p99_us: u32,
    pub max_us: u32,
    pub budget_us: u32,
    /// Frames that exceeded the budget.
    pub over_budget: usize,
}

impl FrameStats {
    pub fn from_micros(samples: &[u32], budget: Duration) -> FrameStats {
        let budget_us = budget.as_micros().min(u32::MAX as u128) as u32;
        if samples.is_empty() {
            return FrameStats {
                frames: 0,
                mean_us: 0.0,
                p50_us: 0,
                p95_us: 0,
                p99_us: 0,
                max_us: 0,
                budget_us,
                over_budget: 0,
            };
        }
        let mut sorted = samples.to_vec();
        sorted.sort_unstable();
        let pct = |p: f64| {
            let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
            sorted[idx]
        };
        FrameStats {
            frames: sorted.len(),
            mean_us: sorted.iter().map(|&v| v as f64).sum::<f64>() / sorted.len() as f64,
            p50_us: pct(0.50),
            p95_us: pct(0.95),
            p99_us: pct(0.99),
            max_us: *sorted.last().unwrap(),
            budget_us,
            over_budget: sorted.iter().filter(|&&v| v > budget_us).count(),
        }
    }

    /// CI gate: p99 within budget. (Max is reported, not gated, so a single
    /// OS hiccup on a shared runner doesn't fail the build.)
    pub fn within_budget(&self) -> bool {
        self.p99_us <= self.budget_us
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stats_percentiles() {
        let mut rec = FrameRecorder::new(1000);
        for i in 1..=100u64 {
            rec.record(Duration::from_micros(i * 100));
        }
        let s = rec.stats(Duration::from_micros(8_333));
        assert_eq!(s.frames, 100);
        assert_eq!(s.max_us, 10_000);
        assert_eq!(s.p50_us, 5_100);
        assert_eq!(s.over_budget, 17);
        assert!(!s.within_budget());
    }

    #[test]
    fn bounded_capacity() {
        let mut rec = FrameRecorder::new(10);
        for _ in 0..1000 {
            rec.record(Duration::from_millis(1));
        }
        assert!(rec.len() <= 10);
        assert!(rec.stats(budgets::FRAME_TIME).within_budget());
    }

    #[test]
    fn empty_is_within_budget() {
        let rec = FrameRecorder::new(10);
        assert!(rec.stats(budgets::FRAME_TIME).within_budget());
    }
}
