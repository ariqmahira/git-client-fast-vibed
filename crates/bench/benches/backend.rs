//! Criterion benches for every `GitBackend` read operation.
//!
//! Fixtures are generated into a temp dir at startup (a few seconds). Set
//! `FGC_BENCH_REPO=/path/to/repo` to also bench against a real repository
//! (e.g. linux.git from `scripts/fetch-bench-repos.sh`).

use std::path::{Path, PathBuf};

use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use fgc_bench::graph_tips;
use fgc_core::{DiffSpec, DiffTarget, GitBackend, StatusOpts};
use fgc_fixtures::{SyntheticSpec, small_repo, synthetic_history, wide_tree};
use fgc_git_backend::Git2Backend;

struct Fixtures {
    _tmp: tempfile::TempDir,
    small: PathBuf,
    history: PathBuf,
    wide: PathBuf,
}

fn fixtures() -> Fixtures {
    let tmp = tempfile::tempdir().unwrap();
    let small = tmp.path().join("small");
    std::fs::create_dir_all(&small).unwrap();
    small_repo(&small);
    let history = tmp.path().join("history");
    synthetic_history(
        &history,
        &SyntheticSpec {
            commits: 30_000,
            ..Default::default()
        },
    )
    .unwrap();
    let wide = tmp.path().join("wide");
    wide_tree(&wide, 20_000).unwrap();
    // One dirty file so status has something to report.
    std::fs::write(wide.join("pkg0000/file0000.txt"), "changed\n").unwrap();
    Fixtures {
        _tmp: tmp,
        small,
        history,
        wide,
    }
}

fn bench_walk(c: &mut Criterion, name: &str, repo: &Path) {
    let mut g = c.benchmark_group(format!("walk/{name}"));
    // Fresh backend per iteration: no cached walk state or libgit2 object cache.
    g.bench_function("first_500", |b| {
        b.iter_batched(
            || {
                let be = Git2Backend::open(repo).unwrap();
                let tips = graph_tips(&be).unwrap();
                (be, tips)
            },
            |(be, tips)| be.walk_commits(&tips, 0..500).unwrap(),
            BatchSize::PerIteration,
        )
    });
    g.bench_function("next_500_after_5000", |b| {
        b.iter_batched(
            || {
                let be = Git2Backend::open(repo).unwrap();
                let tips = graph_tips(&be).unwrap();
                be.walk_commits(&tips, 0..5000).unwrap();
                (be, tips)
            },
            |(be, tips)| be.walk_commits(&tips, 5000..5500).unwrap(),
            BatchSize::PerIteration,
        )
    });
    g.bench_function("open_refs", |b| {
        b.iter(|| Git2Backend::open(repo).unwrap().refs().unwrap())
    });
    g.finish();
}

fn bench_status(c: &mut Criterion, name: &str, repo: &Path) {
    let be = Git2Backend::open(repo).unwrap();
    let opts = StatusOpts::default();
    be.status(&opts).unwrap();
    c.bench_function(&format!("status/{name}/full"), |b| {
        b.iter(|| be.status(&opts).unwrap())
    });
}

fn bench_diff(c: &mut Criterion, name: &str, repo: &Path) {
    let be = Git2Backend::open(repo).unwrap();
    let head = be.repo_info().unwrap().head.unwrap();
    let commit = DiffSpec::new(DiffTarget::Commit(head));
    c.bench_function(&format!("diff/{name}/head_commit"), |b| {
        b.iter(|| be.diff(&commit).unwrap())
    });
    let wd = DiffSpec::new(DiffTarget::IndexToWorkdir {
        include_untracked: true,
    });
    c.bench_function(&format!("diff/{name}/workdir"), |b| {
        b.iter(|| be.diff(&wd).unwrap())
    });
}

fn benches(c: &mut Criterion) {
    let fx = fixtures();
    bench_walk(c, "history-30k", &fx.history);
    bench_status(c, "small", &fx.small);
    bench_status(c, "wide-20k", &fx.wide);
    bench_diff(c, "small", &fx.small);

    if let Ok(path) = std::env::var("FGC_BENCH_REPO") {
        let path = PathBuf::from(path);
        bench_walk(c, "external", &path);
        let be = Git2Backend::open(&path).unwrap();
        if !be.repo_info().unwrap().is_bare {
            bench_status(c, "external", &path);
        }
    }
}

criterion_group! {
    name = backend;
    config = Criterion::default().sample_size(20);
    targets = benches
}
criterion_main!(backend);
