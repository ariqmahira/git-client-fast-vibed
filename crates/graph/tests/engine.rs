use std::ops::Range;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use fgc_core::{
    CommitDag, CommitMeta, Diff, DiffSpec, GitBackend, Oid, RefInfo, RefKind, RepoInfo, Result,
    StatusOpts, StatusSnapshot,
};
use fgc_fixtures::{SyntheticSpec, git, synthetic_history};
use fgc_git_backend::Git2Backend;
use fgc_graph::{
    EngineOptions, GraphEngine, GraphEvent, GraphOrder, GraphSnapshot, LayoutBuilder, Phase,
    Topology,
};

fn repo(commits: u64, commit_graph: bool) -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("r");
    synthetic_history(
        &dir,
        &SyntheticSpec {
            commits,
            commit_graph,
            ..Default::default()
        },
    )
    .unwrap();
    (tmp, dir)
}

fn tips(be: &dyn GitBackend) -> Vec<Oid> {
    be.refs()
        .unwrap()
        .iter()
        .filter(|r| r.kind == RefKind::LocalBranch)
        .map(|r| r.target)
        .collect()
}

fn opts() -> EngineOptions {
    EngineOptions {
        first_screen_rows: 200,
        publish_every_rows: 1_000,
        ..Default::default()
    }
}

fn collect(rx: &Receiver<GraphEvent>) -> Vec<Arc<GraphSnapshot>> {
    let mut out = Vec::new();
    while let Ok(ev) = rx.recv_timeout(Duration::from_secs(60)) {
        match ev {
            GraphEvent::Snapshot(s) => {
                let done = s.phase == Phase::Complete;
                out.push(s);
                if done {
                    break;
                }
            }
            GraphEvent::Failed { error, .. } => panic!("load failed: {error}"),
        }
    }
    out
}

fn load(engine: &GraphEngine, tips: Vec<Oid>) -> Vec<Arc<GraphSnapshot>> {
    let (tx, rx) = mpsc::channel();
    let (_, handle) = engine.load(tips, move |ev| {
        let _ = tx.send(ev);
    });
    let snaps = collect(&rx);
    handle.join().unwrap();
    snaps
}

#[test]
fn phases_and_provisional_rows_match_final() {
    let (_tmp, dir) = repo(8_000, true);
    let be: Arc<dyn GitBackend> = Arc::new(Git2Backend::open(&dir).unwrap());
    let engine = GraphEngine::new(be.clone(), opts());
    let snaps = load(&engine, tips(&*be));

    let phases: Vec<Phase> = snaps.iter().map(|s| s.phase).collect();
    assert_eq!(phases[0], Phase::Provisional);
    assert_eq!(*phases.last().unwrap(), Phase::Complete);
    assert!(
        phases.iter().filter(|p| **p == Phase::Partial).count() >= 5,
        "{phases:?}"
    );
    // Rows laid out only grow.
    let laid: Vec<u32> = snaps[1..].iter().map(|s| s.layout.rows_laid()).collect();
    assert!(laid.windows(2).all(|w| w[0] <= w[1]));

    let prov = &snaps[0].layout;
    let full = &snaps.last().unwrap().layout;
    assert!(full.is_complete());
    assert_eq!(full.topology().len(), 8_000);
    let n = prov.rows_laid();
    assert_eq!(n, 200);
    for r in 0..n {
        assert_eq!(prov.topology().oid(r), full.topology().oid(r));
    }
    assert_eq!(prov.rows(0..n), full.rows(0..n));

    // The complete layout equals a direct build from the backend's DAG.
    let dag = be.commit_dag(&tips(&*be)).unwrap();
    let direct = LayoutBuilder::new(Arc::new(Topology::from_dag(dag, GraphOrder::Date))).finish();
    assert_eq!(direct.rows(0..8_000), full.rows(0..8_000));

    let stats = &snaps.last().unwrap().stats;
    assert_eq!(stats.commits, 8_000);
    assert_eq!(stats.from_commit_graph, 8_000);
    assert!(stats.first_screen.is_some() && stats.provisional.is_some());
    // Selection lookup is ready once complete.
    let some = full.topology().oid(4_321);
    assert_eq!(full.topology().row_of(&some), Some(4_321));
}

#[test]
fn topo_order_skips_provisional_when_graph_exists() {
    let (_tmp, dir) = repo(2_000, true);
    let be: Arc<dyn GitBackend> = Arc::new(Git2Backend::open(&dir).unwrap());
    let engine = GraphEngine::new(
        be.clone(),
        EngineOptions {
            order: GraphOrder::Topo,
            ..opts()
        },
    );
    let snaps = load(&engine, tips(&*be));
    assert!(snaps.iter().all(|s| s.phase != Phase::Provisional));
    assert_eq!(
        snaps.last().unwrap().layout.topology().order(),
        GraphOrder::Topo
    );
}

#[test]
fn without_commit_graph_writes_one_in_background() {
    let (_tmp, dir) = repo(3_000, false);
    let be: Arc<dyn GitBackend> = Arc::new(Git2Backend::open(&dir).unwrap());
    assert!(!be.repo_info().unwrap().has_commit_graph);
    let engine = GraphEngine::new(be.clone(), opts());
    let snaps = load(&engine, tips(&*be));
    assert_eq!(snaps[0].phase, Phase::Provisional);
    let stats = &snaps.last().unwrap().stats;
    assert!(stats.wrote_commit_graph);
    assert_eq!((stats.from_commit_graph, stats.parsed), (3_000, 0));
    assert!(be.repo_info().unwrap().has_commit_graph);
}

#[test]
fn without_commit_graph_and_writing_disabled_parses_commits() {
    let (_tmp, dir) = repo(1_000, false);
    let be: Arc<dyn GitBackend> = Arc::new(Git2Backend::open(&dir).unwrap());
    let engine = GraphEngine::new(
        be.clone(),
        EngineOptions {
            write_commit_graph: false,
            ..opts()
        },
    );
    let stats = load(&engine, tips(&*be)).last().unwrap().stats.clone();
    assert!(!stats.wrote_commit_graph);
    assert_eq!((stats.from_commit_graph, stats.parsed), (0, 1_000));
    assert!(!be.repo_info().unwrap().has_commit_graph);
}

#[test]
fn reload_after_new_commit_reuses_layout() {
    let (_tmp, dir) = repo(6_000, true);
    let be: Arc<dyn GitBackend> = Arc::new(Git2Backend::open(&dir).unwrap());
    let engine = GraphEngine::new(be.clone(), opts());
    let first = load(&engine, tips(&*be));
    let top = first.last().unwrap().layout.topology().oid(0);

    // Commit on top of the newest commit, on a new branch.
    let tree = git(&dir, &["rev-parse", &format!("{top}^{{tree}}")]);
    let new = fgc_fixtures::git_at(
        &dir,
        &["commit-tree", &tree, "-p", &top.to_string(), "-m", "on top"],
        Some(fgc_fixtures::BASE_TIME + 1_000_000_000),
    );
    git(&dir, &["branch", "on-top", &new]);

    let second = load(&engine, tips(&*be));
    let done = second.last().unwrap();
    assert_eq!(done.layout.topology().len(), 6_001);
    assert!(done.stats.reused_from_row.is_some(), "expected chunk reuse");
    let fresh = LayoutBuilder::new(done.layout.topology().clone()).finish();
    assert_eq!(fresh.rows(0..6_001), done.layout.rows(0..6_001));
}

#[test]
fn invalid_tip_reports_failure() {
    let (_tmp, dir) = repo(100, true);
    let be: Arc<dyn GitBackend> = Arc::new(Git2Backend::open(&dir).unwrap());
    let engine = GraphEngine::new(be, opts());
    let (tx, rx) = mpsc::channel();
    let (generation, handle) = engine.load(vec![Oid([9; 20])], move |ev| {
        let _ = tx.send(ev);
    });
    handle.join().unwrap();
    match rx.recv().unwrap() {
        GraphEvent::Failed { generation: g, .. } => assert_eq!(g, generation),
        other => panic!("expected failure, got {other:?}"),
    }
}

/// Delegates to a real backend but blocks `commit_dag` until released, so
/// cancellation can be tested deterministically.
struct Gated {
    inner: Git2Backend,
    gate: Arc<(Mutex<bool>, Condvar)>,
}

impl GitBackend for Gated {
    fn repo_info(&self) -> Result<RepoInfo> {
        self.inner.repo_info()
    }
    fn refs(&self) -> Result<Vec<RefInfo>> {
        self.inner.refs()
    }
    fn walk_commits(&self, tips: &[Oid], range: Range<usize>) -> Result<Vec<CommitMeta>> {
        self.inner.walk_commits(tips, range)
    }
    fn commit_metas(&self, oids: &[Oid]) -> Result<Vec<CommitMeta>> {
        self.inner.commit_metas(oids)
    }
    fn commit_dag(&self, tips: &[Oid]) -> Result<CommitDag> {
        let (lock, cv) = &*self.gate;
        let mut open = lock.lock().unwrap();
        while !*open {
            open = cv.wait(open).unwrap();
        }
        drop(open);
        self.inner.commit_dag(tips)
    }
    fn write_commit_graph(&self) -> Result<()> {
        self.inner.write_commit_graph()
    }
    fn status(&self, opts: &StatusOpts) -> Result<StatusSnapshot> {
        self.inner.status(opts)
    }
    fn diff(&self, spec: &DiffSpec) -> Result<Diff> {
        self.inner.diff(spec)
    }
}

#[test]
fn superseded_load_publishes_nothing_more() {
    let (_tmp, dir) = repo(2_000, true);
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let be = Arc::new(Gated {
        inner: Git2Backend::open(&dir).unwrap(),
        gate: gate.clone(),
    });
    let tips = tips(&be.inner);
    let engine = GraphEngine::new(be.clone(), opts());

    let (tx1, rx1) = mpsc::channel();
    let (g1, h1) = engine.load(tips.clone(), move |ev| {
        let _ = tx1.send(ev);
    });
    // Job 1 publishes its provisional screen, then blocks in commit_dag.
    match rx1.recv_timeout(Duration::from_secs(30)).unwrap() {
        GraphEvent::Snapshot(s) => assert_eq!((s.generation, s.phase), (g1, Phase::Provisional)),
        other => panic!("{other:?}"),
    }
    let (tx2, rx2) = mpsc::channel();
    let (g2, h2) = engine.load(tips, move |ev| {
        let _ = tx2.send(ev);
    });
    assert!(g2 > g1);
    *gate.0.lock().unwrap() = true;
    gate.1.notify_all();

    let snaps2 = collect(&rx2);
    h1.join().unwrap();
    h2.join().unwrap();
    assert!(snaps2.iter().all(|s| s.generation == g2));
    assert_eq!(snaps2.last().unwrap().phase, Phase::Complete);
    // Job 1 stopped at its next checkpoint without publishing anything else.
    assert!(rx1.try_recv().is_err());
}
