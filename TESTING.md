# Testing fgc

fgc is an early build of a fast git client. It covers browsing, the working tree, commits, branches, stash, and fetch/pull/push. This page says how to get it, what to try, and what isn't there yet.

> fgc runs real git operations on the repository you open. Try it on a clone you don't mind experimenting with first.

## Get it

**Prebuilt:** download the archive for your platform from the repository's **Releases** page (GitHub → Releases): `fgc-linux-x86_64.tar.gz`, `fgc-macos-arm64.tar.gz` or `fgc-windows-x86_64.zip`, and unpack it. The macOS binary is unsigned; right-click → Open the first time.

**From source** (Rust stable; on Linux first install `libxkbcommon-dev libxkbcommon-x11-dev libwayland-dev libvulkan-dev libx11-xcb-dev`):

```sh
cargo build --release -p fgc-ui      # binary: target/release/fgc
```

The Linux build runs on glibc 2.35+ (Ubuntu 22.04, Debian 12 and newer). It needs a Vulkan driver to run (any desktop with GPU drivers has one). `git` must be on your `PATH` (it's used for fetch/pull/push and for commits when you have hooks or signing).

## Run it

```sh
fgc path/to/repo     # or run `fgc` inside a repository
```

Outside a repository a small window offers **Open repository…**; **Open…** in the toolbar opens more repositories in new windows.

## What to try

**Browsing**
- [ ] Open a big repository (the bigger the better). The first commits should appear almost instantly; the full history loads in the background.
- [ ] Scroll the graph fast, drag the scrollbar, use ↑/↓ (or j/k), PgUp/PgDn, Home/End.
- [ ] Select commits: details, changed files and the diff appear on the right. `[` and `]` jump between files.

**Working tree** (button in the header, or `w`)
- [ ] Edit files in your editor; the list updates by itself within a fraction of a second.
- [ ] Stage / unstage a file, a single hunk, or everything (Stage all / Unstage all).
- [ ] Write a message and commit (button or ctrl/cmd-enter). Try **Amend last commit**.
- [ ] Commit in a repository with a pre-commit hook or commit signing: fgc runs `git commit` then, so hooks and signing behave exactly as in the terminal.

**Branches** (select a commit)
- [ ] New branch here… → name → enter (creates and checks it out).
- [ ] Check out a local branch; check out a remote branch (creates a tracking branch).
- [ ] Delete a branch (click twice to confirm; unmerged branches ask once more).
- [ ] Try checking out with conflicting local changes: fgc refuses and says why.

**Stash and remotes** (toolbar)
- [ ] Stash (includes untracked files) and Pop.
- [ ] Fetch, Pull (fast-forward only), Push (sets the upstream on a branch's first push). Progress shows in the bar at the bottom.

**Performance**
- [ ] ctrl/cmd-shift-d shows the debug overlay: frame times, input latency, memory, recent git calls with timings. A screenshot of it is the most useful thing you can attach to a "feels slow" report.
- [ ] `fgc path/to/repo --bench` runs a scripted scroll/jump/typing benchmark in the real window and prints the numbers.

The bottom bar shows the equivalent git command for every action, and errors with git's own reason.

## Keys

| Key | Action |
|---|---|
| ↑ ↓ / j k, PgUp PgDn, Home End | move through commits |
| `w` | working tree |
| `[` `]` | previous / next file |
| ctrl/cmd-enter | commit |
| ctrl/cmd-shift-f / -l / -p | fetch / pull / push |
| ctrl/cmd-o | open another repository |
| ctrl/cmd-shift-d | debug overlay |
| ctrl/cmd-q | quit |

## Not there yet

- Discarding changes, merge/rebase/cherry-pick UI, conflict resolution, line-level staging.
- Syntax highlighting, side-by-side diffs, blame, file history, search.
- Tabs (each repository opens its own window), settings, themes, command palette.
- Credential prompts: fetch/pull/push use your credential helper, SSH agent or stored credentials, and never prompt. If a remote needs a password you haven't stored, run the command once in a terminal, then retry in fgc.
- Very wide histories (e.g. linux.git) draw the first 14 lanes of the graph.

## Reporting

Please include: OS and GPU, repository size (`git rev-list --count --all`), what you did, what happened, and a debug overlay screenshot for anything slow. Logs: `RUST_LOG=info fgc path/to/repo`.
