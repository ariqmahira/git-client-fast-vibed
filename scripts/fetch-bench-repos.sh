#!/usr/bin/env bash
# Fetches real-world benchmark repositories into bench-repos/.
#
#   scripts/fetch-bench-repos.sh            # linux.git (bare, blob-less: ~2 GiB)
#   scripts/fetch-bench-repos.sh --commit-graph   # also write a commit-graph file
#
# Synthetic repos (small, history-200k, wide-100k, huge-1m3) come from
# `cargo run --release -p fgc-fixtures -- bench-repos [profiles...]`.
#
# The clone is bare and blob-less: commit/tree history is complete (what the
# graph and revwalk need) without downloading every file version. Diffs that
# need file contents are skipped by fgc-probe on this repo.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
dest="$root/bench-repos/linux.git"
url="${LINUX_GIT_URL:-https://github.com/torvalds/linux.git}"
commit_graph=0
for arg in "$@"; do
  case "$arg" in
    --commit-graph) commit_graph=1 ;;
    *) echo "unknown argument: $arg" >&2; exit 2 ;;
  esac
done

mkdir -p "$root/bench-repos"
if [ -d "$dest" ]; then
  echo "updating $dest"
  git -C "$dest" fetch --filter=blob:none origin '+refs/heads/*:refs/heads/*' --prune
else
  echo "cloning $url into $dest (bare, blob-less)"
  git clone --bare --filter=blob:none "$url" "$dest"
fi

if [ "$commit_graph" = 1 ]; then
  # The graph engine (M2) reads full history from this file; without it the
  # client parses every commit (and writes the file itself in the background).
  echo "writing commit-graph"
  git -C "$dest" commit-graph write --reachable --split
fi

echo "commits: $(git -C "$dest" rev-list --count HEAD)"
