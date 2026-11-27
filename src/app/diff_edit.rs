//! Editable side-by-side diff of a working-copy change: the left pane is the base
//! (git index / svn BASE, read-only but selectable), the right pane is the real
//! editor of the file (the same document as its tab, so editing, undo and saving
//! work as usual). Changed lines are tinted and recomputed as you type, and the two
//! panes scroll together, lined up through the diff.

use super::{
    App, DiffSource, DiskText, Doc, EditorOut, LiveDiffMarks, Tab, background, diff_colors, editor, file_name, gutter, icon_button,
    read_doc_text, receive,
};
use crate::commands::{Command, Setting};
use crate::diff::{Hunk, with_editor_eol, without_cr_at_eol};
use crate::git::BufferBase;
use crate::vcs::Vcs;
use crate::{editor as ed, highlight, icons as ic};
use eframe::egui::{self, Color32};
use similar::{Algorithm, DiffTag};
use std::borrow::Cow;
use std::ops::Range;
use std::path::Path;
use std::sync::{Arc, mpsc::Receiver};
use std::time::Duration;

pub(super) struct LiveDiff {
    base: String,
    initial_disk: Option<DiskText>,
    disk_rx: Option<Receiver<std::io::Result<Option<DiskText>>>>,
    working_eol: Option<(u64, u64, Arc<str>)>,
    base_state: ed::State,
    base_version: u64,
    base_hl: highlight::Cache,
    /// (document id, version, dark, ignore EOL) the ops/tints below were computed for.
    key: Option<(u64, u64, bool, bool)>,
    ops: Vec<(DiffTag, Range<usize>, Range<usize>)>,
    left_bg: Vec<(usize, Color32)>,
    right_bg: Vec<(usize, Color32)>,
    change_marks: Vec<(usize, Color32, bool)>,
    added: usize,
    removed: usize,
    only_line_endings: bool,
    /// Top lines (left, right) as last shown, to tell which pane the user scrolled.
    tops: (f32, f32),
    /// Put the caret on the first change when the view is first drawn.
    pub(super) first_change: bool,
}

impl LiveDiff {
    pub(super) fn new(base: String, initial_disk: Option<DiskText>) -> Self {
        Self {
            base,
            initial_disk,
            disk_rx: None,
            working_eol: None,
            base_state: ed::State::default(),
            base_version: 0,
            base_hl: highlight::Cache::default(),
            key: None,
            ops: Vec::new(),
            left_bg: Vec::new(),
            right_bg: Vec::new(),
            change_marks: Vec::new(),
            added: 0,
            removed: 0,
            only_line_endings: false,
            tops: (0.0, 0.0),
            first_change: false,
        }
    }

    fn update(&mut self, doc: &Doc, dark: bool, ignore_eol: bool) {
        let key = (doc.id, doc.version, dark, ignore_eol);
        if self.key == Some(key) {
            return;
        }
        self.key = Some(key);
        let raw = self
            .working_eol
            .as_ref()
            .and_then(|(id, version, raw)| (*id == doc.id && *version == doc.version).then(|| Arc::clone(raw)));
        let editor = match raw.as_deref() {
            Some(text) => Cow::Borrowed(text),
            None => with_editor_eol(&doc.text, doc.crlf),
        };
        self.update_ops(&editor, ignore_eol);
        let (add, del) = diff_colors(dark);
        let (add, del) = (add.gamma_multiply(0.16), del.gamma_multiply(0.16));
        self.left_bg.clear();
        self.right_bg.clear();
        self.change_marks.clear();
        for (tag, old, new) in &self.ops {
            if *tag != DiffTag::Equal {
                self.left_bg.extend(old.clone().map(|l| (l, del)));
                self.right_bg.extend(new.clone().map(|l| (l, add)));
                let change = match tag {
                    DiffTag::Delete => gutter::Change::Deleted,
                    DiffTag::Insert => gutter::Change::Added,
                    _ => gutter::Change::Modified,
                };
                self.change_marks.push((new.start, gutter::color(change, dark), new.is_empty()));
            }
        }
        self.removed = self.left_bg.len();
        self.added = self.right_bg.len();
    }

    fn update_ops(&mut self, editor: &str, ignore_eol: bool) {
        let old = without_cr_at_eol(&self.base, ignore_eol);
        let new = without_cr_at_eol(editor, ignore_eol);
        self.only_line_endings = old == new && self.base != editor;
        // Bounded: a huge rewrite falls back to a coarser diff instead of stalling a frame.
        let diff = similar::TextDiff::configure()
            .algorithm(Algorithm::Myers)
            .timeout(Duration::from_millis(30))
            .diff_slices(
                &old.split_inclusive('\n').collect::<Vec<_>>(),
                &new.split_inclusive('\n').collect::<Vec<_>>(),
            );
        self.ops = diff.ops().iter().map(|op| op.as_tag_tuple()).collect();
    }

    /// First right-side line of each change.
    fn blocks(&self) -> Vec<usize> {
        self.ops
            .iter()
            .filter(|(t, ..)| *t != DiffTag::Equal)
            .map(|(_, _, new)| new.start)
            .collect()
    }

    fn hunks(&self) -> impl Iterator<Item = Hunk> + '_ {
        self.ops
            .iter()
            .filter(|(tag, ..)| *tag != DiffTag::Equal)
            .map(|(_, old, new)| Hunk {
                old: old.clone(),
                new: new.clone(),
            })
    }

    pub(super) fn keep_scroll_from(&mut self, previous: &Self) {
        self.tops = previous.tops;
        self.base_state.scroll_to_line(self.tops.0);
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
    pub(super) fn hunk_command(&mut self, cmd: Command) {
        let active = self.active;
        let action = match cmd {
            Command::NextChange => gutter::Action::Next,
            Command::PreviousChange => gutter::Action::Previous,
            Command::StageHunk | Command::StageSelectedLines => gutter::Action::Stage,
            _ => gutter::Action::Revert,
        };
        let unstage = matches!(cmd, Command::UnstageHunk | Command::UnstageSelectedLines);
        let selected = matches!(
            cmd,
            Command::StageSelectedLines | Command::UnstageSelectedLines | Command::RevertSelectedLines
        );
        if matches!(self.tabs.get(active), Some(Tab::File(_))) && !unstage {
            self.gutter_action(active, action, selected, None);
            return;
        }
        if let Some(file) = self.live_diff_doc(active)
            && self.is_live_diff(active)
            && !unstage
        {
            self.live_hunk_action(active, file, action, selected, None);
            return;
        }
        let Some(Tab::Diff(d)) = self.tabs.get_mut(active) else { return };
        if d.inline || d.hunks_stale || d.blocks.is_empty() {
            return;
        }
        match cmd {
            Command::NextChange | Command::PreviousChange => d.move_change(cmd == Command::NextChange),
            Command::StageHunk | Command::UnstageHunk | Command::StageSelectedLines | Command::UnstageSelectedLines
                if d.source.git_hunks().is_some_and(|(_, staged)| staged == unstage) =>
            {
                let hunk = if selected {
                    d.selected_hunk()
                } else {
                    Some(d.sbs.blocks[d.current_change()].1.clone())
                };
                if let Some(hunk) = hunk {
                    self.apply_diff_hunk(active, hunk);
                } else {
                    self.message = "Select lines within one hunk".into();
                }
            }
            _ => {}
        }
    }

    fn live_hunk_action(&mut self, active: usize, file: usize, action: gutter::Action, selected: bool, picked: Option<usize>) {
        let (Tab::Diff(d), Tab::File(doc)) = two_tabs(&mut self.tabs, active, file) else {
            return;
        };
        if d.hunks_stale {
            return;
        }
        let live = d.live.as_mut().unwrap();
        live.update(doc, self.ctx.theme() == egui::Theme::Dark, self.settings.diff_ignore_eol);
        let hunks: Vec<_> = live.hunks().collect();
        if hunks.is_empty() {
            return;
        }
        let primary = doc.ed.primary();
        let ix = doc.ed.index(&doc.text, doc.version);
        let line = ix.line_of_byte(primary.head);
        let last = ix.lines() - 1;
        let current = picked.or_else(|| hunks.iter().position(|h| h.at(line, last)));
        if matches!(action, gutter::Action::Next | gutter::Action::Previous) {
            let target = gutter::step(&hunks, current, line, last, matches!(action, gutter::Action::Next));
            gutter::goto(doc, hunks[target].new.start);
            return;
        }
        let hunk = if selected {
            gutter::selected_hunk(&hunks, ix, primary).map(|(_, hunk)| hunk)
        } else {
            current.and_then(|i| hunks.get(i).cloned())
        };
        let Some(hunk) = hunk else {
            self.message = "Place the caret on a changed line or select lines within one hunk".into();
            return;
        };
        match action {
            gutter::Action::Stage if self.bg_task.is_none() => {
                let Some((change, false)) = d.source.git_hunks() else { return };
                let DiffSource::Vcs { repo, vcs, .. } = &d.source else { return };
                let repository = (repo.clone(), *vcs);
                let (path, base, text, crlf) = (change.path.clone(), live.base.clone(), doc.text.clone(), doc.crlf);
                self.stage_buffer_hunk(repository, path, BufferBase::Index(base), text, crlf, hunk);
            }
            gutter::Action::Revert => {
                let base = without_cr_at_eol(&live.base, true);
                let index = ed::buffer::LineIndex::new(&base);
                gutter::revert(doc, &base, &index, hunk);
                self.message = "Reverted hunk in the editor".into();
            }
            _ => {}
        }
    }

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
        let open_doc = self.live_diff_doc(active);
        // The right pane edits the file's own document, using the worker's snapshot if unopened.
        let Tab::Diff(d) = &mut self.tabs[active] else { return None };
        if d.hunks_stale {
            ui.weak("Loading diff…");
            return None;
        }
        let path = d.path.clone();
        let live = d.live.as_mut()?;
        let mut disk = live.initial_disk.take();
        let file = match open_doc {
            Some(i) => i,
            None => {
                if disk.is_none() {
                    let source = path.clone();
                    let rx = &mut live.disk_rx;
                    if rx.is_none() {
                        *rx = Some(background(&self.ctx, move || read_doc_text(&source)));
                    }
                    disk = match receive(rx) {
                        Some(Ok(Some(disk))) => Some(disk),
                        _ if rx.is_some() => {
                            ui.weak("Loading diff…");
                            return None;
                        }
                        _ => {
                            d.live = None;
                            ui.ctx().request_repaint();
                            return None;
                        }
                    };
                }
                let disk = disk.as_mut().unwrap();
                self.add_file_tab(&path, std::mem::take(&mut disk.text), disk.crlf, disk.encoding);
                self.record_tab_recent(self.tabs.len() - 1);
                self.tabs.len() - 1
            }
        };
        let settings = &self.settings;
        let highlights = &self.highlights;
        let dark = ui.visuals().dark_mode;
        let (Tab::Diff(d), Tab::File(doc)) = two_tabs(&mut self.tabs, active, file) else {
            return None;
        };
        let live = d.live.as_mut().unwrap();
        if let Some(disk) = disk
            && (open_doc.is_none() || (disk.crlf == doc.crlf && disk.text == doc.text))
        {
            live.working_eol = disk.raw_crlf.map(|raw| (doc.id, doc.version, raw));
        }
        live.update(doc, dark, settings.diff_ignore_eol);
        if std::mem::take(&mut live.first_change)
            && let Some(&first) = live.blocks().first()
        {
            gutter::goto(doc, first);
        }

        let mut open_path = None;
        let mut navigate = None;
        let mut clicked_hunk = None;
        let hunk_button = match &d.source {
            DiffSource::Vcs { vcs: Vcs::Svn, .. } => Some((gutter::Action::Revert, (ic::ARROW_U_UP_LEFT, "Revert hunk"))),
            _ if d.source.git_hunks().is_some() && self.bg_task.is_none() => Some((gutter::Action::Stage, (ic::PLUS, "Stage hunk"))),
            _ => None,
        };
        let mut ignore_eol = settings.diff_ignore_eol;
        ui.horizontal(|ui| {
            if ui.button(format!("{} Open file", ic::ARROW_SQUARE_OUT)).clicked() {
                open_path = Some(path.clone());
            }
            ui.separator();
            let _ = ui.selectable_label(true, format!("{} Side by side", ic::COLUMNS));
            if ui.selectable_label(false, format!("{} Inline", ic::ROWS)).clicked() {
                d.inline = true;
            }
            ui.checkbox(&mut ignore_eol, "Ignore line endings");
            let blocks = live.blocks();
            if !blocks.is_empty() {
                ui.separator();
                let caret = doc.line_col().0;
                let current = blocks.partition_point(|&b| b <= caret).saturating_sub(1);
                if icon_button(ui, ic::ARROW_UP).on_hover_text("Previous change").clicked() {
                    navigate = Some(gutter::Action::Previous);
                }
                if icon_button(ui, ic::ARROW_DOWN).on_hover_text("Next change").clicked() {
                    navigate = Some(gutter::Action::Next);
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
        if live.only_line_endings {
            ui.weak("Only line endings differ");
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
                palette: settings.themes.active(cols[0].visuals().dark_mode),
                lang: &lang,
                highlight: live.base.len() <= super::HIGHLIGHT_LIMIT,
                whitespace: settings.show_whitespace,
                current_line: false,
                indent: "    ",
                comment: None,
                minimap: false,
                bracket_colors: settings.bracket_colors,
                sticky_scroll: false,
                sticky_control_flow: false,
                sticky_line: true,
                wrap: false,
                sticky_shown: crate::editor::sticky::Kinds::ALL,
                navigation_menu: false,
                read_only: true,
            };
            let extras = ed::Extras {
                line_bg: &live.left_bg,
                highlights,
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
            out = Some(editor(
                &mut cols[1],
                doc,
                settings,
                None,
                &[],
                &[],
                3,
                Some(LiveDiffMarks {
                    line_bg: &live.right_bg,
                    gutter: if hunk_button.is_some() { &live.change_marks } else { &[] },
                    button: hunk_button.map(|(_, button)| button),
                }),
                &[],
                highlights,
            ));
        });
        if let Some((line, deleted, _, _)) = out.as_ref().and_then(|o| o.gutter_click) {
            clicked_hunk = live
                .change_marks
                .iter()
                .position(|&(start, _, empty)| start == line && empty == deleted);
        }
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
        let goto = out.and_then(|o| o.goto);
        if ignore_eol != settings.diff_ignore_eol {
            self.run_command(Command::ToggleSetting(Setting::DiffIgnoreEol));
        }
        // Ctrl/Cmd+click in the working copy: go to the definition, from the file's own
        // tab (the jump and Go back work as in the normal editor).
        if let Some(idx) = goto {
            self.activate_tab(file);
            self.go_to_definition(idx, &ui.ctx().clone());
        }
        if let (Some(hunk), Some((action, _))) = (clicked_hunk, hunk_button) {
            self.live_hunk_action(active, file, action, false, Some(hunk));
        } else if let Some(action) = navigate {
            self.live_hunk_action(active, file, action, false, None);
            ui.ctx().request_repaint();
        }
        open_path
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ignores_only_cr_before_lf() {
        let mut live = LiveDiff::new("a\r\nb\r\n".into(), None);
        live.update_ops("a\nb\n", true);
        assert!(live.blocks().is_empty());
        assert!(live.only_line_endings);
        live.update_ops("a\nb\n", false);
        assert_eq!(live.blocks(), [0]);
        assert!(!live.only_line_endings);
        live.update_ops("a \nb", true);
        assert_eq!(live.blocks(), [0]);
        assert!(!live.only_line_endings);
        live.update_ops("a\nb", true);
        assert_eq!(live.blocks(), [1]);
    }

    #[test]
    fn maps_lines_through_the_diff() {
        let mut live = LiveDiff::new("a\nb\nc\nd\ne\n".into(), None);
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
