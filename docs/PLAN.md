# Fast Git Client: Build Plan

> Goal: a GitKraken replacement that **is** fast and **feels** fast and smooth.
> Stack: Rust core · libgit2 (git2-rs) as the primary engine · gitoxide (gix) on the hottest read paths · git CLI only for network ops and edge cases · GPUI (Zed's GPU UI framework) for rendering.
> Hard rule: **the UI thread never touches Git.**

---

## 1. Performance budgets (enforced in CI)

"Fast" is how long operations take. "Feels fast" means input is never blocked, something shows within one frame, and the user's place on screen never jumps.

| Metric | Budget | Benchmark repo / condition |
|---|---|---|
| Cold start to first frame | < 300 ms | Empty window |
| Open repo to first commits visible | < 400 ms | linux.git (~1.3M commits) |
| Scroll / resize frame time | < 8.3 ms (120 fps) | Commit graph, diff view |
| Input (key/click) to paint | < 16 ms | Any view |
| Working-tree status after a file save | < 150 ms | 100k-file repo, warm cache |
| Open a 5,000-line diff, highlighted | < 100 ms first screen | Any file |
| Idle memory | < 150 MB | linux.git open |

Principles:
1. UI thread = render + input only. Every Git call runs on a worker and returns an immutable snapshot.
2. First screenful first; load everything else progressively.
3. Optimistic UI for local actions (stage, unstage, checkout, branch create), with rollback if the operation fails.
4. Cache everything derivable: graph layout, diff stats, highlighted hunks, avatars.
5. Keep the user's place: scroll position and selection stay stable while data updates.

---

## 2. Landscape (what to learn from)

| Client | UI | Git engine | Lesson |
|---|---|---|---|
| Sublime Merge | Custom C++ toolkit, GPU rendering | Custom Git reader for reads; system `git` for writes | Own the read path, delegate writes to real Git. The benchmark to beat. Their team found `mmap` on object files risky; `pread` was ~2/3 the speed but safer. |
| GitKraken | Electron | libgit2 via NodeGit | Background ops + a smooth graph win on perceived speed despite Electron's cost. |
| GitButler | Tauri + Svelte | git2 + gix in Rust (gix on hot paths) | Mixing both libraries works in production. Tauri IPC serialization is the cost. |
| Fork | Native per OS | git CLI + own parsing | Native feel, but two codebases. |
| Zed (editor) | GPUI | n/a | Built "like a video game", feeding the GPU. GPUI hit 1.0 with Zed in Apr 2026 and is reusable (Apache-2.0). |

---

## 3. Git engine: hybrid behind one trait

```rust
trait GitBackend: Send + Sync {
    fn status(&self, opts: StatusOpts) -> Result<StatusSnapshot>;
    fn walk_commits(&self, tips: &[Oid], range: Range<usize>) -> Result<Vec<CommitMeta>>;
    fn diff(&self, spec: DiffSpec) -> Result<Diff>;
    fn stage(&self, paths: &[PathBuf], hunks: Option<&[HunkSel]>) -> Result<()>;
    fn commit(&self, msg: &str, amend: bool) -> Result<Oid>;
    fn fetch(&self, remote: &str, progress: ProgressTx) -> Result<()>;
    // ...
}
```
Each operation is routed to the best implementation, so you can swap one per method based on benchmarks.

| Layer | Use for | Why | Caveats |
|---|---|---|---|
| **libgit2 (git2-rs)** | revwalk, refs, index, staging (incl. hunk/line staging), commit, blame, diff, merge analysis, stash, cherry-pick | Mature, in-process, no spawn cost | Lags Git on partial clone, split index, SHA-256; fetch can be slow; blocking API; `Repository` is not `Sync`, so use one per worker thread |
| **gitoxide (gix)** | status/dirwalk, commit-graph traversal, pack/object reading, tree/blob diff | Pure Rust, parallel, mmap-heavy | Benchmarks are mixed per repo, so measure. Merge/rebase still maturing. |
| **git CLI** | fetch, push, clone, hooks, LFS, credential helpers, SSH config, sparse-checkout, interactive rebase | Exact parity with what users expect, including auth | ~5–20 ms spawn per call, never on interactive paths. Parse porcelain v2 / `-z` output. |

Extras:
- Read Git's `commit-graph` file (generation numbers + parents without inflating commits). If it's missing, run `git commit-graph write --reachable` in the background.
- Respect `core.fsmonitor` / use your own watcher (see §5).
- Show the equivalent git command for every action (trust-builder, like Sublime Merge).

---

## 4. UI stack decision

**Recommendation: GPUI** (via the `gpui` crate from Zed, or a community distribution such as gpui-ce, pinned to an exact version).

| Option | Feel | Dev speed | Notes |
|---|---|---|---|
| **GPUI** | Best: GPU-rendered, 120 fps, no IPC | Medium (pre-1.0 API, sparse docs; read Zed source) | Same language as the core, so no serialization boundary. Component kit: `gpui-component` (Longbridge). |
| Tauri + Svelte/Solid | Good if graph is drawn on `<canvas>`/WebGL | Fast | IPC JSON serialization becomes the bottleneck on big graphs/diffs; WebView differs per OS. |
| Slint / egui / iced | Good | Medium | Smaller ecosystems for text-heavy UIs (diffs, syntax highlighting). |
| Qt (cxx-qt) | Very good | Slow | Licensing + C++ build complexity. |

Fallback: if GPUI churn becomes painful, keep the Rust core identical and put a Tauri shell on top. Design the core as a UI-agnostic crate from day one.

---

## 5. Architecture

```
┌──────────────────────── UI thread (GPUI) ────────────────────────┐
│ Views: Graph · FileList · Diff · CommitPanel · CommandPalette     │
│ Reads Arc<Snapshot>; sends Commands; never blocks                 │
└───────────────▲───────────────────────────────┬──────────────────┘
                │ snapshots (Arc, versioned)    │ commands (channel)
┌───────────────┴───────────────────────────────▼──────────────────┐
│ RepoActor (one per open repo)                                     │
│  - owns state, dedupes/cancels stale requests (generation ids)    │
│  - schedules jobs by priority: Interactive > Visible > Background │
└──────┬──────────────┬───────────────┬───────────────┬────────────┘
       │              │               │               │
  Worker pool    Graph engine    Diff/highlight   FS watcher
  (git2 repo     (lane layout,   (tree-sitter,    (notify + debounce
   per thread)    chunked)        per-hunk cache)  → targeted status)
       │              │               │
       └──────── Persistent cache (redb/SQLite): layout, stats, highlights
```

Key mechanisms:
- **Snapshots, not locks.** The actor publishes `Arc<RepoSnapshot>` with a version number. Views diff against the previous version to keep scroll/selection stable.
- **Cancellation.** Every request carries a generation id; stale results are dropped (e.g. user clicks through 10 commits quickly, so only the last diff is computed fully).
- **Commit graph.**
  - Load commit metadata in pages (first ~500 commits immediately, rest streamed).
  - Lane assignment is incremental: process commits in topo order, keep active lane state, emit rows in chunks.
  - Cache the layout keyed by the set of ref tips; on new commits, recompute only the new prefix.
  - Render only visible rows (virtualized list); draw edges as GPU paths/quads per row.
- **Diff view.** Virtualized lines; syntax highlight via tree-sitter in the background, showing plain text first and swapping in highlighting without layout shift. Word-level diff computed on demand per visible hunk.
- **Status.** The FS watcher marks dirty paths → status runs only on those paths (`pathspec`), with a full status as a periodic safety net. Ignore `.git/` noise except `HEAD`, `index`, `refs/`.
- **Optimistic actions.** Apply to the snapshot immediately, mark as pending, reconcile on completion, and roll back with a toast on failure.
- **Network ops.** Spawn `git fetch/push` with `--progress`, stream progress to a non-modal indicator; UI stays fully usable.
- **Avatars/remote data.** Async, cached on disk, never on the critical path.

---

## 6. Feature scope

**MVP (daily-driver for you)**
- Open repo, tabs for multiple repos
- Commit graph with branches/tags/remotes, search (message, author, SHA)
- Working tree: file list, stage/unstage file, hunk, and line
- Diff view (side-by-side + unified), syntax highlighted
- Commit, amend, commit message editor
- Branch create/checkout/delete, stash push/pop
- Fetch, pull (ff / rebase), push with progress
- Command palette + full keyboard navigation
- Shows the equivalent git command for each action

**v1**
- Merge conflict resolver (3-way)
- Interactive rebase (drag-reorder, squash, fixup, reword)
- Cherry-pick, revert, reset (soft/mixed/hard) with undo
- Blame + file history
- Worktrees, submodules
- Undo stack via reflog

**Later**
- GitHub/GitLab PRs + CI status
- AI commit messages (optional, off by default)
- Large-repo modes: sparse checkout UI, partial clone

---

## 7. Roadmap with acceptance criteria

Each milestone ends with its benchmarks passing in CI.

| # | Milestone | Done when |
|---|---|---|
| M0 | Workspace + bench harness | Cargo workspace (`core`, `git-backend`, `graph`, `ui`, `bench`); benchmark repos scripted (small, linux.git, a 100k-file monorepo); criterion benches + a frame-time recorder in CI |
| M1 | Core read path | `GitBackend` with git2 impl; commit paging; status; diff. Headless CLI `fgc-probe` prints timings. linux.git first 500 commits < 100 ms. |
| M2 | Graph engine | Incremental lane layout + cache; 1.3M commits laid out in background without blocking; first screen < 400 ms |
| M3 | GPUI shell | Window, virtualized graph, file list, diff view; 120 fps scroll on linux.git; input-to-paint < 16 ms |
| M4 | Write path | Stage/unstage (file/hunk/line), commit, amend, branch, stash with optimistic UI and rollback |
| M5 | Watcher + live status | Save a file → status updates < 150 ms on 100k-file repo |
| M6 | Network | Fetch/pull/push via git CLI with streamed progress and credential helpers |
| M7 | gix experiments | Benchmark gix vs git2 for status, revwalk, diff per repo; switch the trait method only where gix wins by >20% |
| M8 | MVP polish | Command palette, keybindings, themes, settings; daily-driven for 2 weeks |

---

## 8. Risks & open questions

- **GPUI churn**: pre-1.0 API outside Zed. Mitigation: pin version, isolate UI crate, keep core UI-agnostic.
- **libgit2 gaps** (partial clone, SHA-256, some config/auth cases). Mitigation: CLI fallback per operation.
- **gix maturity varies by feature.** Mitigation: benchmark-gated adoption (M7).
- **Windows FS performance** (status, watcher) is usually the slowest platform; test there early.
- **Text rendering/IME** in a custom UI (commit message editor): verify CJK input and accessibility (screen readers) early.
- Open: target platforms for v1 (all three, or macOS/Linux first)? License (open source vs paid)? Name?

---

## 9. Notes for Claude Code

- Start at M0. Do not write UI before M1 benchmarks exist.
- Every new Git operation gets: a trait method, a git2 impl, a bench, and a test against a fixture repo (create fixtures with `git` in test setup).
- Never call Git from a GPUI render or event handler; send a command to the `RepoActor`.
- Prefer `-z` / porcelain v2 output when spawning git.
- Add timing spans (`tracing`) around every backend call; surface them in a debug overlay (toggle with a keybinding).
