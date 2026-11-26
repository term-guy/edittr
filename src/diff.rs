//! Turns a unified diff into aligned side-by-side rows.

use std::{borrow::Cow, fmt::Write as _, ops::Range};

pub struct FileDiff {
    pub text: String,
    pub only_line_endings: bool,
}

impl FileDiff {
    pub fn new(text: String, unfiltered: Option<&str>) -> Self {
        let only_line_endings = text.is_empty() && unfiltered.is_some_and(|patch| patch.lines().any(|line| line.starts_with("@@ ")));
        Self { text, only_line_endings }
    }
}

const NO_NEWLINE: &str = "\\ No newline at end of file";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hunk {
    pub old: Range<usize>,
    pub new: Range<usize>,
}

impl Hunk {
    pub fn reversed(self) -> Self {
        Self {
            old: self.new,
            new: self.old,
        }
    }

    pub fn intersects(&self, lines: &Range<usize>) -> bool {
        !self.new.is_empty() && self.new.start < lines.end && self.new.end > lines.start
    }

    pub fn at(&self, line: usize, last_line: usize) -> bool {
        if self.new.is_empty() {
            self.new.start.min(last_line) == line
        } else {
            self.new.contains(&line)
        }
    }

    /// Slice additions and one-for-one replacements on line boundaries.
    pub fn selected(&self, lines: &Range<usize>) -> Option<Self> {
        let start = self.new.start.max(lines.start);
        let end = self.new.end.min(lines.end);
        if start >= end {
            return None;
        }
        if start == self.new.start && end == self.new.end {
            return Some(self.clone());
        }
        let old = if self.old.is_empty() {
            self.old.clone()
        } else if self.old.len() == self.new.len() {
            self.old.start + (start - self.new.start)..self.old.start + (end - self.new.start)
        } else {
            return None;
        };
        Some(Self { old, new: start..end })
    }
}

pub fn changed_blocks(old: &str, new: &str) -> Vec<Hunk> {
    similar::TextDiff::configure()
        .algorithm(similar::Algorithm::Myers)
        .timeout(std::time::Duration::from_millis(500))
        .diff_slices(
            &old.split_inclusive('\n').collect::<Vec<_>>(),
            &new.split_inclusive('\n').collect::<Vec<_>>(),
        )
        .ops()
        .iter()
        .filter(|op| op.tag() != similar::DiffTag::Equal)
        .map(|op| Hunk {
            old: op.old_range(),
            new: op.new_range(),
        })
        .collect()
}

/// A single change, with up to three lines of exact preimage context.
pub fn patch_block(old: &[&str], start: usize, end: usize, insert: &[&str]) -> String {
    let before = start.saturating_sub(3);
    let after = end.saturating_add(3).min(old.len());
    let old_count = after - before;
    let new_count = old_count - (end - start) + insert.len();
    let mut patch = format!(
        "@@ -{},{} +{},{} @@\n",
        before + usize::from(old_count != 0),
        old_count,
        before + usize::from(new_count != 0),
        new_count,
    );
    for (tag, lines) in [
        (' ', &old[before..start]),
        ('-', &old[start..end]),
        ('+', insert),
        (' ', &old[end..after]),
    ] {
        for line in lines {
            patch.push(tag);
            patch.push_str(line);
            if !line.ends_with('\n') {
                let _ = writeln!(patch, "\n{NO_NEWLINE}");
            }
        }
    }
    patch
}

/// The editor stores LF internally and writes its configured line endings on save.
pub fn with_editor_eol(text: &str, crlf: bool) -> Cow<'_, str> {
    if crlf {
        Cow::Owned(text.replace('\n', "\r\n"))
    } else {
        Cow::Borrowed(text)
    }
}

pub fn without_cr_at_eol(text: &str, ignore_eol: bool) -> Cow<'_, str> {
    if ignore_eol && text.as_bytes().windows(2).any(|pair| pair == b"\r\n") {
        Cow::Owned(text.replace("\r\n", "\n"))
    } else {
        Cow::Borrowed(text)
    }
}

/// Generate hunks using normalized comparison keys and the original text for display.
pub fn unified_hunks(old: &str, new: &str, ignore_eol: bool, context: usize, mut header: impl FnMut(&str) -> String) -> String {
    let old_lines: Vec<_> = old.split_inclusive('\n').collect();
    let new_lines: Vec<_> = new.split_inclusive('\n').collect();
    let old = without_cr_at_eol(old, ignore_eol);
    let new = without_cr_at_eol(new, ignore_eol);
    let diff = similar::TextDiff::from_slices(
        &old.split_inclusive('\n').collect::<Vec<_>>(),
        &new.split_inclusive('\n').collect::<Vec<_>>(),
    );
    let mut out = String::new();
    for hunk in diff.unified_diff().context_radius(context).iter_hunks() {
        let _ = writeln!(out, "{}", header(&hunk.header().to_string()));
        for change in hunk.iter_changes() {
            let line = match change.old_index() {
                Some(i) => old_lines[i],
                None => new_lines[change.new_index().unwrap()],
            };
            let _ = write!(out, "{}{line}", change.tag());
            if !line.ends_with('\n') {
                let _ = writeln!(out, "\n{NO_NEWLINE}");
            }
        }
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Kind {
    Same,
    /// Line differs between the two sides (removed on the left, added on the right).
    Changed,
    /// Only on the left.
    Removed,
    /// Only on the right.
    Added,
    /// Gap between hunks: `n` unchanged lines not shown.
    Gap(usize),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Side {
    pub line: usize, // 1-based
    pub text: String,
    /// Char range that actually changed within the line (for Changed rows).
    pub hl: Option<Range<usize>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub kind: Kind,
    pub left: Option<Side>,
    pub right: Option<Side>,
}

pub struct SideBySide {
    pub rows: Vec<Row>,
    pub blocks: Vec<(usize, Hunk)>,
    /// Set for binary files, renames without content, etc.
    pub note: Option<String>,
}

pub fn side_by_side(unified: &str) -> SideBySide {
    let mut rows = Vec::new();
    let mut blocks = Vec::new();
    let mut note = None;
    let (mut dels, mut adds): (Vec<Side>, Vec<Side>) = (Vec::new(), Vec::new());
    let (mut ln, mut rn) = (0usize, 0usize);
    let mut in_hunk = false;
    let mut last_old_end = 0usize;
    let flush = |rows: &mut Vec<Row>, blocks: &mut Vec<(usize, Hunk)>, dels: &mut Vec<Side>, adds: &mut Vec<Side>, ln: usize, rn: usize| {
        if !dels.is_empty() || !adds.is_empty() {
            blocks.push((
                rows.len(),
                Hunk {
                    old: ln - 1 - dels.len()..ln - 1,
                    new: rn - 1 - adds.len()..rn - 1,
                },
            ));
            flush(rows, dels, adds);
        }
    };

    for line in unified.lines() {
        if let Some(h) = line.strip_prefix("@@ ") {
            flush(&mut rows, &mut blocks, &mut dels, &mut adds, ln, rn);
            let (old_start, new_start) = parse_hunk_header(h);
            if old_start > last_old_end + 1 && !rows.is_empty() {
                rows.push(Row {
                    kind: Kind::Gap(old_start - last_old_end - 1),
                    left: None,
                    right: None,
                });
            } else if rows.is_empty() && old_start > 1 {
                rows.push(Row {
                    kind: Kind::Gap(old_start - 1),
                    left: None,
                    right: None,
                });
            }
            ln = old_start;
            rn = new_start;
            in_hunk = true;
            continue;
        }
        if !in_hunk {
            if line.starts_with("Binary files") {
                note = Some("Binary file changed".into());
            }
            continue;
        }
        match line.as_bytes().first() {
            Some(b' ') => {
                flush(&mut rows, &mut blocks, &mut dels, &mut adds, ln, rn);
                let text = line[1..].to_string();
                rows.push(Row {
                    kind: Kind::Same,
                    left: Some(Side {
                        line: ln,
                        text: text.clone(),
                        hl: None,
                    }),
                    right: Some(Side { line: rn, text, hl: None }),
                });
                ln += 1;
                rn += 1;
                last_old_end = ln - 1;
            }
            Some(b'-') => {
                dels.push(Side {
                    line: ln,
                    text: line[1..].to_string(),
                    hl: None,
                });
                ln += 1;
                last_old_end = ln - 1;
            }
            Some(b'+') => {
                adds.push(Side {
                    line: rn,
                    text: line[1..].to_string(),
                    hl: None,
                });
                rn += 1;
            }
            Some(b'\\') => {} // "\ No newline at end of file"
            None => {
                // Some tools strip the leading space of empty context lines.
                flush(&mut rows, &mut blocks, &mut dels, &mut adds, ln, rn);
                rows.push(Row {
                    kind: Kind::Same,
                    left: Some(Side {
                        line: ln,
                        text: String::new(),
                        hl: None,
                    }),
                    right: Some(Side {
                        line: rn,
                        text: String::new(),
                        hl: None,
                    }),
                });
                ln += 1;
                rn += 1;
            }
            _ => {
                flush(&mut rows, &mut blocks, &mut dels, &mut adds, ln, rn);
                in_hunk = false; // next file header in a multi-file diff
            }
        }
    }
    flush(&mut rows, &mut blocks, &mut dels, &mut adds, ln, rn);
    if rows.is_empty() && note.is_none() {
        note = Some(if unified.trim().is_empty() {
            "No changes".into()
        } else {
            "No text changes (mode or rename only)".into()
        });
    }
    SideBySide { rows, blocks, note }
}

/// Pair up a block of removed and added lines row by row.
fn flush(rows: &mut Vec<Row>, dels: &mut Vec<Side>, adds: &mut Vec<Side>) {
    let n = dels.len().max(adds.len());
    let mut d = dels.drain(..);
    let mut a = adds.drain(..);
    for _ in 0..n {
        let (mut l, mut r) = (d.next(), a.next());
        let kind = match (&l, &r) {
            (Some(_), Some(_)) => Kind::Changed,
            (Some(_), None) => Kind::Removed,
            _ => Kind::Added,
        };
        if let (Some(ls), Some(rs)) = (&mut l, &mut r) {
            let (lh, rh) = changed_ranges(&ls.text, &rs.text);
            ls.hl = Some(lh);
            rs.hl = Some(rh);
        }
        rows.push(Row { kind, left: l, right: r });
    }
}

/// "-12,5 +12,7 @@ fn foo" -> (12, 12)
fn parse_hunk_header(h: &str) -> (usize, usize) {
    let mut it = h.split_whitespace();
    let num = |s: Option<&str>, sign: char| {
        let s = s.and_then(|s| s.strip_prefix(sign)).unwrap_or("");
        let (start, count) = s.split_once(',').unwrap_or((s, "1"));
        let start = start.parse::<usize>().unwrap_or(1);
        if count == "0" { start.saturating_add(1) } else { start.max(1) }
    };
    (num(it.next(), '-'), num(it.next(), '+'))
}

/// Char ranges of the differing middle part of two lines (common prefix/suffix removed).
pub fn changed_ranges(a: &str, b: &str) -> (Range<usize>, Range<usize>) {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let prefix = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let max_suffix = a.len().min(b.len()) - prefix;
    let suffix = a
        .iter()
        .rev()
        .zip(b.iter().rev())
        .take(max_suffix)
        .take_while(|(x, y)| x == y)
        .count();
    (prefix..a.len() - suffix, prefix..b.len() - suffix)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn show(s: &SideBySide) -> Vec<String> {
        s.rows
            .iter()
            .map(|r| {
                let side = |x: &Option<Side>| x.as_ref().map(|s| format!("{}:{}", s.line, s.text)).unwrap_or("-".into());
                match r.kind {
                    Kind::Gap(n) => format!("... {n}"),
                    k => format!("{k:?} {} | {}", side(&r.left), side(&r.right)),
                }
            })
            .collect()
    }

    #[test]
    fn aligns_changes() {
        let diff = "diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -3,5 +3,6 @@ ctx\n a\n-b\n-c\n+B\n+C\n+D\n d\n-e\n f\n";
        assert_eq!(
            show(&side_by_side(diff)),
            [
                "... 2",
                "Same 3:a | 3:a",
                "Changed 4:b | 4:B",
                "Changed 5:c | 5:C",
                "Added - | 6:D",
                "Same 6:d | 7:d",
                "Removed 7:e | -",
                "Same 8:f | 8:f",
            ]
        );
    }

    #[test]
    fn gaps_between_hunks_and_new_files() {
        let diff = "@@ -1,2 +1,2 @@\n-a\n+A\n b\n@@ -10,1 +10,1 @@\n-x\n+X\n";
        let rows = show(&side_by_side(diff));
        assert_eq!(rows[2], "... 7");
        let new_file = "--- /dev/null\n+++ b/n\n@@ -0,0 +1,2 @@\n+one\n+two\n";
        assert_eq!(show(&side_by_side(new_file)), ["Added - | 1:one", "Added - | 2:two"]);
        assert_eq!(
            side_by_side("Binary files a/x and b/x differ\n").note.as_deref(),
            Some("Binary file changed")
        );
    }

    #[test]
    fn side_by_side_tracks_change_ranges_with_empty_sides() {
        let diff = "@@ -2,0 +3,1 @@\n+added\n@@ -6,1 +6,0 @@\n-removed\n";
        let sides = side_by_side(diff);
        assert_eq!(
            sides.blocks.iter().map(|(_, h)| h).collect::<Vec<_>>(),
            [&Hunk { old: 2..2, new: 2..3 }, &Hunk { old: 5..6, new: 6..6 }]
        );
    }

    #[test]
    fn highlights_only_the_changed_part() {
        assert_eq!(changed_ranges("let x = 1;", "let x = 42;"), (8..9, 8..10));
        assert_eq!(changed_ranges("abc", "abc"), (3..3, 3..3));
        assert_eq!(changed_ranges("aXa", "aa"), (1..2, 1..1)); // suffix can't overlap prefix
        assert_eq!(changed_ranges("héllo", "hallo"), (1..2, 1..2));
    }
}
