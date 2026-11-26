//! Line editing: duplicate, move, copy, delete and join lines, and add text to line
//! starts / ends. Each is one undoable step over every selection.

use super::State;
use super::buffer::{self, Kind, LineIndex, Sel};

/// Runs of consecutive lines touched by the selections, as (first, last) lines.
/// A selection ending at column 0 doesn't include that line.
pub(super) fn blocks(st: &State, ix: &LineIndex) -> Vec<(usize, usize)> {
    let lines = super::selected_lines(st, ix);
    let mut out: Vec<(usize, usize)> = Vec::new();
    for l in lines {
        match out.last_mut() {
            Some((_, last)) if *last + 1 >= l => *last = (*last).max(l),
            _ => out.push((l, l)),
        }
    }
    out
}

/// The text of lines `a..=b`, without the final newline.
fn block_range(text: &str, ix: &LineIndex, a: usize, b: usize) -> std::ops::Range<usize> {
    ix.line_start(a)..ix.line_range(text, b).end
}

/// Ctrl/Cmd+D: duplicate each selection after itself (the copy gets selected), or the
/// caret's line below it (the caret moves to the copy).
pub(super) fn duplicate(st: &mut State, text: &mut String, ix: &LineIndex) -> bool {
    let before = st.sels.clone();
    // (insert at, text) per selection, one per line for carets.
    let mut inserts: Vec<(usize, String)> = Vec::new();
    for s in &before {
        let at = if s.is_empty() {
            ix.line_range(text, ix.line_of_byte(s.head)).end
        } else {
            s.range().end
        };
        if s.is_empty() && inserts.iter().any(|(i, _)| *i == at) {
            continue;
        }
        let copy = if s.is_empty() {
            format!("\n{}", &text[ix.line_range(text, ix.line_of_byte(s.head))])
        } else {
            text[s.range()].to_string()
        };
        inserts.push((at, copy));
    }
    inserts.sort_by_key(|(at, _)| *at);
    // Bytes inserted before position `p` (strictly).
    let shift = |p: usize| inserts.iter().filter(|(at, _)| *at < p).map(|(_, t)| t.len()).sum::<usize>();
    let after = before
        .iter()
        .map(|s| {
            if s.is_empty() {
                let l = ix.line_of_byte(s.head);
                let end = ix.line_range(text, l).end;
                let col = s.head - ix.line_start(l);
                Sel::caret(end + shift(end) + 1 + col)
            } else {
                let r = s.range();
                let start = r.end + shift(r.end);
                let (anchor, head) = if s.head >= s.anchor {
                    (start, start + r.len())
                } else {
                    (start + r.len(), start)
                };
                Sel { anchor, head }
            }
        })
        .collect();
    let edits = inserts.into_iter().map(|(at, t)| (at..at, t)).collect();
    st.sels = buffer::edit_select(text, &mut st.history, &before, edits, after);
    true
}

/// Alt+Up / Alt+Down: swap the selected lines with the line above / below.
pub(super) fn move_lines(st: &mut State, text: &mut String, ix: &LineIndex, down: bool) -> bool {
    let blocks = blocks(st, ix);
    let n = ix.lines();
    if blocks.is_empty() || (down && blocks.last().is_some_and(|b| b.1 + 1 >= n)) || (!down && blocks[0].0 == 0) {
        return false;
    }
    let before = st.sels.clone();
    let mut edits = Vec::new();
    // (block byte range, shift) for moving the selections.
    let mut moved: Vec<(std::ops::Range<usize>, isize)> = Vec::new();
    for &(a, b) in &blocks {
        let block = block_range(text, ix, a, b);
        if down {
            let next = ix.line_range(text, b + 1);
            edits.push((block.start..next.end, format!("{}\n{}", &text[next.clone()], &text[block.clone()])));
            moved.push((block.start..block.end + 1, next.len() as isize + 1));
        } else {
            let prev = ix.line_range(text, a - 1);
            edits.push((prev.start..block.end, format!("{}\n{}", &text[block.clone()], &text[prev.clone()])));
            moved.push((block.start..block.end + 1, -(prev.len() as isize + 1)));
        }
    }
    let map = |p: usize| {
        moved
            .iter()
            .find(|(r, _)| r.contains(&p))
            .map_or(p, |(_, d)| (p as isize + d).max(0) as usize)
    };
    let after = before
        .iter()
        .map(|s| Sel {
            anchor: map(s.anchor),
            head: map(s.head),
        })
        .collect();
    st.sels = buffer::edit_select(text, &mut st.history, &before, edits, after);
    true
}

/// Alt+Shift+Up / Down: copy the selected lines above / below; the selections go to the copy.
pub(super) fn copy_lines(st: &mut State, text: &mut String, ix: &LineIndex, down: bool) -> bool {
    let blocks = blocks(st, ix);
    if blocks.is_empty() {
        return false;
    }
    let before = st.sels.clone();
    let mut edits = Vec::new();
    // (block, bytes inserted), for mapping the selections.
    let mut inserted: Vec<(std::ops::Range<usize>, usize)> = Vec::new();
    for &(a, b) in &blocks {
        let block = block_range(text, ix, a, b);
        let copy = &text[block.clone()];
        if down {
            edits.push((block.end..block.end, format!("\n{copy}")));
        } else {
            edits.push((block.start..block.start, format!("{copy}\n")));
        }
        inserted.push((block, copy.len() + 1));
    }
    // Down: the selections move to the copy below. Up: they stay, now on the copy above.
    let map = |p: usize| {
        let shifted: usize = inserted
            .iter()
            .filter(|(block, _)| if down { p >= block.start } else { p > block.end })
            .map(|(_, len)| len)
            .sum();
        p + shifted
    };
    let after = before
        .iter()
        .map(|s| Sel {
            anchor: map(s.anchor),
            head: map(s.head),
        })
        .collect();
    st.sels = buffer::edit_select(text, &mut st.history, &before, edits, after);
    true
}

/// Ctrl/Cmd+Shift+K: delete the selected lines.
pub(super) fn delete_lines(st: &mut State, text: &mut String, ix: &LineIndex) -> bool {
    let blocks = blocks(st, ix);
    if blocks.is_empty() {
        return false;
    }
    let n = ix.lines();
    let edits = blocks
        .iter()
        .map(|&(a, b)| {
            let range = if b + 1 < n {
                ix.line_start(a)..ix.line_start(b + 1)
            } else if a > 0 {
                // The last line: take the newline before it instead.
                ix.line_range(text, a - 1).end..text.len()
            } else {
                0..text.len()
            };
            (range, String::new())
        })
        .collect();
    let before = st.sels.clone();
    st.sels = buffer::edit(text, &mut st.history, &before, edits, Kind::Other);
    st.history.seal();
    true
}

/// Ctrl/Cmd+J: join the selected lines (or the caret's line and the next) with single spaces.
pub(super) fn join_lines(st: &mut State, text: &mut String, ix: &LineIndex) -> bool {
    let n = ix.lines();
    let mut edits = Vec::new();
    for (a, b) in blocks(st, ix) {
        let b = if a == b { a + 1 } else { b };
        for l in a..b.min(n - 1) {
            let next = ix.line_range(text, l + 1);
            let body = text[next.clone()].trim_start();
            let line = text[ix.line_range(text, l)].trim_end();
            // Drop the line's trailing spaces too, then put back a single one between non-empty parts.
            let trail = ix.line_range(text, l).start + line.len();
            let sep = if line.is_empty() || body.is_empty() { "" } else { " " };
            edits.push((trail..next.end - body.len(), sep.to_string()));
        }
    }
    if edits.is_empty() {
        return false;
    }
    let before = st.sels.clone();
    st.sels = buffer::edit_keep(text, &mut st.history, &before, edits);
    st.history.seal();
    true
}

/// Add `prefix` to the start and `suffix` to the end of each selected line (the caret's
/// line without a selection), optionally leaving blank lines alone.
pub fn affix_lines(st: &mut State, text: &mut String, version: u64, prefix: &str, suffix: &str, skip_blank: bool) -> bool {
    let ix = st.index_arc(text, version);
    let mut edits = Vec::new();
    for l in super::selected_lines(st, &ix) {
        let range = ix.line_range(text, l);
        if skip_blank && text[range.clone()].trim().is_empty() {
            continue;
        }
        if !prefix.is_empty() {
            edits.push((range.start..range.start, prefix.to_string()));
        }
        if !suffix.is_empty() {
            edits.push((range.end..range.end, suffix.to_string()));
        }
    }
    if edits.is_empty() {
        return false;
    }
    let before = st.sels.clone();
    st.sels = buffer::edit_keep(text, &mut st.history, &before, edits);
    st.history.seal();
    true
}

/// Alt+Shift+J: drop the last added occurrence (undo one Alt+J).
pub(super) fn remove_last_occurrence(st: &mut State) {
    if st.sels.len() > 1 {
        st.sels.pop();
    }
}
