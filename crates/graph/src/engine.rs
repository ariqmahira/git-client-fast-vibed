//! Background graph loading with progressive snapshots and cancellation.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use fgc_core::{CommitDag, GitBackend, Oid};

use crate::{GraphOrder, Layout, LayoutBuilder, Topology};

#[derive(Debug, Clone)]
pub struct EngineOptions {
    pub order: GraphOrder,
    /// Rows laid out before the first snapshot is published (a screenful
    /// plus scroll headroom).
    pub first_screen_rows: usize,
    /// Rows per subsequent progress snapshot.
    pub publish_every_rows: usize,
    /// Without a commit-graph file: run `git commit-graph write` in the
    /// background after publishing a provisional graph (plan §3).
    pub write_commit_graph: bool,
}

impl Default for EngineOptions {
    fn default() -> Self {
        EngineOptions {
            order: GraphOrder::Date,
            first_screen_rows: 500,
            publish_every_rows: 64 * 1024,
            write_commit_graph: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// The first page only (`layout.rows_laid()` rows), from a time-ordered
    /// walk, published before the full topology is loaded. In date order it
    /// matches the final layout's first rows; in topo order (only used
    /// without a commit-graph) the final rows may differ.
    Provisional,
    /// Full topology, layout in progress (`layout.rows_laid()` rows usable).
    Partial,
    Complete,
}

#[derive(Debug, Clone, Default)]
pub struct LoadStats {
    pub commits: usize,
    pub from_commit_graph: u32,
    pub parsed: u32,
    pub wrote_commit_graph: bool,
    pub provisional: Option<Duration>,
    pub write_commit_graph_time: Option<Duration>,
    pub dag: Duration,
    pub sort: Duration,
    /// Load start → first full-topology snapshot published.
    pub first_screen: Option<Duration>,
    pub layout: Duration,
    pub total: Duration,
    pub reused_from_row: Option<u32>,
    pub max_lanes: u16,
}

/// An immutable view published to the UI.
#[derive(Debug, Clone)]
pub struct GraphSnapshot {
    /// Load generation; snapshots from older generations are never sent
    /// after a newer load starts, but a UI may still receive one in flight
    /// and should drop it.
    pub generation: u64,
    pub phase: Phase,
    pub layout: Layout,
    pub stats: LoadStats,
}

#[derive(Debug, Clone)]
pub enum GraphEvent {
    Snapshot(Arc<GraphSnapshot>),
    Failed { generation: u64, error: String },
}

/// Loads and lays out commit graphs off the UI thread, one repo per engine.
pub struct GraphEngine {
    backend: Arc<dyn GitBackend>,
    opts: EngineOptions,
    generation: Arc<AtomicU64>,
    /// Last complete layout, reused when the next load only adds commits.
    last: Arc<Mutex<Option<Layout>>>,
}

impl GraphEngine {
    pub fn new(backend: Arc<dyn GitBackend>, opts: EngineOptions) -> Self {
        GraphEngine {
            backend,
            opts,
            generation: Arc::new(AtomicU64::new(0)),
            last: Arc::new(Mutex::new(None)),
        }
    }

    /// Cancels any in-flight load (it stops at its next checkpoint and sends
    /// nothing more).
    pub fn cancel(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
    }

    /// Starts loading the graph for `tips` on a background thread,
    /// superseding any previous load. `sink` receives snapshots on that
    /// thread. Returns the generation id of this load.
    pub fn load(
        &self,
        tips: Vec<Oid>,
        sink: impl Fn(GraphEvent) + Send + 'static,
    ) -> (u64, std::thread::JoinHandle<()>) {
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let job = Job {
            backend: self.backend.clone(),
            opts: self.opts.clone(),
            current: self.generation.clone(),
            last: self.last.clone(),
            generation,
            tips,
        };
        let handle = std::thread::Builder::new()
            .name(format!("fgc-graph-{generation}"))
            .spawn(move || {
                let span = tracing::debug_span!("graph_load", generation);
                let _e = span.enter();
                if let Err(error) = job.run(&sink) {
                    if job.is_current() {
                        sink(GraphEvent::Failed {
                            generation,
                            error: error.to_string(),
                        });
                    }
                }
            })
            .expect("spawn graph thread");
        (generation, handle)
    }
}

struct Job {
    backend: Arc<dyn GitBackend>,
    opts: EngineOptions,
    current: Arc<AtomicU64>,
    last: Arc<Mutex<Option<Layout>>>,
    generation: u64,
    tips: Vec<Oid>,
}

impl Job {
    fn is_current(&self) -> bool {
        self.current.load(Ordering::SeqCst) == self.generation
    }

    fn publish(
        &self,
        sink: &impl Fn(GraphEvent),
        phase: Phase,
        layout: Layout,
        stats: &LoadStats,
    ) -> bool {
        if !self.is_current() {
            return false;
        }
        sink(GraphEvent::Snapshot(Arc::new(GraphSnapshot {
            generation: self.generation,
            phase,
            layout,
            stats: stats.clone(),
        })));
        true
    }

    fn provisional_layout(&self) -> fgc_core::Result<Layout> {
        let page = self
            .backend
            .walk_commits(&self.tips, 0..self.opts.first_screen_rows)?;
        let in_page: std::collections::HashSet<Oid> = page.iter().map(|c| c.oid).collect();
        let mut placeholders: Vec<Oid> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for c in &page {
            for p in &c.parents {
                if !in_page.contains(p) && seen.insert(*p) {
                    placeholders.push(*p);
                }
            }
        }
        let dag = CommitDag::from_commits(
            page.iter()
                .map(|c| (c.oid, c.commit_time, c.parents.as_slice()))
                .chain(placeholders.iter().map(|p| (*p, i64::MIN, &[][..]))),
        );
        let topo = Arc::new(Topology::from_dag(dag, self.opts.order));
        let mut builder = LayoutBuilder::new(topo);
        builder.step(page.len());
        Ok(builder.snapshot())
    }

    fn run(&self, sink: &impl Fn(GraphEvent)) -> fgc_core::Result<()> {
        let t0 = Instant::now();
        let mut stats = LoadStats::default();
        let info = self.backend.repo_info()?;

        // Provisional first screen from the first page of the time-ordered
        // walk (~25 ms even on linux.git). In date order its rows and lanes
        // match the final layout (barring clock skew), because parents
        // outside the page become placeholder rows below it: lanes run off
        // the bottom exactly as they will in the full graph, so nothing
        // jumps when the full topology replaces it.
        let provisional = self.opts.order == GraphOrder::Date || !info.has_commit_graph;
        if provisional {
            let layout = self.provisional_layout()?;
            stats.provisional = Some(t0.elapsed());
            if !self.publish(sink, Phase::Provisional, layout, &stats) {
                return Ok(());
            }
        }
        if !info.has_commit_graph && self.opts.write_commit_graph && !info.is_shallow {
            let t = Instant::now();
            match self.backend.write_commit_graph() {
                Ok(()) => stats.wrote_commit_graph = true,
                Err(e) => tracing::warn!("commit-graph write failed, parsing commits instead: {e}"),
            }
            stats.write_commit_graph_time = Some(t.elapsed());
            if !self.is_current() {
                return Ok(());
            }
        }

        let t = Instant::now();
        let dag = self.backend.commit_dag(&self.tips)?;
        stats.dag = t.elapsed();
        stats.commits = dag.len();
        stats.from_commit_graph = dag.stats.from_commit_graph;
        stats.parsed = dag.stats.parsed;
        if !self.is_current() {
            return Ok(());
        }

        let t = Instant::now();
        let topo = Arc::new(Topology::from_dag(dag, self.opts.order));
        stats.sort = t.elapsed();

        let t = Instant::now();
        let prev = self.last.lock().unwrap().clone();
        let mut builder = match &prev {
            Some(prev) => LayoutBuilder::with_reuse(topo.clone(), prev),
            None => LayoutBuilder::new(topo.clone()),
        };
        let mut done = builder.step(self.opts.first_screen_rows);
        if !done {
            stats.first_screen = Some(t0.elapsed());
            stats.max_lanes = builder.snapshot().max_lanes();
            if !self.publish(sink, Phase::Partial, builder.snapshot(), &stats) {
                return Ok(());
            }
        }
        while !done {
            done = builder.step(self.opts.publish_every_rows);
            if !done {
                let snap = builder.snapshot();
                stats.max_lanes = snap.max_lanes();
                if !self.publish(sink, Phase::Partial, snap, &stats) {
                    return Ok(());
                }
            }
        }
        let layout = builder.finish();
        stats.layout = t.elapsed();
        // Selection lookups (keep the user's place across reloads).
        topo.build_index();
        stats.first_screen.get_or_insert(t0.elapsed());
        stats.reused_from_row = layout.reused_from();
        stats.max_lanes = layout.max_lanes();
        stats.total = t0.elapsed();
        if self.is_current() {
            *self.last.lock().unwrap() = Some(layout.clone());
        }
        self.publish(sink, Phase::Complete, layout, &stats);
        Ok(())
    }
}
