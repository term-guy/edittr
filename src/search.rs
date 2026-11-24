//! Project-wide text search, run on a background thread.

use eframe::egui;
use regex::{Regex, RegexBuilder};
use std::collections::{BTreeMap, HashSet};
use std::fs::File;
use std::io::Read;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
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
    let mut pat = if opts.regex {
        opts.query.clone()
    } else {
        regex::escape(&opts.query)
    };
    if opts.whole_word {
        pat = format!(r"\b(?:{pat})\b");
    }
    RegexBuilder::new(&pat)
        .case_insensitive(!opts.match_case)
        .multi_line(true)
        .crlf(true)
        .build()
        .map_err(|e| e.to_string().lines().last().unwrap_or("invalid regex").to_string())
}

/// Search open editor buffers first (`open`: relative path and in-memory text, so
/// unsaved edits count), then `files` (paths relative to `base`, listed on the
/// background thread) on several threads. Results stream back in a stable order:
/// open files, then project order.
pub fn spawn(
    base: PathBuf,
    files: impl FnOnce() -> Vec<String> + Send + 'static,
    open: Vec<(String, String)>,
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
        let mut total = 0;
        let mut searched = 0;
        let send = |rel: String, mut hits: Vec<Hit>, total: &mut usize| -> bool {
            hits.truncate(MAX_HITS - *total);
            *total += hits.len();
            let ok = tx.send(Msg::File(FileHits { rel, hits })).is_ok();
            ctx.request_repaint();
            ok
        };

        // 1. Open files, from memory: instant results for what the user is working on.
        let mut seen = HashSet::new();
        for (rel, text) in open {
            if !seen.insert(rel.clone()) {
                continue;
            }
            searched += 1;
            if total < MAX_HITS && re.is_match(&text) {
                let hits = search_text(&re, &text, MAX_HITS_PER_FILE);
                if !hits.is_empty() && !send(rel, hits, &mut total) {
                    return;
                }
            }
        }

        // 2. The rest of the project, in parallel.
        let files: Vec<String> = files().into_iter().filter(|f| !seen.contains(f)).collect();
        let next = AtomicUsize::new(0);
        let stop = AtomicBool::new(false);
        let threads = std::thread::available_parallelism().map_or(4, |n| n.get()).clamp(1, 12);
        let (wtx, wrx) = channel::<(usize, Option<Vec<Hit>>)>();
        let mut truncated = false;
        std::thread::scope(|s| {
            for _ in 0..threads {
                let wtx = wtx.clone();
                let (files, next, stop, cancel, re, base) = (&files, &next, &stop, &cancel, &re, &base);
                s.spawn(move || {
                    loop {
                        if stop.load(Ordering::Relaxed) || cancel.load(Ordering::Relaxed) {
                            return;
                        }
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(rel) = files.get(i) else { return };
                        let result = search_file(&base.join(rel), re);
                        if wtx.send((i, result)).is_err() {
                            return;
                        }
                    }
                });
            }
            drop(wtx);
            // Re-order: forward results in file-list order as soon as the prefix is complete.
            let mut pending: BTreeMap<usize, Option<Vec<Hit>>> = BTreeMap::new();
            let mut emit = 0;
            for (i, r) in wrx {
                pending.insert(i, r);
                while let Some(r) = pending.remove(&emit) {
                    let rel = &files[emit];
                    emit += 1;
                    let Some(hits) = r else { continue };
                    searched += 1;
                    if hits.is_empty() || truncated {
                        continue;
                    }
                    if !send(rel.clone(), hits, &mut total) || cancel.load(Ordering::Relaxed) {
                        stop.store(true, Ordering::Relaxed);
                        return;
                    }
                    if total >= MAX_HITS {
                        truncated = true;
                        stop.store(true, Ordering::Relaxed);
                    }
                }
            }
        });
        if cancel.load(Ordering::Relaxed) {
            return;
        }
        let _ = tx.send(Msg::Done {
            files: searched,
            truncated,
        });
        ctx.request_repaint();
    });
    rx
}

/// `None`: skipped (unreadable, too big or binary). Opens the file once: on Windows
/// every open is expensive (antivirus filters), so no separate metadata call.
fn search_file(path: &Path, re: &Regex) -> Option<Vec<Hit>> {
    let mut f = File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    if len > MAX_FILE_BYTES {
        return None;
    }
    let mut bytes = Vec::with_capacity(len as usize + 1);
    f.read_to_end(&mut bytes).ok()?;
    if bytes[..bytes.len().min(8000)].contains(&0) {
        return None; // binary
    }
    let text = String::from_utf8_lossy(&bytes);
    // Most files don't match: one pass over the whole buffer rejects them, instead
    // of running the regex line by line. Sound because the regex is multi-line with
    // CRLF-aware anchors, so any per-line match is also a whole-text match.
    if !re.is_match(&text) {
        return Some(Vec::new());
    }
    Some(search_text(re, &text, MAX_HITS_PER_FILE))
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
        from = m.start
            - line[..m.start]
                .chars()
                .rev()
                .take(SNIPPET_BEFORE)
                .map(char::len_utf8)
                .sum::<usize>();
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
        Options {
            query: q.into(),
            ..Default::default()
        }
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
    fn anchors_work_per_line_and_in_the_whole_text_prefilter() {
        let mut o = opts("^world$");
        o.regex = true;
        let re = build_regex(&o).unwrap();
        let text = "hello\r\nworld\r\nx world";
        assert!(re.is_match(text));
        let hits = search_text(&re, text, 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].line, 1);
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
        let rx = spawn(
            dir.clone(),
            move || files,
            vec![("sub/b.rs".into(), "unsaved hello\nhello".into())],
            opts("hello"),
            Arc::new(AtomicBool::new(false)),
            egui::Context::default(),
        );
        let mut found = Vec::new();
        loop {
            match rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap() {
                Msg::File(f) => found.push((f.rel, f.hits.iter().map(|h| (h.line, h.cols.clone())).collect::<Vec<_>>())),
                Msg::Done { files, truncated } => {
                    assert_eq!(files, 2); // open b.rs + a.txt; binary and missing skipped
                    assert!(!truncated);
                    break;
                }
                Msg::Error(e) => panic!("{e}"),
            }
        }
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(
            found,
            vec![
                // Open file first, searched from its unsaved text, not from disk.
                ("sub/b.rs".to_string(), vec![(0, 8..13), (1, 0..5)]),
                ("a.txt".to_string(), vec![(0, 0..5), (1, 6..11)]),
            ]
        );
    }
}
