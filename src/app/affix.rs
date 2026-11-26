//! "Add text to the start / end of lines": prepend and / or append text to the selected
//! lines of the active editor (the caret's line without a selection).

use super::{App, Tab};
use eframe::egui::{self, Key};

#[derive(Default)]
pub(super) struct Affix {
    prefix: String,
    suffix: String,
    skip_blank: bool,
    focus: bool,
}

impl App {
    pub(super) fn open_affix(&mut self) {
        if !matches!(self.tabs.get(self.active), Some(Tab::File(_))) {
            self.message = "Open a file to add text to its lines".into();
            return;
        }
        // Keep the last texts: the same prefix is often added to several blocks.
        let mut affix = self.affix_last.take().unwrap_or_default();
        affix.focus = true;
        self.affix = Some(affix);
    }

    pub(super) fn affix_dialog(&mut self, ctx: &egui::Context) {
        let Some(affix) = &mut self.affix else { return };
        let lines = match self.tabs.get_mut(self.active) {
            Some(Tab::File(doc)) => doc.ed.selected_line_count(&doc.text, doc.version),
            _ => {
                self.affix = None;
                return;
            }
        };
        let mut apply = false;
        let mut close = false;
        egui::Modal::new(egui::Id::unique("affix_lines")).show(ctx, |ui| {
            ui.set_width(420.0);
            ui.heading("Add text to lines");
            ui.weak(format!("{lines} line{} selected", if lines == 1 { "" } else { "s" }));
            ui.add_space(6.0);
            egui::Grid::new("affix_grid").num_columns(2).spacing([8.0, 6.0]).show(ui, |ui| {
                ui.label("At the start");
                let first = ui.add(
                    egui::TextEdit::singleline(&mut affix.prefix)
                        .hint_text("e.g. // or - ")
                        .desired_width(f32::INFINITY),
                );
                if std::mem::take(&mut affix.focus) {
                    first.request_focus();
                }
                ui.end_row();
                ui.label("At the end");
                ui.add(
                    egui::TextEdit::singleline(&mut affix.suffix)
                        .hint_text("e.g. , or ;")
                        .desired_width(f32::INFINITY),
                );
                ui.end_row();
            });
            ui.checkbox(&mut affix.skip_blank, "Skip blank lines");
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                let enabled = !affix.prefix.is_empty() || !affix.suffix.is_empty();
                if ui.add_enabled(enabled, egui::Button::new("Apply")).clicked() || (enabled && ui.input(|i| i.key_pressed(Key::Enter))) {
                    apply = true;
                }
                if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(Key::Escape)) {
                    close = true;
                }
            });
        });
        if apply
            && let Some(Tab::File(doc)) = self.tabs.get_mut(self.active)
            && crate::editor::affix_lines(
                &mut doc.ed,
                &mut doc.text,
                doc.version,
                &affix.prefix,
                &affix.suffix,
                affix.skip_blank,
            )
        {
            doc.version += 1;
            doc.edited_at = Some(std::time::Instant::now());
        }
        if apply || close {
            self.affix_last = self.affix.take();
        }
    }
}
