//! Fixture and benchmark repositories, created with the real `git` CLI so the
//! repositories look exactly like what users have on disk.
//!
//! All git invocations run with an isolated configuration (no global/system
//! config, fixed identity and dates) so fixtures are reproducible.
//!
//! - [`small_repo`]: a tiny repo with branches, a merge, tags and a dirty
//!   worktree; tests assert against its known shape.
//! - [`synthetic_history`]: large commit histories with parallel topic
//!   branches and merges, generated with `git fast-import` (hundreds of
//!   thousands of commits per minute). A network-free stand-in for linux.git.
//! - [`wide_tree`]: a repo with many files (100k-file monorepo profile) for
//!   status benchmarks.

use std::fs;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Fixed epoch for fixture dates: 2020-01-01T00:00:00Z.
pub const BASE_TIME: i64 = 1_577_836_800;

/// Builds a `git` command with an isolated, deterministic environment.
pub fn git_cmd(dir: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_AUTHOR_NAME", "Fixture Author")
        .env("GIT_AUTHOR_EMAIL", "author@example.com")
        .env("GIT_COMMITTER_NAME", "Fixture Committer")
        .env("GIT_COMMITTER_EMAIL", "committer@example.com")
        .env("GIT_TERMINAL_PROMPT", "0")
        .args([
            "-c",
            "init.defaultBranch=main",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "tag.gpgsign=false",
            "-c",
            "core.autocrlf=false",
            "-c",
            "gc.auto=0",
            "-c",
            "maintenance.auto=false",
        ]);
    cmd
}

fn null_device() -> &'static str {
    if cfg!(windows) { "NUL" } else { "/dev/null" }
}

/// Runs git in `dir`, returning trimmed stdout. Panics with stderr on failure
/// (fixtures are test/bench setup; failing loudly is the right behavior).
pub fn git(dir: &Path, args: &[&str]) -> String {
    git_at(dir, args, None)
}

/// Like [`git`] but with author/committer dates fixed to `time` (Unix seconds).
pub fn git_at(dir: &Path, args: &[&str], time: Option<i64>) -> String {
    let mut cmd = git_cmd(dir);
    cmd.args(args);
    if let Some(t) = time {
        let date = format!("{t} +0000");
        cmd.env("GIT_AUTHOR_DATE", &date)
            .env("GIT_COMMITTER_DATE", &date);
    }
    let out = cmd
        .output()
        .expect("failed to spawn git (is it installed?)");
    if !out.status.success() {
        panic!(
            "git {:?} failed in {}:\n{}",
            args,
            dir.display(),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    String::from_utf8_lossy(&out.stdout).trim_end().to_owned()
}

fn write_file(root: &Path, rel: &str, contents: &str) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, contents).unwrap();
}

/// Known facts about the repository [`small_repo`] creates.
#[derive(Debug, Clone)]
pub struct SmallRepo {
    pub path: PathBuf,
    /// Commit ids (hex), oldest first, in the order they were created:
    /// `[c1, c2, f1, f2, c3, merge]`.
    pub commits: Vec<String>,
    pub head: String,
}

/// Creates a small repository at `dir` (which must exist and be empty):
///
/// ```text
/// *   merge   (main, HEAD, tag v1.0 annotated)   "Merge branch 'feature'"
/// |\
/// | * f2      (feature)                          "feature: second"
/// | * f1                                         "feature: first"
/// * | c3                                         "main: third"
/// |/
/// * c2        (tag v0.1 lightweight)             "main: second"
/// * c1                                           "main: initial"
/// ```
///
/// Worktree state after creation:
/// - `README.md` modified and staged
/// - `src/lib.rs` modified, not staged
/// - `notes/todo.txt` untracked
/// - `docs/guide.md` staged as a new file
pub fn small_repo(dir: &Path) -> SmallRepo {
    git(dir, &["init", "-q"]);
    let mut t = BASE_TIME;
    let mut commits = Vec::new();
    let commit = |dir: &Path, msg: &str, t: &mut i64| {
        *t += 3600;
        git_at(dir, &["commit", "-q", "-m", msg], Some(*t));
        git(dir, &["rev-parse", "HEAD"])
    };

    write_file(dir, "README.md", "# Fixture\n\nA small repo.\n");
    write_file(
        dir,
        "src/lib.rs",
        "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n",
    );
    git(dir, &["add", "-A"]);
    commits.push(commit(dir, "main: initial", &mut t));

    write_file(
        dir,
        "src/lib.rs",
        "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n\npub fn sub(a: i32, b: i32) -> i32 {\n    a - b\n}\n",
    );
    git(dir, &["add", "-A"]);
    commits.push(commit(dir, "main: second", &mut t));
    git(dir, &["tag", "v0.1"]);

    git(dir, &["checkout", "-q", "-b", "feature"]);
    write_file(dir, "feature.txt", "one\n");
    git(dir, &["add", "-A"]);
    commits.push(commit(dir, "feature: first", &mut t));
    write_file(dir, "feature.txt", "one\ntwo\n");
    git(dir, &["add", "-A"]);
    commits.push(commit(dir, "feature: second", &mut t));

    git(dir, &["checkout", "-q", "main"]);
    write_file(dir, "main.txt", "main side\n");
    git(dir, &["add", "-A"]);
    commits.push(commit(dir, "main: third", &mut t));

    t += 3600;
    git_at(
        dir,
        &[
            "merge",
            "-q",
            "--no-ff",
            "-m",
            "Merge branch 'feature'",
            "feature",
        ],
        Some(t),
    );
    commits.push(git(dir, &["rev-parse", "HEAD"]));
    git_at(dir, &["tag", "-a", "v1.0", "-m", "release 1.0"], Some(t));

    // Dirty worktree.
    write_file(dir, "README.md", "# Fixture\n\nA small repo, edited.\n");
    write_file(dir, "docs/guide.md", "# Guide\n");
    git(dir, &["add", "README.md", "docs/guide.md"]);
    write_file(
        dir,
        "src/lib.rs",
        "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n\npub fn sub(a: i32, b: i32) -> i32 {\n    a - b\n}\n\npub fn mul(a: i32, b: i32) -> i32 {\n    a * b\n}\n",
    );
    write_file(dir, "notes/todo.txt", "untracked\n");

    let head = commits.last().unwrap().clone();
    SmallRepo {
        path: dir.to_path_buf(),
        commits,
        head,
    }
}

/// Parameters for [`synthetic_history`].
#[derive(Debug, Clone)]
pub struct SyntheticSpec {
    /// Total commits to create (including merges).
    pub commits: u64,
    /// Max concurrently open topic branches (graph width).
    pub max_topics: usize,
    /// Number of distinct files touched (spread over 32 directories).
    pub files: u32,
    /// A lightweight tag every N steps (0 = none).
    pub tag_every: u64,
    /// Create a bare repository (no checkout).
    pub bare: bool,
    /// Write a commit-graph file afterwards.
    pub commit_graph: bool,
    pub seed: u64,
}

impl Default for SyntheticSpec {
    fn default() -> Self {
        SyntheticSpec {
            commits: 10_000,
            max_topics: 8,
            files: 2_000,
            tag_every: 5_000,
            bare: false,
            commit_graph: true,
            seed: 0x5eed,
        }
    }
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64*
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
}

/// Generates a large history into `dir` (created if missing; must be empty)
/// using `git fast-import`. Topic branches fork from `main`, accumulate
/// commits interleaved in time with `main` and with each other, then merge
/// back, so the graph has realistic parallel lanes. A few topics are left
/// open as real branches at the end.
pub fn synthetic_history(dir: &Path, spec: &SyntheticSpec) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    if spec.bare {
        git(dir, &["init", "-q", "--bare"]);
    } else {
        git(dir, &["init", "-q"]);
    }

    let mut child = git_cmd(dir)
        .args(["fast-import", "--quiet", "--done"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()?;
    let stdin = child.stdin.take().unwrap();
    let mut out = BufWriter::with_capacity(1 << 20, stdin);

    let mut rng = Rng(spec.seed | 1);
    let mut mark: u64 = 0;
    let mut main_tip: Option<u64> = None;
    // Open topics: (slot id, tip mark, commits so far).
    let mut topics: Vec<(usize, u64, u32)> = Vec::new();
    let mut next_slot = 0usize;
    let mut time = BASE_TIME;
    let mut tags = 0u64;

    let ident = |out: &mut BufWriter<_>, time: i64, who: u64| -> io::Result<()> {
        writeln!(
            out,
            "author Dev {who} <dev{who}@example.com> {time} +0000\ncommitter Dev {who} <dev{who}@example.com> {time} +0000"
        )
    };

    let mut step = 0u64;
    while step < spec.commits {
        step += 1;
        time += 30 + rng.below(600) as i64;
        let file_no = rng.below(spec.files as u64);
        let path = format!("d{:02}/f{:05}.txt", file_no % 32, file_no);
        let body = format!("step {step}\n");
        let who = rng.below(50);
        mark += 1;

        let roll = rng.below(100);
        let can_open = topics.len() < spec.max_topics && main_tip.is_some();
        if roll < 8 && can_open {
            // Open a topic: first commit forks from main.
            let slot = next_slot;
            next_slot += 1;
            writeln!(out, "commit refs/heads/topic/{slot}\nmark :{mark}")?;
            ident(&mut out, time, who)?;
            let msg = format!("topic {slot}: start (step {step})");
            writeln!(out, "data {}\n{msg}", msg.len())?;
            writeln!(out, "from :{}", main_tip.unwrap())?;
            writeln!(out, "M 100644 inline {path}\ndata {}\n{body}", body.len())?;
            topics.push((slot, mark, 1));
        } else if roll < 14 && !topics.is_empty() && topics.iter().any(|t| t.2 >= 2) {
            // Merge the oldest topic that has at least two commits into main.
            let idx = topics.iter().position(|t| t.2 >= 2).unwrap();
            let (slot, tip, _) = topics.remove(idx);
            writeln!(out, "commit refs/heads/main\nmark :{mark}")?;
            ident(&mut out, time, who)?;
            let msg = format!("Merge branch 'topic/{slot}' (step {step})");
            writeln!(out, "data {}\n{msg}", msg.len())?;
            writeln!(out, "from :{}", main_tip.unwrap())?;
            writeln!(out, "merge :{tip}")?;
            writeln!(out, "M 100644 inline {path}\ndata {}\n{body}", body.len())?;
            // The merged topic ref is deleted, like a real merged PR branch.
            writeln!(
                out,
                "reset refs/heads/topic/{slot}\nfrom 0000000000000000000000000000000000000000"
            )?;
            main_tip = Some(mark);
        } else if roll < 55 && !topics.is_empty() {
            // Commit on a random open topic.
            let idx = rng.below(topics.len() as u64) as usize;
            let (slot, tip, n) = topics[idx];
            writeln!(out, "commit refs/heads/topic/{slot}\nmark :{mark}")?;
            ident(&mut out, time, who)?;
            let msg = format!("topic {slot}: change {n} (step {step})");
            writeln!(out, "data {}\n{msg}", msg.len())?;
            writeln!(out, "from :{tip}")?;
            writeln!(out, "M 100644 inline {path}\ndata {}\n{body}", body.len())?;
            topics[idx] = (slot, mark, n + 1);
        } else {
            // Commit on main.
            writeln!(out, "commit refs/heads/main\nmark :{mark}")?;
            ident(&mut out, time, who)?;
            let msg = format!("main: change (step {step})");
            writeln!(out, "data {}\n{msg}", msg.len())?;
            if let Some(tip) = main_tip {
                writeln!(out, "from :{tip}")?;
            }
            writeln!(out, "M 100644 inline {path}\ndata {}\n{body}", body.len())?;
            main_tip = Some(mark);
        }
        writeln!(out)?;

        if spec.tag_every > 0 && step % spec.tag_every == 0 {
            if let Some(tip) = main_tip {
                tags += 1;
                writeln!(out, "reset refs/tags/v{tags}\nfrom :{tip}\n")?;
            }
        }
    }
    writeln!(out, "done")?;
    out.flush()?;
    drop(out);
    let status = child.wait()?;
    if !status.success() {
        return Err(io::Error::other(format!(
            "git fast-import failed: {status}"
        )));
    }

    if !spec.bare {
        git(dir, &["reset", "-q", "--hard", "main"]);
    }
    if spec.commit_graph {
        git(dir, &["commit-graph", "write", "--reachable"]);
    }
    Ok(())
}

/// Creates a repository at `dir` with `files` committed files spread over
/// `files / 1000` directories (at least one), mimicking a large monorepo
/// working tree for status benchmarks.
pub fn wide_tree(dir: &Path, files: u32) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    git(dir, &["init", "-q"]);
    let per_dir = 1000u32;
    for i in 0..files {
        let d = dir.join(format!("pkg{:04}", i / per_dir));
        if i % per_dir == 0 {
            fs::create_dir_all(&d)?;
        }
        fs::write(
            d.join(format!("file{:04}.txt", i % per_dir)),
            format!("file {i}\n"),
        )?;
    }
    git(dir, &["add", "-A"]);
    git_at(dir, &["commit", "-q", "-m", "wide tree"], Some(BASE_TIME));
    // Make the index stat info "clean" so status measures the steady state.
    git(dir, &["update-index", "--refresh", "-q"]);
    Ok(())
}

/// Creates a repository whose HEAD commit changes every other line of a
/// 10,000-line Rust file: a 5,000-addition / 5,000-deletion diff in one
/// hunk-dense file (plan §1: "open a 5,000-line diff").
pub fn big_diff(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    git(dir, &["init", "-q"]);
    let render = |variant: &str| {
        let mut s = String::with_capacity(10_000 * 48);
        for i in 0..10_000 {
            if i % 2 == 0 {
                s.push_str(&format!(
                    "    let value_{i} = compute({i}, \"{variant}\");\n"
                ));
            } else {
                s.push_str(&format!("    // line {i}: unchanged context\n"));
            }
        }
        s
    };
    fs::write(dir.join("big.rs"), render("before"))?;
    git(dir, &["add", "-A"]);
    git_at(dir, &["commit", "-q", "-m", "big file"], Some(BASE_TIME));
    fs::write(dir.join("big.rs"), render("after"))?;
    git(dir, &["add", "-A"]);
    git_at(
        dir,
        &["commit", "-q", "-m", "change every other line"],
        Some(BASE_TIME + 60),
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "fgc-fixtures-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn small_repo_shape() {
        let d = tmp("small");
        let r = small_repo(&d);
        assert_eq!(r.commits.len(), 6);
        assert_eq!(git(&d, &["rev-list", "--count", "HEAD"]), "6");
        assert_eq!(git(&d, &["rev-list", "--merges", "--count", "HEAD"]), "1");
        let status = git(&d, &["status", "--porcelain"]);
        assert!(status.contains("M  README.md"), "{status}");
        assert!(status.contains(" M src/lib.rs"), "{status}");
        assert!(status.contains("?? notes/"), "{status}");
        assert!(status.contains("A  docs/guide.md"), "{status}");
        // Deterministic: same ids every time.
        let d2 = tmp("small2");
        assert_eq!(small_repo(&d2).commits, r.commits);
        fs::remove_dir_all(d).ok();
        fs::remove_dir_all(d2).ok();
    }

    #[test]
    fn big_diff_shape() {
        let d = tmp("bigdiff");
        big_diff(&d).unwrap();
        let stat = git(&d, &["diff", "--numstat", "HEAD~1", "HEAD"]);
        assert_eq!(stat, "5000\t5000\tbig.rs");
        fs::remove_dir_all(d).ok();
    }

    #[test]
    fn synthetic_history_counts() {
        let d = tmp("synth");
        let spec = SyntheticSpec {
            commits: 2_000,
            ..Default::default()
        };
        synthetic_history(&d, &spec).unwrap();
        let reachable: u64 = git(&d, &["rev-list", "--all", "--count"]).parse().unwrap();
        assert_eq!(reachable, 2_000);
        let merges: u64 = git(&d, &["rev-list", "--merges", "--count", "main"])
            .parse()
            .unwrap();
        assert!(merges > 50, "expected plenty of merges, got {merges}");
        assert_eq!(git(&d, &["status", "--porcelain"]), "");
        fs::remove_dir_all(d).ok();
    }
}
