//! The git CLI, for what must behave exactly like the user's terminal:
//! network operations (credential helpers, SSH config, LFS) and commits when
//! hooks or signing are configured (plan §3). Never on interactive paths.

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use fgc_core::{Error, Result};

fn base(git_dir: &Path, workdir: Option<&Path>) -> Command {
    let mut cmd = Command::new("git");
    cmd.arg("--git-dir").arg(git_dir);
    if let Some(w) = workdir {
        cmd.arg("--work-tree").arg(w).current_dir(w);
    } else {
        cmd.current_dir(git_dir);
    }
    // Never block on an interactive prompt: there's no terminal to answer it.
    cmd.env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "never")
        .args(["-c", "color.ui=false"])
        .stdin(Stdio::null());
    cmd
}

/// Human-readable form of a command for the "equivalent git command" display.
pub(crate) fn display(args: &[&str]) -> String {
    let mut s = String::from("git");
    for a in args {
        s.push(' ');
        if a.contains(' ') {
            s.push('"');
            s.push_str(a);
            s.push('"');
        } else {
            s.push_str(a);
        }
    }
    s
}

/// Runs git, streaming stderr progress lines (git separates progress
/// updates with `\r`) to `progress`, at most ~30 per second.
pub(crate) fn run_streaming(
    git_dir: &Path,
    workdir: Option<&Path>,
    args: &[&str],
    progress: &dyn Fn(&str),
) -> Result<String> {
    let mut child = base(git_dir, workdir)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| Error::Engine(format!("cannot run git: {e}")))?;
    let mut stderr = child.stderr.take().expect("piped");
    let mut stdout = child.stdout.take().expect("piped");
    let (tx, rx) = mpsc::channel::<String>();
    let reader = std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        let mut line = Vec::new();
        let mut all = Vec::new();
        loop {
            let n = match stderr.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            all.extend_from_slice(&buf[..n]);
            for &b in &buf[..n] {
                if b == b'\r' || b == b'\n' {
                    if !line.is_empty() {
                        let _ = tx.send(String::from_utf8_lossy(&line).into_owned());
                        line.clear();
                    }
                } else {
                    line.push(b);
                }
            }
        }
        if !line.is_empty() {
            let _ = tx.send(String::from_utf8_lossy(&line).into_owned());
        }
        all
    });
    let out_reader = std::thread::spawn(move || {
        let mut s = Vec::new();
        let _ = stdout.read_to_end(&mut s);
        s
    });

    let mut last_sent = Instant::now() - Duration::from_secs(1);
    let mut pending: Option<String> = None;
    loop {
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(l) => pending = Some(l),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        if last_sent.elapsed() >= Duration::from_millis(33) {
            if let Some(l) = pending.take() {
                progress(&l);
                last_sent = Instant::now();
            }
        }
    }
    if let Some(l) = pending.take() {
        progress(&l);
    }
    let err_all = reader.join().unwrap_or_default();
    let out_all = out_reader.join().unwrap_or_default();
    let status = child.wait()?;
    if status.success() {
        Ok(String::from_utf8_lossy(&out_all).into_owned())
    } else {
        let text = String::from_utf8_lossy(&err_all);
        // The last meaningful lines carry the reason (auth failure, rejected push...).
        let reason: Vec<&str> = text
            .split(['\r', '\n'])
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect();
        let tail = reason[reason.len().saturating_sub(4)..].join("\n");
        Err(Error::Engine(format!("{} failed: {tail}", display(args))))
    }
}

/// Runs git with `input` on stdin, returning stdout (for `commit -F -`).
pub(crate) fn run_with_input(
    git_dir: &Path,
    workdir: Option<&Path>,
    args: &[&str],
    input: &str,
) -> Result<String> {
    use std::io::Write;
    let mut child = base(git_dir, workdir)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| Error::Engine(format!("cannot run git: {e}")))?;
    child
        .stdin
        .take()
        .expect("piped")
        .write_all(input.as_bytes())?;
    let out = child.wait_with_output()?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        let mut msg = String::from_utf8_lossy(&out.stderr).trim().to_owned();
        if msg.is_empty() {
            msg = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        }
        Err(Error::Engine(format!("{} failed: {msg}", display(args))))
    }
}

/// Runs git and waits, ignoring its exit status (for maintenance commands
/// whose non-zero exit just reports state, like `update-index --refresh`).
pub(crate) fn run_quiet(git_dir: &Path, workdir: Option<&Path>, args: &[&str]) -> Result<()> {
    base(git_dir, workdir)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|_| ())
        .map_err(|e| Error::Engine(format!("cannot run git: {e}")))
}
