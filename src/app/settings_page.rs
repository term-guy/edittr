//! The Settings tab: every option on one searchable page, grouped by area.

use super::*;
use crate::commands::Setting;

enum Control {
    Toggle(Setting),
    TabSize,
    Theme,
    Action(Command, &'static str),
}

struct Item {
    control: Control,
    title: &'static str,
    detail: &'static str,
}

const fn item(control: Control, title: &'static str, detail: &'static str) -> Item {
    Item { control, title, detail }
}

use Control::*;

const SECTIONS: &[(&str, &[Item])] = &[
    (
        "Appearance",
        &[
            item(Theme, "Theme", "Follow the system, or always use the dark or light theme."),
            item(
                Toggle(Setting::Minimap),
                "Minimap",
                "Show an overview of the file on the right of the editor.",
            ),
            item(
                Toggle(Setting::BracketColors),
                "Bracket pair colors",
                "Color nested brackets by their depth.",
            ),
            item(
                Toggle(Setting::SemanticHighlighting),
                "Semantic highlighting",
                "Use colors from the language server for macros, types, functions and disabled code.",
            ),
            item(
                Toggle(Setting::HighlightLine),
                "Highlight current line",
                "Shade the line the caret is on.",
            ),
            item(
                Toggle(Setting::ShowWhitespace),
                "Show whitespace",
                "Draw spaces as dots and tabs as arrows.",
            ),
        ],
    ),
    (
        "Editor",
        &[
            item(
                TabSize,
                "Tab size",
                "Width of a tab character, and the spaces Tab inserts in files without an indentation style of their own.",
            ),
            item(
                Toggle(Setting::FormatOnSave),
                "Format on save",
                "Format the file with its language server before saving.",
            ),
            item(
                Toggle(Setting::StickyScroll),
                "Sticky scroll",
                "Pin the headers of the functions and types you are scrolling through to the top.",
            ),
            item(
                Toggle(Setting::InlineBlame),
                "Inline blame",
                "Show who last changed the current line, dimmed after its end.",
            ),
            item(
                Toggle(Setting::ShowOutline),
                "Outline",
                "Show the symbols of the current file below the Explorer.",
            ),
        ],
    ),
    (
        "Source control",
        &[
            item(
                Toggle(Setting::ChangesTree),
                "View changes as a tree",
                "Group changed files by folder instead of listing them flat.",
            ),
            item(
                Toggle(Setting::DiffIgnoreEol),
                "Ignore line ending differences",
                "Don't show lines in diffs that differ only in CRLF / LF.",
            ),
        ],
    ),
    (
        "Startup",
        &[
            item(
                Toggle(Setting::RestoreSession),
                "Restore previous session",
                "Reopen the files that were open when the folder was last closed.",
            ),
            item(
                Toggle(Setting::ReopenLastFolder),
                "Reopen last folder",
                "Start in the last folder instead of the start page when no folder is given.",
            ),
        ],
    ),
    (
        "C / C++",
        &[
            item(
                Action(Command::CSettings, "Edit…"),
                "Compile flags and build settings",
                "Include paths, defines and the compiler clangd uses for this project.",
            ),
            item(
                Action(Command::GenerateCompileDatabase, "Generate…"),
                "Compile database",
                "Create compile_commands.json from the project's build so clangd sees the exact flags.",
            ),
        ],
    ),
];

#[derive(Default)]
pub(super) struct Page {
    pub(super) filter: String,
    focus: bool,
}

impl Page {
    pub(super) fn focus(&mut self) {
        self.focus = true;
    }
}

fn matches(filter: &str, section: &str, item: &Item) -> bool {
    let filter = filter.trim().to_lowercase();
    filter.is_empty()
        || filter.split_whitespace().all(|word| {
            [section, item.title, item.detail]
                .iter()
                .any(|text| text.to_lowercase().contains(word))
        })
}

/// An on/off switch, drawn like the ones in system settings.
fn switch(ui: &mut egui::Ui, on: &mut bool) -> egui::Response {
    let size = ui.spacing().interact_size.y * egui::vec2(1.8, 1.0);
    let (rect, mut response) = ui.allocate_exact_size(size, egui::Sense::click());
    if response.clicked() {
        *on = !*on;
        response.mark_changed();
    }
    response.widget_info(|| egui::WidgetInfo::selected(egui::Role::Switch, ui.is_enabled(), *on, ""));
    if ui.is_rect_visible(rect) {
        let t = ui.ctx().animate_bool_responsive(response.id, *on);
        let visuals = ui.style().interact_selectable(&response, *on);
        let radius = rect.height() / 2.0;
        let fill = if *on { ui.visuals().selection.bg_fill } else { visuals.bg_fill };
        ui.painter().rect(rect, radius, fill, visuals.bg_stroke, egui::StrokeKind::Inside);
        let x = egui::lerp((rect.left() + radius)..=(rect.right() - radius), t);
        let knob = if *on {
            ui.visuals().strong_text_color()
        } else {
            visuals.fg_stroke.color
        };
        ui.painter().circle_filled(egui::pos2(x, rect.center().y), radius * 0.72, knob);
    }
    response
}

impl App {
    pub(super) fn open_settings(&mut self) {
        self.settings_page.focus();
        self.show_tab(Tab::Settings);
    }

    pub(super) fn settings_tab(&mut self, ui: &mut egui::Ui) {
        let mut run = None;
        let mut tab_size = None;
        egui::ScrollArea::vertical()
            .id_salt("settings-page")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let width = ui.available_width().min(760.0);
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    ui.add_space(((ui.available_width() - width) / 2.0).max(12.0));
                    ui.vertical(|ui| {
                        ui.set_width(width - 24.0);
                        ui.heading("Settings");
                        ui.add_space(6.0);
                        let search = ui.add(
                            egui::TextEdit::singleline(&mut self.settings_page.filter)
                                .hint_text(format!("{}  Search settings", ic::MAGNIFYING_GLASS))
                                .desired_width(f32::INFINITY),
                        );
                        if std::mem::take(&mut self.settings_page.focus) {
                            search.request_focus();
                        }
                        if search.has_focus() && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                            self.settings_page.filter.clear();
                        }
                        let filter = self.settings_page.filter.clone();
                        let mut shown = 0;
                        for (section, items) in SECTIONS {
                            let items: Vec<_> = items.iter().filter(|item| matches(&filter, section, item)).collect();
                            if items.is_empty() {
                                continue;
                            }
                            ui.add_space(18.0);
                            ui.label(RichText::new(*section).strong().size(15.0));
                            ui.add_space(2.0);
                            for item in items {
                                shown += 1;
                                ui.separator();
                                self.settings_row(ui, item, &mut run, &mut tab_size);
                            }
                        }
                        if shown == 0 {
                            ui.add_space(18.0);
                            ui.weak("No settings match your search.");
                        }
                        ui.add_space(24.0);
                        ui.weak(format!(
                            "Settings are saved automatically. Every option is also in the command palette ({}).",
                            commands::entry(Command::Palette).shortcut_text(ui.ctx())
                        ));
                        ui.add_space(12.0);
                    });
                });
            });
        if let Some(n) = tab_size {
            self.set_tab_size(n);
        }
        if let Some(command) = run {
            self.run_command(command);
        }
    }

    fn settings_row(&mut self, ui: &mut egui::Ui, item: &Item, run: &mut Option<Command>, tab_size: &mut Option<usize>) {
        let control_width = 150.0;
        ui.horizontal(|ui| {
            let text_width = (ui.available_width() - control_width - 12.0).max(120.0);
            ui.vertical(|ui| {
                ui.set_width(text_width);
                ui.add_space(4.0);
                ui.label(RichText::new(item.title).strong());
                ui.add(egui::Label::new(RichText::new(item.detail).color(ui.visuals().text_color())).wrap());
                ui.add_space(4.0);
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| match item.control {
                Toggle(setting) => {
                    let mut on = *setting.value(&mut self.settings);
                    if switch(ui, &mut on).on_hover_text(setting.label()).changed() {
                        *run = Some(Command::ToggleSetting(setting));
                    }
                }
                TabSize => {
                    let mut n = self.settings.tab_size;
                    let response = ui.add(egui::DragValue::new(&mut n).range(1..=16).speed(0.1));
                    if response.changed() && n != self.settings.tab_size {
                        *tab_size = Some(n);
                    }
                    for preset in [8, 4, 2].into_iter().filter(|&p| p != self.settings.tab_size) {
                        if ui.small_button(preset.to_string()).clicked() {
                            *tab_size = Some(preset);
                        }
                    }
                }
                Theme => {
                    let mut preference = ui.ctx().options(|o| o.theme_preference);
                    let before = preference;
                    for (value, label) in [
                        (egui::ThemePreference::Light, "Light"),
                        (egui::ThemePreference::Dark, "Dark"),
                        (egui::ThemePreference::System, "System"),
                    ] {
                        ui.selectable_value(&mut preference, value, label);
                    }
                    if preference != before {
                        ui.ctx().set_theme(preference);
                    }
                }
                Action(command, label) => {
                    if ui.button(label).clicked() {
                        *run = Some(command);
                    }
                }
            });
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_toggle_is_on_the_page_once_and_search_matches_all_words() {
        for entry in commands::COMMANDS {
            let Command::ToggleSetting(setting) = entry.id else { continue };
            let count = SECTIONS
                .iter()
                .flat_map(|(_, items)| items.iter())
                .filter(|item| matches!(item.control, Toggle(s) if s == setting))
                .count();
            assert_eq!(count, 1, "{}", setting.label());
        }
        let tab = &SECTIONS[1].1[0];
        assert!(super::matches("tab", "Editor", tab));
        assert!(super::matches("editor indentation", "Editor", tab));
        assert!(!super::matches("tab minimap", "Editor", tab));
    }
}
