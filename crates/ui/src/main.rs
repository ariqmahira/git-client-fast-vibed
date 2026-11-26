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
use fgc_ui::app::{WindowConfig, bind_keys, open_repo_window, open_welcome_window};
use fgc_ui::bench::{Bench, BenchOptions, EXIT_CODE};
use gpui::{App, Application};

#[derive(Parser)]
#[command(about = "A fast git client")]
struct Args {
    /// Repository to open (any path inside it). Without one, the current
    /// directory is used if it's a repository, else a welcome window opens.
    repo: Option<PathBuf>,
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
    // An explicit path must be a repository; the implicit "." may not be.
    let repo = match &args.repo {
        Some(p) => {
            if let Err(e) = fgc_git_backend::Git2Backend::open(p) {
                eprintln!("fgc: {e}");
                std::process::exit(2);
            }
            Some(p.clone())
        }
        None => fgc_git_backend::Git2Backend::open(".")
            .ok()
            .map(|_| PathBuf::from(".")),
    };
    if args.bench && repo.is_none() {
        eprintln!("fgc: --bench needs a repository");
        std::process::exit(2);
    }
    let bench = args.bench.then(|| {
        Bench::new(BenchOptions {
            frames: args.bench_frames,
            inputs: args.bench_inputs,
            report: args.report.clone(),
            budgets: args.budgets.clone().zip(args.profile.clone()),
        })
    });
    let actor = ActorOptions {
        graph: EngineOptions {
            order: match args.order {
                Order::Date => GraphOrder::Date,
                Order::Topo => GraphOrder::Topo,
            },
            ..Default::default()
        },
        // The benchmark measures rendering; keep the watcher out of it.
        watch: !args.bench,
        ..Default::default()
    };

    Application::new().run(move |cx: &mut App| {
        bind_keys(cx);
        match repo {
            Some(path) => {
                let config = WindowConfig {
                    actor,
                    bench,
                    process_start: Some(process_start),
                };
                if let Err(e) = open_repo_window(path, config, cx) {
                    eprintln!("fgc: {e}");
                    EXIT_CODE.store(2, Ordering::SeqCst);
                    cx.quit();
                }
            }
            None => open_welcome_window(cx),
        }
        cx.activate(true);
    });
    std::process::exit(EXIT_CODE.load(Ordering::SeqCst));
}
