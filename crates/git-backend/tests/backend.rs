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
