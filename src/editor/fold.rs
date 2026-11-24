//! Code folding by indentation (like VS Code's default), and the mapping between
//! display rows and document lines when some regions are folded.

use super::buffer::LineIndex;
use std::collections::BTreeSet;
use std::ops::Range;

/// Foldable regions: `start` (the header line, stays visible) -> `end` (exclusive,
/// first line after the region). A region exists where following non-blank lines are
/// indented deeper than the header; it ends before the first line indented no deeper.
pub fn regions(text: &str, ix: &LineIndex) -> Vec<Range<usize>> {
    let n = ix.lines();
    // Indent width per line (tab = 4), None for blank lines.
    let indent: Vec<Option<usize>> = (0..n)
        .map(|l| {
            let line = &text[ix.line_range(text, l)];
            if line.trim().is_empty() {
                return None;
            }
            Some(
                line.chars()
                    .take_while(|c| *c == ' ' || *c == '\t')
                    .map(|c| if c == '\t' { 4 } else { 1 })
                    .sum(),
            )
        })
        .collect();
    let mut out = Vec::new();
    // Stack of (header line, header indent) still open.
    let mut stack: Vec<(usize, usize)> = Vec::new();
    let mut last_non_blank = 0;
    for l in 0..n {
        let Some(ind) = indent[l] else { continue };
        while let Some(&(start, sind)) = stack.last() {
            if ind > sind {
                break;
            }
            stack.pop();
            // A closing line at the header's indent (`}`, `end`) stays visible after the fold.
            if last_non_blank > start {
                out.push(start..last_non_blank + 1);
            }
        }
        // Does the next non-blank line go deeper? Then this line opens a region.
        if let Some(next) = (l + 1..n).find_map(|k| indent[k])
            && next > ind
        {
            stack.push((l, ind));
        }
        last_non_blank = l;
    }
    while let Some((start, _)) = stack.pop() {
        if last_non_blank > start {
            out.push(start..last_non_blank + 1);
        }
    }
    out.sort_by_key(|r| r.start);
    out
}

/// Which document lines are shown, given the folded region starts.
#[derive(Default, Clone)]
pub struct View {
    /// Visible lines in order; None = all lines visible (no folds).
    rows: Option<Vec<usize>>,
    lines: usize,
}

impl View {
    pub fn new(lines: usize, regions: &[Range<usize>], folded: &BTreeSet<usize>) -> Self {
        if folded.is_empty() {
            return Self { rows: None, lines };
        }
        let mut hidden = vec![false; lines];
        for r in regions.iter().filter(|r| folded.contains(&r.start)) {
            for h in hidden.iter_mut().take(r.end.min(lines)).skip(r.start + 1) {
                *h = true;
            }
        }
        let rows = (0..lines).filter(|&l| !hidden[l]).collect();
        Self { rows: Some(rows), lines }
    }

    pub fn count(&self) -> usize {
        self.rows.as_ref().map_or(self.lines, Vec::len)
    }

    pub fn line(&self, row: usize) -> usize {
        match &self.rows {
            None => row.min(self.lines.saturating_sub(1)),
            Some(r) => r.get(row).copied().unwrap_or_else(|| r.last().copied().unwrap_or(0)),
        }
    }

    /// Display row of `line` (a hidden line maps to its fold's header row).
    pub fn row(&self, line: usize) -> usize {
        match &self.rows {
            None => line,
            Some(r) => r.partition_point(|&l| l <= line).saturating_sub(1),
        }
    }

    pub fn is_hidden(&self, line: usize) -> bool {
        self.rows.as_ref().is_some_and(|r| r.binary_search(&line).is_err())
    }

    /// The visible line `rows` display rows away from `line`.
    #[cfg(test)]
    pub fn step(&self, line: usize, rows: isize) -> usize {
        let row = self.row(line) as isize + rows;
        self.line(row.clamp(0, self.count() as isize - 1) as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indentation_regions_and_view() {
        let text = "fn a() {\n    if x {\n        y();\n    }\n\n    z();\n}\nfn b() {}\n";
        let ix = LineIndex::new(text);
        let r = regions(text, &ix);
        // fn a: lines 1..=5 hidden (the closing `}` on line 6 stays); `if x`: line 2 hidden.
        assert_eq!(r, vec![0..6, 1..3]);
        let mut folded = BTreeSet::new();
        folded.insert(1);
        let v = View::new(ix.lines(), &r, &folded);
        assert!(v.is_hidden(2) && !v.is_hidden(3));
        assert_eq!(v.row(3), 2);
        assert_eq!(v.row(2), 1); // hidden line -> its header
        assert_eq!(v.step(1, 1), 3);
        folded.insert(0);
        let v = View::new(ix.lines(), &r, &folded);
        assert_eq!(v.line(1), 6);
        assert_eq!(v.count(), ix.lines() - 5);
    }

    #[test]
    fn python_style_regions_without_closers() {
        let text = "def f():\n    a = 1\n    return a\nx = 2\n";
        let ix = LineIndex::new(text);
        assert_eq!(regions(text, &ix), vec![0..3]);
    }
}
