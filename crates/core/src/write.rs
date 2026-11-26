//! Types for write operations (M4) and network operations (M6).

/// Identifies a hunk by its header numbers, exactly as shown in the diff the
/// user acted on (`@@ -old_start,old_lines +new_start,new_lines @@`). If the
/// file changed since, the hunk no longer matches and the operation fails
/// instead of staging something the user didn't see.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct HunkSel {
    pub old_start: u32,
    pub old_lines: u32,
    pub new_start: u32,
    pub new_lines: u32,
}

impl HunkSel {
    pub fn of(h: &crate::Hunk) -> HunkSel {
        HunkSel {
            old_start: h.old_start,
            old_lines: h.old_lines,
            new_start: h.new_start,
            new_lines: h.new_lines,
        }
    }

    /// The same hunk seen from the other side (for reversed diffs).
    pub fn reversed(self) -> HunkSel {
        HunkSel {
            old_start: self.new_start,
            old_lines: self.new_lines,
            new_start: self.old_start,
            new_lines: self.old_lines,
        }
    }
}

/// Network operations, run through the git CLI so credentials, SSH config,
/// hooks and LFS behave exactly as in the terminal (plan §3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NetworkOp {
    /// `git fetch --all --prune`
    Fetch,
    /// `git pull --ff-only`, or `--rebase`
    Pull { rebase: bool },
    /// `git push`, setting the upstream on first push
    Push,
}
