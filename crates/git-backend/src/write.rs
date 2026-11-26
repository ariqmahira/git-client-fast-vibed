//! Write path (M4) with libgit2, plus the git CLI where parity matters.

use std::cell::Cell;
use std::path::{Path, PathBuf};

use fgc_core::{Error, HunkSel, NetworkOp, Oid, Result};
use git2::{
    ApplyLocation, ApplyOptions, BranchType, DiffOptions, Repository, build::CheckoutBuilder,
};

use crate::cli;
use crate::convert::{map_err, path_to_bytes, to_core, to_git};

fn workdir(repo: &Repository) -> Result<&Path> {
    repo.workdir()
        .ok_or_else(|| Error::Unsupported("bare repository has no working tree".into()))
}

pub(crate) fn stage(repo: &Repository, paths: &[PathBuf]) -> Result<()> {
    let wd = workdir(repo)?.to_path_buf();
    let mut index = repo.index().map_err(map_err)?;
    for p in paths {
        if wd.join(p).symlink_metadata().is_ok() {
            index.add_path(p).map_err(map_err)?;
        } else {
            // Deleted in the working tree: stage the deletion.
            index.remove_path(p).map_err(map_err)?;
        }
    }
    index.write().map_err(map_err)
}

pub(crate) fn unstage(repo: &Repository, paths: &[PathBuf]) -> Result<()> {
    workdir(repo)?;
    match repo.head().ok().and_then(|h| h.peel_to_commit().ok()) {
        Some(head) => repo
            .reset_default(
                Some(head.as_object()),
                paths.iter().map(|p| path_to_bytes(p)),
            )
            .map_err(map_err),
        None => {
            // Unborn branch: unstaging means removing from the index.
            let mut index = repo.index().map_err(map_err)?;
            for p in paths {
                index.remove_path(p).map_err(map_err)?;
            }
            index.write().map_err(map_err)
        }
    }
}

fn hunk_matches(h: &git2::DiffHunk<'_>, sel: HunkSel) -> bool {
    h.old_start() == sel.old_start
        && h.old_lines() == sel.old_lines
        && h.new_start() == sel.new_start
        && h.new_lines() == sel.new_lines
}

/// Applies the hunks of `diff` matching `sel` to the index.
fn apply_one_hunk(repo: &Repository, diff: &git2::Diff<'_>, sel: HunkSel) -> Result<()> {
    let found = Cell::new(false);
    let mut opts = ApplyOptions::new();
    opts.hunk_callback(|h| match h {
        Some(h) => {
            let m = hunk_matches(&h, sel);
            if m {
                found.set(true);
            }
            m
        }
        None => true,
    });
    repo.apply(diff, ApplyLocation::Index, Some(&mut opts))
        .map_err(map_err)?;
    if found.get() {
        Ok(())
    } else {
        Err(Error::InvalidInput(
            "that hunk no longer matches the file (it changed); refresh and try again".into(),
        ))
    }
}

fn path_opts(path: &Path) -> DiffOptions {
    let mut o = DiffOptions::new();
    o.pathspec(path_to_bytes(path))
        .disable_pathspec_match(true)
        .context_lines(3);
    o
}

pub(crate) fn stage_hunk(repo: &Repository, path: &Path, sel: HunkSel) -> Result<()> {
    workdir(repo)?;
    let mut o = path_opts(path);
    o.include_untracked(true)
        .recurse_untracked_dirs(true)
        .show_untracked_content(true);
    let diff = repo
        .diff_index_to_workdir(None, Some(&mut o))
        .map_err(map_err)?;
    let untracked = diff
        .deltas()
        .next()
        .is_some_and(|d| d.status() == git2::Delta::Untracked);
    if untracked {
        // libgit2 can't apply to a path the index doesn't have. An untracked
        // file's diff is one hunk holding the whole file, so staging that
        // hunk is staging the file.
        let patch = git2::Patch::from_diff(&diff, 0).map_err(map_err)?;
        let matches = patch
            .as_ref()
            .and_then(|p| p.hunk(0).ok())
            .is_some_and(|(h, _)| hunk_matches(&h, sel));
        if !matches {
            return Err(Error::InvalidInput(
                "that hunk no longer matches the file (it changed); refresh and try again".into(),
            ));
        }
        return stage(repo, &[path.to_path_buf()]);
    }
    apply_one_hunk(repo, &diff, sel)
}

pub(crate) fn unstage_hunk(repo: &Repository, path: &Path, sel: HunkSel) -> Result<()> {
    workdir(repo)?;
    let head_tree = repo.head().ok().and_then(|h| h.peel_to_tree().ok());
    let mut o = path_opts(path);
    // HEAD→index reversed is index→HEAD: applying a hunk of it to the index
    // undoes exactly that staged hunk.
    o.reverse(true);
    let diff = repo
        .diff_tree_to_index(head_tree.as_ref(), None, Some(&mut o))
        .map_err(map_err)?;
    apply_one_hunk(repo, &diff, sel.reversed())
}

/// Hooks or signing configured: libgit2 would silently skip them, so commit
/// through the CLI for exact parity.
fn commit_needs_cli(repo: &Repository) -> bool {
    let config = repo.config().ok();
    let gpgsign = config
        .as_ref()
        .and_then(|c| c.get_bool("commit.gpgsign").ok())
        .unwrap_or(false);
    if gpgsign {
        return true;
    }
    let hooks_dir = config
        .as_ref()
        .and_then(|c| c.get_path("core.hooksPath").ok())
        .map(|p| {
            if p.is_relative() {
                repo.workdir().unwrap_or(repo.path()).join(p)
            } else {
                p
            }
        })
        .unwrap_or_else(|| repo.commondir().join("hooks"));
    [
        "pre-commit",
        "prepare-commit-msg",
        "commit-msg",
        "post-commit",
    ]
    .iter()
    .any(|h| is_executable(&hooks_dir.join(h)))
}

#[cfg(unix)]
fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    p.metadata()
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(p: &Path) -> bool {
    p.is_file()
}

pub(crate) fn commit(repo: &mut Repository, message: &str, amend: bool) -> Result<Oid> {
    let mut merge_heads: Vec<git2::Oid> = Vec::new();
    if repo.state() == git2::RepositoryState::Merge {
        repo.mergehead_foreach(|oid| {
            merge_heads.push(*oid);
            true
        })
        .map_err(map_err)?;
    }
    let repo: &Repository = repo;
    let message = git2::message_prettify(message, None).map_err(map_err)?;
    if message.trim().is_empty() {
        return Err(Error::InvalidInput("commit message is empty".into()));
    }
    let head = repo.head().ok().and_then(|h| h.peel_to_commit().ok());
    if amend && head.is_none() {
        return Err(Error::InvalidInput(
            "nothing to amend: no commits yet".into(),
        ));
    }
    let mut index = repo.index().map_err(map_err)?;
    if index.has_conflicts() {
        return Err(Error::InvalidInput(
            "resolve conflicts before committing".into(),
        ));
    }
    let tree_id = index.write_tree().map_err(map_err)?;
    let merging = repo.state() == git2::RepositoryState::Merge;
    if !amend && !merging && head.as_ref().is_some_and(|h| h.tree_id() == tree_id) {
        return Err(Error::InvalidInput(
            "nothing to commit: stage changes first".into(),
        ));
    }

    if commit_needs_cli(repo) {
        let mut args = vec!["commit", "-F", "-", "--cleanup=whitespace"];
        if amend {
            args.push("--amend");
        }
        cli::run_with_input(repo.path(), repo.workdir(), &args, &message)?;
        let head = repo
            .head()
            .and_then(|h| h.peel_to_commit())
            .map_err(map_err)?;
        return Ok(to_core(head.id()));
    }

    let sig = repo.signature().map_err(|_| {
        Error::InvalidInput(
            "set your name and email first: git config --global user.name / user.email".into(),
        )
    })?;
    let tree = repo.find_tree(tree_id).map_err(map_err)?;
    let id = if amend {
        let head = head.expect("checked above");
        head.amend(
            Some("HEAD"),
            None,
            Some(&sig),
            None,
            Some(&message),
            Some(&tree),
        )
        .map_err(map_err)?
    } else {
        let mut parents: Vec<git2::Commit<'_>> = head.into_iter().collect();
        for oid in merge_heads {
            parents.push(repo.find_commit(oid).map_err(map_err)?);
        }
        let refs: Vec<&git2::Commit<'_>> = parents.iter().collect();
        repo.commit(Some("HEAD"), &sig, &sig, &message, &tree, &refs)
            .map_err(map_err)?
    };
    if merging {
        repo.cleanup_state().map_err(map_err)?;
    }
    Ok(to_core(id))
}

fn valid_branch_name(name: &str) -> Result<()> {
    if git2::Branch::name_is_valid(name).unwrap_or(false) {
        Ok(())
    } else {
        Err(Error::InvalidInput(format!(
            "not a valid branch name: {name:?}"
        )))
    }
}

pub(crate) fn create_branch(repo: &Repository, name: &str, at: Oid, checkout: bool) -> Result<()> {
    valid_branch_name(name)?;
    let commit = repo.find_commit(to_git(at)).map_err(map_err)?;
    repo.branch(name, &commit, false).map_err(|e| {
        if e.code() == git2::ErrorCode::Exists {
            Error::InvalidInput(format!("a branch named {name:?} already exists"))
        } else {
            map_err(e)
        }
    })?;
    if checkout {
        checkout_branch(repo, name)?;
    }
    Ok(())
}

pub(crate) fn checkout_branch(repo: &Repository, name: &str) -> Result<()> {
    let local = match repo.find_branch(name, BranchType::Local) {
        Ok(b) => b,
        Err(_) => {
            // A remote branch: create (or reuse) the local tracking branch.
            let remote = repo
                .find_branch(name, BranchType::Remote)
                .map_err(|_| Error::NotFound(format!("no branch named {name:?}")))?;
            let short = name.split_once('/').map_or(name, |(_, b)| b);
            match repo.find_branch(short, BranchType::Local) {
                Ok(b) => b,
                Err(_) => {
                    let commit = remote.get().peel_to_commit().map_err(map_err)?;
                    let mut b = repo.branch(short, &commit, false).map_err(map_err)?;
                    b.set_upstream(Some(name)).map_err(map_err)?;
                    b
                }
            }
        }
    };
    let refname = local.get().name().map_err(map_err)?.to_owned();
    let target = local.get().peel_to_commit().map_err(map_err)?;
    let mut co = CheckoutBuilder::new();
    co.safe();
    repo.checkout_tree(target.as_object(), Some(&mut co)).map_err(|e| {
        if e.code() == git2::ErrorCode::Conflict {
            Error::InvalidInput(
                "your local changes would be overwritten by checkout; commit or stash them first".into(),
            )
        } else {
            map_err(e)
        }
    })?;
    repo.set_head(&refname).map_err(map_err)
}

pub(crate) fn delete_branch(repo: &Repository, name: &str, force: bool) -> Result<()> {
    let mut b = repo
        .find_branch(name, BranchType::Local)
        .map_err(|_| Error::NotFound(format!("no local branch named {name:?}")))?;
    if b.is_head() {
        return Err(Error::InvalidInput(
            "cannot delete the checked-out branch".into(),
        ));
    }
    if !force {
        let tip = b.get().peel_to_commit().map_err(map_err)?.id();
        let merged_into = |target: Option<git2::Oid>| {
            target.is_some_and(|t| t == tip || repo.graph_descendant_of(t, tip).unwrap_or(false))
        };
        let head = repo.head().ok().and_then(|h| h.target());
        let upstream = b.upstream().ok().and_then(|u| u.get().target());
        if !merged_into(head) && !merged_into(upstream) {
            return Err(Error::InvalidInput(format!(
                "branch {name:?} is not fully merged; delete it anyway to lose its commits"
            )));
        }
    }
    b.delete().map_err(map_err)
}

pub(crate) fn stash_push(repo: &mut Repository, message: Option<&str>) -> Result<()> {
    let sig = repo.signature().map_err(|_| {
        Error::InvalidInput(
            "set your name and email first (git config user.name / user.email)".into(),
        )
    })?;
    repo.stash_save2(&sig, message, Some(git2::StashFlags::INCLUDE_UNTRACKED))
        .map(|_| ())
        .map_err(|e| {
            if e.code() == git2::ErrorCode::NotFound {
                Error::InvalidInput("no local changes to stash".into())
            } else {
                map_err(e)
            }
        })
}

pub(crate) fn stash_pop(repo: &mut Repository) -> Result<()> {
    repo.stash_pop(0, None).map_err(|e| match e.code() {
        git2::ErrorCode::NotFound => Error::InvalidInput("there is no stash to pop".into()),
        git2::ErrorCode::Conflict => Error::InvalidInput(
            "the stash conflicts with your local changes; commit or stash them first".into(),
        ),
        _ => map_err(e),
    })
}

pub(crate) fn network(repo: &Repository, op: NetworkOp, progress: &dyn Fn(&str)) -> Result<String> {
    let git_dir = repo.path();
    let wd = repo.workdir();
    let args: Vec<String> = match op {
        NetworkOp::Fetch => vec![
            "fetch".into(),
            "--all".into(),
            "--prune".into(),
            "--progress".into(),
        ],
        NetworkOp::Pull { rebase } => {
            workdir(repo)?;
            vec![
                "pull".into(),
                "--progress".into(),
                if rebase {
                    "--rebase".into()
                } else {
                    "--ff-only".into()
                },
            ]
        }
        NetworkOp::Push => {
            let head = repo.head().map_err(map_err)?;
            if !head.is_branch() {
                return Err(Error::InvalidInput(
                    "HEAD is detached: check out a branch to push".into(),
                ));
            }
            let branch = git2::Branch::wrap(head);
            if branch.upstream().is_ok() {
                vec!["push".into(), "--progress".into()]
            } else {
                // First push: publish the branch and set its upstream.
                let remotes = repo.remotes().map_err(map_err)?;
                let names: Vec<String> = remotes
                    .iter_bytes()
                    .map(|b| String::from_utf8_lossy(b).into_owned())
                    .collect();
                let remote = if names.iter().any(|r| r == "origin") {
                    "origin".to_owned()
                } else {
                    names
                        .into_iter()
                        .next()
                        .ok_or_else(|| Error::InvalidInput("no remote configured".into()))?
                };
                vec![
                    "push".into(),
                    "--progress".into(),
                    "-u".into(),
                    remote,
                    "HEAD".into(),
                ]
            }
        }
    };
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    cli::run_streaming(git_dir, wd, &args, progress)?;
    Ok(cli::display(&args))
}
