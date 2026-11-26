# fgc: a fast git client

A GitKraken replacement that **is** fast and **feels** fast: Rust core, libgit2 as the primary engine, gitoxide on the hottest read paths, the git CLI for network ops, and GPUI for rendering. See [`docs/PLAN.md`](docs/PLAN.md).

## Status

| Milestone | State |
|---|---|
| M0 Workspace + bench harness | Done: workspace, scripted fixture repos, criterion benches, `fgc-probe` budget gate, frame-time recorder, CI |
| M1 Core read path | Done: `GitBackend` + git2 impl (refs, paged commit walk, status, diff); linux.git first 500 commits in ~15 ms (budget 100 ms) |
| M2 Graph engine | Next |
| M3–M8 | Not started |

## Layout

| Crate | Purpose |
|---|---|
| `crates/core` (`fgc-core`) | UI- and engine-agnostic data model, the `GitBackend` trait, perf budgets and `FrameRecorder` |
| `crates/git-backend` (`fgc-git-backend`) | libgit2 implementation; one `Repository` per worker thread; resumable commit walk |
| `crates/graph` (`fgc-graph`) | Lane layout (M2, stub) |
| `crates/ui` (`fgc-ui`) | GPUI shell (M3, stub) |
| `crates/fixtures` (`fgc-fixtures`) | Fixture/benchmark repos built with the git CLI (`fast-import` for large histories) |
| `crates/bench` (`fgc-bench`) | Criterion benches and the `fgc-probe` CLI |

## Running

```sh
cargo test --workspace

# Benchmark repos
cargo run --release -p fgc-fixtures -- bench-repos          # small, history-200k, wide-100k
scripts/fetch-bench-repos.sh                                # linux.git (bare, blob-less, ~2 GiB)

# Timings, optionally gated against budgets.toml
cargo run --release -p fgc-bench --bin fgc-probe -- bench-repos/linux.git \
    --budgets budgets.toml --profile linux
cargo run --release -p fgc-bench --bin fgc-probe -- . --trace   # per-call tracing spans

cargo bench -p fgc-bench --bench backend
```

## Measured (4-core Linux container, release build, median of 7)

linux.git: 1,508,433 commits, 955 refs, no commit-graph file.

| Measurement | linux.git | history-200k | wide-100k | Budget |
|---|---|---|---|---|
| open + refs | 8.9 ms | 0.5 ms | 0.1 ms | |
| open → first 500 commits | 24 ms | 3.8 ms | | 400 ms |
| walk first 500 commits | 14.9 ms | 3.2 ms | | 100 ms (M1) |
| next page of 500 | 12.3 ms | 3.5 ms | | |
| re-read cached page | 1.8 ms | 0.1 ms | | |
| status, targeted (1 path) | | | 2.6 ms | 150 ms |
| status, full | | 3.6 ms | 202 ms | (safety net) |

Known gaps, tracked for later milestones:
- **Full history walk** of linux.git takes ~51 s (git rev-list: ~14 s). Fine for paging, too slow for M2's background layout. Next step: read the commit-graph file (or use gix) for parents and generation numbers without parsing commits.
- **Full status** with libgit2 is ~2× slower than `git status` on 100k files (202 vs ~87 ms). The save path uses targeted status (M5); M7 benchmarks gix.
- The walk is commit-time ordered (children before parents along each discovery path). Strict topological order under clock skew is M2's job.
