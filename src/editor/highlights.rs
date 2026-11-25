//! User highlights (Ctrl/Cmd+Shift+H): every occurrence of a word or of selected text gets a
//! background in that highlight's own color.

use super::{LineIndex, State, buffer};
use eframe::egui::Color32;
use std::ops::Range;

/// A highlighted term and the slot of its color (see [`color`]).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Highlight {
    pub term: String,
    pub color: usize,
}

/// Highlight hues, tried first, then [`SHADES`] of them.
const HUES: [(u8, u8, u8); 8] = [
    (0xff, 0xe0, 0x00), // yellow
    (0x3d, 0x8b, 0xfd), // blue
    (0x3c, 0xc8, 0x5a), // green
    (0xff, 0x5f, 0xa8), // pink
    (0xff, 0x8c, 0x1a), // orange
    (0xa0, 0x66, 0xff), // purple
    (0x1e, 0xc8, 0xd8), // cyan
    (0xf0, 0x48, 0x48), // red
];

/// (light theme, dark theme) strength of each shade: light, medium, strong.
const SHADES: [(f32, f32); 3] = [(0.32, 0.26), (0.55, 0.40), (0.80, 0.56)];

/// Number of distinct colors before they repeat.
pub const COLORS: usize = HUES.len() * SHADES.len();

/// Background of color slot `slot`: all hues first, then the next shade of each.
pub fn color(slot: usize, dark: bool) -> Color32 {
    let (r, g, b) = HUES[slot % HUES.len()];
    let (light_a, dark_a) = SHADES[(slot / HUES.len()) % SHADES.len()];
    Color32::from_rgb(r, g, b).gamma_multiply(if dark { dark_a } else { light_a })
}

/// The first color slot no highlight uses (colors of removed highlights are reused).
pub fn next_color(highlights: &[Highlight]) -> usize {
    (0..COLORS)
        .find(|slot| !highlights.iter().any(|h| h.color % COLORS == *slot))
        .unwrap_or(highlights.len())
}

/// Adds `term` with the next free color, or removes it when it is already highlighted.
pub fn toggle(highlights: &mut Vec<Highlight>, term: String) {
    if let Some(i) = highlights.iter().position(|h| h.term == term) {
        highlights.remove(i);
    } else {
        let color = next_color(highlights);
        highlights.push(Highlight { term, color });
    }
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// The term at byte `b`: a single-line selection containing it, else the word there
/// (or just before it, for a caret at the end of a word).
pub(super) fn term_at(text: &str, ix: &LineIndex, sel: Range<usize>, b: usize) -> Option<String> {
    if !sel.is_empty() && sel.start <= b && b <= sel.end {
        let s = &text[sel];
        return (!s.contains('\n') && !s.trim().is_empty()).then(|| s.to_owned());
    }
    let line = ix.line_range(text, ix.line_of_byte(b));
    let mut w = buffer::word_at(text, line.clone(), b);
    if !text[w.clone()].chars().any(is_word) && b > line.start {
        w = buffer::word_at(text, line, buffer::prev_char(text, b));
    }
    let s = &text[w];
    s.chars().all(is_word).then(|| s.to_owned()).filter(|s| !s.is_empty())
}

impl State {
    /// What Ctrl/Cmd+Shift+H highlights: the primary selection (one line), else the word at the caret.
    pub fn highlight_term(&mut self, text: &str, version: u64) -> Option<String> {
        let p = self.primary();
        let ix = self.index_arc(text, version);
        term_at(text, &ix, p.range(), p.head)
    }
}

/// Occurrences of `term` in `line` (relative byte ranges). A term starting or ending
/// with a word character only matches there at a word boundary: `var1` not in `var10`.
pub(super) fn matches<'a>(line: &'a str, term: &'a str) -> impl Iterator<Item = Range<usize>> + 'a {
    let starts_word = term.chars().next().is_some_and(is_word);
    let ends_word = term.chars().next_back().is_some_and(is_word);
    line.match_indices(term).map(|(i, _)| i..i + term.len()).filter(move |r| {
        (!starts_word || !line[..r.start].chars().next_back().is_some_and(is_word))
            && (!ends_word || !line[r.end..].chars().next().is_some_and(is_word))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whole_words_only() {
        let found: Vec<_> = matches("var1 + var10 + xvar1 + var1;", "var1").collect();
        assert_eq!(found, [0..4, 23..27]);
        // Not a word: plain substring matches.
        assert_eq!(matches("a->b->c", "->").count(), 2);
    }

    #[test]
    fn colors_vary_hue_first_then_shade_and_reuse_freed_slots() {
        let mut hs = Vec::new();
        for i in 0..10 {
            toggle(&mut hs, format!("v{i}"));
        }
        assert_eq!(hs.iter().map(|h| h.color).collect::<Vec<_>>(), (0..10).collect::<Vec<_>>());
        assert_ne!(color(0, false), color(1, false));
        assert_ne!(color(0, false), color(8, false));
        toggle(&mut hs, "v2".into());
        toggle(&mut hs, "new".into());
        assert_eq!(
            hs.last(),
            Some(&Highlight {
                term: "new".into(),
                color: 2
            })
        );
    }

    #[test]
    fn term_under_caret_or_selection() {
        let text = "let var1 = f(var1);\n";
        let ix = LineIndex::new(text);
        assert_eq!(term_at(text, &ix, 6..6, 6).as_deref(), Some("var1"));
        // Caret right after the word (before ')').
        assert_eq!(term_at(text, &ix, 17..17, 17).as_deref(), Some("var1"));
        assert_eq!(term_at(text, &ix, 11..18, 12).as_deref(), Some("f(var1)"));
        assert_eq!(term_at(text, &ix, 9..9, 9), None);
    }
}
