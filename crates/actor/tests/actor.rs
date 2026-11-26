use std::sync::mpsc;
use std::time::Duration;

use fgc_actor::{ActorOptions, Command, Event, Phase, RepoActor};
use fgc_core::{Change, Oid};
use fgc_fixtures::small_repo;

fn recv_until<T>(rx: &mpsc::Receiver<Event>, mut f: impl FnMut(Event) -> Option<T>) -> T {
    loop {
        let ev = rx.recv_timeout(Duration::from_secs(30)).expect("event");
        if let Event::Error { op, message } = &ev {
            panic!("{op}: {message}");
        }
        if let Some(v) = f(ev) {
            return v;
        }
    }
}

fn oid(s: &str) -> Oid {
    s.parse().unwrap()
}

#[test]
fn open_publishes_refs_graph_and_status() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = small_repo(tmp.path());
    let (tx, rx) = mpsc::channel();
    let actor = RepoActor::open(tmp.path(), ActorOptions::default(), move |ev| {
        let _ = tx.send(ev);
    })
    .unwrap();

    let (mut refs, mut graph, mut status, mut info) = (None, None, None, None);
    while refs.is_none() || graph.is_none() || status.is_none() || info.is_none() {
        recv_until(&rx, |ev| {
            match ev {
                Event::Refs(r) => refs = Some(r),
                Event::Graph(g) if g.phase == Phase::Complete => graph = Some(g),
                Event::Status(s) => status = Some(s),
                Event::RepoInfo(i) => info = Some(i),
                _ => return None,
            }
            Some(())
        });
    }
    assert_eq!(refs.unwrap().len(), 4);
    assert_eq!(info.unwrap().head, Some(oid(&repo.head)));
    let g = graph.unwrap();
    assert_eq!(g.layout.topology().len(), 6);
    assert_eq!(g.layout.topology().oid(0), oid(&repo.head));
    let st = status.unwrap();
    assert_eq!(st.entries.len(), 4);
    assert!(st.entries.iter().any(|e| e.worktree == Change::Untracked));
    drop(actor);
}

#[test]
fn metas_and_latest_commit_diff_wins() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = small_repo(tmp.path());
    let (tx, rx) = mpsc::channel();
    let actor = RepoActor::open(
        tmp.path(),
        ActorOptions {
            workers: 1,
            ..Default::default()
        },
        move |ev| {
            let _ = tx.send(ev);
        },
    )
    .unwrap();

    let wanted: Vec<Oid> = repo.commits.iter().map(|c| oid(c)).collect();
    actor.send(Command::Metas(wanted.clone()));
    let metas = recv_until(&rx, |ev| match ev {
        Event::Metas(m) => Some(m),
        _ => None,
    });
    assert_eq!(metas.iter().map(|m| m.oid).collect::<Vec<_>>(), wanted);

    // Arrowing through commits quickly: the last diff is always delivered
    // and nothing older arrives after a newer one. (Earlier ones may still
    // be delivered if they finished before the next command was sent.)
    for c in &repo.commits {
        actor.send(Command::CommitDiff(oid(c)));
    }
    let last = oid(repo.commits.last().unwrap());
    let mut delivered = Vec::new();
    let got = recv_until(&rx, |ev| match ev {
        Event::CommitDiff { oid, diff } => {
            delivered.push(oid);
            (oid == last).then_some(diff)
        }
        _ => None,
    });
    assert_eq!(got.files.len(), 1); // merge brings in feature.txt
    let order: Vec<usize> = delivered
        .iter()
        .map(|d| wanted.iter().position(|w| w == d).unwrap())
        .collect();
    assert!(
        order.windows(2).all(|w| w[0] < w[1]),
        "out of order: {order:?}"
    );
    assert!(
        rx.recv_timeout(Duration::from_millis(200))
            .map_or(true, |ev| !matches!(ev, Event::CommitDiff { .. }))
    );
    drop(actor);
}

#[test]
fn workdir_diff_split_into_staged_and_unstaged() {
    let tmp = tempfile::tempdir().unwrap();
    small_repo(tmp.path());
    let (tx, rx) = mpsc::channel();
    let actor = RepoActor::open(tmp.path(), ActorOptions::default(), move |ev| {
        let _ = tx.send(ev);
    })
    .unwrap();
    actor.send(Command::WorkdirDiff(None));
    let (staged, unstaged) = recv_until(&rx, |ev| match ev {
        Event::WorkdirDiff {
            staged, unstaged, ..
        } => Some((staged, unstaged)),
        _ => None,
    });
    assert_eq!(staged.files.len(), 2); // README.md, docs/guide.md
    assert_eq!(unstaged.files.len(), 2); // src/lib.rs, notes/todo.txt (untracked)
    actor.send(Command::WorkdirDiff(Some("src/lib.rs".into())));
    let unstaged = recv_until(&rx, |ev| match ev {
        Event::WorkdirDiff { unstaged, .. } => Some(unstaged),
        _ => None,
    });
    assert_eq!(unstaged.files.len(), 1);
    drop(actor);
}

// ---- M4/M5: writes and live updates ----

use fgc_actor::WriteOp;
use fgc_core::NetworkOp;
use std::path::Path;

fn open(dir: &Path) -> (RepoActor, mpsc::Receiver<Event>) {
    let (tx, rx) = mpsc::channel();
    let actor = RepoActor::open(dir, ActorOptions::default(), move |ev| {
        let _ = tx.send(ev);
    })
    .unwrap();
    (actor, rx)
}

fn identity(dir: &Path) {
    fgc_fixtures::git(dir, &["config", "user.name", "T"]);
    fgc_fixtures::git(dir, &["config", "user.email", "t@example.com"]);
}

/// Waits for `f` to match, ignoring errors (some tests provoke them).
fn wait_for<T>(rx: &mpsc::Receiver<Event>, mut f: impl FnMut(&Event) -> Option<T>) -> T {
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        let ev = rx.recv_timeout(left).expect("timed out waiting for event");
        if let Some(v) = f(&ev) {
            return v;
        }
    }
}

#[test]
fn write_ops_report_and_refresh() {
    let tmp = tempfile::tempdir().unwrap();
    small_repo(tmp.path());
    identity(tmp.path());
    let (actor, rx) = open(tmp.path());

    let id = actor.send(Command::Write(WriteOp::Stage(vec!["src/lib.rs".into()])));
    let (label, command) = wait_for(&rx, |ev| match ev {
        Event::OpStarted {
            id: i,
            label,
            command,
        } if *i == id => Some((label.clone(), command.clone())),
        _ => None,
    });
    assert_eq!(label, "Stage src/lib.rs");
    assert_eq!(command, "git add -- src/lib.rs");
    let r = wait_for(&rx, |ev| match ev {
        Event::OpFinished { id: i, result, .. } if *i == id => Some(result.clone()),
        _ => None,
    });
    assert_eq!(r, Ok(()));
    wait_for(&rx, |ev| match ev {
        Event::Status(s) => s
            .entries
            .iter()
            .any(|e| e.path == Path::new("src/lib.rs") && e.index == Change::Modified)
            .then_some(()),
        _ => None,
    });

    // Commit: the graph reloads with the new commit on top.
    let id = actor.send(Command::Write(WriteOp::Commit {
        message: "from the actor".into(),
        amend: false,
    }));
    let r = wait_for(&rx, |ev| match ev {
        Event::OpFinished { id: i, result, .. } if *i == id => Some(result.clone()),
        _ => None,
    });
    assert_eq!(r, Ok(()));
    let head = fgc_fixtures::git(tmp.path(), &["rev-parse", "HEAD"])
        .parse::<Oid>()
        .unwrap();
    wait_for(&rx, |ev| match ev {
        Event::Graph(g) if g.phase == Phase::Complete && g.layout.topology().len() == 7 => {
            assert_eq!(g.layout.topology().oid(0), head);
            Some(())
        }
        _ => None,
    });

    // Failures come back as results, not panics.
    let id = actor.send(Command::Write(WriteOp::Checkout("no-such-branch".into())));
    let r = wait_for(&rx, |ev| match ev {
        Event::OpFinished { id: i, result, .. } if *i == id => Some(result.clone()),
        _ => None,
    });
    assert!(r.unwrap_err().contains("no-such-branch"));
}

#[test]
fn watcher_updates_status_and_graph() {
    let tmp = tempfile::tempdir().unwrap();
    small_repo(tmp.path());
    identity(tmp.path());
    let (actor, rx) = open(tmp.path());
    wait_for(&rx, |ev| matches!(ev, Event::Status(_)).then_some(()));

    // An editor saves a new file: a status update arrives without any command.
    let t = std::time::Instant::now();
    std::fs::write(tmp.path().join("saved.txt"), "hello\n").unwrap();
    wait_for(&rx, |ev| match ev {
        Event::Status(s) => s
            .entries
            .iter()
            .any(|e| e.path == Path::new("saved.txt"))
            .then_some(()),
        _ => None,
    });
    assert!(
        t.elapsed() < Duration::from_secs(2),
        "took {:?}",
        t.elapsed()
    );

    // A commit made in a terminal: refs change, the graph reloads.
    fgc_fixtures::git(tmp.path(), &["add", "saved.txt"]);
    fgc_fixtures::git(tmp.path(), &["commit", "-qm", "from terminal"]);
    wait_for(&rx, |ev| match ev {
        Event::Graph(g) if g.phase == Phase::Complete && g.layout.topology().len() == 7 => Some(()),
        _ => None,
    });
    drop(actor);
}

#[test]
fn network_ops_report_progress_and_result() {
    let tmp = tempfile::tempdir().unwrap();
    let seed = tmp.path().join("seed");
    std::fs::create_dir_all(&seed).unwrap();
    small_repo(&seed);
    let origin = tmp.path().join("origin.git");
    fgc_fixtures::git(
        tmp.path(),
        &[
            "clone",
            "-q",
            "--bare",
            seed.to_str().unwrap(),
            origin.to_str().unwrap(),
        ],
    );
    let work = tmp.path().join("work");
    fgc_fixtures::git(
        tmp.path(),
        &[
            "clone",
            "-q",
            &format!("file://{}", origin.display()),
            work.to_str().unwrap(),
        ],
    );
    let (actor, rx) = open(&work);
    let id = actor.send(Command::Network(NetworkOp::Fetch));
    let (command, result) = wait_for(&rx, |ev| match ev {
        Event::OpFinished {
            id: i,
            command,
            result,
            ..
        } if *i == id => Some((command.clone(), result.clone())),
        _ => None,
    });
    assert_eq!(result, Ok(()));
    assert_eq!(command, "git fetch --all --prune --progress");
    drop(actor);
}
