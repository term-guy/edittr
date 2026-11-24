//! Turns a unified diff into aligned side-by-side rows.

use std::ops::Range;

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
    /// Set for binary files, renames without content, etc.
    pub note: Option<String>,
}

pub fn side_by_side(unified: &str) -> SideBySide {
    let mut rows = Vec::new();
    let mut note = None;
    let (mut dels, mut adds): (Vec<Side>, Vec<Side>) = (Vec::new(), Vec::new());
    let (mut ln, mut rn) = (0usize, 0usize);
    let mut in_hunk = false;
    let mut last_old_end = 0usize;

    for line in unified.lines() {
        if let Some(h) = line.strip_prefix("@@ ") {
            flush(&mut rows, &mut dels, &mut adds);
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
                flush(&mut rows, &mut dels, &mut adds);
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
                flush(&mut rows, &mut dels, &mut adds);
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
                in_hunk = false; // next file header in a multi-file diff
            }
        }
    }
    flush(&mut rows, &mut dels, &mut adds);
    if rows.is_empty() && note.is_none() {
        note = Some(if unified.trim().is_empty() {
            "No changes".into()
        } else {
            "No text changes (mode or rename only)".into()
        });
    }
    SideBySide { rows, note }
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
        s.and_then(|s| s.strip_prefix(sign))
            .and_then(|s| s.split(',').next())
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(1)
    };
    let old = num(it.next(), '-');
    let new = num(it.next(), '+');
    // An empty side is reported as start 0 ("-0,0"); lines then start at 1.
    (old.max(1), new.max(1))
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
    fn highlights_only_the_changed_part() {
        assert_eq!(changed_ranges("let x = 1;", "let x = 42;"), (8..9, 8..10));
        assert_eq!(changed_ranges("abc", "abc"), (3..3, 3..3));
        assert_eq!(changed_ranges("aXa", "aa"), (1..2, 1..1)); // suffix can't overlap prefix
        assert_eq!(changed_ranges("héllo", "hallo"), (1..2, 1..2));
    }
}
