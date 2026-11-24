//! Column (box) selection: a rectangle of lines × columns, made with a middle-button
//! drag (like VS Code). Typing, deleting and pasting then edit every line at once.
//!
//! Columns are char offsets into each line, clamped to the line's length, so short
//! lines get their cursor at the line end (and typing there appends to them).

use std::ops::Range;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ColumnSel {
    pub anchor_line: usize,
    pub anchor_col: usize,
    pub head_line: usize,
    pub head_col: usize,
}

pub enum Edit<'a> {
    Insert(&'a str),
    /// One piece per selected line (a multi-line paste with a matching line count).
    InsertPerLine(Vec<&'a str>),
    Backspace,
    Delete,
}

/// Effective "end of line" column for `End`: clamps to each line's length.
pub const LINE_END: usize = usize::MAX / 2;

impl ColumnSel {
    pub fn lines(&self) -> Range<usize> {
        self.anchor_line.min(self.head_line)..self.anchor_line.max(self.head_line) + 1
    }

    fn cols(&self) -> (usize, usize) {
        (self.anchor_col.min(self.head_col), self.anchor_col.max(self.head_col))
    }

    pub fn is_empty(&self) -> bool {
        self.anchor_col == self.head_col
    }

    /// Selected char range within a line of `len` chars.
    pub fn range_in(&self, len: usize) -> Range<usize> {
        let (a, b) = self.cols();
        a.min(len)..b.min(len)
    }

    /// Caret column within a line of `len` chars.
    pub fn caret_in(&self, len: usize) -> usize {
        self.head_col.min(len)
    }

    fn collapse_to(&mut self, col: usize) {
        self.anchor_col = col;
        self.head_col = col;
    }

    /// Move the carets (`extend`: Shift held, move only the head).
    pub fn move_by(&mut self, delta: isize, extend: bool, text: &str) {
        // Start from where the caret is visible: a column past every line end acts as the longest line's end.
        let longest = split_lines(text)[self.lines()].iter().map(|l| line_len(l)).max().unwrap_or(0);
        let head = self.head_col.min(longest);
        let col = if !extend && !self.is_empty() {
            let (a, b) = self.cols();
            if delta < 0 { a.min(longest) } else { b.min(longest) }
        } else {
            head.saturating_add_signed(delta)
        };
        if extend {
            self.anchor_col = self.anchor_col.min(longest);
            self.head_col = col;
        } else {
            self.collapse_to(col);
        }
    }

    pub fn home_end(&mut self, end: bool, extend: bool) {
        let col = if end { LINE_END } else { 0 };
        if extend {
            self.head_col = col;
        } else {
            self.collapse_to(col);
        }
    }
}

fn split_lines(text: &str) -> Vec<&str> {
    text.split('\n').collect()
}

/// Line length in chars, not counting a trailing `\r`.
fn line_len(line: &str) -> usize {
    line.strip_suffix('\r').unwrap_or(line).chars().count()
}

fn char_to_byte(line: &str, col: usize) -> usize {
    line.char_indices().nth(col).map_or(line.len(), |(i, _)| i)
}

/// Char offset (into the whole text) of the start of every line.
pub fn line_starts(text: &str) -> Vec<usize> {
    let mut starts = vec![0];
    for (i, c) in text.chars().enumerate() {
        if c == '\n' {
            starts.push(i + 1);
        }
    }
    starts
}

/// Line lengths in chars (without `\r`), for drawing.
pub fn line_lens(text: &str) -> Vec<usize> {
    text.split('\n').map(line_len).collect()
}

/// The selected text: one line per selected line.
pub fn selected_text(text: &str, sel: &ColumnSel) -> String {
    let lines = split_lines(text);
    let range = sel.lines();
    lines[range.start.min(lines.len())..range.end.min(lines.len())]
        .iter()
        .map(|l| {
            let body = l.strip_suffix('\r').unwrap_or(l);
            let r = sel.range_in(line_len(l));
            body[char_to_byte(body, r.start)..char_to_byte(body, r.end)].to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Apply `edit` to every selected line; afterwards the selection is a caret column.
pub fn apply(text: &mut String, sel: &mut ColumnSel, edit: Edit) {
    let mut lines: Vec<String> = text.split('\n').map(str::to_string).collect();
    let range = sel.lines();
    let range = range.start.min(lines.len())..range.end.min(lines.len());
    for (k, line) in lines[range].iter_mut().enumerate() {
        let cr = line.ends_with('\r');
        if cr {
            line.pop();
        }
        let len = line.chars().count();
        let mut r = sel.range_in(len);
        match &edit {
            Edit::Backspace | Edit::Delete if r.is_empty() => {
                if matches!(edit, Edit::Backspace) {
                    if r.start == 0 {
                        r = 0..0;
                    } else {
                        r = r.start - 1..r.start;
                    }
                } else if r.start < len {
                    r = r.start..r.start + 1;
                }
            }
            _ => {}
        }
        let insert = match &edit {
            Edit::Insert(s) => *s,
            Edit::InsertPerLine(pieces) => pieces.get(k).copied().unwrap_or(""),
            _ => "",
        };
        line.replace_range(char_to_byte(line, r.start)..char_to_byte(line, r.end), insert);
        if cr {
            line.push('\r');
        }
    }
    // Every caret moves to the same (unclamped) column; short lines clamp it to their end.
    let base = sel.cols().0;
    sel.collapse_to(match &edit {
        Edit::Insert(s) => base.min(LINE_END) + s.chars().count(),
        Edit::InsertPerLine(p) => base.min(LINE_END) + p.first().map_or(0, |s| s.chars().count()),
        Edit::Backspace if sel.is_empty() => base.saturating_sub(1),
        _ => base,
    });
    *text = lines.join("\n");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sel(l0: usize, c0: usize, l1: usize, c1: usize) -> ColumnSel {
        ColumnSel {
            anchor_line: l0,
            anchor_col: c0,
            head_line: l1,
            head_col: c1,
        }
    }

    #[test]
    fn copy_and_type_over_a_block() {
        let mut t = "abcdef\nab\r\nabcdef".to_string();
        let mut s = sel(0, 1, 2, 4);
        assert_eq!(selected_text(&t, &s), "bcd\nb\nbcd");
        apply(&mut t, &mut s, Edit::Insert("X"));
        assert_eq!(t, "aXef\naX\r\naXef");
        assert_eq!((s.anchor_col, s.head_col), (2, 2));
        apply(&mut t, &mut s, Edit::Insert("y"));
        assert_eq!(t, "aXyef\naXy\r\naXyef");
    }

    #[test]
    fn caret_column_backspace_delete_and_short_lines() {
        let mut t = "hello\nhi\nhello".to_string();
        let mut s = sel(0, 4, 2, 4);
        apply(&mut t, &mut s, Edit::Insert("_"));
        // "hi" is shorter: its caret is at its end.
        assert_eq!(t, "hell_o\nhi_\nhell_o");
        apply(&mut t, &mut s, Edit::Backspace);
        assert_eq!(t, "hello\nhi\nhello");
        assert_eq!(s.head_col, 4);
        apply(&mut t, &mut s, Edit::Delete);
        assert_eq!(t, "hell\nhi\nhell");
        s.home_end(false, false);
        apply(&mut t, &mut s, Edit::Backspace); // nothing before column 0
        assert_eq!(t, "hell\nhi\nhell");
        s.home_end(true, false);
        apply(&mut t, &mut s, Edit::Insert(";"));
        assert_eq!(t, "hell;\nhi;\nhell;");
    }

    #[test]
    fn paste_one_piece_per_line_and_moves() {
        let mut t = "a\nb\nc".to_string();
        let mut s = sel(2, 1, 0, 1);
        apply(&mut t, &mut s, Edit::InsertPerLine(vec!["1", "2", "3"]));
        assert_eq!(t, "a1\nb2\nc3");
        s.move_by(-1, true, &t);
        assert_eq!(selected_text(&t, &s), "1\n2\n3");
        s.move_by(-1, false, &t);
        assert_eq!((s.anchor_col, s.head_col), (1, 1));
        s.home_end(true, false);
        s.move_by(-1, false, &t); // from "end of line", one left of the longest line's end
        assert_eq!(s.head_col, 1);
    }

    #[test]
    fn line_helpers() {
        assert_eq!(line_starts("ab\ncd\n"), vec![0, 3, 6]);
        assert_eq!(line_lens("ab\r\nc"), vec![2, 1]);
    }
}
