//! Per-document language-server decorations: semantic token colors and the other uses
//! of the symbol under the caret (document highlights). Requests run in the background;
//! results stay usable while typing by keeping them for unchanged lines.

use super::{App, EditorOut, Tab, background};
use crate::highlight;
use crate::lsp::features::{HighlightKind, SemanticToken};
use eframe::egui::Color32;
use std::ops::Range;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{Duration, Instant};

const TIMEOUT: Duration = Duration::from_secs(5);
/// Quiet time after an edit before asking for new tokens.
const TOKENS_AFTER_EDIT: Duration = Duration::from_millis(300);
/// The caret rests this long on a symbol before its other uses are looked up.
const MARKS_AFTER_MOVE: Duration = Duration::from_millis(120);
/// Without a language server, whole-word matches stand in; not for huge files.
const FALLBACK_LIMIT: usize = 4 << 20;

/// Colors of one line: (byte range in the line, light color, dark color).
type LineColors = Vec<(Range<usize>, Color32, Color32)>;

/// Semantic colors computed for `text`, per line.
struct Tokens {
    text: Arc<str>,
    lines: Vec<LineColors>,
}

type TokensResult = Result<Option<Tokens>, String>;
/// Semantic colors of one line, for the current theme.
pub(super) type Colors = Vec<(Range<usize>, Color32)>;
type LspRange = ((usize, usize), (usize, usize));

type MarksResult = (u64, Range<usize>, Option<Vec<(Range<usize>, bool)>>);

#[derive(Default)]
pub(super) struct DocLsp {
    tokens: Option<Tokens>,
    tokens_rx: Option<Receiver<(u64, TokensResult)>>,
    /// Version the last token request was for.
    tokens_version: Option<u64>,
    /// The server has no semantic tokens (until it restarts).
    unsupported: bool,
    retry_at: Option<Instant>,
    /// Token colors realigned to `aligned.0` (version, dark).
    aligned: Option<((u64, bool), Vec<Colors>)>,
    /// Uses of the symbol at the caret: (version, word) they are for.
    pub(super) marks: Vec<(Range<usize>, bool)>,
    marks_key: Option<(u64, Range<usize>)>,
    marks_rx: Option<Receiver<MarksResult>>,
    /// Caret word waiting for `MARKS_AFTER_MOVE` before a request.
    marks_wait: Option<((u64, Range<usize>), Instant)>,
}

impl DocLsp {
    /// Semantic colors per line of `text` (version `version`): lines that are the same as
    /// when the tokens were computed keep theirs, changed ones have none until the next
    /// response. Cached per version.
    pub(super) fn align(&mut self, text: &str, version: u64, dark: bool) {
        let Some(t) = &self.tokens else {
            self.aligned = None;
            return;
        };
        if self.aligned.as_ref().map(|a| a.0) != Some((version, dark)) {
            let pick = |l: &LineColors| l.iter().map(|(r, light, d)| (r.clone(), if dark { *d } else { *light })).collect();
            let old: Vec<&str> = t.text.split('\n').collect();
            let new: Vec<&str> = text.split('\n').collect();
            let prefix = old.iter().zip(&new).take_while(|(a, b)| a == b).count();
            let max_suffix = old.len().min(new.len()) - prefix;
            let suffix = old
                .iter()
                .rev()
                .zip(new.iter().rev())
                .take(max_suffix)
                .take_while(|(a, b)| a == b)
                .count();
            let mut lines: Vec<Vec<(Range<usize>, Color32)>> = Vec::with_capacity(new.len());
            lines.extend(t.lines.iter().take(prefix).map(pick));
            lines.resize_with(new.len() - suffix, Vec::new);
            let from = old.len() - suffix;
            lines.extend((from..old.len()).map(|i| t.lines.get(i).map(pick).unwrap_or_default()));
            // Drop trailing empty lines: lookups past the end mean "no colors".
            while lines.last().is_some_and(Vec::is_empty) {
                lines.pop();
            }
            self.aligned = Some(((version, dark), lines));
        }
    }

    /// The colors from the last `align`.
    pub(super) fn colors(&self) -> &[Vec<(Range<usize>, Color32)>] {
        self.aligned.as_ref().map_or(&[], |a| &a.1)
    }

    /// Symbol marks for this version of the text (stale ones are dropped).
    pub(super) fn marks_for(&self, version: u64) -> &[(Range<usize>, bool)] {
        match &self.marks_key {
            Some((v, _)) if *v == version => &self.marks,
            _ => &[],
        }
    }
}

/// Token colors per line, from tokens in UTF-16 columns.
fn token_colors(text: &str, tokens: &[SemanticToken]) -> Vec<LineColors> {
    let lines: Vec<&str> = text.split('\n').collect();
    let mut out: Vec<LineColors> = vec![Vec::new(); lines.len()];
    for t in tokens {
        let Some(line) = lines.get(t.line) else { continue };
        let (Some(light), Some(dark)) = (
            highlight::semantic_color(&t.kind, &t.modifiers, false),
            highlight::semantic_color(&t.kind, &t.modifiers, true),
        ) else {
            continue;
        };
        let byte = |c16: usize| {
            let mut units = 0;
            for (i, c) in line.char_indices() {
                if units >= c16 {
                    return i;
                }
                units += c.len_utf16();
            }
            line.len()
        };
        let (a, b) = (byte(t.start16), byte(t.start16 + t.len16));
        if a < b {
            out[t.line].push((a..b, light, dark));
        }
    }
    for l in &mut out {
        l.sort_by_key(|(r, _, _)| r.start);
        // Overlapping tokens: keep the first.
        let mut end = 0;
        l.retain(|(r, _, _)| {
            let keep = r.start >= end;
            if keep {
                end = r.end;
            }
            keep
        });
    }
    out
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Whole-word occurrences of `text[word]` (the fallback without a language server).
fn word_marks(text: &str, word: Range<usize>) -> Vec<(Range<usize>, bool)> {
    let w = &text[word];
    if w.is_empty() || w.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        return Vec::new();
    }
    let mut out: Vec<(Range<usize>, bool)> = text
        .match_indices(w)
        .filter(|(i, _)| !text[..*i].chars().next_back().is_some_and(is_word) && !text[i + w.len()..].chars().next().is_some_and(is_word))
        .map(|(i, _)| (i..i + w.len(), false))
        .take(5000)
        .collect();
    // A lone occurrence isn't worth marking.
    if out.len() < 2 {
        out.clear();
    }
    out
}

/// Byte range of the identifier the caret is in or right after.
fn caret_word(text: &str, b: usize) -> Range<usize> {
    let b = b.min(text.len());
    let start = text[..b]
        .char_indices()
        .rev()
        .take_while(|&(_, c)| is_word(c))
        .last()
        .map_or(b, |(i, _)| i);
    let end = b + text[b..].chars().take_while(|&c| is_word(c)).map(char::len_utf8).sum::<usize>();
    start..end
}

fn lsp_position(text: &str, b: usize) -> (usize, usize) {
    let start = text[..b].rfind('\n').map_or(0, |i| i + 1);
    (text[..start].matches('\n').count(), text[start..b].encode_utf16().count())
}

/// LSP ranges to byte ranges of `text`.
fn to_bytes(text: &str, ranges: Vec<(LspRange, HighlightKind)>) -> Vec<(Range<usize>, bool)> {
    let starts: Vec<usize> = std::iter::once(0).chain(text.match_indices('\n').map(|(i, _)| i + 1)).collect();
    let at = |(line, c16): (usize, usize)| {
        let Some(&s) = starts.get(line) else { return text.len() };
        let e = starts.get(line + 1).map_or(text.len(), |&n| n - 1);
        let mut units = 0;
        for (i, c) in text[s..e].char_indices() {
            if units >= c16 {
                return s + i;
            }
            units += c.len_utf16();
        }
        e
    };
    let mut out: Vec<(Range<usize>, bool)> = ranges
        .into_iter()
        .map(|((a, b), kind)| (at(a)..at(b), kind == HighlightKind::Write))
        .filter(|(r, _)| !r.is_empty())
        .collect();
    out.sort_by_key(|(r, _)| r.start);
    out
}

impl App {
    /// After the editor ran: keep the active document's semantic tokens and symbol marks
    /// up to date.
    pub(super) fn update_doc_lsp(&mut self, out: &EditorOut) {
        let Some(Tab::File(doc)) = self.tabs.get(self.active) else { return };
        let (path, ext, version) = (doc.path.clone(), doc.ext(), doc.version);
        let ignored = self.path_is_ignored(&path);
        let launch = if ignored { None } else { self.launch_server(&ext, false) };
        let semantic = self.settings.semantic_highlighting;
        let ctx = self.ctx.clone();
        let Some(Tab::File(doc)) = self.tabs.get_mut(self.active) else {
            return;
        };
        let st = &mut doc.lsp;

        // Semantic tokens.
        match st.tokens_rx.as_ref().map(Receiver::try_recv) {
            Some(Ok((v, res))) => {
                st.tokens_rx = None;
                match res {
                    Ok(Some(tokens)) => {
                        st.tokens = Some(tokens);
                        st.aligned = None;
                        st.retry_at = None;
                    }
                    Ok(None) => st.unsupported = true,
                    // Still starting, or the text changed before the server saw it.
                    Err(_) => {
                        st.tokens_version = None;
                        st.retry_at = Some(Instant::now() + Duration::from_secs(2));
                    }
                }
                let _ = v;
            }
            Some(Err(TryRecvError::Disconnected)) => st.tokens_rx = None,
            _ => {}
        }
        if !semantic {
            st.tokens = None;
            st.aligned = None;
        } else if let Some(launch) = launch.clone()
            && !st.unsupported
            && st.tokens_rx.is_none()
            && st.tokens_version != Some(version)
        {
            let quiet = doc.edited_at.is_none_or(|t| t.elapsed() >= TOKENS_AFTER_EDIT);
            let retry_ok = st.retry_at.is_none_or(|t| Instant::now() >= t);
            if quiet && retry_ok {
                st.tokens_version = Some(version);
                let (text, revision) = (doc.text.clone(), (doc.id, version));
                let path = path.clone();
                st.tokens_rx = Some(background(&ctx, move || {
                    let res = (|| {
                        let client = launch.wait().ok_or_else(|| "no server".to_string())?;
                        let tokens = client.semantic_tokens(&path, &text, revision, TIMEOUT)?;
                        Ok(tokens.map(|tokens| Tokens {
                            lines: token_colors(&text, &tokens),
                            text: text.into(),
                        }))
                    })();
                    (revision.1, res)
                }));
            } else {
                let wait = match st.retry_at {
                    Some(t) => t.saturating_duration_since(Instant::now()),
                    None => TOKENS_AFTER_EDIT,
                };
                ctx.request_repaint_after(wait.max(Duration::from_millis(30)));
            }
        }

        // Other uses of the symbol at the caret.
        if let Some(rx) = &st.marks_rx {
            match rx.try_recv() {
                Ok((v, word, marks)) => {
                    st.marks_rx = None;
                    st.marks = match marks {
                        Some(m) => m,
                        // No server support: whole words.
                        None if doc.text.len() <= FALLBACK_LIMIT && v == version => word_marks(&doc.text, word.clone()),
                        None => Vec::new(),
                    };
                    st.marks_key = Some((v, word));
                }
                Err(TryRecvError::Disconnected) => st.marks_rx = None,
                Err(TryRecvError::Empty) => {}
            }
        }
        let p = doc.ed.primary();
        let word = if doc.ed.sels.len() == 1
            && (p.anchor == p.head || caret_word(&doc.text, p.head) == (p.anchor.min(p.head)..p.anchor.max(p.head)))
        {
            caret_word(&doc.text, p.head)
        } else {
            0..0
        };
        let key = (version, word.clone());
        let inside_marks = st.marks_key.as_ref().is_some_and(|(v, _)| *v == version) && st.marks.iter().any(|(r, _)| *r == word);
        if word.is_empty() || doc.text[word.clone()].chars().next().is_some_and(|c| c.is_ascii_digit()) {
            st.marks.clear();
            st.marks_key = None;
            st.marks_wait = None;
        } else if st.marks_key.as_ref() != Some(&key) && !inside_marks && st.marks_rx.is_none() {
            // Moving on to another symbol: clear now, look it up once the caret rests.
            if st.marks_key.as_ref().is_some_and(|(v, w)| *v != version || *w != word) {
                st.marks.clear();
                st.marks_key = None;
            }
            match &st.marks_wait {
                Some((k, since)) if *k == key => {
                    if since.elapsed() >= MARKS_AFTER_MOVE && !out.changed {
                        st.marks_wait = None;
                        let (text, revision) = (doc.text.clone(), (doc.id, version));
                        let at = lsp_position(&text, word.start);
                        let launch = launch.clone();
                        let path = path.clone();
                        st.marks_rx = Some(background(&ctx, move || {
                            let marks = launch.and_then(|l| {
                                let client = l.wait()?;
                                client.document_highlights(&path, &text, revision, at, TIMEOUT).ok().flatten()
                            });
                            (revision.1, word, marks.map(|m| to_bytes(&text, m)))
                        }));
                    } else {
                        ctx.request_repaint_after(MARKS_AFTER_MOVE);
                    }
                }
                _ => {
                    st.marks_wait = Some((key, Instant::now()));
                    ctx.request_repaint_after(MARKS_AFTER_MOVE);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token(line: usize, start16: usize, len16: usize, kind: &str) -> SemanticToken {
        SemanticToken {
            line,
            start16,
            len16,
            kind: kind.into(),
            modifiers: Vec::new(),
        }
    }

    #[test]
    fn keeps_colors_of_unchanged_lines_after_an_edit() {
        let text = "#define N 3\nint x = N;\nint y = N;\n";
        let tokens = [
            token(0, 8, 1, "macro"),
            token(1, 8, 1, "macro"),
            token(2, 8, 1, "macro"),
            token(2, 4, 1, "variable"),
        ];
        let mut st = DocLsp {
            tokens: Some(Tokens {
                lines: token_colors(text, &tokens),
                text: text.into(),
            }),
            ..Default::default()
        };
        let ranges = |c: &[Vec<(Range<usize>, Color32)>]| {
            c.iter()
                .map(|l| l.iter().map(|(r, _)| r.clone()).collect::<Vec<_>>())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            ranges({
                st.align(text, 1, true);
                st.colors()
            }),
            vec![vec![8..9], vec![8..9], vec![8..9]]
        );
        // A line inserted at the top: everything else shifts down and keeps its colors.
        let edited = "// new\n#define N 3\nint x = N;\nint y = N;\n";
        assert_eq!(
            ranges({
                st.align(edited, 2, true);
                st.colors()
            }),
            vec![vec![], vec![8..9], vec![8..9], vec![8..9]]
        );
        // The middle line changed: it loses its colors until the next response.
        let edited = "#define N 3\nint xx = N;\nint y = N;\n";
        assert_eq!(
            ranges({
                st.align(edited, 3, true);
                st.colors()
            }),
            vec![vec![8..9], vec![], vec![8..9]]
        );
    }

    #[test]
    fn token_colors_use_bytes_and_skip_uncolored_kinds() {
        // `é` is 2 bytes but 1 UTF-16 unit.
        let lines = token_colors(
            "é FOO\n",
            &[token(0, 2, 3, "macro"), token(0, 0, 1, "variable"), token(5, 0, 1, "macro")],
        );
        assert_eq!(lines[0].iter().map(|(r, _, _)| r.clone()).collect::<Vec<_>>(), vec![3..6]);
    }

    #[test]
    fn fallback_marks_whole_words() {
        let text = "foo(foo_x, foo); afoo foo";
        assert_eq!(
            word_marks(text, 0..3).iter().map(|m| m.0.clone()).collect::<Vec<_>>(),
            vec![0..3, 11..14, 22..25]
        );
        assert!(word_marks("lonely", 0..6).is_empty());
        assert_eq!(caret_word("ab cd", 5), 3..5);
        assert_eq!(caret_word("ab cd", 2), 0..2);
    }
}
