//! `fgc-probe`: headless timing of the core read path against a repository.
//!
//! ```text
//! fgc-probe /path/to/repo                         # print a timing table
//! fgc-probe linux.git --budgets budgets.toml --profile linux   # CI gate
//! fgc-probe . --json out.json --trace             # JSON report + tracing spans
//! ```
//!
//! Each measurement is the median of `--iters` runs. Measurements that model
//! "open a repo" use a fresh backend per run (cold libgit2 caches, warm OS
//! page cache), which is what a user opening a repo tab experiences.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::{Arc, mpsc};
use std::time::Instant;

use clap::Parser;
use fgc_bench::{BudgetFile, Sample, graph_tips, ms, time_runs};
use fgc_core::{DiffSpec, DiffTarget, Error, GitBackend, Oid, Result, StatusOpts};
use fgc_git_backend::Git2Backend;
use fgc_graph::{
    EngineOptions, GraphEngine, GraphEvent, GraphOrder, GraphSnapshot, LayoutBuilder, Phase,
    Topology,
};

#[derive(Clone, Copy, clap::ValueEnum)]
enum Order {
    Date,
    Topo,
}

impl From<Order> for GraphOrder {
    fn from(o: Order) -> Self {
        match o {
            Order::Date => GraphOrder::Date,
            Order::Topo => GraphOrder::Topo,
        }
    }
}

#[derive(Parser)]
#[command(about = "Time the fast git client's core read path against a repository")]
struct Args {
    /// Repository to probe (any path inside it).
    #[arg(default_value = ".")]
    repo: PathBuf,
    /// Runs per measurement.
    #[arg(long, default_value_t = 7)]
    iters: usize,
    /// Commits per page.
    #[arg(long, default_value_t = 500)]
    page: usize,
    /// Pages to walk for the paging measurement.
    #[arg(long, default_value_t = 20)]
    pages: usize,
    /// Also walk the entire history once and report it.
    #[arg(long)]
    full: bool,
    /// Budget file to gate against (requires --profile).
    #[arg(long, requires = "profile")]
    budgets: Option<PathBuf>,
    /// Profile name inside the budget file.
    #[arg(long)]
    profile: Option<String>,
    /// Paths for the targeted-status measurement (what the FS watcher does
    /// after a save, plan §5). Repeatable.
    #[arg(long = "status-path")]
    status_paths: Vec<PathBuf>,
    /// Skip the commit-graph engine measurements.
    #[arg(long)]
    no_graph: bool,
    /// Row order for the graph measurements.
    #[arg(long, value_enum, default_value_t = Order::Date)]
    order: Order,
    /// Visible rows assumed for first-screen and per-frame measurements.
    #[arg(long, default_value_t = 60)]
    visible_rows: u32,
    /// Write a JSON report here.
    #[arg(long)]
    json: Option<PathBuf>,
    /// Print tracing spans for every backend call (stderr).
    #[arg(long)]
    trace: bool,
}

fn main() -> ExitCode {
    let args = Args::parse();
    if args.trace {
        tracing_subscriber::fmt()
            .with_env_filter("fgc=debug,fgc_git_backend=debug")
            .with_span_events(tracing_subscriber::fmt::format::FmtSpan::CLOSE)
            .with_writer(std::io::stderr)
            .init();
    }
    match run(&args) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("fgc-probe: {e}");
            ExitCode::from(2)
        }
    }
}

fn run(args: &Args) -> Result<ExitCode> {
    let iters = args.iters;
    let page = args.page.max(1);
    let mut samples: Vec<Sample> = Vec::new();

    let probe = Git2Backend::open(&args.repo)?;
    let info = probe.repo_info()?;
    let tips = graph_tips(&probe)?;
    eprintln!(
        "repo: {}  ({}, {} tips, head {})",
        info.git_dir.display(),
        if info.is_bare { "bare" } else { "worktree" },
        tips.len(),
        info.head
            .map(|o| o.short(10))
            .unwrap_or_else(|| "unborn".into())
    );

    let (runs, _) = time_runs(iters, || Git2Backend::open(&args.repo))?;
    samples.push(sample("open", runs, ""));

    let (runs, refs) = time_runs(iters, || {
        let b = Git2Backend::open(&args.repo)?;
        b.refs()
    })?;
    samples.push(sample("open+refs", runs, format!("{} refs", refs.len())));

    // Backend portion of "open repo → first commits visible" (budget 400 ms
    // end to end; M1 asks for the walk itself < 100 ms on linux.git).
    let mut walk_only = Vec::new();
    let (runs, first) = time_runs(iters, || {
        let b = Git2Backend::open(&args.repo)?;
        let tips = graph_tips(&b)?;
        let t = Instant::now();
        let commits = b.walk_commits(&tips, 0..page)?;
        walk_only.push(ms(t.elapsed()));
        Ok(commits)
    })?;
    samples.push(sample(
        "open_to_first_page",
        runs,
        format!("{} commits", first.len()),
    ));
    samples.push(sample(
        "walk_first_page",
        walk_only,
        format!("{} commits", first.len()),
    ));

    // Sequential paging on one backend: cost should be per page, not offset.
    {
        let b = Git2Backend::open(&args.repo)?;
        let mut runs = Vec::new();
        let mut total = 0;
        for p in 0..args.pages {
            let t = Instant::now();
            let got = b.walk_commits(&tips, p * page..(p + 1) * page)?;
            let elapsed = ms(t.elapsed());
            total += got.len();
            if p > 0 {
                runs.push(elapsed);
            }
            if got.len() < page {
                break;
            }
        }
        if !runs.is_empty() {
            samples.push(sample(
                "walk_next_page",
                runs,
                format!("{total} commits over {} pages", args.pages),
            ));
        }
        // Re-reading an already-walked page (scrolling back up).
        if total >= page {
            let (runs, _) = time_runs(iters, || b.walk_commits(&tips, 0..page))?;
            samples.push(sample("walk_cached_page", runs, ""));
        }
    }

    if args.full {
        let b = Git2Backend::open(&args.repo)?;
        let t = Instant::now();
        let mut n = 0;
        let chunk = 50_000;
        loop {
            let got = b.walk_commits(&tips, n..n + chunk)?;
            n += got.len();
            if got.len() < chunk {
                break;
            }
        }
        samples.push(sample(
            "walk_full",
            vec![ms(t.elapsed())],
            format!("{n} commits"),
        ));
    }

    if !args.no_graph && !tips.is_empty() {
        graph_measurements(args, &tips, &mut samples)?;
    }

    if !info.is_bare {
        let b = Git2Backend::open(&args.repo)?;
        let opts = StatusOpts::default();
        b.status(&opts)?; // warm the index / stat cache
        let (runs, st) = time_runs(iters, || b.status(&opts))?;
        samples.push(sample(
            "status",
            runs,
            format!("{} entries", st.entries.len()),
        ));

        if !args.status_paths.is_empty() {
            let targeted = StatusOpts {
                pathspec: args.status_paths.clone(),
                ..StatusOpts::default()
            };
            let (runs, st) = time_runs(iters, || b.status(&targeted))?;
            samples.push(sample(
                "status_targeted",
                runs,
                format!(
                    "{} paths, {} entries",
                    args.status_paths.len(),
                    st.entries.len()
                ),
            ));
        }

        let spec = DiffSpec::new(DiffTarget::IndexToWorkdir {
            include_untracked: false,
        });
        let (runs, d) = time_runs(iters, || b.diff(&spec))?;
        samples.push(sample(
            "diff_workdir",
            runs,
            format!("{} files, {} lines", d.files.len(), d.lines.len()),
        ));
    }

    if let Some(head) = info.head {
        let b = Git2Backend::open(&args.repo)?;
        let spec = DiffSpec::new(DiffTarget::Commit(head));
        // Partial clones (`--filter=blob:none`) have no blobs locally, so
        // content diffs can fail there; report instead of aborting.
        match time_runs(iters, || b.diff(&spec)) {
            Ok((runs, d)) => samples.push(sample(
                "diff_head_commit",
                runs,
                format!("{} files, {} lines", d.files.len(), d.lines.len()),
            )),
            Err(e) => eprintln!("skipping diff_head_commit: {e}"),
        }
    }

    // Report.
    let budgets = match (&args.budgets, &args.profile) {
        (Some(path), Some(profile)) => {
            let file = BudgetFile::load(path)?;
            let Some(b) = file.profiles.get(profile).cloned() else {
                eprintln!(
                    "fgc-probe: profile {profile:?} not found in {}",
                    path.display()
                );
                return Ok(ExitCode::from(2));
            };
            Some(b)
        }
        _ => None,
    };

    println!(
        "{:<22} {:>10} {:>10} {:>10} {:>10}  detail",
        "measurement", "median ms", "min", "max", "budget"
    );
    let mut failures = Vec::new();
    let mut report = Vec::new();
    for s in &samples {
        let median = s.median_ms();
        let budget = budgets.as_ref().and_then(|b| b.get(&s.name)).copied();
        let status = match budget {
            Some(limit) if median > limit => {
                failures.push(format!("{}: {median:.2} ms > {limit} ms", s.name));
                " FAIL"
            }
            Some(_) => "   ok",
            None => "",
        };
        println!(
            "{:<22} {:>10.2} {:>10.2} {:>10.2} {:>10}{status}  {}",
            s.name,
            median,
            s.min_ms(),
            s.max_ms(),
            budget.map(|b| format!("{b}")).unwrap_or_else(|| "-".into()),
            s.detail
        );
        report.push(serde_json::json!({
            "name": s.name,
            "median_ms": median,
            "min_ms": s.min_ms(),
            "max_ms": s.max_ms(),
            "runs_ms": s.runs_ms,
            "budget_ms": budget,
            "detail": s.detail,
        }));
    }
    if let Some(budgets) = &budgets {
        for name in budgets.keys() {
            if !samples.iter().any(|s| &s.name == name) {
                eprintln!("warning: budget {name:?} has no matching measurement");
            }
        }
    }

    if let Some(path) = &args.json {
        let doc = serde_json::json!({
            "repo": info.git_dir,
            "profile": args.profile,
            "iters": iters,
            "page": page,
            "measurements": report,
        });
        std::fs::write(path, serde_json::to_string_pretty(&doc).unwrap())?;
    }

    if failures.is_empty() {
        Ok(ExitCode::SUCCESS)
    } else {
        eprintln!("\nbudget failures:");
        for f in &failures {
            eprintln!("  {f}");
        }
        Ok(ExitCode::from(1))
    }
}

fn sample(name: &str, runs_ms: Vec<f64>, detail: impl Into<String>) -> Sample {
    Sample {
        name: name.to_owned(),
        runs_ms,
        detail: detail.into(),
    }
}

/// M2: commit DAG, ordering, lane layout and the background engine.
fn graph_measurements(args: &Args, tips: &[Oid], samples: &mut Vec<Sample>) -> Result<()> {
    let order: GraphOrder = args.order.into();
    let iters = args.iters.clamp(1, 5);
    let visible = args.visible_rows.max(1);

    // Open → first screen: what a user opening a repo tab waits for. Fresh
    // backend + engine each run; the engine never writes a commit-graph here
    // (the probe must not modify repositories).
    let mut first_screen = Vec::new();
    let mut full_topology = Vec::new();
    let mut detail = String::new();
    let mut full_detail = String::new();
    for _ in 0..iters {
        let start = Instant::now();
        let be: Arc<dyn GitBackend> = Arc::new(Git2Backend::open(&args.repo)?);
        let tips = graph_tips(&*be)?;
        let engine = GraphEngine::new(
            be.clone(),
            EngineOptions {
                order,
                write_commit_graph: false,
                ..Default::default()
            },
        );
        let (tx, rx) = mpsc::channel();
        let (_, handle) = engine.load(tips, move |ev| {
            let _ = tx.send(ev);
        });
        let mut drawn = false;
        let mut provisional: Option<Arc<GraphSnapshot>> = None;
        loop {
            let snap = match rx.recv() {
                Ok(GraphEvent::Snapshot(s)) => s,
                Ok(GraphEvent::Failed { error, .. }) => return Err(Error::Engine(error)),
                Err(_) => return Err(Error::Engine("graph engine stopped".into())),
            };
            let enough =
                snap.layout.rows_laid() >= visible.min(snap.layout.topology().len() as u32);
            if !drawn && enough {
                // What the UI does for its first frame: geometry + metadata
                // for the visible rows.
                let rows = snap.layout.rows(0..visible);
                let oids: Vec<Oid> = rows
                    .iter()
                    .map(|r| snap.layout.topology().oid(r.row))
                    .collect();
                let metas = be.commit_metas(&oids)?;
                first_screen.push(ms(start.elapsed()));
                detail = format!(
                    "{} rows + {} metas from {:?} snapshot",
                    rows.len(),
                    metas.len(),
                    snap.phase
                );
                drawn = true;
            }
            if snap.phase == Phase::Provisional {
                provisional = Some(snap);
                continue;
            }
            full_topology.push(ms(start.elapsed()));
            // Principle 5 (keep the user's place): the provisional rows must
            // be what the full graph shows, so nothing jumps.
            let matches = provisional.as_ref().map(|p| {
                let n = p.layout.rows_laid();
                let same_rows =
                    (0..n).all(|r| p.layout.topology().oid(r) == snap.layout.topology().oid(r));
                same_rows && p.layout.rows(0..n) == snap.layout.rows(0..n)
            });
            full_detail = format!(
                "{} commits (dag {:.0} ms, sort {:.0} ms){}",
                snap.layout.topology().len(),
                ms(snap.stats.dag),
                ms(snap.stats.sort),
                match matches {
                    Some(true) => "; provisional rows identical",
                    Some(false) => "; PROVISIONAL ROWS DIFFER",
                    None => "",
                }
            );
            break;
        }
        engine.cancel();
        let _ = handle.join();
    }
    samples.push(sample("open_to_first_screen", first_screen, detail));
    samples.push(sample("open_to_full_topology", full_topology, full_detail));

    // One full engine run to completion, for its phase timings.
    let complete = run_engine_to_completion(args, order)?;
    let st = &complete.stats;
    samples.push(sample(
        "engine_complete",
        vec![ms(st.total)],
        format!(
            "{} commits ({} via commit-graph, {} parsed), {} max lanes",
            st.commits, st.from_commit_graph, st.parsed, st.max_lanes
        ),
    ));

    // Pieces individually.
    let be = Git2Backend::open(&args.repo)?;
    let (runs, dag) = time_runs(iters, || be.commit_dag(tips))?;
    samples.push(sample(
        "commit_dag",
        runs,
        format!(
            "{} commits, {} from commit-graph, {} parsed",
            dag.len(),
            dag.stats.from_commit_graph,
            dag.stats.parsed
        ),
    ));
    let (runs, topo) = time_runs(iters, || Ok(Topology::from_dag(dag.clone(), order)))?;
    samples.push(sample("topo_sort", runs, format!("{:?} order", order)));
    let topo = Arc::new(topo);
    let (runs, layout) = time_runs(iters, || Ok(LayoutBuilder::new(topo.clone()).finish()))?;
    topo.build_index();
    samples.push(sample(
        "layout_full",
        runs,
        format!(
            "{} lanes max; memory: topology {:.1} MB + layout {:.1} MB",
            layout.max_lanes(),
            topo.heap_bytes() as f64 / 1e6,
            layout.heap_bytes() as f64 / 1e6
        ),
    ));

    // Per-frame cost: geometry for a visible window anywhere in the graph.
    let n = layout.rows_laid().max(1);
    let mut rng = 0x9e37_79b9_7f4a_7c15u64;
    let mut runs = Vec::new();
    for _ in 0..300 {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        let start = (rng % n as u64) as u32;
        let t = Instant::now();
        std::hint::black_box(layout.rows(start..start + visible));
        runs.push(ms(t.elapsed()));
    }
    samples.push(sample(
        "geometry_window",
        runs,
        format!("{visible} rows at 300 random offsets"),
    ));

    // You commit on top of the newest commit: relayout reusing the previous
    // layout. Simulated with a virtual commit added to the DAG (the probe
    // never writes to the repository). Reuse kicks in when new commits
    // extend the top row's lane; otherwise a full layout runs (layout_full).
    let mut grown = dag.clone();
    if let Some(top) = grown.oids.iter().position(|o| *o == topo.oid(0)) {
        let t_max = grown.commit_times.iter().copied().max().unwrap_or(0);
        grown.oids.push(Oid([0xfe; 20]));
        grown.commit_times.push(t_max + 1);
        grown.parents.push(top as u32);
        grown.parent_offsets.push(grown.parents.len() as u32);
        let new_topo = Arc::new(Topology::from_dag(grown, order));
        let mut reused = None;
        let (runs, _) = time_runs(iters, || {
            let l = LayoutBuilder::with_reuse(new_topo.clone(), &layout).finish();
            reused = l.reused_from();
            Ok(l)
        })?;
        samples.push(sample(
            "relayout_new_commit",
            runs,
            match reused {
                Some(r) => format!("converged at row {r}, rest reused"),
                None => "no convergence: full layout".into(),
            },
        ));
    }
    Ok(())
}

fn run_engine_to_completion(args: &Args, order: GraphOrder) -> Result<Arc<GraphSnapshot>> {
    let be: Arc<dyn GitBackend> = Arc::new(Git2Backend::open(&args.repo)?);
    let tips = graph_tips(&*be)?;
    let engine = GraphEngine::new(
        be,
        EngineOptions {
            order,
            write_commit_graph: false,
            ..Default::default()
        },
    );
    let (tx, rx) = mpsc::channel();
    let (_, handle) = engine.load(tips, move |ev| {
        let _ = tx.send(ev);
    });
    let mut result = None;
    for ev in rx {
        match ev {
            GraphEvent::Snapshot(s) if s.phase == Phase::Complete => result = Some(s),
            GraphEvent::Snapshot(_) => {}
            GraphEvent::Failed { error, .. } => return Err(Error::Engine(error)),
        }
    }
    let _ = handle.join();
    result.ok_or_else(|| Error::Engine("graph engine finished without a result".into()))
}
