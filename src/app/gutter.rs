//! Gutter change markers: lines added, modified or deleted vs. the committed file.

use super::{App, Tab, background, receive};
use eframe::egui::Color32;
use std::sync::Arc;
use std::sync::mpsc::Receiver;
use std::time::Duration;

#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) enum Change {
    Added,
    Modified,
    /// Lines were deleted just above this line.
    Deleted,
}

/// Changed lines of one version of a document.
type Diffed = (u64, Vec<(usize, Change)>);

/// Per-document state (lives in `Doc`).
#[derive(Default)]
pub(super) struct Markers {
    /// Committed text; Some(None) = loaded, file not versioned.
    base: Option<Option<Arc<String>>>,
    base_rx: Option<Receiver<Option<String>>>,
    /// (line, change) for `version`.
    pub(super) lines: Vec<(usize, Change)>,
    version: Option<u64>,
    rx: Option<Receiver<Diffed>>,
}

impl Markers {
    /// Forget the committed text (after a commit, checkout or discard).
    pub(super) fn reset_base(&mut self) {
        self.base = None;
        self.base_rx = None;
        self.version = None;
    }
}

/// Line diff of `base` -> `text`.
pub(super) fn diff(base: &str, text: &str) -> Vec<(usize, Change)> {
    use similar::{DiffOp, TextDiff};
    let d = TextDiff::configure()
        .algorithm(similar::Algorithm::Myers)
        .timeout(Duration::from_millis(500))
        .diff_lines(base, text);
    let mut out = Vec::new();
    for op in d.ops() {
        match *op {
            DiffOp::Equal { .. } => {}
            DiffOp::Delete { new_index, .. } => out.push((new_index, Change::Deleted)),
            DiffOp::Insert { new_index, new_len, .. } => out.extend((new_index..new_index + new_len).map(|l| (l, Change::Added))),
            DiffOp::Replace { new_index, new_len, .. } => out.extend((new_index..new_index + new_len).map(|l| (l, Change::Modified))),
        }
    }
    out
}

pub(super) fn color(change: Change, dark: bool) -> Color32 {
    match (change, dark) {
        (Change::Added, true) => Color32::from_rgb(0x58, 0x9b, 0x5f),
        (Change::Added, false) => Color32::from_rgb(0x48, 0x98, 0x5a),
        (Change::Modified, true) => Color32::from_rgb(0x3f, 0x7f, 0xc4),
        (Change::Modified, false) => Color32::from_rgb(0x2f, 0x6f, 0xc0),
        (Change::Deleted, _) => Color32::from_rgb(0xd0, 0x55, 0x55),
    }
}

impl App {
    /// Keep the active document's change markers current (all work off the UI thread).
    pub(super) fn poll_markers(&mut self) {
        const SETTLE: Duration = Duration::from_millis(300);
        let (Some(root), Some(vcs)) = (self.repo.clone(), self.vcs) else {
            return;
        };
        let ctx = self.ctx.clone();
        let Some(Tab::File(doc)) = self.tabs.get_mut(self.active) else {
            return;
        };
        let m = &mut doc.markers;
        if let Some(base) = receive(&mut m.base_rx) {
            m.base = Some(base.map(Arc::new));
            m.version = None;
        }
        if let Some((version, lines)) = receive(&mut m.rx) {
            m.lines = lines;
            m.version = Some(version);
        }
        if m.base.is_none() {
            if m.base_rx.is_none() {
                let Ok(rel) = doc.path.strip_prefix(&root) else { return };
                let rel = rel.to_string_lossy().replace('\\', "/");
                // The editor keeps LF text (CRLF files too): compare like with like, or a
                // CRLF file would show every line as modified.
                m.base_rx = Some(background(&ctx, move || {
                    vcs.base_text(&root, &rel).map(|t| t.replace("\r\n", "\n"))
                }));
            }
            return;
        }
        let Some(Some(base)) = m.base.clone() else {
            m.lines.clear();
            return;
        };
        if m.version == Some(doc.version) || m.rx.is_some() {
            return;
        }
        if let Some(t) = doc.edited_at
            && t.elapsed() < SETTLE
        {
            ctx.request_repaint_after(SETTLE - t.elapsed());
            return;
        }
        let (text, version) = (doc.text.clone(), doc.version);
        // Diff against the text as it is on disk: the base has the file's line endings.
        m.rx = Some(background(&ctx, move || {
            let base = base.replace("\r\n", "\n");
            (version, diff(&base, &text))
        }));
    }

    /// After commits, checkouts, discards or file changes: refetch committed texts.
    pub(super) fn reset_markers(&mut self) {
        for tab in &mut self.tabs {
            if let Tab::File(doc) = tab {
                doc.markers.reset_base();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markers_from_line_diff() {
        let base = "a\nb\nc\nd\n";
        assert_eq!(diff(base, base), vec![]);
        assert_eq!(diff(base, "a\nB\nc\nd\n"), vec![(1, Change::Modified)]);
        assert_eq!(diff(base, "a\nb\nnew\nc\nd\n"), vec![(2, Change::Added)]);
        assert_eq!(diff(base, "a\nd\n"), vec![(1, Change::Deleted)]);
    }
}
