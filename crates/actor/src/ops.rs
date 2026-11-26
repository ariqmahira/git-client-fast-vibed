//! Write and network operations as the UI requests them, with a label for
//! progress UI and the equivalent git command (plan §6: show the git command
//! for every action).

use std::path::PathBuf;

use fgc_core::{HunkSel, NetworkOp, Oid};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteOp {
    Stage(Vec<PathBuf>),
    Unstage(Vec<PathBuf>),
    StageHunk {
        path: PathBuf,
        hunk: HunkSel,
    },
    UnstageHunk {
        path: PathBuf,
        hunk: HunkSel,
    },
    Commit {
        message: String,
        amend: bool,
    },
    CreateBranch {
        name: String,
        at: Oid,
        checkout: bool,
    },
    Checkout(String),
    DeleteBranch {
        name: String,
        force: bool,
    },
    StashPush(Option<String>),
    StashPop,
}

fn paths_arg(paths: &[PathBuf]) -> String {
    const SHOWN: usize = 3;
    let mut s: Vec<String> = paths
        .iter()
        .take(SHOWN)
        .map(|p| quote(&p.display().to_string()))
        .collect();
    if paths.len() > SHOWN {
        s.push(format!("… ({} more)", paths.len() - SHOWN));
    }
    s.join(" ")
}

fn quote(s: &str) -> String {
    if s.chars()
        .any(|c| c.is_whitespace() || "\"'$`\\".contains(c))
    {
        format!("'{}'", s.replace('\'', "'\\''"))
    } else {
        s.to_owned()
    }
}

impl WriteOp {
    /// Short label for progress and toasts.
    pub fn label(&self) -> String {
        match self {
            WriteOp::Stage(p) if p.len() == 1 => format!("Stage {}", p[0].display()),
            WriteOp::Stage(p) => format!("Stage {} files", p.len()),
            WriteOp::Unstage(p) if p.len() == 1 => format!("Unstage {}", p[0].display()),
            WriteOp::Unstage(p) => format!("Unstage {} files", p.len()),
            WriteOp::StageHunk { path, .. } => format!("Stage hunk in {}", path.display()),
            WriteOp::UnstageHunk { path, .. } => format!("Unstage hunk in {}", path.display()),
            WriteOp::Commit { amend: false, .. } => "Commit".into(),
            WriteOp::Commit { amend: true, .. } => "Amend commit".into(),
            WriteOp::CreateBranch {
                name,
                checkout: true,
                ..
            } => format!("Create and check out {name}"),
            WriteOp::CreateBranch { name, .. } => format!("Create branch {name}"),
            WriteOp::Checkout(name) => format!("Check out {name}"),
            WriteOp::DeleteBranch { name, .. } => format!("Delete branch {name}"),
            WriteOp::StashPush(_) => "Stash changes".into(),
            WriteOp::StashPop => "Pop stash".into(),
        }
    }

    /// The equivalent git command.
    pub fn git_command(&self) -> String {
        match self {
            WriteOp::Stage(p) => format!("git add -- {}", paths_arg(p)),
            WriteOp::Unstage(p) => format!("git restore --staged -- {}", paths_arg(p)),
            WriteOp::StageHunk { path, hunk } => format!(
                "git add -p -- {}   # hunk @@ -{},{} +{},{} @@",
                quote(&path.display().to_string()),
                hunk.old_start,
                hunk.old_lines,
                hunk.new_start,
                hunk.new_lines
            ),
            WriteOp::UnstageHunk { path, hunk } => format!(
                "git restore --staged -p -- {}   # hunk @@ -{},{} +{},{} @@",
                quote(&path.display().to_string()),
                hunk.old_start,
                hunk.old_lines,
                hunk.new_start,
                hunk.new_lines
            ),
            WriteOp::Commit { message, amend } => {
                let subject = message.lines().next().unwrap_or("");
                let more = if message.lines().count() > 1 {
                    " …"
                } else {
                    ""
                };
                format!(
                    "git commit{} -m {}{more}",
                    if *amend { " --amend" } else { "" },
                    quote(subject)
                )
            }
            WriteOp::CreateBranch {
                name,
                at,
                checkout: true,
            } => format!("git switch -c {name} {}", at.short(7)),
            WriteOp::CreateBranch { name, at, .. } => format!("git branch {name} {}", at.short(7)),
            WriteOp::Checkout(name) => format!("git switch {name}"),
            WriteOp::DeleteBranch { name, force } => {
                format!("git branch {} {name}", if *force { "-D" } else { "-d" })
            }
            WriteOp::StashPush(Some(m)) => format!("git stash push -u -m {}", quote(m)),
            WriteOp::StashPush(None) => "git stash push -u".into(),
            WriteOp::StashPop => "git stash pop".into(),
        }
    }

    /// Whether refs (and so the graph) may change.
    pub fn changes_refs(&self) -> bool {
        !matches!(
            self,
            WriteOp::Stage(_)
                | WriteOp::Unstage(_)
                | WriteOp::StageHunk { .. }
                | WriteOp::UnstageHunk { .. }
        )
    }
}

pub fn network_label(op: NetworkOp) -> &'static str {
    match op {
        NetworkOp::Fetch => "Fetch",
        NetworkOp::Pull { rebase: false } => "Pull",
        NetworkOp::Pull { rebase: true } => "Pull (rebase)",
        NetworkOp::Push => "Push",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands() {
        assert_eq!(
            WriteOp::Stage(vec!["a b.txt".into()]).git_command(),
            "git add -- 'a b.txt'"
        );
        assert_eq!(
            WriteOp::Stage((0..5).map(|i| PathBuf::from(format!("f{i}"))).collect()).git_command(),
            "git add -- f0 f1 f2 … (2 more)"
        );
        assert_eq!(
            WriteOp::Commit {
                message: "Fix it\n\nbody".into(),
                amend: true
            }
            .git_command(),
            "git commit --amend -m 'Fix it' …"
        );
        assert_eq!(
            WriteOp::DeleteBranch {
                name: "x".into(),
                force: true
            }
            .git_command(),
            "git branch -D x"
        );
        assert!(!WriteOp::Stage(vec![]).changes_refs());
        assert!(WriteOp::StashPop.changes_refs());
    }
}
