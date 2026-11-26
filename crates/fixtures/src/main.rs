//! Generates benchmark repositories.
//!
//! ```text
//! fgc-fixtures bench-repos            # CI set: small, history-200k, wide-100k
//! fgc-fixtures bench-repos huge-1m3   # 1.3M-commit linux.git stand-in (bare)
//! ```
//!
//! linux.git itself is fetched by `scripts/fetch-bench-repos.sh`.

use std::path::{Path, PathBuf};
use std::time::Instant;

use clap::{Parser, ValueEnum};
use fgc_fixtures::{SyntheticSpec, small_repo, synthetic_history, wide_tree};

#[derive(Parser)]
#[command(about = "Generate benchmark repositories for the fast git client")]
struct Args {
    /// Output directory; each profile becomes a subdirectory.
    out: PathBuf,
    /// Profiles to generate. Default: small, history-200k, wide-100k.
    profiles: Vec<Profile>,
    /// Regenerate even if the directory already exists.
    #[arg(long)]
    force: bool,
}

#[derive(Clone, Copy, ValueEnum, PartialEq, Eq)]
enum Profile {
    /// Tiny repo with branches, tags, merge and a dirty worktree.
    Small,
    /// 200k commits, 8 parallel topics, with checkout.
    #[value(name = "history-200k")]
    History200k,
    /// 1.3M commits, bare: linux.git-scale history without the download.
    #[value(name = "huge-1m3")]
    Huge,
    /// 100k files committed: monorepo working tree for status.
    #[value(name = "wide-100k")]
    Wide100k,
}

impl Profile {
    fn dir_name(self) -> &'static str {
        match self {
            Profile::Small => "small",
            Profile::History200k => "history-200k",
            Profile::Huge => "huge-1m3",
            Profile::Wide100k => "wide-100k",
        }
    }
}

fn main() -> std::io::Result<()> {
    let args = Args::parse();
    let profiles = if args.profiles.is_empty() {
        vec![Profile::Small, Profile::History200k, Profile::Wide100k]
    } else {
        args.profiles.clone()
    };
    std::fs::create_dir_all(&args.out)?;
    for p in profiles {
        let dir = args.out.join(p.dir_name());
        if dir.exists() {
            if !args.force {
                eprintln!("{}: exists, skipping (use --force)", dir.display());
                continue;
            }
            std::fs::remove_dir_all(&dir)?;
        }
        let start = Instant::now();
        generate(p, &dir)?;
        eprintln!("{}: generated in {:.1?}", dir.display(), start.elapsed());
    }
    Ok(())
}

fn generate(p: Profile, dir: &Path) -> std::io::Result<()> {
    match p {
        Profile::Small => {
            std::fs::create_dir_all(dir)?;
            small_repo(dir);
            Ok(())
        }
        Profile::History200k => synthetic_history(
            dir,
            &SyntheticSpec {
                commits: 200_000,
                ..Default::default()
            },
        ),
        Profile::Huge => synthetic_history(
            dir,
            &SyntheticSpec {
                commits: 1_300_000,
                max_topics: 16,
                files: 20_000,
                tag_every: 20_000,
                bare: true,
                ..Default::default()
            },
        ),
        Profile::Wide100k => wide_tree(dir, 100_000),
    }
}
