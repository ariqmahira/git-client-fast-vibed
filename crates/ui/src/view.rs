//! The repository window: commit graph, commit details, file list, diff.
//!
//! All Git work goes through the [`RepoActor`]; this view only renders
//! immutable snapshots it receives and sends commands. Every list is
//! virtualized (`uniform_list`): per frame we touch only visible rows.

use std::collections::{HashMap, HashSet, VecDeque};
use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use fgc_actor::{ActorOptions, Command, Event, GraphSnapshot, Phase, RepoActor};
use fgc_core::{CommitMeta, LineKind, Oid, RefInfo, RefKind, RepoInfo, StatusSnapshot};
use futures::StreamExt;
use gpui::{
    AnyElement, App, Context, FocusHandle, Focusable, FontWeight, InteractiveElement, IntoElement,
    ParentElement, Render, ScrollStrategy, SharedString, Styled, UniformListScrollHandle, Window,
    actions, canvas, div, prelude::*, px, uniform_list,
};

use crate::bench::Bench;
use crate::diff_doc::{DiffDoc, Row, change_letter};
use crate::graph_paint::graph_cell;
use crate::perf::SharedPerf;
use crate::theme::{self, DIFF_ROW_H, FILE_ROW_H, MAX_GRAPH_LANES, ROW_H};
use crate::util::{format_time, rss_bytes};

actions!(
    fgc,
    [
        SelectNext,
        SelectPrev,
        PageDown,
        PageUp,
        SelectFirst,
        SelectLast,
        ShowWorkingTree,
        NextFile,
        PrevFile,
        ToggleDebugOverlay,
        Quit
    ]
);

/// Rows of metadata requested around the visible window (scroll headroom).
const META_PREFETCH: usize = 120;
/// Metadata cache size before it is reset.
const META_CACHE_MAX: usize = 20_000;
/// Rows that may newly gain text per frame. Shaping text is the dominant
/// cost of a frame after a jump (40 fresh rows ≈ 2,500 glyphs); spreading
/// it over a few frames keeps each frame inside the 8.3 ms budget, and the
/// text fills in within ~2 frames, faster than the eye notices.
const TEXT_REVEAL_PER_FRAME: usize = 8;
const PAGE_ROWS: usize = 30;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selection {
    Commit(Oid),
    WorkingTree,
}

pub struct RepoView {
    pub(crate) actor: RepoActor,
    repo_name: SharedString,
    focus: FocusHandle,
    pub(crate) perf: SharedPerf,

    info: Option<Arc<RepoInfo>>,
    refs_by_oid: HashMap<Oid, Vec<RefInfo>>,
    pub(crate) snapshot: Option<Arc<GraphSnapshot>>,
    metas: HashMap<Oid, CommitMeta>,
    metas_in_flight: HashSet<Oid>,
    /// Rows drawn with text in the previous frame (their shaped text is
    /// still cached) and in the current one. The list processor can run more
    /// than once per frame (GPUI measures one item first), so the current
    /// frame's set accumulates and is rotated at the start of each render.
    text_shown_prev: HashSet<Oid>,
    text_shown: HashSet<Oid>,
    diff_error: Option<String>,
    revealed_this_frame: usize,
    status: Option<Arc<StatusSnapshot>>,

    pub(crate) selection: Option<Selection>,
    pub(crate) selected_row: Option<usize>,
    pub(crate) doc: DiffDoc,
    pub(crate) doc_for: Option<Selection>,
    selected_file: Option<usize>,

    pub(crate) graph_scroll: UniformListScrollHandle,
    pub(crate) diff_scroll: UniformListScrollHandle,
    files_scroll: UniformListScrollHandle,

    overlay: bool,
    timings: VecDeque<(&'static str, Duration, String)>,
    errors: VecDeque<String>,
    pub(crate) bench: Option<Bench>,
}

impl RepoView {
    pub fn new(
        repo: PathBuf,
        opts: ActorOptions,
        perf: SharedPerf,
        bench: Option<Bench>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> fgc_core::Result<Self> {
        let (tx, mut rx) = futures::channel::mpsc::unbounded::<Event>();
        let actor = RepoActor::open(&repo, opts, move |ev| {
            let _ = tx.unbounded_send(ev);
        })?;

        // Actor events → view state, batched per wakeup.
        cx.spawn(async move |this, cx| {
            while let Some(first) = rx.next().await {
                let mut batch = vec![first];
                while let Ok(ev) = rx.try_recv() {
                    batch.push(ev);
                }
                let alive = this.update(cx, |view, cx| {
                    for ev in batch {
                        view.apply(ev);
                    }
                    cx.notify();
                });
                if alive.is_err() {
                    break;
                }
            }
        })
        .detach();

        let focus = cx.focus_handle();
        window.focus(&focus);
        let repo_name = repo
            .canonicalize()
            .unwrap_or(repo.clone())
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| repo.display().to_string());

        let view = RepoView {
            actor,
            repo_name: repo_name.into(),
            focus,
            perf,
            info: None,
            refs_by_oid: HashMap::new(),
            snapshot: None,
            metas: HashMap::new(),
            metas_in_flight: HashSet::new(),
            text_shown_prev: HashSet::new(),
            text_shown: HashSet::new(),
            diff_error: None,
            revealed_this_frame: 0,
            status: None,
            selection: None,
            selected_row: None,
            doc: DiffDoc::default(),
            doc_for: None,
            selected_file: None,
            graph_scroll: UniformListScrollHandle::new(),
            diff_scroll: UniformListScrollHandle::new(),
            files_scroll: UniformListScrollHandle::new(),
            overlay: false,
            timings: VecDeque::new(),
            errors: VecDeque::new(),
            bench,
        };
        if view.bench.is_some() {
            cx.on_next_frame(window, |this, window, cx| this.bench_tick(window, cx));
        }
        Ok(view)
    }

    fn bench_tick(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(mut bench) = self.bench.take() else {
            return;
        };
        let keep_going = bench.tick(self, window, cx);
        self.bench = Some(bench);
        if keep_going {
            cx.on_next_frame(window, |this, window, cx| this.bench_tick(window, cx));
            cx.notify();
        }
    }

    fn apply(&mut self, ev: Event) {
        match ev {
            Event::RepoInfo(info) => self.info = Some(info),
            Event::Refs(refs) => {
                self.refs_by_oid.clear();
                for r in refs.iter() {
                    self.refs_by_oid
                        .entry(r.target)
                        .or_default()
                        .push(r.clone());
                }
                for v in self.refs_by_oid.values_mut() {
                    // HEAD branch first, then branches, remotes, tags.
                    v.sort_by_key(|r| (!r.is_head, r.kind as u8, r.shorthand.clone()));
                }
            }
            Event::Graph(snap) => {
                let first = self.snapshot.is_none();
                {
                    let mut perf = self.perf.borrow_mut();
                    let t = perf.process_start.elapsed();
                    if snap.phase == Phase::Complete {
                        perf.graph_complete.get_or_insert(t);
                    }
                    perf.graph_first.get_or_insert(t);
                }
                // Keep the user's place: selection follows its commit.
                if let Some(Selection::Commit(oid)) = &self.selection {
                    let row = snap.layout.topology().row_of(oid).map(|r| r as usize);
                    if row.is_some() || snap.phase != Phase::Provisional {
                        self.selected_row = row;
                    }
                }
                self.snapshot = Some(snap);
                if first && self.selection.is_none() && self.rows() > 0 {
                    self.select_row(0);
                }
            }
            Event::Metas(metas) => {
                if self.metas.len() > META_CACHE_MAX {
                    self.metas.clear();
                }
                for m in metas {
                    self.metas_in_flight.remove(&m.oid);
                    self.metas.insert(m.oid, m);
                }
            }
            Event::CommitDiff { oid, diff } => {
                if self.selection == Some(Selection::Commit(oid)) {
                    self.set_doc(DiffDoc::new(vec![("Commit", diff)]), Selection::Commit(oid));
                }
            }
            Event::Status(st) => self.status = Some(st),
            Event::WorkdirDiff {
                staged, unstaged, ..
            } => {
                if self.selection == Some(Selection::WorkingTree) {
                    self.set_doc(
                        DiffDoc::new(vec![("Staged", staged), ("Unstaged", unstaged)]),
                        Selection::WorkingTree,
                    );
                }
            }
            Event::Timing {
                op,
                duration,
                detail,
            } => {
                self.timings.push_front((op, duration, detail));
                self.timings.truncate(10);
            }
            Event::Error { op, message } => {
                if matches!(op, "commit_diff" | "workdir_diff") {
                    self.diff_error = Some(message.clone());
                    self.doc = DiffDoc::default();
                    self.doc_for = self.selection.clone();
                }
                self.errors.push_front(format!("{op}: {message}"));
                self.errors.truncate(4);
            }
        }
    }

    fn set_doc(&mut self, doc: DiffDoc, for_: Selection) {
        {
            let mut perf = self.perf.borrow_mut();
            if perf.diff_open_at.is_some() {
                perf.diff_ready = true;
            }
        }
        self.doc = doc;
        self.doc_for = Some(for_);
        self.diff_error = None;
        self.selected_file = None;
        self.diff_scroll
            .scroll_to_item_strict(0, ScrollStrategy::Top);
        self.files_scroll
            .scroll_to_item_strict(0, ScrollStrategy::Top);
    }

    /// Rows that can be drawn right now (provisional/partial snapshots
    /// cover a prefix of the graph).
    pub(crate) fn metas_len(&self) -> usize {
        self.metas.len()
    }

    pub(crate) fn rows(&self) -> usize {
        self.snapshot
            .as_ref()
            .map_or(0, |s| s.layout.rows_laid() as usize)
    }

    pub(crate) fn select_row(&mut self, row: usize) {
        let Some(snap) = &self.snapshot else { return };
        if row >= snap.layout.rows_laid() as usize {
            return;
        }
        let oid = snap.layout.topology().oid(row as u32);
        self.selected_row = Some(row);
        if self.selection != Some(Selection::Commit(oid)) {
            self.selection = Some(Selection::Commit(oid));
            self.diff_error = None;
            self.actor.send(Command::CommitDiff(oid));
            if !self.metas.contains_key(&oid) {
                self.request_metas(vec![oid]);
            }
        }
    }

    fn request_metas(&mut self, oids: Vec<Oid>) {
        let want: Vec<Oid> = oids
            .into_iter()
            .filter(|o| !self.metas.contains_key(o) && self.metas_in_flight.insert(*o))
            .collect();
        if !want.is_empty() {
            self.actor.send(Command::Metas(want));
        }
    }

    pub(crate) fn move_selection(&mut self, delta: isize) {
        let n = self.rows();
        if n == 0 {
            return;
        }
        let cur = self.selected_row.map_or(-1, |r| r as isize);
        let next = (cur + delta).clamp(0, n as isize - 1) as usize;
        self.select_row(next);
        self.graph_scroll
            .scroll_to_item(next, ScrollStrategy::Center);
    }

    fn show_working_tree(&mut self) {
        self.selection = Some(Selection::WorkingTree);
        self.selected_row = None;
        self.diff_error = None;
        self.actor.send(Command::WorkdirDiff(None));
        self.actor.send(Command::Status);
    }

    fn select_file(&mut self, i: usize) {
        if let Some(entry) = self.doc.files.get(i) {
            self.selected_file = Some(i);
            self.diff_scroll
                .scroll_to_item_strict(entry.row, ScrollStrategy::Top);
            self.files_scroll.scroll_to_item(i, ScrollStrategy::Center);
        }
    }

    fn input(&self) {
        self.perf.borrow_mut().input_received();
    }

    // ---- rendering ----

    fn graph_lanes(&self) -> u16 {
        self.snapshot
            .as_ref()
            .map_or(1, |s| s.layout.max_lanes().clamp(1, MAX_GRAPH_LANES))
    }

    fn render_graph_rows(
        &mut self,
        range: Range<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let Some(snap) = self.snapshot.clone() else {
            return Vec::new();
        };
        let layout = &snap.layout;
        let topo = layout.topology();
        let laid = layout.rows_laid() as usize;
        let lanes = self.graph_lanes();
        let mut geoms = layout.rows_clipped(range.start as u32..range.end as u32, lanes);

        // Metadata for visible rows plus headroom, requested in one batch.
        let pre = range.start.saturating_sub(META_PREFETCH)..(range.end + META_PREFETCH).min(laid);
        let missing: Vec<Oid> = pre
            .map(|r| topo.oid(r as u32))
            .filter(|o| !self.metas.contains_key(o) && !self.metas_in_flight.contains(o))
            .collect();
        if !missing.is_empty() {
            self.request_metas(missing);
        }

        let mut out = Vec::with_capacity(range.len());
        let mut reveal = TEXT_REVEAL_PER_FRAME.saturating_sub(self.revealed_this_frame);
        let mut deferred = false;
        for (i, row) in range.clone().enumerate() {
            let oid = topo.oid(row as u32);
            let geom = geoms.get_mut(i).map(std::mem::take);
            let selected = self.selected_row == Some(row);
            let mut meta = self.metas.get(&oid);
            if meta.is_some()
                && !self.text_shown_prev.contains(&oid)
                && !self.text_shown.contains(&oid)
            {
                if reveal > 0 {
                    reveal -= 1;
                    self.revealed_this_frame += 1;
                } else {
                    meta = None;
                    deferred = true;
                }
            }
            if meta.is_some() {
                self.text_shown.insert(oid);
            }
            let refs = self.refs_by_oid.get(&oid);
            out.push(
                div()
                    .id(row)
                    .h(px(ROW_H))
                    .w_full()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .pr_2()
                    .when(selected, |d| d.bg(theme::selection()))
                    .when(!selected, |d| d.hover(|s| s.bg(theme::hover())))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.input();
                        this.select_row(row);
                        cx.notify();
                    }))
                    .child(graph_cell(geom, lanes))
                    .children(refs.into_iter().flatten().take(4).map(badge))
                    .child(div().flex_1().min_w_0().truncate().child(
                        meta.map_or(SharedString::default(), |m| m.summary.to_string().into()),
                    ))
                    .child(
                        div()
                            .w(px(130.))
                            .flex_shrink_0()
                            .truncate()
                            .text_color(theme::muted())
                            .child(meta.map_or(SharedString::default(), |m| {
                                m.author_name.to_string().into()
                            })),
                    )
                    .child(
                        div()
                            .w(px(116.))
                            .flex_shrink_0()
                            .text_color(theme::muted())
                            .child(meta.map_or(SharedString::default(), |m| {
                                format_time(m.author_time, m.author_tz_minutes).into()
                            })),
                    )
                    .child(
                        div()
                            .w(px(64.))
                            .flex_shrink_0()
                            .font_family(theme::MONO_FONT)
                            .text_size(px(theme::MONO_SIZE))
                            .text_color(theme::muted())
                            .child(oid.short(7)),
                    )
                    .into_any_element(),
            );
        }
        if deferred {
            window.request_animation_frame();
        }
        out
    }

    fn render_file_rows(&mut self, range: Range<usize>, cx: &mut Context<Self>) -> Vec<AnyElement> {
        range
            .filter_map(|i| {
                let entry = self.doc.files.get(i)?;
                let diff = self.doc.diff(entry.diff);
                let f = &diff.files[entry.file as usize];
                let path = f
                    .path()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default();
                let section = self.doc.sections[entry.diff].0;
                let selected = self.selected_file == Some(i);
                Some(
                    div()
                        .id(i)
                        .h(px(FILE_ROW_H))
                        .w_full()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .px_2()
                        .when(selected, |d| d.bg(theme::selection()))
                        .when(!selected, |d| d.hover(|s| s.bg(theme::hover())))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.input();
                            this.select_file(i);
                            cx.notify();
                        }))
                        .child(
                            div()
                                .w(px(14.))
                                .flex_shrink_0()
                                .font_family(theme::MONO_FONT)
                                .text_color(match f.change {
                                    fgc_core::FileChange::Added
                                    | fgc_core::FileChange::Untracked => theme::add_fg(),
                                    fgc_core::FileChange::Deleted => theme::del_fg(),
                                    _ => theme::hunk_fg(),
                                })
                                .child(change_letter(f.change)),
                        )
                        .child(div().flex_1().min_w_0().truncate().child(path))
                        .when(self.doc.sections.len() > 1, |d| {
                            d.child(
                                div()
                                    .text_color(theme::muted())
                                    .text_size(px(11.))
                                    .child(section),
                            )
                        })
                        .child(
                            div()
                                .text_color(theme::add_fg())
                                .child(format!("+{}", f.additions)),
                        )
                        .child(
                            div()
                                .text_color(theme::del_fg())
                                .child(format!("-{}", f.deletions)),
                        )
                        .into_any_element(),
                )
            })
            .collect()
    }

    fn render_diff_rows(&mut self, range: Range<usize>) -> Vec<AnyElement> {
        range
            .filter_map(|i| {
                let row = *self.doc.rows.get(i)?;
                let base = div()
                    .h(px(DIFF_ROW_H))
                    .w_full()
                    .flex()
                    .flex_row()
                    .items_center()
                    .font_family(theme::MONO_FONT)
                    .text_size(px(theme::MONO_SIZE));
                Some(
                    match row {
                        Row::Section(s) => base
                            .px_2()
                            .bg(theme::header())
                            .font_family(theme::UI_FONT)
                            .font_weight(FontWeight::BOLD)
                            .child(self.doc.sections[s].0),
                        Row::File { diff, file } => {
                            let f = &self.doc.diff(diff).files[file as usize];
                            let path = match (&f.old_path, &f.new_path) {
                                (Some(o), Some(n)) if o != n => {
                                    format!("{} → {}", o.display(), n.display())
                                }
                                _ => f
                                    .path()
                                    .map(|p| p.display().to_string())
                                    .unwrap_or_default(),
                            };
                            base.px_2()
                                .gap_2()
                                .bg(theme::header())
                                .border_t_1()
                                .border_color(theme::border())
                                .child(
                                    div()
                                        .text_color(theme::hunk_fg())
                                        .child(change_letter(f.change)),
                                )
                                .child(div().flex_1().min_w_0().truncate().child(path))
                        }
                        Row::Hunk { diff, hunk } => base
                            .px_2()
                            .text_color(theme::hunk_fg())
                            .child(self.doc.diff(diff).hunks[hunk as usize].header.to_string()),
                        Row::Binary { .. } => base
                            .px_2()
                            .text_color(theme::muted())
                            .child("Binary file not shown"),
                        Row::Line { diff, line } => {
                            let d = self.doc.diff(diff);
                            let l = &d.lines[line as usize];
                            let (bg, sign, fg) = match l.kind {
                                LineKind::Addition => (Some(theme::add_bg()), "+", theme::add_fg()),
                                LineKind::Deletion => (Some(theme::del_bg()), "-", theme::del_fg()),
                                LineKind::Context => (None, " ", theme::text()),
                                LineKind::NoNewline => (None, "\\", theme::muted()),
                            };
                            let num = |n: Option<u32>| n.map(|n| n.to_string()).unwrap_or_default();
                            base.when_some(bg, |d, bg| d.bg(bg))
                                .child(
                                    div()
                                        .w(px(44.))
                                        .flex_shrink_0()
                                        .text_color(theme::muted())
                                        .child(num(l.old_lineno)),
                                )
                                .child(
                                    div()
                                        .w(px(44.))
                                        .flex_shrink_0()
                                        .text_color(theme::muted())
                                        .child(num(l.new_lineno)),
                                )
                                .child(div().w(px(14.)).flex_shrink_0().text_color(fg).child(sign))
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .whitespace_nowrap()
                                        .overflow_hidden()
                                        .child(d.line_text(l).replace('\t', "    ")),
                                )
                        }
                    }
                    .into_any_element(),
                )
            })
            .collect()
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let branch = self
            .info
            .as_ref()
            .and_then(|i| i.head_branch.clone())
            .map(|b| b.to_string())
            .unwrap_or_else(|| "detached".into());
        let graph = match &self.snapshot {
            None => "loading…".to_string(),
            Some(s) => {
                let total = s.layout.topology().len();
                match s.phase {
                    Phase::Complete => format!("{total} commits"),
                    _ => format!(
                        "{} commits, loading…",
                        s.layout.rows_laid().max(total as u32)
                    ),
                }
            }
        };
        let changes = self.status.as_ref().map_or(0, |s| s.entries.len());
        let wt_selected = self.selection == Some(Selection::WorkingTree);
        div()
            .h(px(34.))
            .flex_shrink_0()
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .px_3()
            .bg(theme::header())
            .border_b_1()
            .border_color(theme::border())
            .child(
                div()
                    .font_weight(FontWeight::BOLD)
                    .child(self.repo_name.clone()),
            )
            .child(div().text_color(theme::muted()).child(branch))
            .child(div().text_color(theme::muted()).child(graph))
            .child(div().flex_1())
            .child(
                div()
                    .id("wt")
                    .px_2()
                    .py_0p5()
                    .rounded(px(4.))
                    .cursor_pointer()
                    .when(wt_selected, |d| d.bg(theme::selection()))
                    .when(!wt_selected, |d| {
                        d.bg(theme::panel()).hover(|s| s.bg(theme::hover()))
                    })
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.input();
                        this.show_working_tree();
                        cx.notify();
                    }))
                    .child(format!("Working tree · {changes}")),
            )
            .child(
                div()
                    .text_color(theme::muted())
                    .text_size(px(11.))
                    .child("ctrl-shift-d: debug"),
            )
    }

    fn render_details(&self) -> impl IntoElement {
        let body: AnyElement = match &self.selection {
            Some(Selection::Commit(oid)) => match self.metas.get(oid) {
                Some(m) => div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .font_weight(FontWeight::BOLD)
                            .truncate()
                            .child(m.summary.to_string()),
                    )
                    .child(div().text_color(theme::muted()).truncate().child(format!(
                        "{} <{}> · {}",
                        m.author_name,
                        m.author_email,
                        format_time(m.author_time, m.author_tz_minutes)
                    )))
                    .child(
                        div()
                            .font_family(theme::MONO_FONT)
                            .text_size(px(theme::MONO_SIZE))
                            .text_color(theme::muted())
                            .child(format!(
                                "{}  parents {}",
                                oid.short(12),
                                m.parents
                                    .iter()
                                    .map(|p| p.short(7))
                                    .collect::<Vec<_>>()
                                    .join(" ")
                            )),
                    )
                    .into_any_element(),
                None => div()
                    .text_color(theme::muted())
                    .child(oid.to_string())
                    .into_any_element(),
            },
            Some(Selection::WorkingTree) => div()
                .font_weight(FontWeight::BOLD)
                .child("Uncommitted changes")
                .into_any_element(),
            None => div()
                .text_color(theme::muted())
                .child("No selection")
                .into_any_element(),
        };
        div()
            .flex_shrink_0()
            .p_3()
            .border_b_1()
            .border_color(theme::border())
            .child(body)
    }

    fn render_overlay(&self) -> impl IntoElement {
        let perf = self.perf.borrow();
        let fs = perf.frame_stats();
        let is = perf.input_stats();
        let mut lines: Vec<String> = vec![
            format!(
                "frame  p50 {:.2}  p99 {:.2}  max {:.2} ms  ({} frames, budget 8.33)",
                fs.p50_us as f64 / 1000.0,
                fs.p99_us as f64 / 1000.0,
                fs.max_us as f64 / 1000.0,
                fs.frames
            ),
            format!(
                "input→paint  p50 {:.2}  p99 {:.2} ms  ({} events, budget 16)",
                is.p50_us as f64 / 1000.0,
                is.p99_us as f64 / 1000.0,
                is.frames
            ),
            format!(
                "first paint {:.0} ms   RSS {:.0} MB",
                perf.first_paint.map_or(0.0, |d| d.as_secs_f64() * 1000.0),
                rss_bytes().unwrap_or(0) as f64 / 1e6
            ),
        ];
        if let Some(s) = &self.snapshot {
            lines.push(format!(
                "graph {:?}: {}/{} rows, {} lanes, metas cached {}",
                s.phase,
                s.layout.rows_laid(),
                s.layout.topology().len(),
                s.layout.max_lanes(),
                self.metas.len()
            ));
        }
        lines.push("recent backend calls:".into());
        for (op, d, detail) in &self.timings {
            lines.push(format!(
                "  {op:<14} {:>8.2} ms  {detail}",
                d.as_secs_f64() * 1000.0
            ));
        }
        for e in &self.errors {
            lines.push(format!("error: {e}"));
        }
        div()
            .absolute()
            .top(px(40.))
            .right(px(8.))
            .w(px(560.))
            .p_2()
            .rounded(px(6.))
            .bg(gpui::rgba(0x000000d8))
            .border_1()
            .border_color(theme::border())
            .font_family(theme::MONO_FONT)
            .text_size(px(11.5))
            .flex()
            .flex_col()
            .children(lines.into_iter().map(|l| div().child(l)))
    }
}

fn badge(r: &RefInfo) -> AnyElement {
    let bg = match r.kind {
        _ if r.is_head => theme::head_badge(),
        RefKind::LocalBranch => theme::branch_badge(),
        RefKind::RemoteBranch => theme::remote_badge(),
        RefKind::Tag => theme::tag_badge(),
        _ => theme::panel(),
    };
    div()
        .flex_shrink_0()
        .px_1p5()
        .rounded(px(3.))
        .bg(bg)
        .text_size(px(11.5))
        .max_w(px(180.))
        .truncate()
        .when(r.is_head, |d| d.font_weight(FontWeight::BOLD))
        .child(r.shorthand.to_string())
        .into_any_element()
}

impl Focusable for RepoView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for RepoView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.perf.borrow_mut().frame_started();
        self.text_shown_prev = std::mem::take(&mut self.text_shown);
        self.revealed_this_frame = 0;
        let rows = self.rows();
        let perf = self.perf.clone();

        div()
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .bg(theme::bg())
            .text_color(theme::text())
            .font_family(theme::UI_FONT)
            .text_size(px(theme::TEXT_SIZE))
            .track_focus(&self.focus)
            .key_context("RepoView")
            .on_action(cx.listener(|this, _: &SelectNext, _, cx| {
                this.input();
                this.move_selection(1);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &SelectPrev, _, cx| {
                this.input();
                this.move_selection(-1);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &PageDown, _, cx| {
                this.input();
                this.move_selection(PAGE_ROWS as isize);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &PageUp, _, cx| {
                this.input();
                this.move_selection(-(PAGE_ROWS as isize));
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &SelectFirst, _, cx| {
                this.input();
                this.move_selection(isize::MIN / 2);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &SelectLast, _, cx| {
                this.input();
                this.move_selection(isize::MAX / 2);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &ShowWorkingTree, _, cx| {
                this.input();
                this.show_working_tree();
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &NextFile, _, cx| {
                this.input();
                let n = this.doc.files.len();
                if n > 0 {
                    let next = this.selected_file.map_or(0, |f| (f + 1).min(n - 1));
                    this.select_file(next);
                }
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &PrevFile, _, cx| {
                this.input();
                if let Some(f) = this.selected_file {
                    this.select_file(f.saturating_sub(1));
                }
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &ToggleDebugOverlay, _, cx| {
                this.input();
                this.overlay = !this.overlay;
                cx.notify();
            }))
            .child(self.render_header(cx))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_row()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .flex()
                            .flex_col()
                            .child(
                                uniform_list(
                                    "graph",
                                    rows,
                                    cx.processor(|this, range, window, cx| {
                                        this.render_graph_rows(range, window, cx)
                                    }),
                                )
                                .track_scroll(self.graph_scroll.clone())
                                .flex_1(),
                            )
                            .when(rows == 0, |d| {
                                d.child(
                                    div()
                                        .p_4()
                                        .text_color(theme::muted())
                                        .child("Loading history…"),
                                )
                            }),
                    )
                    .child(
                        div()
                            .w(px(560.))
                            .flex_shrink_0()
                            .h_full()
                            .flex()
                            .flex_col()
                            .bg(theme::panel())
                            .border_l_1()
                            .border_color(theme::border())
                            .child(self.render_details())
                            .child(
                                uniform_list(
                                    "files",
                                    self.doc.files.len(),
                                    cx.processor(|this, range, _, cx| {
                                        this.render_file_rows(range, cx)
                                    }),
                                )
                                .track_scroll(self.files_scroll.clone())
                                .h(px((self.doc.files.len().clamp(1, 8) as f32) * FILE_ROW_H))
                                .flex_shrink_0()
                                .border_b_1()
                                .border_color(theme::border()),
                            )
                            .when_some(self.diff_error.clone(), |d, e| {
                                d.child(
                                    div()
                                        .p_3()
                                        .text_color(theme::muted())
                                        .child(format!("Can't show this diff: {e}")),
                                )
                            })
                            .when(
                                self.diff_error.is_none()
                                    && self.selection.is_some()
                                    && self.doc_for != self.selection,
                                |d| {
                                    d.child(
                                        div()
                                            .p_3()
                                            .text_color(theme::muted())
                                            .child("Loading diff…"),
                                    )
                                },
                            )
                            .child(
                                uniform_list(
                                    "diff",
                                    self.doc.rows.len(),
                                    cx.processor(|this, range, _, _| this.render_diff_rows(range)),
                                )
                                .track_scroll(self.diff_scroll.clone())
                                .flex_1(),
                            ),
                    ),
            )
            .when(self.overlay, |d| d.child(self.render_overlay()))
            // Painted last: marks the end of the frame for the recorder.
            .child(
                canvas(
                    |_, _, _| (),
                    move |_, _, _, _| perf.borrow_mut().frame_painted(),
                )
                .absolute()
                .size_0(),
            )
    }
}
