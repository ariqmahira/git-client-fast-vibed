//! Built-in UI benchmark (`fgc --bench`): drives the real window through
//! scripted phases, one step per frame, and records what the plan §1 budgets
//! talk about.
//!
//! 1. wait until the full graph is laid out
//! 2. scroll the graph continuously (3 rows per frame)
//! 3. jump to random positions (scrollbar drags: cold geometry + metadata)
//! 4. move the selection with dispatched key actions (input → paint)
//! 5. open the newest commit's diff (command → first paint showing it)
//! 6. scroll that diff continuously
//!
//! Each step runs in a next-frame callback, so it mutates state right
//! before GPUI draws: the recorded frame is the frame that shows the step.
//! Frame times are CPU-side (render + layout + paint, no GPU wait).

use std::path::PathBuf;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::{Duration, Instant};

use fgc_actor::Phase as GraphPhase;
use fgc_bench::BudgetFile;
use fgc_core::perf::{FrameRecorder, FrameStats, budgets};
use gpui::{Context, ScrollStrategy, UniformListScrollHandle, Window, point, px};

use crate::theme::{DIFF_ROW_H, ROW_H};
use crate::util::{app_anon_bytes, app_rss_bytes, rss_bytes};
use crate::view::{RepoView, SelectNext};

pub static EXIT_CODE: AtomicI32 = AtomicI32::new(0);

#[derive(Debug, Clone)]
pub struct BenchOptions {
    pub frames: usize,
    pub inputs: usize,
    pub report: Option<PathBuf>,
    pub budgets: Option<(PathBuf, String)>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Step {
    WaitReady { warm: usize },
    ScrollGraph { left: usize },
    JumpGraph { left: usize },
    Input { left: usize, tick: usize },
    DiffOpen { since: Instant },
    ScrollDiff { left: usize },
    Done,
}

#[derive(Default)]
struct Results {
    rss_ready: Option<u64>,
    app_rss_ready: Option<u64>,
    app_anon_ready: Option<u64>,
    scroll_graph: Option<(FrameStats, FrameStats)>,
    jump_graph: Option<(FrameStats, FrameStats)>,
    diff_open: Option<Duration>,
    diff_lines: usize,
    scroll_diff: Option<(FrameStats, FrameStats)>,
    commits: usize,
    max_lanes: u16,
}

pub struct Bench {
    opts: BenchOptions,
    step: Step,
    started: Instant,
    rng: u64,
    results: Results,
}

impl Bench {
    pub fn new(opts: BenchOptions) -> Bench {
        Bench {
            opts,
            step: Step::WaitReady { warm: 0 },
            started: Instant::now(),
            rng: 0x2545_f491_4f6c_dd1d,
            results: Results::default(),
        }
    }

    fn rand(&mut self) -> u64 {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        self.rng
    }

    fn start_phase(view: &RepoView) {
        let mut perf = view.perf.borrow_mut();
        perf.phase = Some(FrameRecorder::new(8192));
        perf.phase_cpu = Some(FrameRecorder::new(8192));
    }

    fn end_phase(view: &RepoView) -> Option<(FrameStats, FrameStats)> {
        if std::env::var_os("FGC_BENCH_MEM").is_some() {
            eprintln!(
                "bench mem: app rss {:.1} MB, anon {:.1} MB, metas {}",
                app_rss_bytes().unwrap_or(0) as f64 / 1e6,
                app_anon_bytes().unwrap_or(0) as f64 / 1e6,
                view.metas_len()
            );
            let smaps = std::fs::read_to_string("/proc/self/smaps").unwrap_or_default();
            let mut by: std::collections::HashMap<String, u64> = Default::default();
            let mut name = String::new();
            for line in smaps.lines() {
                let f = line.split_whitespace().next().unwrap_or("");
                if f.contains('-') && !f.ends_with(':') {
                    name = line
                        .split_whitespace()
                        .nth(5)
                        .unwrap_or("[anon]")
                        .to_string();
                } else if let Some(r) = line.strip_prefix("Rss:") {
                    *by.entry(name.clone()).or_default() +=
                        r.split_whitespace().next().unwrap().parse::<u64>().unwrap();
                }
            }
            let mut v: Vec<_> = by.into_iter().filter(|(_, kb)| *kb > 3000).collect();
            v.sort_by_key(|e| std::cmp::Reverse(e.1));
            for (n, kb) in v.iter().take(7) {
                eprintln!("    {:>7} KB {n}", kb);
            }
        }
        let mut perf = view.perf.borrow_mut();
        let wall = perf.phase.take()?.stats(budgets::FRAME_TIME);
        let cpu = perf.phase_cpu.take()?.stats(budgets::FRAME_TIME);
        Some((wall, cpu))
    }

    fn scroll_by(handle: &UniformListScrollHandle, dy: f32) {
        let base = handle.0.borrow().base_handle.clone();
        let off = base.offset();
        let max = base.max_offset().height;
        let mut y = off.y - px(dy);
        if -y > max {
            y = px(0.);
        }
        base.set_offset(point(off.x, y));
    }

    /// One step per frame. Returns false when finished.
    pub fn tick(
        &mut self,
        view: &mut RepoView,
        window: &mut Window,
        cx: &mut Context<RepoView>,
    ) -> bool {
        self.step = match self.step {
            Step::WaitReady { warm } => {
                let complete = view
                    .snapshot
                    .as_ref()
                    .is_some_and(|s| s.phase == GraphPhase::Complete);
                if self.started.elapsed() > Duration::from_secs(600) {
                    eprintln!("bench: graph never completed");
                    EXIT_CODE.store(2, Ordering::SeqCst);
                    Step::Done
                } else if complete && warm >= 30 {
                    self.results.rss_ready = rss_bytes();
                    self.results.app_rss_ready = app_rss_bytes();
                    self.results.app_anon_ready = app_anon_bytes();
                    let s = view.snapshot.as_ref().unwrap();
                    self.results.commits = s.layout.topology().len();
                    self.results.max_lanes = s.layout.max_lanes();
                    Self::start_phase(view);
                    Step::ScrollGraph {
                        left: self.opts.frames,
                    }
                } else {
                    Step::WaitReady {
                        warm: if complete { warm + 1 } else { 0 },
                    }
                }
            }
            Step::ScrollGraph { left } => {
                if left == 0 {
                    self.results.scroll_graph = Self::end_phase(view);
                    Self::start_phase(view);
                    Step::JumpGraph {
                        left: self.opts.frames / 3,
                    }
                } else {
                    Self::scroll_by(&view.graph_scroll, ROW_H * 3.0);
                    Step::ScrollGraph { left: left - 1 }
                }
            }
            Step::JumpGraph { left } => {
                if left == 0 {
                    self.results.jump_graph = Self::end_phase(view);
                    view.graph_scroll
                        .scroll_to_item_strict(0, ScrollStrategy::Top);
                    view.select_row(0);
                    Step::Input {
                        left: self.opts.inputs,
                        tick: 0,
                    }
                } else {
                    let base = view.graph_scroll.0.borrow().base_handle.clone();
                    let max = f32::from(base.max_offset().height).max(1.0);
                    let y = (self.rand() % max as u64) as f32;
                    base.set_offset(point(base.offset().x, px(-y)));
                    Step::JumpGraph { left: left - 1 }
                }
            }
            Step::Input { left, tick } => {
                if left == 0 {
                    // Diff open: clear the selection, then select the newest
                    // commit and time until its diff is painted.
                    view.selection = None;
                    view.doc = Default::default();
                    view.doc_for = None;
                    view.perf.borrow_mut().diff_open_at = Some(Instant::now());
                    view.select_row(0);
                    Step::DiffOpen {
                        since: Instant::now(),
                    }
                } else if tick % 2 == 0 {
                    window.dispatch_action(Box::new(SelectNext), cx);
                    Step::Input {
                        left: left - 1,
                        tick: tick + 1,
                    }
                } else {
                    Step::Input {
                        left,
                        tick: tick + 1,
                    }
                }
            }
            Step::DiffOpen { since } => {
                let opened = view.perf.borrow().diff_open;
                if let Some(d) = opened {
                    self.results.diff_open = Some(d);
                    self.results.diff_lines = view.doc.rows.len();
                    Self::start_phase(view);
                    Step::ScrollDiff {
                        left: self.opts.frames / 2,
                    }
                } else if since.elapsed() > Duration::from_secs(20) {
                    eprintln!(
                        "bench: diff did not open (e.g. blob-less clone); skipping diff phases"
                    );
                    Step::Done
                } else {
                    Step::DiffOpen { since }
                }
            }
            Step::ScrollDiff { left } => {
                if left == 0 {
                    self.results.scroll_diff = Self::end_phase(view);
                    Step::Done
                } else {
                    Self::scroll_by(&view.diff_scroll, DIFF_ROW_H * 3.0);
                    Step::ScrollDiff { left: left - 1 }
                }
            }
            Step::Done => Step::Done,
        };
        if self.step == Step::Done {
            self.finish(view);
            cx.quit();
            return false;
        }
        true
    }

    fn finish(&self, view: &RepoView) {
        let perf = view.perf.borrow();
        let r = &self.results;
        let ms = |d: Duration| d.as_secs_f64() * 1000.0;
        let us = |v: u32| v as f64 / 1000.0;
        let input = perf.input_stats();
        let mut m: Vec<(&str, f64, String)> = Vec::new();
        if let Some(d) = perf.first_paint {
            m.push((
                "ui_first_paint",
                ms(d),
                "process start → first frame painted".into(),
            ));
        }
        if let Some(d) = perf.graph_first {
            m.push((
                "ui_graph_first_snapshot",
                ms(d),
                "process start → first graph rows received".into(),
            ));
        }
        if let Some(d) = perf.graph_complete {
            m.push((
                "ui_graph_complete",
                ms(d),
                format!("process start → all {} commits laid out", r.commits),
            ));
        }
        // Two numbers per phase: wall time of the frame window (includes
        // preemption, e.g. by a software rasterizer's threads) and the UI
        // thread's own CPU time in the same window.
        type Phase = Option<(FrameStats, FrameStats)>;
        let frames = |m: &mut Vec<(&str, f64, String)>,
                      wall_name: &'static str,
                      cpu_name: &'static str,
                      s: &Phase| {
            if let Some((w, c)) = s {
                m.push((
                    wall_name,
                    us(w.p99_us),
                    format!(
                        "wall: p50 {:.2} max {:.2} ms over {} frames, {} over budget",
                        us(w.p50_us),
                        us(w.max_us),
                        w.frames,
                        w.over_budget
                    ),
                ));
                m.push((
                    cpu_name,
                    us(c.p99_us),
                    format!(
                        "UI-thread CPU: p50 {:.2} max {:.2} ms, {} over budget",
                        us(c.p50_us),
                        us(c.max_us),
                        c.over_budget
                    ),
                ));
            }
        };
        frames(
            &mut m,
            "ui_scroll_graph_p99",
            "ui_scroll_graph_cpu_p99",
            &r.scroll_graph,
        );
        frames(
            &mut m,
            "ui_jump_graph_p99",
            "ui_jump_graph_cpu_p99",
            &r.jump_graph,
        );
        if input.frames > 0 {
            m.push((
                "ui_input_to_paint_p99",
                us(input.p99_us),
                format!(
                    "p50 {:.2} max {:.2} ms over {} key actions",
                    us(input.p50_us),
                    us(input.max_us),
                    input.frames
                ),
            ));
        }
        if let Some(d) = r.diff_open {
            m.push(("ui_diff_open", ms(d), format!("{} diff rows", r.diff_lines)));
        }
        frames(
            &mut m,
            "ui_scroll_diff_p99",
            "ui_scroll_diff_cpu_p99",
            &r.scroll_diff,
        );
        if let Some(rss) = r.rss_ready {
            m.push((
                "ui_rss_ready_mb",
                rss as f64 / 1e6,
                "resident memory once the graph is loaded (MB)".into(),
            ));
        }
        if let Some(rss) = rss_bytes() {
            m.push((
                "ui_rss_mb",
                rss as f64 / 1e6,
                "resident memory at the end (MB)".into(),
            ));
        }
        if let Some(rss) = r.app_rss_ready {
            m.push((
                "ui_app_rss_ready_mb",
                rss as f64 / 1e6,
                "graph loaded, excluding software-GPU mappings (MB)".into(),
            ));
        }
        if let Some(rss) = app_rss_bytes() {
            m.push((
                "ui_app_rss_mb",
                rss as f64 / 1e6,
                "at the end, excluding software-GPU mappings (MB)".into(),
            ));
        }
        if let Some(b) = r.app_anon_ready {
            m.push((
                "ui_app_anon_idle_mb",
                b as f64 / 1e6,
                "graph loaded: memory the app allocated (no mapped files, no software GPU) (MB)"
                    .into(),
            ));
        }

        let budgets =
            self.opts
                .budgets
                .as_ref()
                .and_then(|(path, profile)| match BudgetFile::load(path) {
                    Ok(f) => f.profiles.get(profile).cloned(),
                    Err(e) => {
                        eprintln!("bench: cannot read {}: {e}", path.display());
                        None
                    }
                });
        println!(
            "{:<24} {:>10} {:>10}  detail",
            "measurement", "value", "budget"
        );
        let mut failed = false;
        for (name, v, detail) in &m {
            let b = budgets.as_ref().and_then(|b| b.get(*name)).copied();
            let mark = match b {
                Some(limit) if *v > limit => {
                    failed = true;
                    " FAIL"
                }
                Some(_) => "   ok",
                None => "",
            };
            println!(
                "{name:<24} {v:>10.2} {:>10}{mark}  {detail}",
                b.map(|b| b.to_string()).unwrap_or_else(|| "-".into())
            );
        }
        println!("graph: {} commits, {} lanes max", r.commits, r.max_lanes);
        if let Some(path) = &self.opts.report {
            let doc = serde_json::json!({
                "commits": r.commits,
                "max_lanes": r.max_lanes,
                "measurements": m.iter().map(|(n, v, d)| serde_json::json!({
                    "name": n, "value": v, "detail": d,
                    "budget": budgets.as_ref().and_then(|b| b.get(*n)),
                })).collect::<Vec<_>>(),
            });
            if let Err(e) = std::fs::write(path, serde_json::to_string_pretty(&doc).unwrap()) {
                eprintln!("bench: cannot write report: {e}");
            }
        }
        if failed {
            EXIT_CODE.store(1, Ordering::SeqCst);
        }
    }
}
