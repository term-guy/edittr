//! Project-wide text search, run on a background thread.

use eframe::egui;
use regex::{Regex, RegexBuilder};
use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, channel};

const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;
const MAX_HITS: usize = 20_000;
const MAX_HITS_PER_FILE: usize = 1_000;
const SNIPPET_BEFORE: usize = 40; // chars of context shown before a match
const SNIPPET_LEN: usize = 200;

#[derive(Clone, Default, PartialEq)]
pub struct Options {
    pub query: String,
    pub match_case: bool,
    pub whole_word: bool,
    pub regex: bool,
}

pub struct Hit {
    /// 0-based line number.
    pub line: usize,
    /// Match position within the full line, in chars (for placing the cursor).
    pub cols: Range<usize>,
    /// Trimmed excerpt of the line, and the match's byte range inside it.
    pub snippet: String,
    pub highlight: Range<usize>,
}

pub struct FileHits {
    pub rel: String,
    pub hits: Vec<Hit>,
}

pub enum Msg {
    File(FileHits),
    Done { files: usize, truncated: bool },
    Error(String),
}

pub fn build_regex(opts: &Options) -> Result<Regex, String> {
    let mut pat = if opts.regex { opts.query.clone() } else { regex::escape(&opts.query) };
    if opts.whole_word {
        pat = format!(r"\b(?:{pat})\b");
    }
    RegexBuilder::new(&pat)
        .case_insensitive(!opts.match_case)
        .build()
        .map_err(|e| e.to_string().lines().last().unwrap_or("invalid regex").to_string())
}

/// Search `files` (paths relative to `base`). Results stream back as each file finishes.
pub fn spawn(
    base: PathBuf,
    files: impl FnOnce() -> Vec<String> + Send + 'static,
    opts: Options,
    cancel: Arc<AtomicBool>,
    ctx: egui::Context,
) -> Receiver<Msg> {
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        let re = match build_regex(&opts) {
            Ok(r) => r,
            Err(e) => {
                let _ = tx.send(Msg::Error(e));
                ctx.request_repaint();
                return;
            }
        };
        let files = files();
        let mut total = 0;
        let mut truncated = false;
        let mut searched = 0;
        for rel in files {
            if cancel.load(Ordering::Relaxed) {
                return;
            }
            let path = base.join(&rel);
            if std::fs::metadata(&path).map(|m| m.len() > MAX_FILE_BYTES).unwrap_or(true) {
                continue;
            }
            let Ok(bytes) = std::fs::read(&path) else { continue };
            if bytes.iter().take(8000).any(|&b| b == 0) {
                continue; // binary
            }
            searched += 1;
            let text = String::from_utf8_lossy(&bytes);
            let hits = search_text(&re, &text, MAX_HITS_PER_FILE.min(MAX_HITS - total));
            if hits.is_empty() {
                continue;
            }
            total += hits.len();
            if tx.send(Msg::File(FileHits { rel, hits })).is_err() {
                return;
            }
            ctx.request_repaint();
            if total >= MAX_HITS {
                truncated = true;
                break;
            }
        }
        let _ = tx.send(Msg::Done { files: searched, truncated });
        ctx.request_repaint();
    });
    rx
}

fn search_text(re: &Regex, text: &str, limit: usize) -> Vec<Hit> {
    let mut hits = Vec::new();
    for (line_no, line) in text.split('\n').enumerate() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        for m in re.find_iter(line) {
            if m.start() == m.end() {
                continue;
            }
            hits.push(make_hit(line_no, line, m.range()));
            if hits.len() >= limit {
                return hits;
            }
        }
    }
    hits
}

fn make_hit(line_no: usize, line: &str, m: Range<usize>) -> Hit {
    let col_start = line[..m.start].chars().count();
    let col_end = col_start + line[m.clone()].chars().count();

    // Excerpt: skip leading indentation, keep a little context before the match.
    let indent = line.len() - line.trim_start().len();
    let mut from = indent.min(m.start);
    let before = line[from..m.start].chars().count();
    let mut prefix = "";
    if before > SNIPPET_BEFORE {
        from = m.start - line[..m.start].chars().rev().take(SNIPPET_BEFORE).map(char::len_utf8).sum::<usize>();
        prefix = "…";
    }
    let rest = &line[from..];
    let to = from + rest.char_indices().nth(SNIPPET_LEN).map(|(i, _)| i).unwrap_or(rest.len());
    let to = to.max(m.end.min(line.len()));
    let snippet = format!("{prefix}{}", &line[from..to]);
    let shift = prefix.len() + m.start - from;
    Hit {
        line: line_no,
        cols: col_start..col_end,
        highlight: shift..shift + (m.end - m.start),
        snippet,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(q: &str) -> Options {
        Options { query: q.into(), ..Default::default() }
    }

    #[test]
    fn finds_case_insensitive_by_default() {
        let re = build_regex(&opts("foo")).unwrap();
        let hits = search_text(&re, "a\n  let FOO = foo;\r\n", 100);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].line, 1);
        assert_eq!(hits[0].cols, 6..9);
        assert_eq!(&hits[0].snippet[hits[0].highlight.clone()], "FOO");
        assert_eq!(hits[0].snippet, "let FOO = foo;");
    }

    #[test]
    fn whole_word_and_case() {
        let mut o = opts("foo");
        o.whole_word = true;
        o.match_case = true;
        let re = build_regex(&o).unwrap();
        assert_eq!(search_text(&re, "foo food Foo", 100).len(), 1);
    }

    #[test]
    fn literal_mode_escapes_regex_chars() {
        let re = build_regex(&opts("a.b(")).unwrap();
        assert_eq!(search_text(&re, "a.b( axb(", 100).len(), 1);
        let mut o = opts("(");
        o.regex = true;
        assert!(build_regex(&o).is_err());
    }

    #[test]
    fn long_lines_get_trimmed_with_unicode() {
        let line = format!("{}needle", "é".repeat(100));
        let re = build_regex(&opts("needle")).unwrap();
        let h = &search_text(&re, &line, 10)[0];
        assert!(h.snippet.starts_with('…'));
        assert_eq!(&h.snippet[h.highlight.clone()], "needle");
        assert_eq!(h.cols, 100..106);
    }

    #[test]
    fn background_search_over_folder() {
        let dir = std::env::temp_dir().join(format!("edittr-search-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("a.txt"), "hello\r\nworld hello\r\n").unwrap();
        std::fs::write(dir.join("sub/b.rs"), "fn hello() {}").unwrap();
        std::fs::write(dir.join("bin.dat"), b"hello\0\0").unwrap();
        let files = vec!["a.txt".to_string(), "sub/b.rs".into(), "bin.dat".into(), "missing".into()];
        let rx = spawn(dir.clone(), move || files, opts("hello"), Arc::new(AtomicBool::new(false)), egui::Context::default());
        let mut found = Vec::new();
        loop {
            match rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap() {
                Msg::File(f) => found.push((f.rel, f.hits.iter().map(|h| (h.line, h.cols.clone())).collect::<Vec<_>>())),
                Msg::Done { files, truncated } => {
                    assert_eq!(files, 2); // binary + missing skipped
                    assert!(!truncated);
                    break;
                }
                Msg::Error(e) => panic!("{e}"),
            }
        }
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(found, vec![
            ("a.txt".to_string(), vec![(0, 0..5), (1, 6..11)]),
            ("sub/b.rs".to_string(), vec![(0, 3..8)]),
        ]);
    }
}
