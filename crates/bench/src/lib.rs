//! Shared pieces of the benchmark harness.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, Instant};

use fgc_core::{GitBackend, Oid, RefKind, Result};

/// The tips a graph view starts from: HEAD plus every branch and remote
/// branch (tags are only labels; they rarely add unreachable history).
pub fn graph_tips(backend: &dyn GitBackend) -> Result<Vec<Oid>> {
    let mut tips: Vec<Oid> = backend
        .refs()?
        .into_iter()
        .filter(|r| matches!(r.kind, RefKind::LocalBranch | RefKind::RemoteBranch))
        .map(|r| r.target)
        .collect();
    if let Some(head) = backend.repo_info()?.head {
        tips.push(head);
    }
    tips.sort_unstable();
    tips.dedup();
    Ok(tips)
}

/// Timing samples for one named measurement.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Sample {
    pub name: String,
    pub runs_ms: Vec<f64>,
    /// Free-form detail, e.g. "500 commits".
    pub detail: String,
}

impl Sample {
    pub fn median_ms(&self) -> f64 {
        let mut v = self.runs_ms.clone();
        v.sort_by(|a, b| a.total_cmp(b));
        match v.len() {
            0 => f64::NAN,
            n if n % 2 == 1 => v[n / 2],
            n => (v[n / 2 - 1] + v[n / 2]) / 2.0,
        }
    }

    pub fn min_ms(&self) -> f64 {
        self.runs_ms.iter().copied().fold(f64::INFINITY, f64::min)
    }

    pub fn max_ms(&self) -> f64 {
        self.runs_ms
            .iter()
            .copied()
            .fold(f64::NEG_INFINITY, f64::max)
    }
}

/// Times `f` `iters` times.
pub fn time_runs<T>(iters: usize, mut f: impl FnMut() -> Result<T>) -> Result<(Vec<f64>, T)> {
    let mut runs = Vec::with_capacity(iters);
    let mut last = None;
    for _ in 0..iters.max(1) {
        let start = Instant::now();
        let v = f()?;
        runs.push(ms(start.elapsed()));
        last = Some(v);
    }
    Ok((runs, last.expect("at least one run")))
}

pub fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

/// `budgets.toml`: per-profile budgets in milliseconds, keyed by
/// measurement name. Measurements without a budget are reported, not gated.
#[derive(Debug, Default, serde::Deserialize)]
pub struct BudgetFile {
    #[serde(default)]
    pub profiles: BTreeMap<String, BTreeMap<String, f64>>,
}

impl BudgetFile {
    pub fn load(path: &Path) -> std::io::Result<BudgetFile> {
        let text = std::fs::read_to_string(path)?;
        toml::from_str(&text).map_err(|e| std::io::Error::other(e.to_string()))
    }
}
