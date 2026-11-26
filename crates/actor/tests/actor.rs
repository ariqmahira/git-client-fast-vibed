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
