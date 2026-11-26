//! A diff (or several, e.g. staged + unstaged) flattened into display rows
//! for a virtualized list. Building is one pass over small integers (no
//! text copies), so even 100k-line diffs index in well under a millisecond.

use std::sync::Arc;

use fgc_core::{Diff, FileChange};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Row {
    /// "Staged" / "Unstaged" heading (working-tree view).
    Section(usize),
    File {
        diff: usize,
        file: u32,
    },
    Hunk {
        diff: usize,
        hunk: u32,
    },
    Line {
        diff: usize,
        line: u32,
    },
    Binary {
        diff: usize,
    },
}

#[derive(Debug, Clone)]
pub struct FileEntry {
    pub diff: usize,
    pub file: u32,
    /// Index of the file's header row.
    pub row: usize,
}

#[derive(Debug, Clone, Default)]
pub struct DiffDoc {
    pub sections: Vec<(&'static str, Arc<Diff>)>,
    pub rows: Vec<Row>,
    pub files: Vec<FileEntry>,
}

impl DiffDoc {
    pub fn new(sections: Vec<(&'static str, Arc<Diff>)>) -> DiffDoc {
        let show_headings = sections.len() > 1;
        let total: usize = sections
            .iter()
            .map(|(_, d)| d.lines.len() + d.hunks.len() + d.files.len())
            .sum();
        let mut rows = Vec::with_capacity(total + sections.len());
        let mut files = Vec::new();
        for (di, (_, diff)) in sections.iter().enumerate() {
            if show_headings && !diff.files.is_empty() {
                rows.push(Row::Section(di));
            }
            for (fi, f) in diff.files.iter().enumerate() {
                files.push(FileEntry {
                    diff: di,
                    file: fi as u32,
                    row: rows.len(),
                });
                rows.push(Row::File {
                    diff: di,
                    file: fi as u32,
                });
                if f.binary {
                    rows.push(Row::Binary { diff: di });
                    continue;
                }
                for h in f.hunks.clone() {
                    rows.push(Row::Hunk { diff: di, hunk: h });
                    let lines = diff.hunks[h as usize].lines.clone();
                    rows.extend(lines.map(|l| Row::Line { diff: di, line: l }));
                }
            }
        }
        DiffDoc {
            sections,
            rows,
            files,
        }
    }

    pub fn diff(&self, i: usize) -> &Diff {
        &self.sections[i].1
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
}

pub fn change_letter(c: FileChange) -> &'static str {
    match c {
        FileChange::Added => "A",
        FileChange::Deleted => "D",
        FileChange::Modified => "M",
        FileChange::Renamed => "R",
        FileChange::Copied => "C",
        FileChange::TypeChange => "T",
        FileChange::Untracked => "?",
        FileChange::Unreadable => "!",
        FileChange::Conflicted => "U",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fgc_core::{DiffLine, FileDiff, Hunk, LineKind};

    #[test]
    fn rows_and_files() {
        let d = Diff {
            files: vec![
                FileDiff {
                    old_path: Some("a".into()),
                    new_path: Some("a".into()),
                    change: FileChange::Modified,
                    binary: false,
                    additions: 1,
                    deletions: 0,
                    hunks: 0..1,
                },
                FileDiff {
                    old_path: None,
                    new_path: Some("b.bin".into()),
                    change: FileChange::Added,
                    binary: true,
                    additions: 0,
                    deletions: 0,
                    hunks: 1..1,
                },
            ],
            hunks: vec![Hunk {
                header: "@@ -1 +1,2 @@".into(),
                old_start: 1,
                old_lines: 1,
                new_start: 1,
                new_lines: 2,
                lines: 0..2,
            }],
            lines: vec![
                DiffLine {
                    kind: LineKind::Context,
                    old_lineno: Some(1),
                    new_lineno: Some(1),
                    text: 0..1,
                },
                DiffLine {
                    kind: LineKind::Addition,
                    old_lineno: None,
                    new_lineno: Some(2),
                    text: 1..2,
                },
            ],
            text: "xy".into(),
        };
        let doc = DiffDoc::new(vec![("Commit", Arc::new(d))]);
        assert_eq!(
            doc.rows,
            vec![
                Row::File { diff: 0, file: 0 },
                Row::Hunk { diff: 0, hunk: 0 },
                Row::Line { diff: 0, line: 0 },
                Row::Line { diff: 0, line: 1 },
                Row::File { diff: 0, file: 1 },
                Row::Binary { diff: 0 },
            ]
        );
        assert_eq!(
            doc.files.iter().map(|f| f.row).collect::<Vec<_>>(),
            vec![0, 4]
        );

        // Two sections get headings.
        let two = DiffDoc::new(vec![
            ("Staged", doc.sections[0].1.clone()),
            ("Unstaged", doc.sections[0].1.clone()),
        ]);
        assert_eq!(two.rows[0], Row::Section(0));
        assert_eq!(two.rows[7], Row::Section(1));
        assert_eq!(two.files.len(), 4);
    }
}
