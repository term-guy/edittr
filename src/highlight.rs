//! Incremental syntax highlighting for the editor.
//!
//! egui_extras re-highlights the whole buffer whenever it changes (≈230 ms for a
//! 20k-line C file, i.e. on every keystroke). Here each line keeps its highlighted
//! spans plus the parser state after it; an edit re-highlights from the first
//! changed line only until the state matches the cached one again (usually one
//! line), and the laid-out galley is reused while the text doesn't change.

use eframe::egui::{self, Color32};
use std::hash::{Hash, Hasher};
use std::ops::Range;
use std::sync::OnceLock;
use syntect::highlighting::{FontStyle, HighlightIterator, HighlightState, Highlighter, Theme, ThemeSet};
use syntect::parsing::{ParseState, ScopeStack, SyntaxReference, SyntaxSet};

struct Assets {
    syntaxes: SyntaxSet,
    themes: ThemeSet,
}

fn assets() -> &'static Assets {
    static A: OnceLock<Assets> = OnceLock::new();
    A.get_or_init(|| Assets {
        syntaxes: SyntaxSet::load_defaults_newlines(),
        themes: ThemeSet::load_defaults(),
    })
}

/// Load the syntax definitions on a background thread so opening the first file doesn't stall.
pub fn warm_up() {
    std::thread::spawn(|| {
        assets();
    });
}

fn theme(dark: bool) -> &'static Theme {
    // Same themes as egui_extras' CodeTheme defaults.
    &assets().themes.themes[if dark { "base16-mocha.dark" } else { "Solarized (light)" }]
}

#[derive(Clone)]
pub struct Span {
    pub range: Range<usize>, // bytes, within the line
    pub color: Color32,
    pub italics: bool,
    pub underline: bool,
}

pub struct Line {
    hash: u64,
    pub spans: Vec<Span>,
    /// Brackets outside strings and comments: (byte in line, is an opener).
    pub brackets: Vec<(u32, bool)>,
    /// Parser + highlighter state after this line.
    end: (ParseState, HighlightState),
}

/// Files with more lines than this get their first full highlight on a background
/// thread (shown as plain text meanwhile) instead of stalling the frame that opens them.
const BACKGROUND_LINES: usize = 2_000;

#[derive(Default)]
pub struct Cache {
    key: Option<(String, bool)>, // language, dark
    lines: Vec<Line>,
    /// A background full highlight in progress, for `key`.
    pending: Option<std::sync::mpsc::Receiver<Vec<Line>>>,
    /// Document version the lines were last brought up to date with (`spans`).
    version: Option<u64>,
}

fn hash_str(s: &str) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

fn syntax(lang: &str) -> Option<&'static SyntaxReference> {
    let ss = &assets().syntaxes;
    ss.find_syntax_by_name(lang).or_else(|| ss.find_syntax_by_extension(lang))
}

impl Cache {
    /// Start or collect the background first pass; false while it runs.
    fn ensure(&mut self, ctx: &egui::Context, text: &str, syntax: &'static SyntaxReference, lang: &str, dark: bool) -> bool {
        let key = (lang.to_string(), dark);
        if self.key.as_ref() != Some(&key) {
            self.lines.clear();
            self.pending = None;
            self.key = Some(key);
        }
        if let Some(rx) = &self.pending {
            match rx.try_recv() {
                Ok(lines) => {
                    self.lines = lines;
                    self.pending = None;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => return false,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => self.pending = None,
            }
        } else if self.lines.is_empty() && text.bytes().filter(|&b| b == b'\n').count() > BACKGROUND_LINES {
            let (tx, rx) = std::sync::mpsc::channel();
            let (text, ctx) = (text.to_string(), ctx.clone());
            std::thread::spawn(move || {
                let mut c = Cache::default();
                c.update(&text, syntax, dark);
                let _ = tx.send(c.lines);
                ctx.request_repaint();
            });
            self.pending = Some(rx);
            return false;
        }
        true
    }

    /// Highlighted spans per line of `text` (version `version` of the document), updated
    /// incrementally; None while a big file's first pass runs in the background or when
    /// `lang` has no grammar.
    pub fn spans(&mut self, ctx: &egui::Context, version: u64, text: &str, lang: &str, dark: bool) -> Option<&[Line]> {
        let syntax = syntax(lang)?;
        let key = (lang.to_string(), dark);
        if self.version == Some(version) && self.key.as_ref() == Some(&key) && self.pending.is_none() {
            return Some(&self.lines);
        }
        if !self.ensure(ctx, text, syntax, lang, dark) {
            return None;
        }
        self.update(text, syntax, dark);
        self.version = Some(version);
        Some(&self.lines)
    }

    /// Bring `self.lines` in sync with `text`, re-highlighting as little as possible.
    fn update(&mut self, text: &str, syntax: &SyntaxReference, dark: bool) {
        let new: Vec<&str> = text.split_inclusive('\n').collect();
        let hashes: Vec<u64> = new.iter().map(|l| hash_str(l)).collect();
        let old = std::mem::take(&mut self.lines);
        // Unchanged lines at the start and at the end.
        let prefix = old.iter().zip(&hashes).take_while(|(o, h)| o.hash == **h).count();
        let max_suffix = old.len().min(new.len()) - prefix;
        let suffix = old
            .iter()
            .rev()
            .zip(hashes.iter().rev())
            .take(max_suffix)
            .take_while(|(o, h)| o.hash == **h)
            .count();

        let highlighter = Highlighter::new(theme(dark));
        let mut old = old.into_iter();
        let mut lines: Vec<Line> = old.by_ref().take(prefix).collect();
        let mut state = match lines.last() {
            Some(l) => l.end.clone(),
            None => (ParseState::new(syntax), HighlightState::new(&highlighter, ScopeStack::new())),
        };
        // State entering the first changed line, in the old text too.
        let prefix_state = state.clone();
        // Old lines after the prefix; the last `suffix` of them can be reused once states match.
        let rest: Vec<Line> = old.collect();
        let reuse_from = rest.len() - suffix; // index into `rest` of the first reusable line
        let new_suffix_start = new.len() - suffix;
        let mut i = prefix;
        while i < new.len() {
            if i >= new_suffix_start {
                let k = reuse_from + (i - new_suffix_start);
                let old_entry = if k == 0 { &prefix_state } else { &rest[k - 1].end };
                // Same line, same state entering it: everything from here on is unchanged.
                if *old_entry == state {
                    lines.extend(rest.into_iter().skip(k));
                    break;
                }
            }
            let line = new[i];
            let (spans, brackets) = highlight_line(line, &mut state, &highlighter);
            lines.push(Line {
                hash: hashes[i],
                spans,
                brackets,
                end: state.clone(),
            });
            i += 1;
        }
        self.lines = lines;
    }
}

fn highlight_line(line: &str, state: &mut (ParseState, HighlightState), highlighter: &Highlighter) -> (Vec<Span>, Vec<(u32, bool)>) {
    let Ok(ops) = state.0.parse_line(line, &assets().syntaxes) else {
        return Default::default();
    };
    let brackets = code_brackets(line, &ops, state.1.path.clone());
    let mut spans = Vec::new();
    let mut pos = 0;
    for (style, piece) in HighlightIterator::new(&mut state.1, &ops, line, highlighter) {
        let fg = style.foreground;
        spans.push(Span {
            range: pos..pos + piece.len(),
            color: Color32::from_rgb(fg.r, fg.g, fg.b),
            italics: style.font_style.contains(FontStyle::ITALIC),
            underline: style.font_style.contains(FontStyle::UNDERLINE),
        });
        pos += piece.len();
    }
    (spans, brackets)
}

/// Brackets in `line` that are code (not inside a string or comment scope).
fn code_brackets(line: &str, ops: &[(usize, syntect::parsing::ScopeStackOp)], mut stack: ScopeStack) -> Vec<(u32, bool)> {
    use syntect::parsing::Scope;
    static LITERAL: OnceLock<[Scope; 2]> = OnceLock::new();
    let literal = LITERAL.get_or_init(|| [Scope::new("string").unwrap(), Scope::new("comment").unwrap()]);
    if !line.bytes().any(|b| matches!(b, b'(' | b')' | b'[' | b']' | b'{' | b'}')) {
        return Vec::new();
    }
    let mut out = Vec::new();
    for (range, op) in syntect::easy::ScopeRangeIterator::new(ops, line) {
        let in_literal = stack.as_slice().iter().any(|s| literal.iter().any(|l| l.is_prefix_of(*s)));
        if !in_literal {
            for (i, b) in line.as_bytes()[range.clone()].iter().enumerate() {
                match b {
                    b'(' | b'[' | b'{' => out.push(((range.start + i) as u32, true)),
                    b')' | b']' | b'}' => out.push(((range.start + i) as u32, false)),
                    _ => {}
                }
            }
        }
        let _ = stack.apply(op);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full(text: &str, lang: &str) -> Vec<Vec<(Range<usize>, Color32)>> {
        let mut c = Cache {
            key: Some((lang.into(), true)),
            ..Default::default()
        };
        c.update(text, syntax(lang).unwrap(), true);
        c.lines
            .iter()
            .map(|l| l.spans.iter().map(|s| (s.range.clone(), s.color)).collect())
            .collect()
    }

    fn incremental(steps: &[&str], lang: &str) -> Vec<Vec<(Range<usize>, Color32)>> {
        let mut c = Cache::default();
        for t in steps {
            c.update(t, syntax(lang).unwrap(), true);
        }
        c.lines
            .iter()
            .map(|l| l.spans.iter().map(|s| (s.range.clone(), s.color)).collect())
            .collect()
    }

    #[test]
    fn brackets_outside_strings_and_comments() {
        let mut c = Cache::default();
        c.update("let s = \"(\"; // [\nf(a[0]);\n", syntax("rs").unwrap(), true);
        assert!(c.lines[0].brackets.is_empty());
        assert_eq!(c.lines[1].brackets, vec![(1, true), (3, true), (5, false), (6, false)]);
    }

    #[test]
    fn incremental_matches_full_highlighting() {
        let base = "fn main() {\n    let x = 1;\n    // note\n    call(x);\n}\n";
        let edits = [
            // typing inside a line
            "fn main() {\n    let xy = 1;\n    // note\n    call(x);\n}\n",
            // opening a block comment changes everything after it
            "fn main() {\n    /* let x = 1;\n    // note\n    call(x);\n}\n",
            // closing it again
            "fn main() {\n    /* let x = 1; */\n    // note\n    call(x);\n}\n",
            // inserting and deleting lines
            "use a;\nfn main() {\n    let x = 1;\n    call(x);\n}\n",
            "",
            "x",
        ];
        for e in edits {
            assert_eq!(incremental(&[base, e], "rs"), full(e, "rs"), "edit {e:?}");
        }
    }

    #[test]
    fn reuses_unchanged_lines_after_an_edit() {
        let mut lines: Vec<String> = (0..2000).map(|i| format!("int f{i}(void) {{ return {i}; }}\n")).collect();
        let mut c = Cache::default();
        let syn = syntax("c").unwrap();
        c.update(&lines.concat(), syn, true);
        lines[1000] = "int changed(void) { return 0; }\n".into();
        let t = std::time::Instant::now();
        c.update(&lines.concat(), syn, true);
        // Re-highlighting one line (plus hashing) is far below a full pass.
        assert!(t.elapsed() < std::time::Duration::from_millis(50), "{:?}", t.elapsed());
        assert_eq!(c.lines.len(), 2000);
    }
}
