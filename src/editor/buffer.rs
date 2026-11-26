//! Text model for the editor: a line index over a `String`, multi-cursor edits and undo.
//!
//! Offsets are byte offsets into the text (always on char boundaries). The rest of
//! the app talks in char indices (`Doc::cursor`); [`LineIndex`] converts both ways in
//! O(log lines + line length).

use std::ops::Range;
use std::time::{Duration, Instant};

/// A selection: `head` is where the caret is, `anchor` the other end.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sel {
    pub anchor: usize,
    pub head: usize,
}

impl Sel {
    pub fn caret(at: usize) -> Self {
        Self { anchor: at, head: at }
    }
    pub fn range(&self) -> Range<usize> {
        self.anchor.min(self.head)..self.anchor.max(self.head)
    }
    pub fn is_empty(&self) -> bool {
        self.anchor == self.head
    }
}

/// Byte and char offsets of every line start.
#[derive(Default, Clone)]
pub struct LineIndex {
    byte: Vec<usize>,
    char: Vec<usize>,
    len_bytes: usize,
    len_chars: usize,
    /// Longest line, in bytes (to size horizontal scrolling).
    pub max_line_bytes: usize,
}

impl LineIndex {
    pub fn new(text: &str) -> Self {
        let mut byte = vec![0];
        let mut char = vec![0];
        let mut chars = 0;
        let mut max_line_bytes = 0;
        let mut line_start = 0;
        for (i, &b) in text.as_bytes().iter().enumerate() {
            if b & 0xC0 != 0x80 {
                chars += 1;
            }
            if b == b'\n' {
                max_line_bytes = max_line_bytes.max(i - line_start);
                line_start = i + 1;
                byte.push(i + 1);
                char.push(chars);
            }
        }
        max_line_bytes = max_line_bytes.max(text.len() - line_start);
        Self {
            byte,
            char,
            len_bytes: text.len(),
            len_chars: chars,
            max_line_bytes,
        }
    }

    pub fn lines(&self) -> usize {
        self.byte.len()
    }

    /// Byte range of line `l`, without its `\n` (and without a trailing `\r`).
    pub fn line_range(&self, text: &str, l: usize) -> Range<usize> {
        let start = self.byte[l];
        let mut end = self.byte.get(l + 1).map_or(self.len_bytes, |&n| n - 1);
        if end > start && text.as_bytes()[end - 1] == b'\r' {
            end -= 1;
        }
        start..end
    }

    pub fn line_start(&self, l: usize) -> usize {
        self.byte[l]
    }

    pub fn line_of_byte(&self, b: usize) -> usize {
        self.byte.partition_point(|&s| s <= b).saturating_sub(1)
    }

    pub fn byte_to_char(&self, text: &str, b: usize) -> usize {
        let l = self.line_of_byte(b);
        self.char[l] + text[self.byte[l]..b].chars().count()
    }

    pub fn char_to_byte(&self, text: &str, c: usize) -> usize {
        let c = c.min(self.len_chars);
        let l = self.char.partition_point(|&s| s <= c).saturating_sub(1);
        let start = self.byte[l];
        let line_end = self.byte.get(l + 1).copied().unwrap_or(self.len_bytes);
        text[start..line_end]
            .char_indices()
            .nth(c - self.char[l])
            .map_or(line_end, |(i, _)| start + i)
    }

    /// (line, byte column within the line).
    pub fn line_col(&self, b: usize) -> (usize, usize) {
        let l = self.line_of_byte(b);
        (l, b - self.byte[l])
    }
}

/// One multi-range replacement, in the coordinates of the text *after* it.
#[derive(Clone, Debug)]
struct Change {
    /// (range in the new text, text that was there before, text inserted)
    parts: Vec<(Range<usize>, String, String)>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Typing,
    Deleting,
    Other,
}

struct Group {
    changes: Vec<Change>,
    before: Vec<Sel>,
    after: Vec<Sel>,
    kind: Kind,
    at: Instant,
}

/// Undo/redo history. Consecutive typing (or deleting) within a short pause is one step.
#[derive(Default)]
pub struct History {
    undo: Vec<Group>,
    redo: Vec<Group>,
    /// Bytes of text kept in history; old steps are dropped past `BUDGET`.
    bytes: usize,
    /// Every replacement made to the text (edits, undo, redo) since the last `take_log`,
    /// in order, as (start, end, inserted length): for keeping marks such as bookmarks on their text.
    log: Vec<(usize, usize, usize)>,
    /// The log grew too long without being read: marks can't be mapped exactly.
    log_overflow: bool,
}

const MERGE_WINDOW: Duration = Duration::from_millis(800);
const BUDGET: usize = 64 * 1024 * 1024;

impl History {
    fn log(&mut self, start: usize, end: usize, inserted: usize) {
        if self.log.len() >= 100_000 {
            self.log.clear();
            self.log_overflow = true;
        }
        self.log.push((start, end, inserted));
    }

    /// The replacements since the last call (see `log`), and whether some were dropped.
    pub fn take_log(&mut self) -> (Vec<(usize, usize, usize)>, bool) {
        (std::mem::take(&mut self.log), std::mem::take(&mut self.log_overflow))
    }

    fn record(&mut self, change: Change, before: &[Sel], after: &[Sel], kind: Kind) {
        self.redo.clear();
        let size: usize = change.parts.iter().map(|(_, a, b)| a.len() + b.len()).sum();
        self.bytes += size;
        let now = Instant::now();
        if let Some(g) = self.undo.last_mut()
            && kind != Kind::Other
            && g.kind == kind
            && now.duration_since(g.at) < MERGE_WINDOW
            && g.after == before
        {
            g.changes.push(change);
            g.after = after.to_vec();
            g.at = now;
        } else {
            self.undo.push(Group {
                changes: vec![change],
                before: before.to_vec(),
                after: after.to_vec(),
                kind,
                at: now,
            });
        }
        while self.bytes > BUDGET && self.undo.len() > 1 {
            let g = self.undo.remove(0);
            self.bytes -= g
                .changes
                .iter()
                .flat_map(|c| &c.parts)
                .map(|(_, a, b)| a.len() + b.len())
                .sum::<usize>();
        }
    }

    /// Ends the current typing group (e.g. after the caret moved).
    pub fn seal(&mut self) {
        if let Some(g) = self.undo.last_mut() {
            g.kind = Kind::Other;
        }
    }

    pub fn undo(&mut self, text: &mut String) -> Option<Vec<Sel>> {
        let mut g = self.undo.pop()?;
        for c in g.changes.iter_mut().rev() {
            *c = invert(text, c, &mut self.log, &mut self.log_overflow);
        }
        g.changes.reverse();
        let sels = g.before.clone();
        std::mem::swap(&mut g.before, &mut g.after);
        g.kind = Kind::Other;
        self.redo.push(g);
        Some(sels)
    }

    pub fn redo(&mut self, text: &mut String) -> Option<Vec<Sel>> {
        let mut g = self.redo.pop()?;
        for c in g.changes.iter_mut().rev() {
            *c = invert(text, c, &mut self.log, &mut self.log_overflow);
        }
        g.changes.reverse();
        let sels = g.before.clone();
        std::mem::swap(&mut g.before, &mut g.after);
        self.undo.push(g);
        Some(sels)
    }
}

/// Undo `c` on `text` (which is in `c`'s "after" coordinates); returns the inverse change.
fn invert(text: &mut String, c: &Change, log: &mut Vec<(usize, usize, usize)>, overflow: &mut bool) -> Change {
    replace_ranges(text, c.parts.iter().map(|(range, removed, _)| (range, removed.as_str())));
    for (range, removed, _) in c.parts.iter().rev() {
        if log.len() >= 100_000 {
            log.clear();
            *overflow = true;
        }
        log.push((range.start, range.end, removed.len()));
    }
    let mut parts = Vec::with_capacity(c.parts.len());
    let mut delta: isize = 0;
    for (range, removed, inserted) in &c.parts {
        let start = (range.start as isize + delta) as usize;
        parts.push((start..start + removed.len(), inserted.clone(), removed.clone()));
        delta += removed.len() as isize - (range.end - range.start) as isize;
    }
    Change { parts }
}

/// Replace sorted, disjoint ranges. For bulk edits such as trimming on save,
/// copy the buffer in one pass to avoid shifting the tail once per line.
fn replace_ranges<'a>(text: &mut String, edits: impl DoubleEndedIterator<Item = (&'a Range<usize>, &'a str)> + ExactSizeIterator) {
    if edits.len() <= 32 {
        for (range, replacement) in edits.rev() {
            text.replace_range(range.clone(), replacement);
        }
        return;
    }
    let mut output = String::with_capacity(text.len());
    let mut start = 0;
    for (range, replacement) in edits {
        output.push_str(&text[start..range.start]);
        output.push_str(replacement);
        start = range.end;
    }
    output.push_str(&text[start..]);
    *text = output;
}

/// Replace each `(range, insert)` (ranges in the current text, any order) and put a
/// caret after each insertion. Returns the new selections.
pub fn edit(text: &mut String, history: &mut History, sels: &[Sel], edits: Vec<(Range<usize>, String)>, kind: Kind) -> Vec<Sel> {
    let edits = edits
        .into_iter()
        .map(|(r, s)| {
            let n = s.len();
            (r, s, n)
        })
        .collect();
    edit_carets(text, history, sels, edits, kind)
}

/// Like [`edit`], with each caret placed `caret` bytes into its insertion.
pub fn edit_carets(
    text: &mut String,
    history: &mut History,
    sels: &[Sel],
    edits: Vec<(Range<usize>, String, usize)>,
    kind: Kind,
) -> Vec<Sel> {
    let merged = merge(edits);
    let mut carets = Vec::with_capacity(merged.len());
    let mut delta: isize = 0;
    for (r, s, caret) in &merged {
        let start = (r.start as isize + delta) as usize;
        carets.push(Sel::caret(start + (*caret).min(s.len())));
        delta += s.len() as isize - (r.end - r.start) as isize;
    }
    apply(text, history, sels, &merged, &carets, kind);
    carets
}

/// Apply `edits` and map each selection through them (for indenting or commenting
/// lines, where the selection should stay on the same text).
pub fn edit_keep(text: &mut String, history: &mut History, sels: &[Sel], edits: Vec<(Range<usize>, String)>) -> Vec<Sel> {
    let merged = merge(edits.into_iter().map(|(r, s)| (r, s, 0)).collect());
    let map = |p: usize| {
        let mut out = p as isize;
        for (r, s, _) in &merged {
            if r.end <= p {
                out += s.len() as isize - (r.end - r.start) as isize;
            } else if r.start < p {
                // Inside a replaced range: move to the end of its replacement.
                out += (r.start + s.len()) as isize - p as isize;
            }
        }
        out.max(0) as usize
    };
    let after: Vec<Sel> = sels
        .iter()
        .map(|s| Sel {
            anchor: map(s.anchor),
            head: map(s.head),
        })
        .collect();
    apply(text, history, sels, &merged, &after, Kind::Other);
    after
}

/// Apply non-overlapping `edits` (ranges in the current text) as one undoable step and
/// set `after` (positions in the new text) as the selections.
pub fn edit_select(
    text: &mut String,
    history: &mut History,
    sels: &[Sel],
    edits: Vec<(Range<usize>, String)>,
    after: Vec<Sel>,
) -> Vec<Sel> {
    let merged = merge(edits.into_iter().map(|(r, s)| (r, s, 0)).collect());
    apply(text, history, sels, &merged, &after, Kind::Other);
    history.seal();
    after
}

/// Where byte `p` of the text before `log`'s replacements is afterwards (inside a replaced
/// range: its start).
pub fn map_through(log: &[(usize, usize, usize)], mut p: usize) -> usize {
    for &(start, end, inserted) in log {
        if p >= end {
            p = p - (end - start) + inserted;
        } else if p > start {
            p = start;
        }
    }
    p
}

fn merge(mut edits: Vec<(Range<usize>, String, usize)>) -> Vec<(Range<usize>, String, usize)> {
    edits.sort_by_key(|(r, _, _)| (r.start, r.end));
    let mut merged: Vec<(Range<usize>, String, usize)> = Vec::with_capacity(edits.len());
    for e in edits {
        match merged.last_mut() {
            // Overlapping or duplicate ranges (two carets on the same spot): one edit.
            Some((last, _, _)) if e.0.start < last.end || (e.0.start == last.start && e.0.end == last.end) => {
                last.end = last.end.max(e.0.end);
            }
            _ => merged.push(e),
        }
    }
    merged
}

fn apply(text: &mut String, history: &mut History, before: &[Sel], merged: &[(Range<usize>, String, usize)], after: &[Sel], kind: Kind) {
    let mut parts = Vec::with_capacity(merged.len());
    let mut delta: isize = 0;
    for (r, s, _) in merged {
        let start = (r.start as isize + delta) as usize;
        parts.push((start..start + s.len(), text[r.clone()].to_string(), s.clone()));
        delta += s.len() as isize - (r.end - r.start) as isize;
    }
    replace_ranges(text, merged.iter().map(|(range, replacement, _)| (range, replacement.as_str())));
    // Logged from the back, each in the coordinates left by the previous ones (see `map_through`).
    for (r, s, _) in merged.iter().rev() {
        history.log(r.start, r.end, s.len());
    }
    history.record(Change { parts }, before, after, kind);
}

// ------------------------------------------------------------------ movement

#[derive(PartialEq, Eq, Clone, Copy)]
enum Class {
    Space,
    Word,
    Punct,
}

fn class(c: char) -> Class {
    if c.is_whitespace() {
        Class::Space
    } else if c.is_alphanumeric() || c == '_' {
        Class::Word
    } else {
        Class::Punct
    }
}

pub fn prev_char(text: &str, b: usize) -> usize {
    text[..b].char_indices().next_back().map_or(0, |(i, _)| i)
}

pub fn next_char(text: &str, b: usize) -> usize {
    text[b..].chars().next().map_or(b, |c| b + c.len_utf8())
}

/// Start of the word before `b` (skipping whitespace first), like Ctrl/Alt+Left.
pub fn prev_word(text: &str, b: usize) -> usize {
    let mut it = text[..b].char_indices().rev().peekable();
    while let Some(&(_, c)) = it.peek() {
        if c == '\n' || !c.is_whitespace() {
            break;
        }
        it.next();
    }
    let Some(&(i, c)) = it.peek() else { return 0 };
    if c == '\n' {
        return if i + 1 == b { i } else { i + 1 };
    }
    let k = class(c);
    let mut start = i;
    for (i, c) in it {
        if class(c) != k || c == '\n' {
            break;
        }
        start = i;
    }
    start
}

/// End of the word after `b`, like Ctrl/Alt+Right.
pub fn next_word(text: &str, b: usize) -> usize {
    let mut it = text[b..].char_indices().map(|(i, c)| (b + i, c)).peekable();
    while let Some(&(_, c)) = it.peek() {
        if c == '\n' || !c.is_whitespace() {
            break;
        }
        it.next();
    }
    let Some(&(i, c)) = it.peek() else { return text.len() };
    if c == '\n' {
        return if i == b { i + 1 } else { i };
    }
    let k = class(c);
    let mut end = i;
    for (i, c) in it {
        if class(c) != k || c == '\n' {
            return i;
        }
        end = i + c.len_utf8();
    }
    end
}

/// The word (or run of punctuation / whitespace) around `b`, for double-click.
pub fn word_at(text: &str, line: Range<usize>, b: usize) -> Range<usize> {
    let s = &text[line.clone()];
    let rel = b - line.start;
    let Some(c) = s[rel..].chars().next().or_else(|| s[..rel].chars().next_back()) else {
        return b..b;
    };
    let k = class(c);
    let start = s[..rel]
        .char_indices()
        .rev()
        .take_while(|&(_, c)| class(c) == k)
        .last()
        .map_or(rel, |(i, _)| i);
    let end = s[rel..]
        .char_indices()
        .find(|&(_, c)| class(c) != k)
        .map_or(s.len(), |(i, _)| rel + i);
    line.start + start..line.start + end
}

/// Leading whitespace of a line.
pub fn indent_of(line: &str) -> &str {
    &line[..line.len() - line.trim_start_matches([' ', '\t']).len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_index_conversions() {
        let t = "héllo\r\nwörld\n\nend";
        let ix = LineIndex::new(t);
        assert_eq!(ix.lines(), 4);
        assert_eq!(&t[ix.line_range(t, 0)], "héllo");
        assert_eq!(&t[ix.line_range(t, 1)], "wörld");
        assert_eq!(&t[ix.line_range(t, 2)], "");
        assert_eq!(&t[ix.line_range(t, 3)], "end");
        for c in 0..=t.chars().count() {
            let b = ix.char_to_byte(t, c);
            assert!(t.is_char_boundary(b));
            assert_eq!(ix.byte_to_char(t, b), c, "char {c}");
        }
        assert_eq!(ix.line_col(ix.char_to_byte(t, 7)), (1, 0));
        assert_eq!(ix.max_line_bytes, 7);
    }

    #[test]
    fn multi_cursor_edit_and_undo_redo() {
        let mut t = String::from("ab\nab\nab");
        let mut h = History::default();
        let sels = vec![Sel::caret(1), Sel::caret(4), Sel::caret(7)];
        let edits = sels.iter().map(|s| (s.range(), "X".to_string())).collect();
        let after = edit(&mut t, &mut h, &sels, edits, Kind::Typing);
        assert_eq!(t, "aXb\naXb\naXb");
        assert_eq!(after, vec![Sel::caret(2), Sel::caret(6), Sel::caret(10)]);
        // Typing again merges into one undo step.
        let edits = after.iter().map(|s| (s.range(), "Y".to_string())).collect();
        let after2 = edit(&mut t, &mut h, &after, edits, Kind::Typing);
        assert_eq!(t, "aXYb\naXYb\naXYb");
        assert_eq!(h.undo(&mut t), Some(sels.clone()));
        assert_eq!(t, "ab\nab\nab");
        assert_eq!(h.redo(&mut t), Some(after2));
        assert_eq!(t, "aXYb\naXYb\naXYb");
        assert!(h.undo(&mut t).is_some());
        assert!(h.undo(&mut t).is_none());
    }

    #[test]
    fn replace_selections_and_overlapping_deletes() {
        let mut t = String::from("hello world");
        let mut h = History::default();
        let sels = vec![Sel { anchor: 0, head: 5 }, Sel { anchor: 11, head: 6 }];
        let edits = sels.iter().map(|s| (s.range(), "Z".to_string())).collect();
        edit(&mut t, &mut h, &sels, edits, Kind::Other);
        assert_eq!(t, "Z Z");
        // Two carets deleting the same char (e.g. after both moved): merged, no panic.
        let edits = vec![(0..1, String::new()), (0..1, String::new())];
        edit(&mut t, &mut h, &[Sel::caret(1), Sel::caret(1)], edits, Kind::Deleting);
        assert_eq!(t, " Z");
        h.undo(&mut t);
        h.undo(&mut t);
        assert_eq!(t, "hello world");
    }

    #[test]
    fn edit_keep_maps_selections_through_line_start_inserts() {
        let mut t = String::from("a\nb\nc");
        let mut h = History::default();
        let sels = vec![Sel { anchor: 0, head: 3 }];
        let after = edit_keep(&mut t, &mut h, &sels, vec![(0..0, "  ".into()), (2..2, "  ".into())]);
        assert_eq!(t, "  a\n  b\nc");
        // The insertion at the anchor's position moves it (VS Code keeps whole lines selected).
        assert_eq!(after, vec![Sel { anchor: 2, head: 7 }]);
        let c = edit_carets(&mut t, &mut h, &[Sel::caret(0)], vec![(0..0, "{\n}".into(), 2)], Kind::Other);
        assert_eq!(c, vec![Sel::caret(2)]);
    }

    #[test]
    fn word_movement() {
        let t = "foo.bar  baz\nqux";
        assert_eq!(next_word(t, 0), 3);
        assert_eq!(next_word(t, 3), 4);
        assert_eq!(next_word(t, 4), 7);
        assert_eq!(next_word(t, 7), 12);
        assert_eq!(next_word(t, 12), 13);
        assert_eq!(prev_word(t, 12), 9);
        assert_eq!(prev_word(t, 9), 4);
        assert_eq!(prev_word(t, 13), 12);
        assert_eq!(prev_word(t, 0), 0);
        assert_eq!(word_at(t, 0..12, 5), 4..7);
        assert_eq!(word_at(t, 0..12, 3), 3..4);
        assert_eq!(indent_of("\t  x"), "\t  ");
    }
}
