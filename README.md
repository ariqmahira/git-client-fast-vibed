# fgc: a fast git client

A GitKraken replacement that **is** fast and **feels** fast: Rust core, libgit2 as the primary engine, gitoxide on the hottest read paths, the git CLI for network ops, and GPUI for rendering. See [`docs/PLAN.md`](docs/PLAN.md).

## Status

| Milestone | State |
|---|---|
| M0 Workspace + bench harness | Done: workspace, scripted fixture repos, criterion benches, `fgc-probe` budget gate, frame-time recorder, CI |
| M1 Core read path | Done: `GitBackend` + git2 impl (refs, paged commit walk, status, diff); linux.git first 500 commits in ~15 ms (budget 100 ms) |
| M2 Graph engine | Done: commit-graph reader, date/topo ordering, chunked incremental lane layout with reuse, background `GraphEngine`; linux.git first screen in ~24 ms, full 1.5M-commit layout in ~0.5 s in the background |
| M3 GPUI shell | Next |
| M4–M8 | Not started |

## Layout

| Crate | Purpose |
|---|---|
| `crates/core` (`fgc-core`) | UI- and engine-agnostic data model, the `GitBackend` trait, perf budgets and `FrameRecorder` |
| `crates/git-backend` (`fgc-git-backend`) | libgit2 implementation; one `Repository` per worker thread; resumable commit walk; own commit-graph reader for full-history DAGs |
| `crates/graph` (`fgc-graph`) | Row ordering, chunked lane layout, background `GraphEngine` with progressive snapshots |
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

## Measured (4-core Linux container, release build, median of 5–7)

linux.git: 1,508,433 commits, 955 refs, commit-graph file present (bare, blob-less clone).

| Measurement | linux.git | history-200k | wide-100k | Budget |
|---|---|---|---|---|
| open + refs | 6 ms | 0.5 ms | 0.1 ms | |
| walk first 500 commits | 14 ms | 3.2 ms | | 100 ms (M1) |
| **open → first screen** (graph + metadata for 60 rows) | **24 ms** | 5 ms | | 400 ms (M2) |
| open → full topology (background) | 340 ms | 31 ms | | |
| full layout complete (background) | 505 ms | 45 ms | | |
| commit DAG from commit-graph | 97 ms | 8 ms | | |
| topological sort (date order) | 172 ms | 8 ms | | |
| lane layout, all rows | 19 ms | 2 ms | | |
| geometry for a 60-row window (per frame) | 0.16 ms | 0.03 ms | | 8.3 ms frame |
| relayout after a new commit | 3.5 ms | 0.3 ms | | |
| status, targeted (1 path) | | | 2.6 ms | 150 ms |
| status, full | | 3.6 ms | 202 ms | (safety net) |

Graph memory on linux.git: topology 49 MB + layout 18 MB.

### How the first screen is fast

The engine publishes a provisional snapshot built from the first page of the time-ordered walk (~25 ms on any repo size). Parents outside the page become placeholder rows, so lanes run off the bottom exactly as they will in the full graph. In date order these rows are the same as the final layout's first rows (`fgc-probe` checks this on every repo: "provisional rows identical"), so nothing moves when the full topology lands a few hundred ms later.

Known gaps, tracked for later milestones:
- **Graph width.** linux.git is 3,802 lanes wide in date order and 1,680 in topo order. These are real concurrent edges (long-lived merges from old bases). Rendering only touches the visible window, so it stays cheap, but a UX policy for very long edges (e.g. gitk-style arrow stubs) is worth adding in M3.
- **Topo order** has no provisional screen (the time walk can't predict it), so its first screen waits for the full topology: ~340 ms on linux.git.
- **No commit-graph file:** the engine shows the provisional screen, then runs `git commit-graph write --split` in the background (21 s on linux.git the first time) before loading the full graph. With writing disabled it parses every commit with libgit2.
- **Layout reuse** applies when new commits extend the top row's lane (e.g. you commit on the newest branch). Otherwise lanes shift and a full relayout runs, which costs ~20 ms on linux.git.
- **Full status** with libgit2 is ~2× slower than `git status` on 100k files (202 vs ~87 ms). The save path uses targeted status (M5); M7 benchmarks gix.
- **The graph cache is in memory only.** A full rebuild is ~0.5 s on linux.git, so a persistent cache isn't needed yet.
