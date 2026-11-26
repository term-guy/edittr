//! Merge conflict blocks (`<<<<<<<` / `|||||||` / `=======` / `>>>>>>>`) and resolving them.

use super::buffer::LineIndex;

/// One conflict, by line: the markers, with the common ancestor part (diff3) optional.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Conflict {
    pub start: usize,
    pub base: Option<usize>,
    pub mid: usize,
    pub end: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Choice {
    Current,
    Incoming,
    Both,
}

fn marker(line: &str, c: char) -> bool {
    let mut chars = line.chars();
    (0..7).all(|_| chars.next() == Some(c)) && chars.next().is_none_or(|n| n == ' ' || n == '\t' || n == '\r')
}

/// The conflicts in `text`, in order (cheap when there are none).
pub fn find(text: &str, ix: &LineIndex) -> Vec<Conflict> {
    let mut out = Vec::new();
    if !text.contains("<<<<<<<") {
        return out;
    }
    let mut open: Option<(usize, Option<usize>, Option<usize>)> = None;
    for l in 0..ix.lines() {
        let line = &text[ix.line_range(text, l)];
        if !line.starts_with(['<', '|', '=', '>']) {
            continue;
        }
        if marker(line, '<') {
            open = Some((l, None, None));
        } else if let Some((start, base, mid)) = &mut open {
            if marker(line, '|') && mid.is_none() {
                *base = Some(l);
            } else if marker(line, '=') && mid.is_none() {
                *mid = Some(l);
            } else if marker(line, '>')
                && let Some(mid) = *mid
            {
                out.push(Conflict {
                    start: *start,
                    base: *base,
                    mid,
                    end: l,
                });
                open = None;
            }
        }
    }
    out
}

impl Conflict {
    /// The lines kept by `choice`, as line ranges.
    fn kept(&self, choice: Choice) -> Vec<std::ops::Range<usize>> {
        let current = self.start + 1..self.base.unwrap_or(self.mid);
        let incoming = self.mid + 1..self.end;
        match choice {
            Choice::Current => vec![current],
            Choice::Incoming => vec![incoming],
            Choice::Both => vec![current, incoming],
        }
    }

    pub fn contains(&self, line: usize) -> bool {
        (self.start..=self.end).contains(&line)
    }

    /// The edit resolving this conflict: (byte range of the whole block, replacement).
    pub fn resolve(&self, text: &str, ix: &LineIndex, choice: Choice) -> (std::ops::Range<usize>, String) {
        let line_with_newline = |l: usize| {
            let start = ix.line_start(l);
            let end = if l + 1 < ix.lines() { ix.line_start(l + 1) } else { text.len() };
            &text[start..end]
        };
        let mut out = String::new();
        for range in self.kept(choice) {
            for l in range {
                out.push_str(line_with_newline(l));
            }
        }
        let start = ix.line_start(self.start);
        let end = if self.end + 1 < ix.lines() {
            ix.line_start(self.end + 1)
        } else {
            text.len()
        };
        // The block ended the file without a newline: don't leave one the file didn't have.
        if end == text.len() && !text.ends_with('\n') && out.ends_with('\n') {
            out.pop();
        }
        (start..end, out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_and_resolves_conflicts() {
        let text = "a\n<<<<<<< HEAD\nours\n||||||| base\nold\n=======\ntheirs\n>>>>>>> topic\nz\n<<<<<<<\nx\n=======\ny\n>>>>>>>";
        let ix = LineIndex::new(text);
        let found = find(text, &ix);
        assert_eq!(
            found,
            [
                Conflict {
                    start: 1,
                    base: Some(3),
                    mid: 5,
                    end: 7
                },
                Conflict {
                    start: 9,
                    base: None,
                    mid: 11,
                    end: 13
                }
            ]
        );
        let apply = |c: &Conflict, choice| {
            let (r, s) = c.resolve(text, &ix, choice);
            format!("{}{s}{}", &text[..r.start], &text[r.end..])
        };
        assert!(apply(&found[0], Choice::Current).starts_with("a\nours\nz\n"));
        assert!(apply(&found[0], Choice::Incoming).starts_with("a\ntheirs\nz\n"));
        assert!(apply(&found[0], Choice::Both).starts_with("a\nours\ntheirs\nz\n"));
        assert!(apply(&found[1], Choice::Both).ends_with("z\nx\ny"));
        let unmatched = "no <<<<<<<< markers\n<<<<<<<<\n=======\n";
        assert!(find(unmatched, &LineIndex::new(unmatched)).is_empty());
    }
}
