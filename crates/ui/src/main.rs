//! `fgc [REPO]`: open a repository window.
//!
//! `fgc REPO --bench [--budgets budgets.toml --profile NAME] [--report out.json]`
//! runs the scripted UI benchmark and exits (see `bench.rs`). Headless Linux:
//! `scripts/ui-headless.sh target/release/fgc REPO --bench`.

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::time::Instant;

use clap::Parser;

use fgc_actor::{ActorOptions, EngineOptions, GraphOrder};
use fgc_ui::bench::{Bench, BenchOptions, EXIT_CODE};
use fgc_ui::perf::Perf;
use fgc_ui::view::{
    NextFile, PageDown, PageUp, PrevFile, Quit, RepoView, SelectFirst, SelectLast, SelectNext,
    SelectPrev, ShowWorkingTree, ToggleDebugOverlay,
};
use gpui::{
    App, Application, Bounds, KeyBinding, WindowBounds, WindowOptions, prelude::*, px, size,
};

#[derive(Parser)]
#[command(about = "A fast git client")]
struct Args {
    /// Repository to open (any path inside it).
    #[arg(default_value = ".")]
    repo: PathBuf,
    /// Commit graph row order.
    #[arg(long, value_enum, default_value_t = Order::Date)]
    order: Order,
    /// Run the scripted UI benchmark, print results and exit.
    #[arg(long)]
    bench: bool,
    /// Frames per benchmark scroll phase.
    #[arg(long, default_value_t = 600)]
    bench_frames: usize,
    /// Key actions in the input benchmark phase.
    #[arg(long, default_value_t = 100)]
    bench_inputs: usize,
    /// Write the benchmark report (JSON) here.
    #[arg(long)]
    report: Option<PathBuf>,
    /// Budget file to gate the benchmark against (requires --profile).
    #[arg(long, requires = "profile")]
    budgets: Option<PathBuf>,
    #[arg(long)]
    profile: Option<String>,
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum Order {
    Date,
    Topo,
}

fn main() {
    let process_start = Instant::now();
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    let args = Args::parse();
    if let Err(e) = fgc_git_backend_check(&args.repo) {
        eprintln!("fgc: {e}");
        std::process::exit(2);
    }
    let perf = Perf::new(process_start);
    let bench = args.bench.then(|| {
        Bench::new(BenchOptions {
            frames: args.bench_frames,
            inputs: args.bench_inputs,
            report: args.report.clone(),
            budgets: args.budgets.clone().zip(args.profile.clone()),
        })
    });
    let opts = ActorOptions {
        graph: EngineOptions {
            order: match args.order {
                Order::Date => GraphOrder::Date,
                Order::Topo => GraphOrder::Topo,
            },
            ..Default::default()
        },
        ..Default::default()
    };
    let repo = args.repo.clone();

    Application::new().run(move |cx: &mut App| {
        cx.bind_keys([
            KeyBinding::new("down", SelectNext, Some("RepoView")),
            KeyBinding::new("j", SelectNext, Some("RepoView")),
            KeyBinding::new("up", SelectPrev, Some("RepoView")),
            KeyBinding::new("k", SelectPrev, Some("RepoView")),
            KeyBinding::new("pagedown", PageDown, Some("RepoView")),
            KeyBinding::new("pageup", PageUp, Some("RepoView")),
            KeyBinding::new("home", SelectFirst, Some("RepoView")),
            KeyBinding::new("end", SelectLast, Some("RepoView")),
            KeyBinding::new("w", ShowWorkingTree, Some("RepoView")),
            KeyBinding::new("]", NextFile, Some("RepoView")),
            KeyBinding::new("[", PrevFile, Some("RepoView")),
            KeyBinding::new("ctrl-shift-d", ToggleDebugOverlay, Some("RepoView")),
            KeyBinding::new("cmd-shift-d", ToggleDebugOverlay, Some("RepoView")),
            KeyBinding::new("ctrl-q", Quit, None),
            KeyBinding::new("cmd-q", Quit, None),
        ]);
        cx.on_action(|_: &Quit, cx| cx.quit());

        let bounds = Bounds::centered(None, size(px(1440.), px(900.)), cx);
        let result = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            move |window, cx| {
                cx.new(|cx| {
                    RepoView::new(repo, opts, perf, bench, window, cx)
                        .expect("repository was checked before opening the window")
                })
            },
        );
        if let Err(e) = result {
            eprintln!("fgc: cannot open window: {e}");
            EXIT_CODE.store(2, Ordering::SeqCst);
            cx.quit();
        }
        cx.activate(true);
    });
    std::process::exit(EXIT_CODE.load(Ordering::SeqCst));
}

/// Fails fast (before creating a window) if the path isn't a repository.
fn fgc_git_backend_check(path: &std::path::Path) -> fgc_core::Result<()> {
    fgc_git_backend::Git2Backend::open(path).map(|_| ())
}
