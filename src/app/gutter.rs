//! Gutter changes against the committed file (Git HEAD or Subversion BASE).

use super::{App, Doc, Tab, background, diff_colors, icon_button, receive};
use crate::diff::{Hunk, changed_blocks};
use crate::editor::buffer::{LineIndex, Sel};
use crate::{git, icons as ic, vcs::Vcs};
use eframe::egui::{self, Color32};
use std::ops::Range;
use std::sync::Arc;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) enum Change {
    Added,
    Modified,
    Deleted,
}

struct Base {
    text: Arc<str>,
    index: LineIndex,
}

struct Diffed {
    version: u64,
    hunks: Vec<Hunk>,
    lines: Vec<(usize, Change)>,
}

pub(super) type Click = (usize, bool, egui::Rect, egui::Id);

struct Preview {
    hunk: usize,
    pane: u8,
    anchor: egui::Rect,
    editor: egui::Id,
}

#[derive(Default)]
pub(super) struct Markers {
    base: Option<Option<Arc<Base>>>,
    base_rx: Option<Receiver<Option<Base>>>,
    diff: Option<Diffed>,
    rx: Option<Receiver<Diffed>>,
    preview: Option<Preview>,
}

impl Markers {
    fn current(&self, version: u64) -> Option<&Diffed> {
        self.diff.as_ref().filter(|diff| diff.version == version)
    }

    pub(super) fn lines(&self, version: u64) -> &[(usize, Change)] {
        self.current(version).map_or(&[], |diff| &diff.lines)
    }
}

#[derive(Clone, Copy)]
pub(super) enum Action {
    Next,
    Previous,
    Stage,
    Revert,
}

fn diff(base: &str, text: &str, version: u64) -> Diffed {
    let hunks = changed_blocks(base, text);
    let mut lines = Vec::new();
    for hunk in &hunks {
        if hunk.new.is_empty() {
            lines.push((hunk.new.start, Change::Deleted));
        } else {
            let change = if hunk.old.is_empty() { Change::Added } else { Change::Modified };
            lines.extend(hunk.new.clone().map(|line| (line, change)));
        }
    }
    Diffed { version, hunks, lines }
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

pub(super) fn step(hunks: &[Hunk], current: Option<usize>, line: usize, last: usize, next: bool) -> usize {
    if let Some(i) = current {
        return if next {
            (i + 1) % hunks.len()
        } else {
            i.checked_sub(1).unwrap_or(hunks.len() - 1)
        };
    }
    let anchor = |h: &Hunk| h.new.start.min(last);
    if next {
        hunks.iter().position(|h| anchor(h) > line).unwrap_or(0)
    } else {
        hunks.iter().rposition(|h| anchor(h) < line).unwrap_or(hunks.len() - 1)
    }
}

fn byte_range(text: &str, index: &LineIndex, lines: Range<usize>) -> Range<usize> {
    let at = |line| if line < index.lines() { index.line_start(line) } else { text.len() };
    at(lines.start)..at(lines.end)
}

pub(super) fn selected_hunk<'a>(hunks: &'a [Hunk], ix: &LineIndex, selection: Sel) -> Option<(&'a Hunk, Hunk)> {
    let range = selection.range();
    if range.is_empty() {
        return None;
    }
    let lines = ix.line_of_byte(range.start)..ix.line_of_byte(range.end - 1) + 1;
    let mut found = hunks.iter().filter(|h| h.intersects(&lines));
    let hunk = found.next()?;
    if found.next().is_some() {
        return None;
    }
    Some((hunk, hunk.selected(&lines)?))
}

pub(super) fn goto(doc: &mut Doc, line: usize) {
    let ix = doc.ed.index(&doc.text, doc.version);
    let byte = ix.line_start(line.min(ix.lines() - 1));
    let char = ix.byte_to_char(&doc.text, byte);
    doc.cursor = char;
    doc.anchor = char;
    doc.ed.select(byte..byte);
}

pub(super) fn revert(doc: &mut Doc, base: &str, index: &LineIndex, hunk: Hunk) {
    let old = byte_range(base, index, hunk.old);
    let new = byte_range(&doc.text, doc.ed.index(&doc.text, doc.version), hunk.new);
    doc.ed.replace(&mut doc.text, new, &base[old]);
    doc.version += 1;
    doc.edited_at = Some(Instant::now());
    doc.clear_blame();
    doc.markers.preview = None;
}

fn preview_ui(ui: &mut egui::Ui, old: &str, new: &str, can_stage: bool, busy: bool) -> Option<Action> {
    let mut action = None;
    ui.horizontal(|ui| {
        if can_stage
            && ui
                .add_enabled(!busy, egui::Button::new(format!("{} Stage hunk", ic::PLUS)))
                .clicked()
        {
            action = Some(Action::Stage);
        }
        if ui.button(format!("{} Revert hunk", ic::ARROW_U_UP_LEFT)).clicked() {
            action = Some(Action::Revert);
        }
        if icon_button(ui, ic::ARROW_UP).on_hover_text("Previous change").clicked() {
            action = Some(Action::Previous);
        }
        if icon_button(ui, ic::ARROW_DOWN).on_hover_text("Next change").clicked() {
            action = Some(Action::Next);
        }
    });
    ui.separator();
    let (add, del) = diff_colors(ui.visuals().dark_mode);
    egui::ScrollArea::both().max_height(220.0).max_width(480.0).show(ui, |ui| {
        let mut lines = old
            .split_inclusive('\n')
            .map(|s| ("- ", s, del))
            .chain(new.split_inclusive('\n').map(|s| ("+ ", s, add)));
        for (prefix, line, color) in lines.by_ref().take(100) {
            ui.add(
                egui::Label::new(
                    egui::RichText::new(format!("{prefix}{}", line.trim_end_matches('\n')))
                        .monospace()
                        .color(color),
                )
                .extend(),
            );
        }
        if lines.next().is_some() {
            ui.weak("Preview limited to 100 lines");
        }
    });
    action
}

impl App {
    pub(super) fn poll_markers(&mut self) {
        const SETTLE: Duration = Duration::from_millis(300);
        let fallback = self.repository();
        let active = self.active;
        for (i, tab) in self.tabs.iter_mut().enumerate() {
            if i != active && Some(i) != self.split {
                continue;
            }
            let Tab::File(doc) = tab else { continue };
            let Some((root, vcs)) = super::repositories::owner(&self.repos.list, &doc.path).or_else(|| fallback.clone()) else {
                continue;
            };
            let root = &root;
            let m = &mut doc.markers;
            if let Some(base) = receive(&mut m.base_rx) {
                m.base = Some(base.map(Arc::new));
            }
            if let Some(diff) = receive(&mut m.rx) {
                m.diff = Some(diff);
            }
            if m.base.is_none() && m.base_rx.is_none() {
                let Ok(rel) = doc.path.strip_prefix(root) else { continue };
                let rel = repo_path(rel);
                let root = root.clone();
                m.base_rx = Some(background(&self.ctx, move || {
                    let text = vcs.base_text(&root, &rel)?.replace("\r\n", "\n");
                    let index = LineIndex::new(&text);
                    Some(Base {
                        text: Arc::from(text),
                        index,
                    })
                }));
            }
            let Some(Some(base)) = m.base.clone() else { continue };
            if m.current(doc.version).is_some() || m.rx.is_some() {
                continue;
            }
            if let Some(t) = doc.edited_at
                && t.elapsed() < SETTLE
            {
                self.ctx.request_repaint_after(SETTLE - t.elapsed());
                continue;
            }
            let (text, version) = (doc.text.clone(), doc.version);
            m.rx = Some(background(&self.ctx, move || diff(&base.text, &text, version)));
        }
    }

    pub(super) fn reset_markers(&mut self) {
        for tab in &mut self.tabs {
            if let Tab::File(doc) = tab {
                doc.markers = Markers::default();
            }
        }
    }

    pub(super) fn gutter_popup(&mut self, ui: &egui::Ui, tab: usize, pane: u8, click: Option<Click>) {
        let Some(Tab::File(doc)) = self.tabs.get(tab) else { return };
        let git = self.repo_of(&doc.path).is_some_and(|(_, vcs)| vcs == Vcs::Git);
        let Some(Tab::File(doc)) = self.tabs.get_mut(tab) else { return };
        let Some(Diffed { hunks, .. }) = doc.markers.current(doc.version) else {
            doc.markers.preview = None;
            return;
        };
        if let Some((line, deleted, anchor, editor)) = click
            && let Some(hunk) = hunks.iter().position(|h| {
                if deleted {
                    h.new.is_empty() && h.new.start == line
                } else {
                    h.new.contains(&line)
                }
            })
        {
            doc.markers.preview = Some(Preview {
                hunk,
                pane,
                anchor,
                editor,
            });
        }
        let Some(preview) = &doc.markers.preview else { return };
        if preview.pane != pane {
            return;
        }
        let Some(hunk) = doc.markers.current(doc.version).and_then(|d| d.hunks.get(preview.hunk)) else {
            return;
        };
        let Some(Some(base)) = &doc.markers.base else { return };
        let old = &base.text[byte_range(&base.text, &base.index, hunk.old.clone())];
        let new = byte_range(&doc.text, doc.ed.index(&doc.text, doc.version), hunk.new.clone());
        let mut open = true;
        let result = egui::Popup::new(
            ui.make_persistent_id(("gutter-hunk", doc.id, pane)),
            ui.ctx().clone(),
            preview.anchor,
            ui.layer_id(),
        )
        .open_bool(&mut open)
        .close_behavior(if click.is_some() {
            egui::PopupCloseBehavior::IgnoreClicks
        } else {
            egui::PopupCloseBehavior::CloseOnClickOutside
        })
        .show(|ui| preview_ui(ui, old, &doc.text[new], git, self.bg_task.is_some()));
        let picked = preview.hunk;
        let editor = preview.editor;
        if !open {
            doc.markers.preview = None;
        }
        if let Some(action) = result.and_then(|r| r.inner) {
            self.gutter_action(tab, action, false, Some(picked));
            if matches!(action, Action::Stage | Action::Revert) {
                ui.memory_mut(|m| m.request_focus(editor));
            }
        }
    }

    pub(super) fn gutter_action(&mut self, tab: usize, action: Action, selected: bool, picked: Option<usize>) {
        let Some(Tab::File(doc)) = self.tabs.get(tab) else { return };
        let Some(repository) = self.repo_of(&doc.path) else { return };
        let (root, vcs) = (repository.0.clone(), repository.1);
        let root = root.as_path();
        let Some(Tab::File(doc)) = self.tabs.get_mut(tab) else { return };
        let Some(diff) = doc.markers.current(doc.version) else {
            self.message = "No current gutter changes".into();
            return;
        };
        let hunks = &diff.hunks;
        let primary = doc.ed.primary();
        let ix = doc.ed.index(&doc.text, doc.version);
        let line = ix.line_of_byte(primary.head);
        let last = ix.lines() - 1;
        let current = picked.or_else(|| hunks.iter().position(|h| h.at(line, last)));
        if matches!(action, Action::Next | Action::Previous) {
            if !hunks.is_empty() {
                let current = doc.markers.preview.as_ref().map(|p| p.hunk).or(current);
                let i = step(hunks, current, line, last, matches!(action, Action::Next));
                let line = hunks[i].new.start;
                if let Some(preview) = &mut doc.markers.preview {
                    preview.hunk = i;
                }
                goto(doc, line);
            }
            return;
        }
        let hunk = if selected {
            selected_hunk(hunks, ix, primary)
        } else {
            current.and_then(|i| hunks.get(i)).map(|h| (h, h.clone()))
        };
        let Some((full, hunk)) = hunk else {
            self.message = if selected {
                "Select lines in one addition or one-for-one replacement, or select the whole hunk"
            } else {
                "Place the caret on a changed line"
            }
            .into();
            return;
        };
        let Some(Some(base)) = doc.markers.base.clone() else { return };
        match action {
            Action::Revert => {
                revert(doc, &base.text, &base.index, hunk);
                self.message = "Reverted hunk in the editor".into();
            }
            Action::Stage if vcs.has_index() && self.bg_task.is_none() => {
                let Ok(path) = doc.path.strip_prefix(root) else { return };
                let path = repo_path(path);
                let (text, crlf) = (doc.text.clone(), doc.crlf);
                let base = git::BufferBase::Committed {
                    text: base.text.clone(),
                    full_new: full.new.clone(),
                };
                doc.markers.preview = None;
                self.stage_buffer_hunk(repository, path, base, text, crlf, hunk);
            }
            Action::Stage | Action::Next | Action::Previous => {}
        }
    }
}

fn repo_path(path: &std::path::Path) -> String {
    path.components()
        .map(|part| part.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markers_and_navigation_include_deletions() {
        let base = "a\nb\nc\nd\nlast\n";
        assert!(diff(base, base, 0).lines.is_empty());
        let result = diff(base, "a\nB\nc\nd\n", 1);
        assert_eq!(result.lines, [(1, Change::Modified), (4, Change::Deleted)]);
        assert_eq!(step(&result.hunks, None, 0, 4, true), 0);
        assert_eq!(step(&result.hunks, Some(0), 1, 4, true), 1);
        assert_eq!(step(&result.hunks, Some(0), 1, 4, false), 1);
        assert_eq!(step(&result.hunks, Some(1), 4, 4, true), 0);
        assert!(result.hunks[1].at(3, 3));
    }

    #[test]
    fn preview_dispatches_revert_and_hides_stage_without_an_index() {
        let ctx = egui::Context::default();
        ctx.set_fonts(crate::icons::definitions(4, None, None));
        let frame = |events| {
            let mut action = None;
            let mut output = ctx.run_ui(
                egui::RawInput {
                    events,
                    ..Default::default()
                },
                |ui| {
                    action = preview_ui(ui, "before\n", "after\n", false, false);
                },
            );
            output.textures_delta.clear();
            (action, output.shapes)
        };
        let (_, shapes) = frame(vec![]);
        let labels: Vec<_> = shapes
            .iter()
            .filter_map(|shape| match &shape.shape {
                egui::Shape::Text(text) => Some((text.galley.job.text.as_str(), text.pos + text.galley.size() / 2.0)),
                _ => None,
            })
            .collect();
        assert!(labels.iter().any(|(text, _)| *text == "- before"));
        assert!(labels.iter().all(|(text, _)| !text.contains("Stage hunk")));
        let pos = labels.iter().find(|(text, _)| text.contains("Revert hunk")).unwrap().1;
        frame(vec![egui::Event::PointerMoved(pos)]);
        for pressed in [true, false] {
            let (action, _) = frame(vec![egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::NONE,
            }]);
            assert_eq!(matches!(action, Some(Action::Revert)), !pressed);
        }
    }
}
