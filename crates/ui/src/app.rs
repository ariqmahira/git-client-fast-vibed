//! Application setup: key bindings, repository windows, the welcome window.

use std::path::PathBuf;
use std::time::Instant;

use fgc_actor::ActorOptions;
use gpui::{
    App, Bounds, Context, KeyBinding, Render, Window, WindowBounds, WindowOptions, div, prelude::*,
    px, size,
};

use crate::bench::Bench;
use crate::perf::Perf;
use crate::theme;
use crate::view::{
    Commit, Fetch, NextFile, OpenRepository, PageDown, PageUp, PrevFile, Pull, Push, Quit,
    RepoView, SelectFirst, SelectLast, SelectNext, SelectPrev, ShowWorkingTree, ToggleDebugOverlay,
};

/// Everything a repository window needs besides the path.
#[derive(Default)]
pub struct WindowConfig {
    pub actor: ActorOptions,
    pub bench: Option<Bench>,
    /// Process start, for first-paint timing of the first window.
    pub process_start: Option<Instant>,
}

pub fn bind_keys(cx: &mut App) {
    // Navigation keys must not fire while typing in a text field.
    let nav = Some("RepoView && !TextInput");
    let view = Some("RepoView");
    cx.bind_keys([
        KeyBinding::new("down", SelectNext, nav),
        KeyBinding::new("j", SelectNext, nav),
        KeyBinding::new("up", SelectPrev, nav),
        KeyBinding::new("k", SelectPrev, nav),
        KeyBinding::new("pagedown", PageDown, nav),
        KeyBinding::new("pageup", PageUp, nav),
        KeyBinding::new("home", SelectFirst, nav),
        KeyBinding::new("end", SelectLast, nav),
        KeyBinding::new("w", ShowWorkingTree, nav),
        KeyBinding::new("]", NextFile, nav),
        KeyBinding::new("[", PrevFile, nav),
        KeyBinding::new("ctrl-shift-d", ToggleDebugOverlay, view),
        KeyBinding::new("cmd-shift-d", ToggleDebugOverlay, view),
        KeyBinding::new("ctrl-o", OpenRepository, view),
        KeyBinding::new("cmd-o", OpenRepository, view),
        KeyBinding::new("ctrl-shift-f", Fetch, view),
        KeyBinding::new("cmd-shift-f", Fetch, view),
        KeyBinding::new("ctrl-shift-l", Pull, view),
        KeyBinding::new("cmd-shift-l", Pull, view),
        KeyBinding::new("ctrl-shift-p", Push, view),
        KeyBinding::new("cmd-shift-p", Push, view),
        KeyBinding::new("ctrl-enter", Commit, nav),
        KeyBinding::new("cmd-enter", Commit, nav),
        KeyBinding::new("ctrl-q", Quit, None),
        KeyBinding::new("cmd-q", Quit, None),
    ]);
    crate::text_input::bind_keys(cx);
    cx.on_action(|_: &Quit, cx| cx.quit());
    // Closing the last window quits (Linux/Windows convention; also what
    // users expect from a single-purpose tool on macOS).
    cx.on_window_closed(|cx| {
        if cx.windows().is_empty() {
            cx.quit();
        }
    })
    .detach();
}

fn window_options(cx: &App) -> WindowOptions {
    let bounds = Bounds::centered(None, size(px(1440.), px(900.)), cx);
    WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        ..Default::default()
    }
}

/// Opens a repository window. Fails (without a window) if `path` isn't a
/// repository.
pub fn open_repo_window(path: PathBuf, config: WindowConfig, cx: &mut App) -> fgc_core::Result<()> {
    fgc_git_backend::Git2Backend::open(&path)?;
    let perf = Perf::new(config.process_start.unwrap_or_else(Instant::now));
    let WindowConfig { actor, bench, .. } = config;
    let result = cx.open_window(window_options(cx), |window, cx| {
        cx.new(|cx| {
            RepoView::new(path, actor, perf, bench, window, cx)
                .expect("repository was opened successfully a moment ago")
        })
    });
    result
        .map(|_| ())
        .map_err(|e| fgc_core::Error::Engine(format!("cannot open window: {e}")))
}

/// Shown when fgc starts outside a repository.
pub struct Welcome {
    message: Option<String>,
}

impl Welcome {
    fn open(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Open repository".into()),
        });
        let handle = window.window_handle();
        cx.spawn(async move |this, cx| {
            let result = rx.await;
            let _ = cx.update(|cx| match result {
                Ok(Ok(Some(paths))) => {
                    if let Some(path) = paths.into_iter().next() {
                        match open_repo_window(path.clone(), WindowConfig::default(), cx) {
                            Ok(()) => {
                                let _ = handle.update(cx, |_, window, _| window.remove_window());
                            }
                            Err(e) => {
                                let _ = this.update(cx, |w, cx| {
                                    w.message = Some(format!("{}: {e}", path.display()));
                                    cx.notify();
                                });
                            }
                        }
                    }
                }
                Ok(Err(e)) => {
                    let _ = this.update(cx, |w, cx| {
                        w.message = Some(format!(
                            "Can't show a folder picker on this system ({e}). Start fgc with a path instead: fgc ~/code/my-repo"
                        ));
                        cx.notify();
                    });
                }
                _ => {}
            });
        })
        .detach();
    }
}

impl Render for Welcome {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_3()
            .bg(theme::bg())
            .text_color(theme::text())
            .font_family(theme::UI_FONT)
            .text_size(px(theme::TEXT_SIZE))
            .child(div().text_size(px(22.)).child("fgc"))
            .child(
                div()
                    .text_color(theme::muted())
                    .child("Not inside a git repository."),
            )
            .child(
                div()
                    .id("welcome-open")
                    .px_4()
                    .py_1p5()
                    .rounded(px(5.))
                    .cursor_pointer()
                    .bg(theme::head_badge())
                    .hover(|s| s.bg(theme::branch_badge()))
                    .on_click(cx.listener(|this, _, window, cx| this.open(window, cx)))
                    .child("Open repository…"),
            )
            .child(
                div()
                    .text_color(theme::muted())
                    .text_size(px(12.))
                    .child("or run: fgc path/to/repo"),
            )
            .when_some(self.message.clone(), |d, m| {
                d.child(div().text_color(theme::del_fg()).max_w(px(560.)).child(m))
            })
    }
}

pub fn open_welcome_window(cx: &mut App) {
    let bounds = Bounds::centered(None, size(px(640.), px(360.)), cx);
    let _ = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            ..Default::default()
        },
        |_, cx| cx.new(|_| Welcome { message: None }),
    );
}
