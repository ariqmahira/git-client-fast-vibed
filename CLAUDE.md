# Fast Git Client

The full plan is in `docs/PLAN.md`. Read §1 (budgets), §5 (architecture) and §7 (milestones) before starting work.

## Rules
- The UI thread never touches Git. Never call a backend from a GPUI render or event handler; send a command to the `RepoActor`.
- Every new Git operation gets: a `GitBackend` trait method (`crates/core/src/backend.rs`), a git2 impl (`crates/git-backend`), a criterion bench (`crates/bench/benches/backend.rs`), a probe measurement if it's on a budgeted path, and a test against a fixture repo (`fgc-fixtures`, built with the `git` CLI).
- Engine types (`git2::*`, `gix::*`) never appear in `fgc-core`'s public API.
- Prefer `-z` / porcelain v2 output when spawning git. Spawn git only for network ops and edge cases, never on interactive paths.
- Add a `tracing` span around every backend call.
- A milestone is done when its budgets are gated in `budgets.toml` and pass in CI.
- Graph rows the UI shows come from `GraphSnapshot`s published by `fgc_graph::GraphEngine`; the UI never sorts or lays out commits itself.

## Commands
- `cargo test --workspace` · `cargo clippy --workspace --all-targets -- -D warnings` · `cargo fmt --all`
- `cargo run --release -p fgc-fixtures -- bench-repos` generates small / history-200k / wide-100k (`huge-1m3` on request).
- `scripts/fetch-bench-repos.sh --commit-graph` clones linux.git (bare, blob-less) into `bench-repos/` and writes its commit-graph.
- `cargo run --release -p fgc-bench --bin fgc-probe -- <repo> [--budgets budgets.toml --profile <name>] [--trace]`
- `cargo bench -p fgc-bench --bench backend` (set `FGC_BENCH_REPO` to add a real repo).
- `cargo run --release -p fgc-ui -- <repo>` runs the app. `scripts/ui-headless.sh ./target/release/fgc <repo> --bench --budgets budgets.toml --profile <name>` runs the UI benchmark headlessly (Xvfb + lavapipe). `FGC_BENCH_MEM=1` prints memory per phase.

## UI notes
- GPUI is pinned to `=0.2.2`. `third_party/xattr-0.2.3` patches a transitive dependency that doesn't build with current libc.
- Every list is a `uniform_list`. Row processors can run more than once per frame, so per-frame state is rotated at the start of `RepoView::render`.
- Keep per-frame work bounded: geometry via `Layout::rows_clipped`, text revealed at most `TEXT_REVEAL_PER_FRAME` rows per frame, metadata requested in batches around the viewport.
- After a UI change, look at it: take a screenshot under Xvfb (`import -window root`), don't rely on the benchmark alone.
