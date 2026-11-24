//! Editable side-by-side diff of a working-copy change: the left pane is the base
//! (git index / svn BASE, read-only but selectable), the right pane is the real
//! editor of the file (the same document as its tab, so editing, undo and saving
//! work as usual). Changed lines are tinted and recomputed as you type, and the two
//! panes scroll together, lined up through the diff.

use super::{App, Doc, EditorOut, Tab, diff_colors, editor, file_name, icon_button};
use crate::{editor as ed, highlight, icons as ic};
use eframe::egui::{self, Color32};
use similar::{Algorithm, DiffTag};
use std::ops::Range;
use std::path::Path;
use std::time::Duration;

pub(super) struct LiveDiff {
    base: String,
    base_state: ed::State,
    base_version: u64,
    base_hl: highlight::Cache,
    /// (document id, version, dark) the ops/tints below were computed for.
    key: Option<(u64, u64, bool)>,
    ops: Vec<(DiffTag, Range<usize>, Range<usize>)>,
    left_bg: Vec<(usize, Color32)>,
    right_bg: Vec<(usize, Color32)>,
    added: usize,
    removed: usize,
    /// Top lines (left, right) as last shown, to tell which pane the user scrolled.
    tops: (f32, f32),
}

impl LiveDiff {
    pub(super) fn new(base: String) -> Self {
        Self {
            base,
            base_state: ed::State::default(),
            base_version: 0,
            base_hl: highlight::Cache::default(),
            key: None,
            ops: Vec::new(),
            left_bg: Vec::new(),
            right_bg: Vec::new(),
            added: 0,
            removed: 0,
            tops: (0.0, 0.0),
        }
    }

    fn update(&mut self, doc: &Doc, dark: bool) {
        let key = (doc.id, doc.version, dark);
        if self.key == Some(key) {
            return;
        }
        self.key = Some(key);
        // Bounded: a huge rewrite falls back to a coarser diff instead of stalling a frame.
        let diff = similar::TextDiff::configure()
            .algorithm(Algorithm::Myers)
            .timeout(Duration::from_millis(30))
            .diff_lines(&self.base, &doc.text);
        self.ops = diff.ops().iter().map(|op| op.as_tag_tuple()).collect();
        let (add, del) = diff_colors(dark);
        let (add, del) = (add.gamma_multiply(0.16), del.gamma_multiply(0.16));
        self.left_bg.clear();
        self.right_bg.clear();
        for (tag, old, new) in &self.ops {
            if *tag != DiffTag::Equal {
                self.left_bg.extend(old.clone().map(|l| (l, del)));
                self.right_bg.extend(new.clone().map(|l| (l, add)));
            }
        }
        self.removed = self.left_bg.len();
        self.added = self.right_bg.len();
    }

    /// First right-side line of each change.
    fn blocks(&self) -> Vec<usize> {
        self.ops
            .iter()
            .filter(|(t, ..)| *t != DiffTag::Equal)
            .map(|(_, _, new)| new.start)
            .collect()
    }

    /// The line on the other side matching `line` (fractional) on this side.
    fn map(&self, line: f32, from_right: bool) -> f32 {
        let whole = line.floor() as usize;
        for (tag, old, new) in &self.ops {
            let (from, to) = if from_right { (new, old) } else { (old, new) };
            if whole < from.end {
                return if *tag == DiffTag::Equal {
                    (to.start + (whole - from.start)) as f32 + line.fract()
                } else {
                    (to.start + (whole - from.start).min(to.len().saturating_sub(1))) as f32
                };
            }
        }
        self.ops
            .last()
            .map_or(line, |(_, old, new)| if from_right { old.end } else { new.end } as f32)
    }
}

/// Mutable access to two different tabs.
fn two_tabs(tabs: &mut [Tab], a: usize, b: usize) -> (&mut Tab, &mut Tab) {
    assert_ne!(a, b);
    if a < b {
        let (x, y) = tabs.split_at_mut(b);
        (&mut x[a], &mut y[0])
    } else {
        let (x, y) = tabs.split_at_mut(a);
        (&mut y[0], &mut x[b])
    }
}

impl App {
    /// The file tab edited by the live diff in tab `i`, if it is one.
    pub(super) fn live_diff_doc(&self, i: usize) -> Option<usize> {
        let Some(Tab::Diff(d)) = self.tabs.get(i) else { return None };
        d.live.as_ref()?;
        self.tabs.iter().position(|t| matches!(t, Tab::File(doc) if doc.path == d.path))
    }

    pub(super) fn is_live_diff(&self, i: usize) -> bool {
        matches!(self.tabs.get(i), Some(Tab::Diff(d)) if d.live.is_some() && !d.inline)
    }

    /// Draws the live diff in tab `active`; returns a path to open, if any.
    pub(super) fn live_diff_ui(&mut self, ui: &mut egui::Ui, active: usize) -> Option<std::path::PathBuf> {
        let path = match &self.tabs[active] {
            Tab::Diff(d) => d.path.clone(),
            _ => return None,
        };
        // The right pane edits the file's own document: open it (in the background) if needed.
        let file = match self.live_diff_doc(active) {
            Some(i) => i,
            None => {
                let Ok(Some(disk)) = super::read_doc_text(&path) else {
                    if let Tab::Diff(d) = &mut self.tabs[active] {
                        d.live = None;
                    }
                    return None;
                };
                self.add_file_tab(&path, disk.text, disk.crlf);
                self.tabs.len() - 1
            }
        };
        let settings = self.settings;
        let dark = ui.visuals().dark_mode;
        let (Tab::Diff(d), Tab::File(doc)) = two_tabs(&mut self.tabs, active, file) else {
            return None;
        };
        let live = d.live.as_mut().unwrap();
        live.update(doc, dark);

        let mut open_path = None;
        let mut jump = None;
        ui.horizontal(|ui| {
            if ui.button(format!("{} Open file", ic::ARROW_SQUARE_OUT)).clicked() {
                open_path = Some(path.clone());
            }
            ui.separator();
            let _ = ui.selectable_label(true, format!("{} Side by side", ic::COLUMNS));
            if ui.selectable_label(false, format!("{} Inline", ic::ROWS)).clicked() {
                d.inline = true;
            }
            let blocks = live.blocks();
            if !blocks.is_empty() {
                ui.separator();
                let top = live.tops.1 + 3.0;
                let current = blocks.iter().rposition(|&b| b as f32 <= top).unwrap_or(0);
                if icon_button(ui, ic::ARROW_UP).on_hover_text("Previous change").clicked() {
                    jump = blocks.iter().rev().find(|&&b| (b as f32) + 1.0 < top).or(blocks.last()).copied();
                }
                if icon_button(ui, ic::ARROW_DOWN).on_hover_text("Next change").clicked() {
                    jump = blocks.iter().find(|&&b| b as f32 > top).or(blocks.first()).copied();
                }
                ui.weak(format!("change {} of {}", current + 1, blocks.len()));
            }
            ui.weak(if doc.dirty() { "· editing (unsaved)" } else { "· editable" });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let (add, del) = diff_colors(ui.visuals().dark_mode);
                ui.colored_label(del, format!("−{}", live.removed));
                ui.colored_label(add, format!("+{}", live.added));
            });
        });
        if let Some(line) = jump {
            doc.ed.scroll_to_line(line.saturating_sub(3) as f32);
        }

        let lang = Path::new(&path)
            .extension()
            .map(|e| e.to_string_lossy().into_owned())
            .unwrap_or_default();
        let mut out: Option<EditorOut> = None;
        let mut left_top = live.tops.0;
        ui.columns(2, |cols| {
            cols[0].weak(format!("{} (base)", file_name(&path.to_string_lossy())));
            let opts = ed::Options {
                lang: &lang,
                highlight: live.base.len() <= super::HIGHLIGHT_LIMIT,
                whitespace: settings.show_whitespace,
                current_line: false,
                indent: "    ",
                comment: None,
                minimap: false,
                bracket_colors: settings.bracket_colors,
                read_only: true,
            };
            let extras = ed::Extras {
                line_bg: &live.left_bg,
                ..Default::default()
            };
            let id = cols[0].make_persistent_id(("diff-base", doc.id));
            let left = ed::show(
                &mut cols[0],
                id,
                &mut live.base_state,
                &mut live.base,
                &mut live.base_version,
                &mut live.base_hl,
                &opts,
                &extras,
            );
            left_top = left.top_line;
            cols[1].weak(format!("{} (working copy)", file_name(&path.to_string_lossy())));
            out = Some(editor(&mut cols[1], doc, settings, None, &[], &[], 3, &live.right_bg));
        });
        // Keep the panes lined up: whichever one the user scrolled leads.
        let right_top = out.as_ref().map_or(live.tops.1, |o| o.top_line);
        if (right_top - live.tops.1).abs() > 0.01 {
            live.base_state.scroll_to_line(live.map(right_top, true));
            live.tops = (live.map(right_top, true), right_top);
        } else if (left_top - live.tops.0).abs() > 0.01 {
            doc.ed.scroll_to_line(live.map(left_top, false));
            live.tops = (left_top, live.map(left_top, false));
        }
        if live.tops.0 != left_top || live.tops.1 != right_top {
            ui.ctx().request_repaint();
        }
        open_path
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_lines_through_the_diff() {
        let mut live = LiveDiff::new("a\nb\nc\nd\ne\n".into());
        let new = "a\nX\nY\nc\nd\nZ\ne\n";
        let diff = similar::TextDiff::from_lines(live.base.as_str(), new);
        live.ops = diff.ops().iter().map(|op| op.as_tag_tuple()).collect();
        // Right "c" (line 3) is left line 2; right "e" (6) is left 4.
        assert_eq!(live.map(3.0, true), 2.0);
        assert_eq!(live.map(6.0, true), 4.0);
        assert_eq!(live.map(3.5, true), 2.5);
        // Inside a change: the start of the other side's block.
        assert_eq!(live.map(2.0, true), 1.0);
        assert_eq!(live.map(5.0, true), 4.0);
        // And back.
        assert_eq!(live.map(2.0, false), 3.0);
        assert_eq!(live.map(4.0, false), 6.0);
        assert_eq!(live.blocks(), vec![1, 5]);
    }
}
