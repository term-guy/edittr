//! Read-only viewer for very large files (multi-gigabyte logs).
//!
//! Nothing is loaded whole: a background thread scans the file once and keeps the
//! byte offset of every 64th line (≈12 MB of index for 100 million lines). The
//! visible lines are read from disk on demand, on a background thread, so neither
//! opening nor scrolling blocks the UI. Find also scans on a background thread.

use regex::bytes::Regex;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex};

/// Files at least this big open in the viewer instead of the editor.
pub const THRESHOLD: u64 = 64 * 1024 * 1024;
const STRIDE: usize = 64;
/// Longest part of a line that is shown (the rest is cut).
const MAX_SHOWN: usize = 4096;
const MAX_MATCHES: usize = 100_000;

/// Sparse line index, filled by the indexing thread.
pub struct Index {
    /// Byte offset of lines 0, STRIDE, 2*STRIDE, ...
    checkpoints: Mutex<Vec<u64>>,
    lines: AtomicUsize,
    scanned: AtomicU64,
    done: AtomicBool,
    cancel: AtomicBool,
}

impl Index {
    pub fn lines(&self) -> usize {
        self.lines.load(Ordering::Relaxed)
    }
    pub fn done(&self) -> bool {
        self.done.load(Ordering::Relaxed)
    }
    pub fn scanned(&self) -> u64 {
        self.scanned.load(Ordering::Relaxed)
    }
    fn checkpoint(&self, line: usize) -> Option<(usize, u64)> {
        let cp = self.checkpoints.lock().unwrap();
        let i = (line / STRIDE).min(cp.len().checked_sub(1)?);
        Some((i * STRIDE, cp[i]))
    }
}

/// Start indexing `path` in the background; `wake` is called as progress is made.
pub fn index(path: &Path, wake: impl Fn() + Send + 'static) -> Arc<Index> {
    let idx = Arc::new(Index {
        checkpoints: Mutex::new(vec![0]),
        lines: AtomicUsize::new(0),
        scanned: AtomicU64::new(0),
        done: AtomicBool::new(false),
        cancel: AtomicBool::new(false),
    });
    let (path, shared) = (path.to_path_buf(), idx.clone());
    std::thread::spawn(move || {
        let Ok(mut f) = File::open(&path) else {
            shared.done.store(true, Ordering::Relaxed);
            wake();
            return;
        };
        let mut buf = vec![0u8; 4 << 20];
        let (mut offset, mut lines) = (0u64, 0usize);
        let mut pending = Vec::new();
        let mut last_wake = std::time::Instant::now();
        loop {
            if shared.cancel.load(Ordering::Relaxed) {
                return;
            }
            let n = match f.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            for (i, _) in buf[..n].iter().enumerate().filter(|&(_, &b)| b == b'\n') {
                lines += 1;
                if lines % STRIDE == 0 {
                    pending.push(offset + i as u64 + 1);
                }
            }
            offset += n as u64;
            shared.checkpoints.lock().unwrap().append(&mut pending);
            shared.lines.store(lines + 1, Ordering::Relaxed);
            shared.scanned.store(offset, Ordering::Relaxed);
            if last_wake.elapsed().as_millis() > 100 {
                last_wake = std::time::Instant::now();
                wake();
            }
        }
        shared.lines.store(lines + 1, Ordering::Relaxed);
        shared.done.store(true, Ordering::Relaxed);
        wake();
    });
    idx
}

/// Read lines `range` (as many as exist), each cut to MAX_SHOWN bytes.
pub fn read_lines(path: &Path, idx: &Index, range: Range<usize>) -> Vec<String> {
    let Some((mut line, offset)) = idx.checkpoint(range.start) else {
        return Vec::new();
    };
    let Ok(mut f) = File::open(path) else { return Vec::new() };
    if f.seek(SeekFrom::Start(offset)).is_err() {
        return Vec::new();
    }
    let mut r = BufReader::with_capacity(1 << 16, f);
    let mut out = Vec::with_capacity(range.len());
    let mut buf = Vec::new();
    while line < range.end {
        buf.clear();
        // Read up to the newline, keeping at most MAX_SHOWN bytes of it.
        let mut kept = 0;
        let mut eof = true;
        loop {
            let chunk = match r.fill_buf() {
                Ok([]) | Err(_) => break,
                Ok(c) => c,
            };
            eof = false;
            let (take, found) = match chunk.iter().position(|&b| b == b'\n') {
                Some(i) => (i, true),
                None => (chunk.len(), false),
            };
            let room = MAX_SHOWN.saturating_sub(kept).min(take);
            buf.extend_from_slice(&chunk[..room]);
            kept += room;
            r.consume(if found { take + 1 } else { take });
            if found {
                break;
            }
        }
        if eof && buf.is_empty() && line >= idx.lines() {
            break;
        }
        if line >= range.start {
            let mut s = String::from_utf8_lossy(&buf).into_owned();
            if s.ends_with('\r') {
                s.pop();
            }
            out.push(s);
        }
        line += 1;
        if eof {
            break;
        }
    }
    out
}

/// Lines matching `re`, in order (at most MAX_MATCHES), scanning on the caller's thread.
pub fn find(path: &Path, re: &Regex, cancel: &AtomicBool, progress: &AtomicU64) -> Vec<usize> {
    let Ok(f) = File::open(path) else { return Vec::new() };
    let mut r = BufReader::with_capacity(4 << 20, f);
    let mut out = Vec::new();
    let mut line = 0;
    let mut buf = Vec::new();
    let mut scanned = 0u64;
    loop {
        if cancel.load(Ordering::Relaxed) || out.len() >= MAX_MATCHES {
            break;
        }
        buf.clear();
        match r.read_until(b'\n', &mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => scanned += n as u64,
        }
        if re.is_match(&buf) {
            out.push(line);
        }
        line += 1;
        if line % 4096 == 0 {
            progress.store(scanned, Ordering::Relaxed);
        }
    }
    out
}

/// A large file open in a tab.
pub struct Viewer {
    pub path: PathBuf,
    pub size: u64,
    pub index: Arc<Index>,
    /// Lines shown last (first line, text), and a pending read.
    pub window: (usize, Vec<String>),
    pub window_rx: Option<(Range<usize>, Receiver<Vec<String>>)>,
    pub query: String,
    pub matches: Vec<usize>,
    pub current: usize,
    pub find_rx: Option<Receiver<Vec<usize>>>,
    pub find_cancel: Arc<AtomicBool>,
    pub find_progress: Arc<AtomicU64>,
    pub goto_line: String,
    /// Scroll so this line is at the top next frame.
    pub scroll_to: Option<usize>,
    pub error: Option<String>,
}

impl Drop for Viewer {
    fn drop(&mut self) {
        self.index.cancel.store(true, Ordering::Relaxed);
        self.find_cancel.store(true, Ordering::Relaxed);
    }
}

impl Viewer {
    pub fn open(path: &Path, size: u64, ctx: &eframe::egui::Context) -> Self {
        let ctx = ctx.clone();
        Self {
            path: path.to_path_buf(),
            size,
            index: index(path, move || ctx.request_repaint()),
            window: (0, Vec::new()),
            window_rx: None,
            query: String::new(),
            matches: Vec::new(),
            current: 0,
            find_rx: None,
            find_cancel: Arc::new(AtomicBool::new(false)),
            find_progress: Arc::new(AtomicU64::new(0)),
            goto_line: String::new(),
            scroll_to: None,
            error: None,
        }
    }

    /// Make sure lines `range` are (being) read; returns what's available.
    pub fn request(&mut self, range: Range<usize>, ctx: &eframe::egui::Context) {
        if let Some((_, rx)) = &self.window_rx
            && let Ok(lines) = rx.try_recv()
        {
            let (r, _) = self.window_rx.take().unwrap();
            self.window = (r.start, lines);
        }
        let have = self.window.0..self.window.0 + self.window.1.len();
        let covered = have.start <= range.start && range.end <= have.end;
        let pending = self
            .window_rx
            .as_ref()
            .is_some_and(|(r, _)| r.start <= range.start && range.end <= r.end);
        if covered || pending {
            return;
        }
        // Read a margin around the visible lines so small scrolls don't need a read.
        let want = range.start.saturating_sub(100)..range.end + 100;
        let (tx, rx) = channel();
        let (path, idx, ctx, r) = (self.path.clone(), self.index.clone(), ctx.clone(), want.clone());
        std::thread::spawn(move || {
            let _ = tx.send(read_lines(&path, &idx, r));
            ctx.request_repaint();
        });
        self.window_rx = Some((want, rx));
    }

    pub fn line(&self, l: usize) -> Option<&str> {
        l.checked_sub(self.window.0).and_then(|i| self.window.1.get(i)).map(String::as_str)
    }

    pub fn start_find(&mut self, ctx: &eframe::egui::Context) {
        self.find_cancel.store(true, Ordering::Relaxed);
        self.matches.clear();
        self.current = 0;
        self.error = None;
        if self.query.is_empty() {
            self.find_rx = None;
            return;
        }
        let re = match regex::bytes::RegexBuilder::new(&regex::escape(&self.query))
            .case_insensitive(true)
            .build()
        {
            Ok(re) => re,
            Err(e) => {
                self.error = Some(e.to_string());
                return;
            }
        };
        self.find_cancel = Arc::new(AtomicBool::new(false));
        self.find_progress = Arc::new(AtomicU64::new(0));
        let (tx, rx) = channel();
        let (path, cancel, progress, ctx) = (self.path.clone(), self.find_cancel.clone(), self.find_progress.clone(), ctx.clone());
        std::thread::spawn(move || {
            let _ = tx.send(find(&path, &re, &cancel, &progress));
            ctx.request_repaint();
        });
        self.find_rx = Some(rx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sparse_index_reads_any_line_and_finds() {
        let dir = std::env::temp_dir().join(format!("edittr-large-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("big.log");
        let mut text = String::new();
        for i in 0..1000 {
            text.push_str(&format!("line {i}{}\r\n", if i == 777 { " needle" } else { "" }));
        }
        text.push_str(&"x".repeat(10_000)); // a long last line without newline
        std::fs::write(&path, &text).unwrap();
        let idx = index(&path, || {});
        while !idx.done() {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(idx.lines(), 1001);
        assert_eq!(read_lines(&path, &idx, 0..2), vec!["line 0", "line 1"]);
        assert_eq!(read_lines(&path, &idx, 130..132), vec!["line 130", "line 131"]);
        let last = read_lines(&path, &idx, 999..1005);
        assert_eq!(last.len(), 2);
        assert_eq!(last[1].len(), MAX_SHOWN);
        let re = Regex::new("needle").unwrap();
        assert_eq!(find(&path, &re, &AtomicBool::new(false), &AtomicU64::new(0)), vec![777]);
        std::fs::remove_dir_all(&dir).ok();
    }
}
