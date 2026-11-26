use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use fgc_core::{
    Change, DiffSpec, DiffTarget, Error, FileChange, GitBackend, LineKind, Oid, RefKind, StatusOpts,
};
use fgc_fixtures::{SmallRepo, SyntheticSpec, git, small_repo, synthetic_history};
use fgc_git_backend::Git2Backend;

fn small() -> (tempfile::TempDir, SmallRepo, Git2Backend) {
    let tmp = tempfile::tempdir().unwrap();
    let repo = small_repo(tmp.path());
    let be = Git2Backend::open(tmp.path()).unwrap();
    (tmp, repo, be)
}

fn oid(s: &str) -> Oid {
    s.parse().unwrap()
}

fn rev_list(dir: &Path, args: &[&str]) -> Vec<Oid> {
    let mut a = vec!["rev-list"];
    a.extend_from_slice(args);
    git(dir, &a).lines().map(oid).collect()
}

#[test]
fn open_rejects_non_repo() {
    let tmp = tempfile::tempdir().unwrap();
    match Git2Backend::open(tmp.path()) {
        Err(Error::NotARepo(_)) => {}
        other => panic!("expected NotARepo, got {other:?}"),
    }
}

#[test]
fn open_discovers_from_subdirectory() {
    let (tmp, repo, _) = small();
    let be = Git2Backend::open(tmp.path().join("src")).unwrap();
    assert_eq!(be.repo_info().unwrap().head, Some(oid(&repo.head)));
}

#[test]
fn repo_info() {
    let (tmp, repo, be) = small();
    let info = be.repo_info().unwrap();
    assert_eq!(info.head, Some(oid(&repo.head)));
    assert_eq!(info.head_branch.as_deref(), Some("main"));
    assert!(!info.is_bare);
    assert!(!info.is_shallow);
    assert_eq!(
        info.workdir.unwrap().canonicalize().unwrap(),
        tmp.path().canonicalize().unwrap()
    );
}

#[test]
fn repo_info_unborn() {
    let tmp = tempfile::tempdir().unwrap();
    git(tmp.path(), &["init", "-q"]);
    let be = Git2Backend::open(tmp.path()).unwrap();
    let info = be.repo_info().unwrap();
    assert_eq!(info.head, None);
    assert_eq!(info.head_branch.as_deref(), Some("main"));
    assert!(be.refs().unwrap().is_empty());
    assert!(be.walk_commits(&[], 0..100).unwrap().is_empty());
}

#[test]
fn refs() {
    let (_tmp, repo, be) = small();
    let refs = be.refs().unwrap();
    let by_name: HashMap<&str, _> = refs.iter().map(|r| (&*r.name, r)).collect();

    let main = by_name["refs/heads/main"];
    assert_eq!(main.kind, RefKind::LocalBranch);
    assert_eq!(&*main.shorthand, "main");
    assert!(main.is_head);
    assert_eq!(main.target, oid(&repo.head));

    let feature = by_name["refs/heads/feature"];
    assert!(!feature.is_head);
    assert_eq!(feature.target, oid(&repo.commits[3]));

    // Lightweight and annotated tags both resolve to commits.
    assert_eq!(by_name["refs/tags/v0.1"].target, oid(&repo.commits[1]));
    assert_eq!(by_name["refs/tags/v1.0"].target, oid(&repo.head));
    assert_eq!(by_name["refs/tags/v1.0"].kind, RefKind::Tag);
    assert_eq!(refs.len(), 4);
}

#[test]
fn refs_skip_non_commit_tags() {
    let (tmp, _repo, be) = small();
    let tree = git(tmp.path(), &["rev-parse", "HEAD^{tree}"]);
    git(tmp.path(), &["tag", "tree-tag", &tree]);
    assert!(
        be.refs()
            .unwrap()
            .iter()
            .all(|r| &*r.name != "refs/tags/tree-tag")
    );
}

#[test]
fn walk_small_matches_git_date_order() {
    let (tmp, repo, be) = small();
    let commits = be.walk_commits(&[oid(&repo.head)], 0..100).unwrap();
    let ours: Vec<Oid> = commits.iter().map(|c| c.oid).collect();
    assert_eq!(ours, rev_list(tmp.path(), &["--date-order", "main"]));

    let merge = &commits[0];
    assert_eq!(&*merge.summary, "Merge branch 'feature'");
    assert!(merge.is_merge());
    assert_eq!(
        merge.parents,
        vec![oid(&repo.commits[4]), oid(&repo.commits[3])]
    );
    assert_eq!(&*merge.author_name, "Fixture Author");
    assert_eq!(&*merge.author_email, "author@example.com");
    assert_eq!(merge.author_tz_minutes, 0);
    assert_eq!(merge.commit_time, merge.author_time);
    let root = commits.last().unwrap();
    assert_eq!(root.oid, oid(&repo.commits[0]));
    assert!(root.parents.is_empty());
    assert_eq!(&*root.summary, "main: initial");
}

#[test]
fn walk_paging_is_consistent() {
    let (_tmp, repo, be) = small();
    let tips = [oid(&repo.head)];
    let all: Vec<Oid> = be
        .walk_commits(&tips, 0..100)
        .unwrap()
        .iter()
        .map(|c| c.oid)
        .collect();

    // A fresh backend paged in odd-sized steps yields the same sequence.
    let be2 = Git2Backend::open(&repo.path).unwrap();
    let mut paged = Vec::new();
    for r in [0..2, 2..3, 3..5, 5..50] {
        paged.extend(be2.walk_commits(&tips, r).unwrap().iter().map(|c| c.oid));
    }
    assert_eq!(paged, all);

    // Re-reading an already walked window, out of order.
    let mid: Vec<Oid> = be2
        .walk_commits(&tips, 1..4)
        .unwrap()
        .iter()
        .map(|c| c.oid)
        .collect();
    assert_eq!(mid, all[1..4]);
    // Past the end and empty ranges.
    assert!(be2.walk_commits(&tips, 10..20).unwrap().is_empty());
    assert!(be2.walk_commits(&tips, 3..3).unwrap().is_empty());
    // Tip order and duplicates don't matter.
    let feature = oid(&repo.commits[3]);
    let a: Vec<Oid> = be2
        .walk_commits(&[tips[0], feature], 0..100)
        .unwrap()
        .iter()
        .map(|c| c.oid)
        .collect();
    let b: Vec<Oid> = be2
        .walk_commits(&[feature, tips[0], feature], 0..100)
        .unwrap()
        .iter()
        .map(|c| c.oid)
        .collect();
    assert_eq!(a, b);
    assert_eq!(a, all);
}

#[test]
fn walk_rejects_non_commit_tip() {
    let (tmp, _repo, be) = small();
    let tree = oid(&git(tmp.path(), &["rev-parse", "HEAD^{tree}"]));
    assert!(matches!(
        be.walk_commits(&[tree], 0..10),
        Err(Error::InvalidInput(_))
    ));
}

#[test]
fn walk_synthetic_history() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("h");
    synthetic_history(
        &dir,
        &SyntheticSpec {
            commits: 3_000,
            ..Default::default()
        },
    )
    .unwrap();
    let be = Git2Backend::open(&dir).unwrap();
    let tips: Vec<Oid> = be
        .refs()
        .unwrap()
        .iter()
        .filter(|r| r.kind == RefKind::LocalBranch)
        .map(|r| r.target)
        .collect();
    let mut walked = Vec::new();
    let mut start = 0;
    loop {
        let page = be.walk_commits(&tips, start..start + 500).unwrap();
        start += page.len();
        let done = page.len() < 500;
        walked.extend(page);
        if done {
            break;
        }
    }
    assert_eq!(walked.len(), 3_000);
    // Fixture timestamps strictly increase, so time order is a valid
    // topological order: every parent comes after all of its children.
    let pos: HashMap<Oid, usize> = walked.iter().enumerate().map(|(i, c)| (c.oid, i)).collect();
    for (i, c) in walked.iter().enumerate() {
        for p in &c.parents {
            assert!(pos[p] > i, "parent {p} emitted before child {}", c.oid);
        }
    }
    let ours: Vec<Oid> = walked.iter().map(|c| c.oid).collect();
    assert_eq!(ours, rev_list(&dir, &["--date-order", "--branches"]));
}

#[test]
fn status_small() {
    let (_tmp, _repo, be) = small();
    let st = be.status(&StatusOpts::default()).unwrap();
    let by_path: HashMap<PathBuf, _> = st.entries.iter().map(|e| (e.path.clone(), e)).collect();
    assert_eq!(st.entries.len(), 4, "{:?}", st.entries);

    let readme = by_path[Path::new("README.md")];
    assert_eq!(
        (readme.index, readme.worktree),
        (Change::Modified, Change::Unmodified)
    );
    let guide = by_path[Path::new("docs/guide.md")];
    assert_eq!(
        (guide.index, guide.worktree),
        (Change::Added, Change::Unmodified)
    );
    let lib = by_path[Path::new("src/lib.rs")];
    assert_eq!(
        (lib.index, lib.worktree),
        (Change::Unmodified, Change::Modified)
    );
    let todo = by_path[Path::new("notes/todo.txt")];
    assert_eq!(todo.worktree, Change::Untracked);

    assert_eq!(st.staged().count(), 2);
    assert_eq!(st.unstaged().count(), 2);
    // Sorted by path.
    assert!(st.entries.windows(2).all(|w| w[0].path < w[1].path));
}

#[test]
fn status_options() {
    let (tmp, _repo, be) = small();
    let only_lib = StatusOpts {
        pathspec: vec!["src/lib.rs".into()],
        ..Default::default()
    };
    let st = be.status(&only_lib).unwrap();
    assert_eq!(st.entries.len(), 1);
    assert_eq!(st.entries[0].path, Path::new("src/lib.rs"));

    let no_untracked = StatusOpts {
        include_untracked: false,
        ..Default::default()
    };
    assert_eq!(be.status(&no_untracked).unwrap().entries.len(), 3);

    std::fs::write(tmp.path().join(".gitignore"), "*.log\n").unwrap();
    std::fs::write(tmp.path().join("debug.log"), "x").unwrap();
    let st = be.status(&StatusOpts::default()).unwrap();
    assert!(st.entries.iter().all(|e| e.path != Path::new("debug.log")));
    let st = be
        .status(&StatusOpts {
            include_ignored: true,
            ..Default::default()
        })
        .unwrap();
    let log = st
        .entries
        .iter()
        .find(|e| e.path == Path::new("debug.log"))
        .unwrap();
    assert_eq!(log.worktree, Change::Ignored);
}

#[test]
fn status_renames_and_conflicts() {
    let (tmp, _repo, be) = small();
    git(tmp.path(), &["mv", "main.txt", "renamed.txt"]);
    let st = be
        .status(&StatusOpts {
            detect_renames: true,
            ..Default::default()
        })
        .unwrap();
    let e = st
        .entries
        .iter()
        .find(|e| e.path == Path::new("renamed.txt"))
        .unwrap();
    assert_eq!(e.index, Change::Renamed);
    assert_eq!(e.orig_path.as_deref(), Some(Path::new("main.txt")));

    // Produce a merge conflict.
    let tmp2 = tempfile::tempdir().unwrap();
    let d = tmp2.path();
    git(d, &["init", "-q"]);
    std::fs::write(d.join("f.txt"), "base\n").unwrap();
    git(d, &["add", "."]);
    git(d, &["commit", "-qm", "base"]);
    git(d, &["checkout", "-qb", "other"]);
    std::fs::write(d.join("f.txt"), "other\n").unwrap();
    git(d, &["commit", "-qam", "other"]);
    git(d, &["checkout", "-q", "main"]);
    std::fs::write(d.join("f.txt"), "main\n").unwrap();
    git(d, &["commit", "-qam", "main"]);
    let out = fgc_fixtures::git_cmd(d)
        .args(["merge", "other"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let st = Git2Backend::open(d)
        .unwrap()
        .status(&StatusOpts::default())
        .unwrap();
    assert_eq!(st.entries.len(), 1);
    assert_eq!(st.entries[0].index, Change::Conflicted);
    drop(be);
}

#[test]
fn status_bare_is_empty() {
    let tmp = tempfile::tempdir().unwrap();
    git(tmp.path(), &["init", "-q", "--bare"]);
    let be = Git2Backend::open(tmp.path()).unwrap();
    assert!(be.repo_info().unwrap().is_bare);
    assert!(be.status(&StatusOpts::default()).unwrap().is_clean());
}

#[test]
fn diff_head_to_index() {
    let (_tmp, _repo, be) = small();
    let d = be.diff(&DiffSpec::new(DiffTarget::HeadToIndex)).unwrap();
    let paths: Vec<_> = d.files.iter().map(|f| f.path().unwrap().clone()).collect();
    assert_eq!(
        paths,
        vec![PathBuf::from("README.md"), PathBuf::from("docs/guide.md")]
    );

    let readme = &d.files[0];
    assert_eq!(readme.change, FileChange::Modified);
    assert_eq!((readme.additions, readme.deletions), (1, 1));
    let hunks = d.file_hunks(readme);
    assert_eq!(hunks.len(), 1);
    let lines = d.hunk_lines(&hunks[0]);
    let del = lines.iter().find(|l| l.kind == LineKind::Deletion).unwrap();
    let add = lines.iter().find(|l| l.kind == LineKind::Addition).unwrap();
    assert_eq!(d.line_text(del), "A small repo.");
    assert_eq!(d.line_text(add), "A small repo, edited.");
    assert_eq!(del.old_lineno, Some(3));
    assert_eq!(add.new_lineno, Some(3));
    assert!(hunks[0].header.starts_with("@@ -1,3 +1,3 @@"));

    let guide = &d.files[1];
    assert_eq!(guide.change, FileChange::Added);
    assert_eq!(guide.old_path, None);
    assert_eq!((guide.additions, guide.deletions), (1, 0));
}

#[test]
fn diff_index_to_workdir() {
    let (_tmp, _repo, be) = small();
    let d = be
        .diff(&DiffSpec::new(DiffTarget::IndexToWorkdir {
            include_untracked: false,
        }))
        .unwrap();
    assert_eq!(d.files.len(), 1);
    let lib = &d.files[0];
    assert_eq!(lib.path().unwrap(), Path::new("src/lib.rs"));
    assert_eq!((lib.additions, lib.deletions), (4, 0));
    let added: Vec<&str> = d
        .hunk_lines(&d.file_hunks(lib)[0])
        .iter()
        .filter(|l| l.kind == LineKind::Addition)
        .map(|l| d.line_text(l))
        .collect();
    assert_eq!(
        added,
        vec!["", "pub fn mul(a: i32, b: i32) -> i32 {", "    a * b", "}"]
    );

    let d = be
        .diff(&DiffSpec::new(DiffTarget::IndexToWorkdir {
            include_untracked: true,
        }))
        .unwrap();
    let todo = d
        .files
        .iter()
        .find(|f| f.path().unwrap() == Path::new("notes/todo.txt"))
        .unwrap();
    assert_eq!(todo.change, FileChange::Untracked);
    assert_eq!(todo.additions, 1);
}

#[test]
fn diff_commits() {
    let (_tmp, repo, be) = small();
    // Root commit: everything added.
    let d = be
        .diff(&DiffSpec::new(DiffTarget::Commit(oid(&repo.commits[0]))))
        .unwrap();
    assert_eq!(d.files.len(), 2);
    assert!(d.files.iter().all(|f| f.change == FileChange::Added));

    // Merge vs first parent: brings in feature.txt.
    let d = be
        .diff(&DiffSpec::new(DiffTarget::Commit(oid(&repo.head))))
        .unwrap();
    assert_eq!(d.files.len(), 1);
    assert_eq!(d.files[0].path().unwrap(), Path::new("feature.txt"));
    assert_eq!(d.total_additions(), 2);

    // Arbitrary range with a path filter.
    let spec = DiffSpec::new(DiffTarget::Commits {
        old: oid(&repo.commits[0]),
        new: oid(&repo.head),
    })
    .paths(["src/lib.rs".into()]);
    let d = be.diff(&spec).unwrap();
    assert_eq!(d.files.len(), 1);
    assert_eq!((d.total_additions(), d.total_deletions()), (4, 0));

    // Unknown commit.
    let missing = Oid([0xab; 20]);
    assert!(matches!(
        be.diff(&DiffSpec::new(DiffTarget::Commit(missing))),
        Err(Error::NotFound(_))
    ));
}

#[test]
fn diff_renames_binary_and_no_newline() {
    let (tmp, _repo, be) = small();
    let d = tmp.path();
    git(d, &["reset", "-q", "--hard"]);
    git(d, &["mv", "main.txt", "moved.txt"]);
    std::fs::write(d.join("blob.bin"), [0u8, 1, 2, 0, 255, 0]).unwrap();
    std::fs::write(d.join("nonl.txt"), "no newline").unwrap();
    git(d, &["add", "."]);
    let diff = be.diff(&DiffSpec::new(DiffTarget::HeadToIndex)).unwrap();

    let moved = diff
        .files
        .iter()
        .find(|f| f.change == FileChange::Renamed)
        .expect("rename detected");
    assert_eq!(moved.old_path.as_deref(), Some(Path::new("main.txt")));
    assert_eq!(moved.new_path.as_deref(), Some(Path::new("moved.txt")));

    let bin = diff
        .files
        .iter()
        .find(|f| f.path().unwrap() == Path::new("blob.bin"))
        .unwrap();
    assert!(bin.binary);
    assert!(diff.file_hunks(bin).is_empty());

    let nonl = diff
        .files
        .iter()
        .find(|f| f.path().unwrap() == Path::new("nonl.txt"))
        .unwrap();
    let lines = diff.hunk_lines(&diff.file_hunks(nonl)[0]);
    assert_eq!(diff.line_text(&lines[0]), "no newline");
    assert_eq!(lines.last().unwrap().kind, LineKind::NoNewline);
    assert_eq!(nonl.additions, 1);
}

#[test]
fn backend_is_shareable_across_threads() {
    let (_tmp, repo, be) = small();
    let be = Arc::new(be);
    let tips = [oid(&repo.head)];
    let expected = be.walk_commits(&tips, 0..100).unwrap();
    let handles: Vec<_> = (0..4)
        .map(|_| {
            let be = Arc::clone(&be);
            let expected = expected.clone();
            std::thread::spawn(move || {
                for _ in 0..20 {
                    assert_eq!(be.walk_commits(&tips, 0..100).unwrap(), expected);
                    assert_eq!(be.status(&StatusOpts::default()).unwrap().entries.len(), 4);
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
}

#[test]
fn trait_object_usable() {
    let (_tmp, _repo, be) = small();
    let dynbe: Arc<dyn GitBackend> = Arc::new(be);
    assert!(dynbe.repo_info().unwrap().head.is_some());
}

// ---- M2: commit DAG, metadata, commit-graph ----

/// `git rev-list --parents` as oid → parents, for comparing DAGs.
fn git_dag(dir: &Path, tips: &[&str]) -> HashMap<Oid, Vec<Oid>> {
    let mut args = vec!["rev-list", "--parents"];
    args.extend_from_slice(tips);
    git(dir, &args)
        .lines()
        .map(|l| {
            let mut it = l.split(' ').map(oid);
            (it.next().unwrap(), it.collect())
        })
        .collect()
}

fn dag_map(d: &fgc_core::CommitDag) -> HashMap<Oid, Vec<Oid>> {
    (0..d.len() as u32)
        .map(|i| {
            (
                d.oids[i as usize],
                d.parents_of(i)
                    .iter()
                    .map(|&p| d.oids[p as usize])
                    .collect(),
            )
        })
        .collect()
}

fn branch_tips(be: &Git2Backend) -> Vec<Oid> {
    be.refs()
        .unwrap()
        .iter()
        .filter(|r| r.kind == RefKind::LocalBranch)
        .map(|r| r.target)
        .collect()
}

fn synthetic(commits: u64, commit_graph: bool) -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("h");
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

#[test]
fn commit_dag_from_commit_graph_matches_git() {
    let (_tmp, dir) = synthetic(3_000, true);
    let be = Git2Backend::open(&dir).unwrap();
    assert!(be.repo_info().unwrap().has_commit_graph);
    let dag = be.commit_dag(&branch_tips(&be)).unwrap();
    assert_eq!(dag.stats.from_commit_graph, 3_000);
    assert_eq!(dag.stats.parsed, 0);
    assert_eq!(dag_map(&dag), git_dag(&dir, &["--branches"]));
    // Commit times agree with libgit2.
    let metas = be.commit_metas(&dag.oids[..50]).unwrap();
    for (i, m) in metas.iter().enumerate() {
        assert_eq!(m.oid, dag.oids[i]);
        assert_eq!(m.commit_time, dag.commit_times[i]);
    }
}

#[test]
fn commit_dag_without_commit_graph_matches_git() {
    let (_tmp, dir) = synthetic(1_500, false);
    let be = Git2Backend::open(&dir).unwrap();
    assert!(!be.repo_info().unwrap().has_commit_graph);
    let dag = be.commit_dag(&branch_tips(&be)).unwrap();
    assert_eq!((dag.stats.from_commit_graph, dag.stats.parsed), (0, 1_500));
    assert_eq!(dag_map(&dag), git_dag(&dir, &["--branches"]));
}

#[test]
fn commit_dag_hybrid_after_new_commits() {
    // Graph written, then new commits (including a merge) on top: the new
    // ones are parsed, the rest comes from the graph.
    let (_tmp, dir) = synthetic(2_000, true);
    git(&dir, &["checkout", "-q", "-b", "side", "main~5"]);
    std::fs::write(dir.join("side.txt"), "side\n").unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-qm", "side"]);
    git(&dir, &["checkout", "-q", "main"]);
    std::fs::write(dir.join("new.txt"), "new\n").unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-qm", "new"]);
    git(
        &dir,
        &["merge", "-q", "--no-ff", "-m", "merge side", "side"],
    );

    let be = Git2Backend::open(&dir).unwrap();
    let dag = be.commit_dag(&branch_tips(&be)).unwrap();
    assert_eq!(dag.stats.parsed, 3);
    assert_eq!(dag.stats.from_commit_graph as usize + 3, dag.len());
    assert_eq!(dag_map(&dag), git_dag(&dir, &["--branches"]));
}

#[test]
fn write_commit_graph_and_split_chain() {
    let (_tmp, dir) = synthetic(1_000, false);
    let be = Git2Backend::open(&dir).unwrap();
    be.write_commit_graph().unwrap();
    assert!(be.repo_info().unwrap().has_commit_graph);
    let tips = branch_tips(&be);
    let dag = be.commit_dag(&tips).unwrap();
    assert_eq!(dag.stats.from_commit_graph, 1_000);

    // A second layer after new commits: a split chain of two files.
    std::fs::write(dir.join("x.txt"), "x\n").unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-qm", "x"]);
    be.write_commit_graph().unwrap();
    let chain = dir.join(".git/objects/info/commit-graphs/commit-graph-chain");
    assert_eq!(std::fs::read_to_string(&chain).unwrap().lines().count(), 2);
    let dag = be.commit_dag(&branch_tips(&be)).unwrap();
    assert_eq!((dag.stats.from_commit_graph, dag.stats.parsed), (1_001, 0));
    assert_eq!(dag_map(&dag), git_dag(&dir, &["--branches"]));
}

#[test]
fn commit_dag_with_octopus_merge() {
    // Octopus merges use the commit-graph's EDGE chunk.
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    git(d, &["init", "-q"]);
    std::fs::write(d.join("base"), "b").unwrap();
    git(d, &["add", "."]);
    git(d, &["commit", "-qm", "base"]);
    for b in ["a", "b", "c"] {
        git(d, &["checkout", "-q", "-b", b, "main"]);
        std::fs::write(d.join(b), b).unwrap();
        git(d, &["add", "."]);
        git(d, &["commit", "-qm", b]);
    }
    git(d, &["checkout", "-q", "main"]);
    git(
        d,
        &["merge", "-q", "--no-ff", "-m", "octopus", "a", "b", "c"],
    );
    git(d, &["commit-graph", "write", "--reachable"]);
    let be = Git2Backend::open(d).unwrap();
    let dag = be.commit_dag(&branch_tips(&be)).unwrap();
    assert_eq!(dag.stats.parsed, 0);
    let map = dag_map(&dag);
    assert!(
        map.values().any(|p| p.len() == 4),
        "octopus merge has 4 parents"
    );
    assert_eq!(map, git_dag(d, &["--branches"]));
}

#[test]
fn corrupt_commit_graph_falls_back_to_parsing() {
    let (_tmp, dir) = synthetic(500, true);
    let path = dir.join(".git/objects/info/commit-graph");
    let mut bytes = std::fs::read(&path).unwrap();
    bytes.truncate(bytes.len() / 2);
    let _ = std::fs::remove_file(&path);
    std::fs::write(&path, bytes).unwrap();
    let be = Git2Backend::open(&dir).unwrap();
    let dag = be.commit_dag(&branch_tips(&be)).unwrap();
    assert_eq!((dag.stats.from_commit_graph, dag.stats.parsed), (0, 500));
    assert_eq!(dag_map(&dag), git_dag(&dir, &["--branches"]));
}

#[test]
fn commit_dag_shallow_clone_ignores_graph_and_stops_at_boundary() {
    let (_tmp, dir) = synthetic(300, true);
    let tmp2 = tempfile::tempdir().unwrap();
    let url = format!("file://{}", dir.display());
    let clone = tmp2.path().join("c");
    git(
        tmp2.path(),
        &[
            "clone",
            "-q",
            "--depth",
            "10",
            &url,
            clone.to_str().unwrap(),
        ],
    );
    git(&clone, &["commit-graph", "write", "--reachable"]);
    let be = Git2Backend::open(&clone).unwrap();
    assert!(be.repo_info().unwrap().is_shallow);
    let head = be.repo_info().unwrap().head.unwrap();
    let dag = be.commit_dag(&[head]).unwrap();
    assert_eq!(dag.stats.from_commit_graph, 0);
    let expected = git_dag(&clone, &["HEAD"]);
    assert_eq!(dag.len(), expected.len());
    // Boundary commits have their (missing) parents dropped.
    for (oid, parents) in dag_map(&dag) {
        assert!(parents.iter().all(|p| expected.contains_key(p)), "{oid}");
    }
}

#[test]
fn commit_dag_rejects_non_commit_tip() {
    let (tmp, _repo, be) = small();
    let tree = oid(&git(tmp.path(), &["rev-parse", "HEAD^{tree}"]));
    assert!(matches!(
        be.commit_dag(&[tree]),
        Err(Error::InvalidInput(_))
    ));
    assert!(be.commit_dag(&[]).unwrap().is_empty());
}

#[test]
fn commit_metas_in_order() {
    let (_tmp, repo, be) = small();
    let want = [
        oid(&repo.commits[2]),
        oid(&repo.head),
        oid(&repo.commits[0]),
    ];
    let metas = be.commit_metas(&want).unwrap();
    assert_eq!(metas.iter().map(|m| m.oid).collect::<Vec<_>>(), want);
    assert_eq!(&*metas[0].summary, "feature: first");
    assert!(matches!(
        be.commit_metas(&[Oid([7; 20])]),
        Err(Error::NotFound(_))
    ));
}

// ---- M4: write path ----

use fgc_core::{HunkSel, NetworkOp};

/// small_repo plus a local identity (tests must not depend on ~/.gitconfig).
fn small_writable() -> (tempfile::TempDir, SmallRepo, Git2Backend) {
    let (tmp, repo, be) = small();
    git(tmp.path(), &["config", "user.name", "Test User"]);
    git(tmp.path(), &["config", "user.email", "test@example.com"]);
    (tmp, repo, be)
}

fn status_of(be: &Git2Backend, path: &str) -> Option<(Change, Change)> {
    be.status(&StatusOpts::default())
        .unwrap()
        .entries
        .into_iter()
        .find(|e| e.path == Path::new(path))
        .map(|e| (e.index, e.worktree))
}

#[test]
fn stage_and_unstage_files() {
    let (tmp, _repo, be) = small_writable();
    be.stage(&["src/lib.rs".into(), "notes/todo.txt".into()])
        .unwrap();
    assert_eq!(
        status_of(&be, "src/lib.rs"),
        Some((Change::Modified, Change::Unmodified))
    );
    assert_eq!(
        status_of(&be, "notes/todo.txt"),
        Some((Change::Added, Change::Unmodified))
    );

    be.unstage(&["README.md".into(), "docs/guide.md".into()])
        .unwrap();
    assert_eq!(
        status_of(&be, "README.md"),
        Some((Change::Unmodified, Change::Modified))
    );
    assert_eq!(
        status_of(&be, "docs/guide.md"),
        Some((Change::Unmodified, Change::Untracked))
    );

    // Deleted files stage as deletions.
    std::fs::remove_file(tmp.path().join("main.txt")).unwrap();
    be.stage(&["main.txt".into()]).unwrap();
    assert_eq!(
        status_of(&be, "main.txt"),
        Some((Change::Deleted, Change::Unmodified))
    );
    assert!(git(tmp.path(), &["diff", "--cached", "--name-status"]).contains("D\tmain.txt"));
}

fn two_hunk_file(dir: &Path) {
    let mut lines: Vec<String> = (1..=40).map(|i| format!("line {i}")).collect();
    std::fs::write(dir.join("hunks.txt"), lines.join("\n") + "\n").unwrap();
    git(dir, &["add", "hunks.txt"]);
    git(dir, &["commit", "-qm", "hunks"]);
    lines[2] = "line 3 changed".into();
    lines[35] = "line 36 changed".into();
    std::fs::write(dir.join("hunks.txt"), lines.join("\n") + "\n").unwrap();
}

fn file_hunks(be: &Git2Backend, target: DiffTarget, path: &str) -> Vec<HunkSel> {
    let d = be
        .diff(&DiffSpec::new(target).paths([path.into()]))
        .unwrap();
    d.files
        .first()
        .map(|f| d.file_hunks(f).iter().map(HunkSel::of).collect())
        .unwrap_or_default()
}

#[test]
fn stage_and_unstage_single_hunks() {
    let (tmp, _repo, be) = small_writable();
    two_hunk_file(tmp.path());
    let wd = || DiffTarget::IndexToWorkdir {
        include_untracked: true,
    };
    let hunks = file_hunks(&be, wd(), "hunks.txt");
    assert_eq!(hunks.len(), 2);

    be.stage_hunk(Path::new("hunks.txt"), hunks[1]).unwrap();
    let staged = file_hunks(&be, DiffTarget::HeadToIndex, "hunks.txt");
    let unstaged = file_hunks(&be, wd(), "hunks.txt");
    assert_eq!(staged.len(), 1);
    assert_eq!(unstaged.len(), 1);
    assert!(git(tmp.path(), &["diff", "--cached"]).contains("+line 36 changed"));
    assert!(git(tmp.path(), &["diff"]).contains("+line 3 changed"));

    // A hunk header the file no longer has is refused, not guessed.
    assert!(matches!(
        be.stage_hunk(Path::new("hunks.txt"), hunks[1]),
        Err(Error::InvalidInput(_))
    ));

    // Stage the rest, then unstage just the first hunk again.
    be.stage(&["hunks.txt".into()]).unwrap();
    let staged = file_hunks(&be, DiffTarget::HeadToIndex, "hunks.txt");
    assert_eq!(staged.len(), 2);
    be.unstage_hunk(Path::new("hunks.txt"), staged[0]).unwrap();
    assert!(git(tmp.path(), &["diff", "--cached"]).contains("+line 36 changed"));
    assert!(!git(tmp.path(), &["diff", "--cached"]).contains("+line 3 changed"));
    assert!(git(tmp.path(), &["diff"]).contains("+line 3 changed"));
}

#[test]
fn stage_hunk_of_untracked_file() {
    let (tmp, _repo, be) = small_writable();
    let hunks = file_hunks(
        &be,
        DiffTarget::IndexToWorkdir {
            include_untracked: true,
        },
        "notes/todo.txt",
    );
    assert_eq!(hunks.len(), 1);
    be.stage_hunk(Path::new("notes/todo.txt"), hunks[0])
        .unwrap();
    assert_eq!(
        status_of(&be, "notes/todo.txt"),
        Some((Change::Added, Change::Unmodified))
    );
    drop(tmp);
}

#[test]
fn commit_and_amend() {
    let (tmp, repo, be) = small_writable();
    let id = be.commit("Add docs\n\n  body line  \n\n\n", false).unwrap();
    assert_eq!(git(tmp.path(), &["rev-parse", "HEAD"]), id.to_string());
    // Like `git commit --cleanup=whitespace`: trailing whitespace and extra
    // blank lines go, indentation stays.
    assert_eq!(
        git(tmp.path(), &["log", "-1", "--format=%B"]),
        "Add docs\n\n  body line"
    );
    assert_eq!(git(tmp.path(), &["rev-parse", "HEAD^"]), repo.head);
    assert_eq!(git(tmp.path(), &["log", "-1", "--format=%an"]), "Test User");
    assert_eq!(status_of(&be, "README.md"), None);

    // Nothing staged now.
    assert!(matches!(
        be.commit("again", false),
        Err(Error::InvalidInput(_))
    ));
    assert!(matches!(
        be.commit("   \n", false),
        Err(Error::InvalidInput(_))
    ));

    // Amend: same parent, new message, plus newly staged content.
    be.stage(&["src/lib.rs".into()]).unwrap();
    let amended = be.commit("Add docs and mul", true).unwrap();
    assert_ne!(amended, id);
    assert_eq!(git(tmp.path(), &["rev-parse", "HEAD^"]), repo.head);
    assert_eq!(
        git(tmp.path(), &["log", "-1", "--format=%s"]),
        "Add docs and mul"
    );
    assert!(git(tmp.path(), &["show", "--stat", "HEAD"]).contains("src/lib.rs"));
}

#[cfg(unix)]
#[test]
fn commit_runs_hooks_via_cli() {
    use std::os::unix::fs::PermissionsExt;
    let (tmp, _repo, be) = small_writable();
    let hook = tmp.path().join(".git/hooks/pre-commit");
    std::fs::write(
        &hook,
        "#!/bin/sh\ntouch \"$(git rev-parse --git-dir)/hook-ran\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    be.commit("with hook", false).unwrap();
    assert!(tmp.path().join(".git/hook-ran").exists());
    assert_eq!(git(tmp.path(), &["log", "-1", "--format=%s"]), "with hook");

    // A failing hook blocks the commit and its output reaches the user.
    std::fs::write(
        &hook,
        "#!/bin/sh\necho 'lint failed: trailing spaces' >&2\nexit 1\n",
    )
    .unwrap();
    be.stage(&["src/lib.rs".into()]).unwrap();
    match be.commit("blocked", false) {
        Err(Error::Engine(msg)) => assert!(msg.contains("lint failed"), "{msg}"),
        other => panic!("expected hook failure, got {other:?}"),
    }
    assert_eq!(git(tmp.path(), &["log", "-1", "--format=%s"]), "with hook");
}

#[test]
fn commit_concludes_a_merge() {
    let (tmp, _repo, be) = small_writable();
    let d = tmp.path();
    git(d, &["stash", "-u", "-q"]);
    git(d, &["checkout", "-q", "-b", "side", "HEAD~1"]);
    std::fs::write(d.join("side.txt"), "side\n").unwrap();
    git(d, &["add", "."]);
    git(d, &["commit", "-qm", "side"]);
    git(d, &["checkout", "-q", "main"]);
    git(d, &["merge", "--no-commit", "--no-ff", "side"]);
    be.commit("Merge side", false).unwrap();
    assert_eq!(git(d, &["log", "-1", "--format=%P"]).split(' ').count(), 2);
    assert!(!d.join(".git/MERGE_HEAD").exists());
}

#[test]
fn branches_create_checkout_delete() {
    let (tmp, repo, be) = small_writable();
    let d = tmp.path();
    git(d, &["stash", "-u", "-q"]); // clean tree for checkouts
    let c2 = oid(&repo.commits[1]);

    be.create_branch("topic", c2, false).unwrap();
    assert_eq!(git(d, &["rev-parse", "topic"]), repo.commits[1]);
    assert_eq!(git(d, &["branch", "--show-current"]), "main");
    assert!(matches!(
        be.create_branch("topic", c2, false),
        Err(Error::InvalidInput(_))
    ));
    assert!(matches!(
        be.create_branch("bad..name", c2, false),
        Err(Error::InvalidInput(_))
    ));

    be.checkout_branch("topic").unwrap();
    assert_eq!(git(d, &["branch", "--show-current"]), "topic");
    assert_eq!(git(d, &["status", "--porcelain"]), "");
    assert!(!d.join("feature.txt").exists());

    be.create_branch("work", c2, true).unwrap();
    assert_eq!(git(d, &["branch", "--show-current"]), "work");

    // Checked-out branch can't be deleted; merged one can; unmerged needs force.
    assert!(be.delete_branch("work", false).is_err());
    be.checkout_branch("main").unwrap();
    be.delete_branch("topic", false).unwrap();
    git(d, &["checkout", "-q", "-b", "unmerged"]);
    std::fs::write(d.join("u.txt"), "u").unwrap();
    git(d, &["add", "."]);
    git(d, &["commit", "-qm", "u"]);
    git(d, &["checkout", "-q", "main"]);
    assert!(matches!(
        be.delete_branch("unmerged", false),
        Err(Error::InvalidInput(_))
    ));
    be.delete_branch("unmerged", true).unwrap();
    assert!(git(d, &["branch", "--list", "unmerged"]).is_empty());
}

#[test]
fn checkout_refuses_to_overwrite_local_changes() {
    let (tmp, _repo, be) = small_writable();
    let d = tmp.path();
    // feature.txt differs between main and feature~1... use a branch where
    // the locally modified file differs.
    git(d, &["stash", "-u", "-q"]);
    git(d, &["checkout", "-q", "-b", "older", "HEAD~2"]);
    git(d, &["checkout", "-q", "main"]);
    std::fs::write(d.join("main.txt"), "local edit\n").unwrap();
    match be.checkout_branch("older") {
        Err(Error::InvalidInput(msg)) => assert!(msg.contains("local changes"), "{msg}"),
        other => panic!("expected refusal, got {other:?}"),
    }
    assert_eq!(git(d, &["branch", "--show-current"]), "main");
    assert_eq!(
        std::fs::read_to_string(d.join("main.txt")).unwrap(),
        "local edit\n"
    );
}

#[test]
fn stash_push_and_pop() {
    let (tmp, _repo, be) = small_writable();
    be.stash_push(Some("wip")).unwrap();
    assert!(be.status(&StatusOpts::default()).unwrap().is_clean());
    assert!(git(tmp.path(), &["stash", "list"]).contains("wip"));
    assert!(matches!(be.stash_push(None), Err(Error::InvalidInput(_))));
    be.stash_pop().unwrap();
    assert_eq!(
        status_of(&be, "src/lib.rs").map(|s| s.1),
        Some(Change::Modified)
    );
    assert_eq!(
        status_of(&be, "notes/todo.txt").map(|s| s.1),
        Some(Change::Untracked)
    );
    assert!(matches!(be.stash_pop(), Err(Error::InvalidInput(_))));
}

#[test]
fn network_push_fetch_pull_through_cli() {
    let tmp = tempfile::tempdir().unwrap();
    let origin = tmp.path().join("origin.git");
    let seed = tmp.path().join("seed");
    std::fs::create_dir_all(&seed).unwrap();
    small_repo(&seed);
    git(
        tmp.path(),
        &[
            "clone",
            "-q",
            "--bare",
            seed.to_str().unwrap(),
            origin.to_str().unwrap(),
        ],
    );
    let url = format!("file://{}", origin.display());
    let a = tmp.path().join("a");
    let b = tmp.path().join("b");
    git(tmp.path(), &["clone", "-q", &url, a.to_str().unwrap()]);
    git(tmp.path(), &["clone", "-q", &url, b.to_str().unwrap()]);
    for d in [&a, &b] {
        git(d, &["config", "user.name", "T"]);
        git(d, &["config", "user.email", "t@example.com"]);
    }

    let be_a = Git2Backend::open(&a).unwrap();
    std::fs::write(a.join("pushed.txt"), "x\n").unwrap();
    be_a.stage(&["pushed.txt".into()]).unwrap();
    let c = be_a.commit("pushed", false).unwrap();
    let lines = std::cell::RefCell::new(Vec::new());
    let cmd = be_a
        .network(NetworkOp::Push, &|l| lines.borrow_mut().push(l.to_owned()))
        .unwrap();
    assert_eq!(cmd, "git push --progress");
    assert_eq!(git(&origin, &["rev-parse", "main"]), c.to_string());

    // First push of a new branch sets its upstream.
    git(&a, &["checkout", "-q", "-b", "fresh"]);
    let cmd = be_a.network(NetworkOp::Push, &|_| {}).unwrap();
    assert_eq!(cmd, "git push --progress -u origin HEAD");
    assert_eq!(
        git(&a, &["rev-parse", "--abbrev-ref", "fresh@{upstream}"]),
        "origin/fresh"
    );

    let be_b = Git2Backend::open(&b).unwrap();
    be_b.network(NetworkOp::Fetch, &|_| {}).unwrap();
    assert_eq!(git(&b, &["rev-parse", "origin/main"]), c.to_string());
    be_b.network(NetworkOp::Pull { rebase: false }, &|_| {})
        .unwrap();
    assert_eq!(git(&b, &["rev-parse", "HEAD"]), c.to_string());

    // Failures carry git's reason.
    git(
        &b,
        &[
            "remote",
            "set-url",
            "origin",
            "file:///nonexistent/repo.git",
        ],
    );
    match be_b.network(NetworkOp::Fetch, &|_| {}) {
        Err(Error::Engine(msg)) => assert!(msg.contains("git fetch"), "{msg}"),
        other => panic!("expected failure, got {other:?}"),
    }
}
