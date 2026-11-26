//! Write path (M4) and network (M6) UI: commit box, stage/unstage controls,
//! branch and stash actions, fetch/pull/push, the status bar.
//!
//! Every action is a `Command::Write` / `Command::Network` sent to the
//! actor. Affected files are marked pending immediately (optimistic UI) and
//! the bar shows the equivalent git command, then the result.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use fgc_actor::{Command, WriteOp};
use fgc_core::{HunkSel, NetworkOp, Oid, RefKind};
use gpui::Focusable;
use gpui::{
    AnyElement, Context, FontWeight, InteractiveElement, IntoElement, ParentElement, SharedString,
    Stateful, StatefulInteractiveElement, Styled, Window, div, prelude::*, px,
};

use super::{RepoView, Selection};
use crate::text_input::InputEvent;
use crate::theme;

#[derive(Debug, Clone)]
pub(crate) struct Running {
    pub label: String,
    pub progress: String,
}

#[derive(Debug, Clone)]
pub(crate) struct Toast {
    pub ok: bool,
    pub text: String,
    pub at: Instant,
}

/// What a pending operation should clear when it finishes.
#[derive(Debug, Clone, Default)]
pub(crate) struct Pending {
    pub paths: Vec<PathBuf>,
    pub commit: bool,
    pub branch_delete: Option<String>,
}

pub(crate) fn button(
    id: impl Into<SharedString>,
    label: impl Into<SharedString>,
    enabled: bool,
) -> Stateful<gpui::Div> {
    let id: SharedString = id.into();
    div()
        .id(id)
        .flex_shrink_0()
        .px_2()
        .py_0p5()
        .rounded(px(4.))
        .text_size(px(12.5))
        .when(enabled, |d| {
            d.cursor_pointer()
                .bg(theme::panel())
                .hover(|s| s.bg(theme::hover()))
        })
        .when(!enabled, |d| d.text_color(theme::muted()))
        .border_1()
        .border_color(theme::border())
        .child(label.into())
}

impl RepoView {
    pub(crate) fn subscribe_inputs(
        commit: &gpui::Entity<crate::text_input::TextInput>,
        branch: &gpui::Entity<crate::text_input::TextInput>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.subscribe_in(
            commit,
            window,
            |this, _, ev: &InputEvent, window, cx| match ev {
                InputEvent::Submit => this.do_commit(cx),
                InputEvent::Cancel => window.focus(&this.focus),
                InputEvent::Changed => cx.notify(),
            },
        )
        .detach();
        cx.subscribe_in(
            branch,
            window,
            |this, _, ev: &InputEvent, window, cx| match ev {
                InputEvent::Submit => {
                    this.do_create_branch(cx);
                    window.focus(&this.focus);
                }
                InputEvent::Cancel => {
                    this.branch_input_open = false;
                    window.focus(&this.focus);
                    cx.notify();
                }
                InputEvent::Changed => {}
            },
        )
        .detach();
    }

    fn send_op(&mut self, cmd: Command, pending: Pending) -> u64 {
        self.input();
        for p in &pending.paths {
            self.pending_paths.insert(p.clone());
        }
        let id = self.actor.send(cmd);
        self.pending_ops.insert(id, pending);
        id
    }

    pub(crate) fn write(&mut self, op: WriteOp) -> u64 {
        let paths = match &op {
            WriteOp::Stage(p) | WriteOp::Unstage(p) => p.clone(),
            WriteOp::StageHunk { path, .. } | WriteOp::UnstageHunk { path, .. } => {
                vec![path.clone()]
            }
            _ => Vec::new(),
        };
        let commit = matches!(op, WriteOp::Commit { .. });
        let branch_delete = match &op {
            WriteOp::DeleteBranch { name, .. } => Some(name.clone()),
            _ => None,
        };
        self.send_op(
            Command::Write(op),
            Pending {
                paths,
                commit,
                branch_delete,
            },
        )
    }

    pub(crate) fn network(&mut self, op: NetworkOp) {
        if self
            .running
            .values()
            .any(|r| r.label == fgc_actor::network_label(op))
        {
            return;
        }
        self.send_op(Command::Network(op), Pending::default());
    }

    /// Op lifecycle events from the actor.
    pub(crate) fn apply_op_started(&mut self, id: u64, label: String, command: String) {
        let progress = if command.is_empty() {
            String::new()
        } else {
            command
        };
        self.running.insert(id, Running { label, progress });
    }

    pub(crate) fn apply_op_progress(&mut self, id: u64, line: String) {
        if let Some(r) = self.running.get_mut(&id) {
            r.progress = line;
        }
    }

    pub(crate) fn apply_op_finished(
        &mut self,
        id: u64,
        label: String,
        command: String,
        result: Result<(), String>,
    ) {
        self.running.remove(&id);
        let pending = self.pending_ops.remove(&id).unwrap_or_default();
        for p in &pending.paths {
            self.pending_paths.remove(p);
        }
        match result {
            Ok(()) => {
                if pending.commit {
                    self.commit_clear = true;
                    self.amend = false;
                }
                if pending.branch_delete.is_some() {
                    self.confirm_delete = None;
                    self.force_delete = None;
                }
                self.toast = Some(Toast {
                    ok: true,
                    text: if command.is_empty() { label } else { command },
                    at: Instant::now(),
                });
            }
            Err(e) => {
                if let Some(name) = pending.branch_delete {
                    if e.contains("not fully merged") {
                        self.force_delete = Some(name);
                    }
                }
                self.toast = Some(Toast {
                    ok: false,
                    text: format!("{label} failed: {e}"),
                    at: Instant::now(),
                });
            }
        }
    }

    // ---- actions ----

    pub(crate) fn do_commit(&mut self, cx: &mut Context<Self>) {
        let message = self.commit_input.read(cx).text().to_owned();
        if message.trim().is_empty() {
            self.toast = Some(Toast {
                ok: false,
                text: "Write a commit message first".into(),
                at: Instant::now(),
            });
            cx.notify();
            return;
        }
        if self.running.values().any(|r| r.label.contains("ommit")) {
            return;
        }
        self.write(WriteOp::Commit {
            message,
            amend: self.amend,
        });
        cx.notify();
    }

    pub(crate) fn toggle_amend(&mut self, cx: &mut Context<Self>) {
        self.amend = !self.amend;
        // Amending with an empty box: start from HEAD's message.
        if self.amend && self.commit_input.read(cx).text().trim().is_empty() {
            if let Some(head) = self.info.as_ref().and_then(|i| i.head) {
                match self.metas.get(&head) {
                    Some(m) => {
                        let summary = m.summary.to_string();
                        self.commit_input
                            .update(cx, |i, cx| i.set_text(summary, cx));
                    }
                    None => {
                        self.prefill_amend = true;
                        self.request_metas(vec![head]);
                    }
                }
            }
        }
        cx.notify();
    }

    fn do_create_branch(&mut self, cx: &mut Context<Self>) {
        let name = self.branch_input.read(cx).text().trim().to_owned();
        let Some(Selection::Commit(at)) = self.selection.clone() else {
            return;
        };
        if name.is_empty() {
            return;
        }
        self.write(WriteOp::CreateBranch {
            name,
            at,
            checkout: true,
        });
        self.branch_input_open = false;
        self.branch_input.update(cx, |i, cx| i.set_text("", cx));
        cx.notify();
    }

    /// Stage or unstage file `i` of the working-tree file list.
    pub(crate) fn toggle_file(&mut self, i: usize) {
        let Some(entry) = self.doc.files.get(i) else {
            return;
        };
        let section = self.doc.sections[entry.diff].0;
        let f = &self.doc.diff(entry.diff).files[entry.file as usize];
        let Some(path) = f.path().cloned() else {
            return;
        };
        // A rename: stage/unstage both sides.
        let mut paths = vec![path];
        if let Some(old) = f.old_path.clone() {
            if !paths.contains(&old) {
                paths.push(old);
            }
        }
        if section == "Staged" {
            self.write(WriteOp::Unstage(paths));
        } else {
            self.write(WriteOp::Stage(paths));
        }
    }

    pub(crate) fn toggle_hunk(&mut self, diff: usize, hunk: u32) {
        let section = self.doc.sections[diff].0;
        let d = self.doc.diff(diff);
        let Some(file) = d.files.iter().find(|f| f.hunks.contains(&hunk)) else {
            return;
        };
        let Some(path) = file.path().cloned() else {
            return;
        };
        let sel = HunkSel::of(&d.hunks[hunk as usize]);
        if section == "Staged" {
            self.write(WriteOp::UnstageHunk { path, hunk: sel });
        } else {
            self.write(WriteOp::StageHunk { path, hunk: sel });
        }
    }

    pub(crate) fn stage_all(&mut self, staged_section: bool) {
        let want = if staged_section { "Staged" } else { "Unstaged" };
        let mut paths = Vec::new();
        for e in &self.doc.files {
            if self.doc.sections[e.diff].0 != want {
                continue;
            }
            let f = &self.doc.diff(e.diff).files[e.file as usize];
            paths.extend(f.path().cloned());
            if let Some(old) = &f.old_path {
                if !paths.contains(old) {
                    paths.push(old.clone());
                }
            }
        }
        if paths.is_empty() {
            return;
        }
        if staged_section {
            self.write(WriteOp::Unstage(paths));
        } else {
            self.write(WriteOp::Stage(paths));
        }
    }

    pub(crate) fn open_repository(&mut self, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Open repository".into()),
        });
        cx.spawn(async move |this, cx| {
            let chosen = match rx.await {
                Ok(Ok(Some(paths))) => paths.into_iter().next(),
                Ok(Err(e)) => {
                    let _ = this.update(cx, |v, cx| {
                        v.toast = Some(Toast {
                            ok: false,
                            text: format!("Can't show a folder picker ({e}); pass the path on the command line: fgc <path>"),
                            at: Instant::now(),
                        });
                        cx.notify();
                    });
                    None
                }
                _ => None,
            };
            if let Some(path) = chosen {
                let _ = cx.update(|cx| {
                    if let Err(e) = crate::app::open_repo_window(path.clone(), crate::app::WindowConfig::default(), cx) {
                        let _ = this.update(cx, |v, cx| {
                            v.toast = Some(Toast {
                                ok: false,
                                text: format!("{}: {e}", path.display()),
                                at: Instant::now(),
                            });
                            cx.notify();
                        });
                    }
                });
            }
        })
        .detach();
    }

    // ---- rendering ----

    fn running_label(&self, label: &str) -> Option<&Running> {
        self.running.values().find(|r| r.label == label)
    }

    pub(crate) fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let net =
            |id: &'static str, label: &'static str, busy_label: &'static str, op: NetworkOp| {
                let busy = self.running_label(label).is_some();
                button(id, if busy { busy_label } else { label }, !busy).on_click(cx.listener(
                    move |this, _, _, cx| {
                        this.network(op);
                        cx.notify();
                    },
                ))
            };
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .child(
                button("open", "Open…", true)
                    .on_click(cx.listener(|this, _, _, cx| this.open_repository(cx))),
            )
            .child(div().w(px(8.)))
            .child(net("fetch", "Fetch", "Fetching…", NetworkOp::Fetch))
            .child(net(
                "pull",
                "Pull",
                "Pulling…",
                NetworkOp::Pull { rebase: false },
            ))
            .child(net("push", "Push", "Pushing…", NetworkOp::Push))
            .child(div().w(px(8.)))
            .child(
                button("stash", "Stash", true).on_click(cx.listener(|this, _, _, cx| {
                    this.write(WriteOp::StashPush(None));
                    cx.notify();
                })),
            )
            .child(
                button("pop", "Pop", true).on_click(cx.listener(|this, _, _, cx| {
                    this.write(WriteOp::StashPop);
                    cx.notify();
                })),
            )
    }

    /// Working-tree header: commit message box, amend, commit, stage all.
    pub(crate) fn render_commit_box(&self, cx: &mut Context<Self>) -> AnyElement {
        let staged = self.status.as_ref().map_or(0, |s| s.staged().count());
        let unstaged = self.status.as_ref().map_or(0, |s| s.unstaged().count());
        let committing = self.running.values().any(|r| r.label.contains("ommit"));
        let can_commit = (staged > 0 || self.amend) && !committing;
        let commit_label = match (committing, self.amend) {
            (true, _) => "Committing…".to_string(),
            (false, true) => "Amend".to_string(),
            (false, false) => format!("Commit {staged} file{}", if staged == 1 { "" } else { "s" }),
        };
        div()
            .flex_shrink_0()
            .p_3()
            .flex()
            .flex_col()
            .gap_2()
            .border_b_1()
            .border_color(theme::border())
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .font_weight(FontWeight::BOLD)
                            .child("Uncommitted changes"),
                    )
                    .child(
                        div()
                            .text_color(theme::muted())
                            .child(format!("{staged} staged · {unstaged} unstaged")),
                    )
                    .child(div().flex_1())
                    .child(
                        button("stage-all", "Stage all", unstaged > 0).on_click(cx.listener(
                            |this, _, _, cx| {
                                this.stage_all(false);
                                cx.notify();
                            },
                        )),
                    )
                    .child(
                        button("unstage-all", "Unstage all", staged > 0).on_click(cx.listener(
                            |this, _, _, cx| {
                                this.stage_all(true);
                                cx.notify();
                            },
                        )),
                    ),
            )
            .child(self.commit_input.clone())
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .id("amend")
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_1()
                            .cursor_pointer()
                            .on_click(cx.listener(|this, _, _, cx| this.toggle_amend(cx)))
                            .child(
                                div()
                                    .size(px(13.))
                                    .rounded(px(3.))
                                    .border_1()
                                    .border_color(theme::muted())
                                    .when(self.amend, |d| d.bg(theme::hunk_fg())),
                            )
                            .child("Amend last commit"),
                    )
                    .child(div().flex_1())
                    .child(
                        div()
                            .text_color(theme::muted())
                            .text_size(px(11.))
                            .child("ctrl/cmd-enter"),
                    )
                    .child(
                        button("commit", commit_label, can_commit)
                            .when(can_commit, |b| b.bg(theme::head_badge()))
                            .on_click(cx.listener(|this, _, _, cx| this.do_commit(cx))),
                    ),
            )
            .into_any_element()
    }

    /// Commit details: branch actions for the selected commit.
    pub(crate) fn render_branch_actions(&self, oid: Oid, cx: &mut Context<Self>) -> AnyElement {
        let refs = self.refs_by_oid.get(&oid).cloned().unwrap_or_default();
        let mut row = div()
            .flex()
            .flex_row()
            .flex_wrap()
            .items_center()
            .gap_1()
            .mt_1();
        for r in refs
            .iter()
            .filter(|r| matches!(r.kind, RefKind::LocalBranch | RefKind::RemoteBranch))
        {
            let name = r.shorthand.to_string();
            let local = r.kind == RefKind::LocalBranch;
            // A remote branch whose local counterpart is right here needs no
            // checkout button of its own.
            let shadowed = !local
                && name.split_once('/').is_some_and(|(_, short)| {
                    refs.iter()
                        .any(|o| o.kind == RefKind::LocalBranch && &*o.shorthand == short)
                });
            if !r.is_head && !shadowed {
                let n = name.clone();
                row = row.child(
                    button(
                        SharedString::from(format!("co-{name}")),
                        format!("Check out {name}"),
                        true,
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.write(WriteOp::Checkout(n.clone()));
                        cx.notify();
                    })),
                );
            }
            if local && !r.is_head {
                let n = name.clone();
                let confirming = self.confirm_delete.as_deref() == Some(&name);
                let forcing = self.force_delete.as_deref() == Some(&name);
                let label = if forcing {
                    format!("Delete {name} anyway (unmerged)")
                } else if confirming {
                    format!("Confirm delete {name}")
                } else {
                    format!("Delete {name}")
                };
                row = row.child(
                    button(SharedString::from(format!("del-{name}")), label, true)
                        .when(confirming || forcing, |b| b.text_color(theme::del_fg()))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if this.force_delete.as_deref() == Some(&n) {
                                this.write(WriteOp::DeleteBranch {
                                    name: n.clone(),
                                    force: true,
                                });
                            } else if this.confirm_delete.as_deref() == Some(&n) {
                                this.write(WriteOp::DeleteBranch {
                                    name: n.clone(),
                                    force: false,
                                });
                            } else {
                                this.confirm_delete = Some(n.clone());
                            }
                            cx.notify();
                        })),
                );
            }
        }
        row = row.child(
            button("new-branch", "New branch here…", true).on_click(cx.listener(
                |this, _, window, cx| {
                    this.branch_input_open = true;
                    let input = this.branch_input.clone();
                    window.focus(&input.read(cx).focus_handle(cx));
                    cx.notify();
                },
            )),
        );
        let mut col = div().flex().flex_col().gap_1().child(row);
        if self.branch_input_open {
            col = col.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(div().flex_1().child(self.branch_input.clone()))
                    .child(
                        div()
                            .text_color(theme::muted())
                            .text_size(px(11.))
                            .child("enter: create & check out · esc: cancel"),
                    ),
            );
        }
        col.into_any_element()
    }

    pub(crate) fn render_status_bar(&self) -> impl IntoElement {
        let running: Vec<String> = self
            .running
            .values()
            .map(|r| {
                if r.progress.is_empty() {
                    format!("{}…", r.label)
                } else {
                    format!("{}: {}", r.label, r.progress)
                }
            })
            .collect();
        let toast = self
            .toast
            .as_ref()
            .filter(|t| !t.ok || t.at.elapsed() < Duration::from_secs(8));
        div()
            .h(px(24.))
            .flex_shrink_0()
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .px_3()
            .bg(theme::header())
            .border_t_1()
            .border_color(theme::border())
            .text_size(px(12.))
            .when_some(toast, |d, t| {
                d.child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .font_family(if t.ok {
                            theme::MONO_FONT
                        } else {
                            theme::UI_FONT
                        })
                        .text_color(if t.ok {
                            theme::add_fg()
                        } else {
                            theme::del_fg()
                        })
                        .child(format!(
                            "{} {}",
                            if t.ok { "✓" } else { "✗" },
                            t.text.replace('\n', " · ")
                        )),
                )
            })
            .when(toast.is_none(), |d| d.child(div().flex_1()))
            .children(running.into_iter().map(|r| {
                div()
                    .text_color(theme::hunk_fg())
                    .max_w(px(520.))
                    .truncate()
                    .child(r)
            }))
    }
}

pub(crate) type RunningOps = BTreeMap<u64, Running>;
