use super::{App, Doc, Instant, Tab, background, ic, receive, toggle};
use crate::{replace, search};
use eframe::egui::{self, Key, Modifiers};
use std::ops::Range;
use std::sync::mpsc::Receiver;

const QUERY: &str = "file-find-query";
const REPLACEMENT: &str = "file-find-replacement";

struct Target {
    id: u64,
    version: u64,
    opts: search::Options,
}

impl Target {
    fn is_current(&self, doc: &Doc, opts: &search::Options) -> bool {
        self.id == doc.id && self.version == doc.version && &self.opts == opts
    }
}

type Matches = Result<Vec<Range<usize>>, String>;
type FindResult = (Target, Matches);
type EditResult = (Target, Result<Option<replace::Edit>, String>);

#[derive(Default)]
pub(super) struct Find {
    open: bool,
    replacing: bool,
    focus_query: bool,
    restore_focus: Option<egui::Id>,
    opts: search::Options,
    replacement: String,
    cache: Option<FindResult>,
    rendered_version: u64,
    match_rx: Option<Receiver<FindResult>>,
    edit_rx: Option<Receiver<EditResult>>,
    current: usize,
    reveal_from: Option<(u64, usize)>,
    pending_step: Option<bool>,
}

impl App {
    pub(super) fn show_find(&mut self, replacing: bool) {
        let Some(Tab::File(doc)) = self.tabs.get(self.active) else { return };
        let find = &mut self.find;
        if let Some(text) = doc.selected_text() {
            find.opts.query = if find.opts.regex { regex::escape(&text) } else { text };
        }
        find.open = true;
        find.replacing = replacing;
        find.focus_query = true;
        find.reveal_from = Some((doc.id, doc.cursor.min(doc.anchor)));
    }

    pub(super) fn poll_find_edit(&mut self) {
        let Some((target, result)) = receive(&mut self.find.edit_rx) else {
            return;
        };
        let Ok(Some(edit)) = result else { return };
        for (i, tab) in self.tabs.iter_mut().enumerate() {
            let Tab::File(doc) = tab else { continue };
            if doc.id != target.id {
                continue;
            }
            if doc.version != target.version {
                self.message = "Document changed while replacing. Run replace again.".into();
                break;
            }
            if self.active == i && self.find.open && self.find.opts == target.opts {
                self.find.reveal_from = edit.next.map(|pos| (doc.id, pos));
            }
            apply_text(doc, edit.text, edit.char_len);
            break;
        }
    }

    // The editor can consume these ranges when match highlighting is added.
    #[allow(dead_code)]
    pub(super) fn find_matches(&self) -> Option<&[Range<usize>]> {
        let Some(Tab::File(doc)) = self.tabs.get(self.active) else {
            return None;
        };
        self.find.matches(doc)
    }
}

impl Find {
    #[cfg(feature = "tour")]
    pub(super) fn close_for_tour(&mut self) {
        self.open = false;
    }

    #[cfg(feature = "tour")]
    pub(super) fn set_query_for_tour(&mut self, query: &str) {
        self.opts.query = query.into();
        self.replacement = "track".into();
    }

    fn matches(&self, doc: &Doc) -> Option<&[Range<usize>]> {
        let (target, result) = self.cache.as_ref()?;
        (self.open && target.is_current(doc, &self.opts)).then_some(result.as_ref().ok()?.as_slice())
    }

    pub(super) fn bar(&mut self, ui: &mut egui::Ui, doc: &mut Doc, dialog_open: bool) {
        let finished = receive(&mut self.match_rx);
        if !self.open {
            return;
        }
        if !dialog_open && ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape)) {
            self.close(doc, ui.ctx());
            return;
        }
        if let Some((target, result)) = finished
            && target.is_current(doc, &self.opts)
        {
            self.update_cache(doc, (target, result));
        }

        self.rendered_version = doc.version;
        let query_id = ui.make_persistent_id(QUERY);
        let replace_id = ui.make_persistent_id(REPLACEMENT);
        let focus = ui
            .memory(|m| m.focused())
            .filter(|id| *id == query_id || (self.replacing && *id == replace_id));
        let step = |key| {
            ui.input_mut(|i| {
                if i.consume_key(Modifiers::SHIFT, key) {
                    Some(true)
                } else {
                    i.consume_key(Modifiers::NONE, key).then_some(false)
                }
            })
        };
        if !dialog_open && self.edit_rx.is_none() {
            self.pending_step = step(Key::F3).or_else(|| focus.and_then(|_| step(Key::Enter))).or(self.pending_step);
        }

        // Tab moves from the find box to the replace box (Shift+Tab back), instead of
        // through the option toggles in between.
        if self.replacing && !dialog_open {
            let tab = ui.input(|i| {
                let pressed = |m: Modifiers| {
                    i.events
                        .iter()
                        .any(|e| matches!(e, egui::Event::Key { key: Key::Tab, pressed: true, modifiers, .. } if *modifiers == m))
                };
                (pressed(Modifiers::NONE), pressed(Modifiers::SHIFT))
            });
            let target = match (focus, tab) {
                (Some(id), (true, _)) if id == query_id => Some(replace_id),
                (Some(id), (_, true)) if id == replace_id => Some(query_id),
                _ => None,
            };
            if let Some(id) = target {
                ui.memory_mut(|m| {
                    m.move_focus(egui::FocusDirection::None);
                    m.request_focus(id);
                });
            }
        }

        let mut close = false;
        ui.horizontal_wrapped(|ui| {
            let caret = if self.replacing { ic::CARET_DOWN } else { ic::CARET_RIGHT };
            toggle(ui, &mut self.replacing, caret, "Toggle replace");
            let mut query = super::tab_field::show(ui, query_id, &mut self.opts.query, |field| {
                field.desired_width(190.0).hint_text("Find")
            });
            if std::mem::take(&mut self.focus_query) {
                use egui::text::{CCursor, CCursorRange};
                query.state.cursor.set_char_range(Some(CCursorRange::two(
                    CCursor::new(0),
                    CCursor::new(self.opts.query.chars().count()),
                )));
                query.state.store(ui.ctx(), query_id);
                query.response.request_focus();
                self.restore_focus = Some(query_id);
            }
            let changed = query.response.changed()
                | toggle_option(ui, &mut self.opts.match_case, "Aa", "Match case")
                | toggle_option(ui, &mut self.opts.whole_word, "ab", "Match whole word")
                | toggle_option(ui, &mut self.opts.regex, ".*", "Use regular expression");
            if changed {
                self.reveal_from = Some((doc.id, doc.cursor.min(doc.anchor)));
            }
            self.refresh(doc, ui.ctx());
            self.navigate(doc);

            let ready = self.matches(doc).is_some_and(|matches| !matches.is_empty()) && self.edit_rx.is_none();
            for (back, icon, tooltip) in [
                (true, ic::ARROW_UP, "Previous match (Shift+Enter / Shift+F3)"),
                (false, ic::ARROW_DOWN, "Next match (Enter / F3)"),
            ] {
                if ui
                    .add_enabled(ready, egui::Button::new(icon).small())
                    .on_hover_text(tooltip)
                    .clicked()
                {
                    self.pending_step = Some(back);
                    self.navigate(doc);
                }
            }
            match self.cache.as_ref() {
                Some((_, Ok(matches))) => {
                    let current = if matches.is_empty() { 0 } else { self.current + 1 };
                    ui.weak(format!("{current} of {}", matches.len()));
                }
                Some((_, Err(err))) => {
                    ui.colored_label(ui.visuals().error_fg_color, err);
                }
                None => {
                    ui.weak("Finding…");
                }
            }
            close = ui.small_button(ic::X).on_hover_text("Close find (Esc)").clicked();
        });

        if self.replacing {
            ui.horizontal_wrapped(|ui| {
                super::tab_field::show(ui, replace_id, &mut self.replacement, |field| {
                    field.desired_width(190.0).hint_text("Replace")
                });
                let ready = self.matches(doc).is_some_and(|matches| !matches.is_empty()) && self.edit_rx.is_none();
                let one = ui.add_enabled(ready, egui::Button::new("Replace")).clicked();
                let all = ui.add_enabled(ready, egui::Button::new("Replace all")).clicked();
                if one || all {
                    self.start_edit(doc, ui.ctx(), one.then_some(self.current));
                    self.refresh(doc, ui.ctx());
                    self.navigate(doc);
                }
                if self.edit_rx.is_some() {
                    ui.weak("Replacing…");
                }
            });
        }

        if close {
            self.close(doc, ui.ctx());
        } else if doc.goto.is_some() {
            ui.ctx().request_repaint();
            self.restore_focus = self.restore_focus.or_else(|| {
                ui.memory(|m| m.focused())
                    .filter(|id| *id == query_id || (self.replacing && *id == replace_id))
                    .or(focus)
            });
        }
        ui.separator();
    }

    fn close(&mut self, doc: &mut Doc, ctx: &egui::Context) {
        self.open = false;
        self.reveal_from = None;
        self.pending_step = None;
        self.restore_focus = None;
        doc.goto.get_or_insert((doc.cursor, doc.anchor));
        ctx.request_repaint();
    }

    pub(super) fn after_editor(&mut self, ui: &egui::Ui, doc: &Doc) {
        if self.open && self.rendered_version != doc.version {
            ui.ctx().request_repaint();
        }
        // Revealing a selection asks the editor for focus. Keep typing in the bar
        // if the user was already there.
        if let Some(id) = self.restore_focus.take() {
            ui.memory_mut(|m| m.request_focus(id));
        }
    }

    fn refresh(&mut self, doc: &Doc, ctx: &egui::Context) {
        if self.cache.as_ref().is_some_and(|(target, _)| target.is_current(doc, &self.opts)) {
            return;
        }
        self.cache = None;
        if self.match_rx.is_some() && !self.opts.query.is_empty() && replace::needs_background(&doc.text) {
            return;
        }
        let target = Target {
            id: doc.id,
            version: doc.version,
            opts: self.opts.clone(),
        };
        if !self.opts.query.is_empty() && replace::needs_background(&doc.text) {
            let text = doc.text.clone();
            self.match_rx = Some(background(ctx, move || {
                let matches = replace::find(&text, &target.opts);
                (target, matches)
            }));
        } else {
            let matches = replace::find(&doc.text, &target.opts);
            self.update_cache(doc, (target, matches));
        }
    }

    fn update_cache(&mut self, doc: &Doc, result: FindResult) {
        self.current = match &result.1 {
            Ok(matches) => match_at(matches, doc.cursor.min(doc.anchor)),
            Err(_) => 0,
        };
        self.cache = Some(result);
    }

    fn navigate(&mut self, doc: &mut Doc) {
        let Some(matches) = self.matches(doc) else {
            if self.cache.is_some() {
                self.pending_step = None;
            }
            return;
        };
        let from = self.reveal_from.filter(|(id, _)| *id == doc.id).map(|(_, pos)| pos);
        let step = self.pending_step;
        if matches.is_empty() {
            self.reveal_from = None;
            self.pending_step = None;
            return;
        }
        let mut index = from.map_or(self.current, |pos| match_at(matches, pos));
        if let Some(back) = step {
            index = if back {
                (index + matches.len() - 1) % matches.len()
            } else {
                (index + 1) % matches.len()
            };
        }
        if from.is_some() || step.is_some() {
            let range = &matches[index];
            doc.goto = Some((range.start, range.end));
            doc.goto_keeps_focus = true;
            self.current = index;
            self.reveal_from = None;
            self.pending_step = None;
        }
    }

    fn start_edit(&mut self, doc: &mut Doc, ctx: &egui::Context, index: Option<usize>) {
        if replace::needs_background(&doc.text) {
            let target = Target {
                id: doc.id,
                version: doc.version,
                opts: self.opts.clone(),
            };
            let (text, replacement) = (doc.text.clone(), self.replacement.clone());
            self.edit_rx = Some(background(ctx, move || {
                let result = replace::document_edit(&text, &target.opts, &replacement, index);
                (target, result)
            }));
        } else {
            let result = replace::document_edit(&doc.text, &self.opts, &self.replacement, index);
            if let Ok(Some(edit)) = result {
                self.reveal_from = edit.next.map(|pos| (doc.id, pos));
                apply_text(doc, edit.text, edit.char_len);
            }
        }
    }
}

fn match_at(matches: &[Range<usize>], pos: usize) -> usize {
    let index = matches.partition_point(|range| range.start < pos && range.end <= pos);
    if index == matches.len() { 0 } else { index }
}

fn toggle_option(ui: &mut egui::Ui, on: &mut bool, label: &str, tip: &str) -> bool {
    let was_on = *on;
    toggle(ui, on, label, tip);
    was_on != *on
}

pub(super) fn apply_text(doc: &mut Doc, text: String, char_len: usize) {
    if doc.text != text {
        doc.text = text;
        doc.cursor = doc.cursor.min(char_len);
        doc.anchor = doc.anchor.min(char_len);
        doc.goto = doc.goto.map(|(a, b)| (a.min(char_len), b.min(char_len)));
        doc.version += 1;
        doc.edited_at = Some(Instant::now());
    }
}
