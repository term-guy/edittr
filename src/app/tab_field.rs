//! Tabs in single-line search fields: Tab moves the focus, so Ctrl+Tab types a tab,
//! and tabs get a faint background so they can be told apart from spaces.

use eframe::egui::{
    self, Galley, Key, TextBuffer,
    text::{CCursor, CCursorRange, CharIndex, LayoutJob, TextFormat},
};
use std::sync::Arc;

const HINT: &str = "Ctrl+Tab types a tab";

/// Inserts a tab at the cursor of the focused field `id` when Ctrl+Tab was pressed
/// (call before showing the field). Returns whether the text changed.
fn insert_typed_tab(ui: &egui::Ui, id: egui::Id, text: &mut String) -> bool {
    if !ui.memory(|m| m.has_focus(id)) {
        return false;
    }
    let pressed = ui.input_mut(|i| {
        let before = i.events.len();
        i.events.retain(|e| {
            !matches!(e, egui::Event::Key { key: Key::Tab, pressed: true, modifiers, .. }
                if modifiers.ctrl && !modifiers.shift && !modifiers.alt)
        });
        i.events.len() != before
    });
    if !pressed {
        return false;
    }
    let mut state = egui::TextEdit::load_state(ui.ctx(), id).unwrap_or_default();
    let chars = text.chars().count();
    let (a, b) = state.cursor.char_range().map_or((chars, chars), |r| {
        let (a, b) = (r.primary.index.0.min(chars), r.secondary.index.0.min(chars));
        (a.min(b), a.max(b))
    });
    text.delete_char_range(CharIndex(a)..CharIndex(b));
    text.insert_text("\t", CharIndex(a));
    state.cursor.set_char_range(Some(CCursorRange::one(CCursor::new(a + 1))));
    egui::TextEdit::store_state(ui.ctx(), id, state);
    true
}

/// A single-line field where Ctrl+Tab types a tab and tabs show as the editor's
/// whitespace arrow. `build` adds the field's own settings (hint, width, ...).
pub(super) fn show(
    ui: &mut egui::Ui,
    id: egui::Id,
    text: &mut String,
    build: impl FnOnce(egui::TextEdit<'_>) -> egui::TextEdit<'_>,
) -> egui::text_edit::TextEditOutput {
    let typed = insert_typed_tab(ui, id, text);
    let mut out = build(egui::TextEdit::singleline(text).id(id).layouter(&mut layouter)).show(ui);
    if typed {
        out.response.mark_changed();
    }
    paint_tabs(ui, &out);
    out.response.response = out.response.response.clone().on_hover_text(HINT);
    out
}

/// Draws the whitespace arrow over each tab of the field's text.
fn paint_tabs(ui: &egui::Ui, out: &egui::text_edit::TextEditOutput) {
    let painter = ui.painter_at(out.text_clip_rect);
    let stroke = egui::Stroke::new(1.0, ui.visuals().weak_text_color());
    for row in &out.galley.rows {
        let y = out.galley_pos.y + row.rect().center().y;
        for g in row.row.glyphs.iter().filter(|g| g.chr == '\t') {
            let x0 = out.galley_pos.x + row.pos.x + g.pos.x;
            crate::editor::paint_tab_arrow(&painter, x0, x0 + g.advance_width, y, stroke);
        }
    }
}

/// Layouter for a single-line field that marks tab characters.
fn layouter(ui: &egui::Ui, text: &dyn TextBuffer, _wrap: f32) -> Arc<Galley> {
    let text = text.as_str();
    let font = egui::FontSelection::default().resolve(ui.style()); // what TextEdit uses
    let color = ui.visuals().text_color();
    let plain = TextFormat::simple(font.clone(), color);
    let tab = TextFormat {
        background: ui.visuals().selection.bg_fill.gamma_multiply(0.35),
        ..plain.clone()
    };
    let mut job = LayoutJob::default();
    let mut start = 0;
    for (i, _) in text.match_indices('\t') {
        job.append(&text[start..i], 0.0, plain.clone());
        job.append("\t", 0.0, tab.clone());
        start = i + 1;
    }
    job.append(&text[start..], 0.0, plain);
    job.wrap.max_width = f32::INFINITY;
    ui.fonts_mut(|f| f.layout_job(job))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ctrl_tab_types_a_tab_at_the_cursor() {
        let ctx = egui::Context::default();
        let id = egui::Id::unique("field");
        let mut text = String::from("ab");
        for frame in 0..3 {
            let mut input = egui::RawInput::default();
            if frame == 2 {
                input.events.push(egui::Event::Key {
                    key: Key::Tab,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::CTRL,
                });
            }
            let mut out = ctx.run_ui(input, |ui| {
                if frame == 0 {
                    ui.memory_mut(|m| m.request_focus(id));
                }
                if frame == 1 {
                    let mut state = egui::TextEdit::load_state(ui.ctx(), id).unwrap_or_default();
                    state.cursor.set_char_range(Some(CCursorRange::one(CCursor::new(1))));
                    egui::TextEdit::store_state(ui.ctx(), id, state);
                }
                insert_typed_tab(ui, id, &mut text);
                ui.add(egui::TextEdit::singleline(&mut text).id(id).layouter(&mut layouter));
            });
            out.textures_delta.clear();
        }
        assert_eq!(text, "a\tb");
    }
}
