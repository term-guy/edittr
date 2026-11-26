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

/// Which document lines are shown, given the folded region starts, and (with word wrap)
/// how many display rows each shown line takes.
#[derive(Default, Clone)]
pub struct View {
    /// Visible lines in order; None = all lines visible (no folds).
    rows: Option<Vec<usize>>,
    lines: usize,
    /// Word wrap: first display row of each visible line, plus the total at the end.
    /// None = one row per visible line.
    starts: Option<Vec<usize>>,
}

impl View {
    pub fn new(lines: usize, regions: &[Range<usize>], folded: &BTreeSet<usize>) -> Self {
        if folded.is_empty() {
            return Self {
                rows: None,
                lines,
                starts: None,
            };
        }
        let mut hidden = vec![false; lines];
        for r in regions.iter().filter(|r| folded.contains(&r.start)) {
            for h in hidden.iter_mut().take(r.end.min(lines)).skip(r.start + 1) {
                *h = true;
            }
        }
        let rows = (0..lines).filter(|&l| !hidden[l]).collect();
        Self {
            rows: Some(rows),
            lines,
            starts: None,
        }
    }

    /// The same view with `rows_of(line)` display rows per line (word wrap).
    pub fn wrapped(&self, mut rows_of: impl FnMut(usize) -> usize) -> Self {
        let visible = self.visible_count();
        let mut starts = Vec::with_capacity(visible + 1);
        let mut row = 0;
        for i in 0..visible {
            starts.push(row);
            row += rows_of(self.visible_line(i)).max(1);
        }
        starts.push(row);
        Self {
            rows: self.rows.clone(),
            lines: self.lines,
            starts: Some(starts),
        }
    }

    fn visible_count(&self) -> usize {
        self.rows.as_ref().map_or(self.lines, Vec::len)
    }

    fn visible_line(&self, i: usize) -> usize {
        match &self.rows {
            None => i.min(self.lines.saturating_sub(1)),
            Some(r) => r.get(i).copied().unwrap_or_else(|| r.last().copied().unwrap_or(0)),
        }
    }

    /// Index among the visible lines (a hidden line maps to its fold's header).
    fn visible_index(&self, line: usize) -> usize {
        match &self.rows {
            None => line,
            Some(r) => r.partition_point(|&l| l <= line).saturating_sub(1),
        }
    }

    /// Display rows in total.
    pub fn count(&self) -> usize {
        match &self.starts {
            Some(starts) => starts.last().copied().unwrap_or(0),
            None => self.visible_count(),
        }
    }

    /// The line shown on display row `row`.
    pub fn line(&self, row: usize) -> usize {
        match &self.starts {
            Some(starts) => {
                let i = starts[..starts.len().saturating_sub(1)]
                    .partition_point(|&s| s <= row)
                    .saturating_sub(1);
                self.visible_line(i)
            }
            None => self.visible_line(row),
        }
    }

    /// First display row of `line` (a hidden line maps to its fold's header row).
    pub fn row(&self, line: usize) -> usize {
        let i = self.visible_index(line);
        match &self.starts {
            Some(starts) => starts.get(i).copied().unwrap_or_else(|| self.count()),
            None => i,
        }
    }

    /// Display rows `line` takes (1 without word wrap).
    pub fn rows_of(&self, line: usize) -> usize {
        match &self.starts {
            Some(starts) => {
                let i = self.visible_index(line);
                starts.get(i + 1).zip(starts.get(i)).map_or(1, |(b, a)| b - a)
            }
            None => 1,
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
    fn wrapped_rows_map_to_lines_and_back() {
        let text = "fn a() {\n    x;\n}\nlong\nend\n";
        let ix = LineIndex::new(text);
        let r = regions(text, &ix);
        // Line 3 takes three rows, everything else one.
        let rows = |l: usize| if l == 3 { 3 } else { 1 };
        let v = View::new(ix.lines(), &r, &BTreeSet::new()).wrapped(rows);
        assert_eq!(v.count(), ix.lines() + 2);
        assert_eq!((v.row(3), v.rows_of(3), v.row(4)), (3, 3, 6));
        assert_eq!([v.line(3), v.line(4), v.line(5), v.line(6)], [3, 3, 3, 4]);
        // Folded and wrapped: line 1 is hidden, so line 3 starts on row 2.
        let v = View::new(ix.lines(), &r, &BTreeSet::from([0])).wrapped(rows);
        assert!(v.is_hidden(1));
        assert_eq!((v.row(3), v.line(4), v.row(4)), (2, 3, 5));
        assert_eq!(v.row(1), 0);
    }

    #[test]
    fn python_style_regions_without_closers() {
        let text = "def f():\n    a = 1\n    return a\nx = 2\n";
        let ix = LineIndex::new(text);
        assert_eq!(regions(text, &ix), vec![0..3]);
    }
}
