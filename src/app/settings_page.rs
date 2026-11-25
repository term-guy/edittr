//! The Settings tab: every option on one searchable page, grouped by area.

use super::*;
use crate::commands::Setting;
use crate::settings::{DEFAULT_BREADCRUMB_FONT_SIZE, DEFAULT_FONT_SIZE, FONT_SIZE_RANGE, FontChoice};
use crate::theme::{PRESETS, Palette, Part};

enum Control {
    Toggle(Setting),
    TabSize,
    Theme,
    Font(fonts::Target),
    BreadcrumbSize,
    ExplorerMenu,
    StickyKinds,
    About,
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

/// Section shown only on Windows.
const WINDOWS: &str = "Windows";

const SECTIONS: &[(&str, &[Item])] = &[
    (
        "Appearance",
        &[
            item(Theme, "Theme", "Follow the system, or always use the dark or light theme."),
            item(
                Font(fonts::Target::Ui),
                "UI font",
                "Font family and size for menus, panels and dialogs.",
            ),
            item(
                BreadcrumbSize,
                "Breadcrumbs font size",
                "Size of the file path shown above the editor.",
            ),
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
            item(
                Toggle(Setting::HeaderLines),
                "Lines under the breadcrumbs and sticky scroll",
                "Draw a divider line below the file path bar and below the pinned sticky scroll lines.",
            ),
            item(
                Toggle(Setting::ShowFps),
                "Show FPS",
                "Show the frames drawn per second and the time the last frame took in the status bar. \
                 edittr only redraws when something changes, so an idle window shows few frames.",
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
                Font(fonts::Target::Editor),
                "Editor font",
                "Font family and size for code, line numbers and diffs.",
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
                Toggle(Setting::StickyControlFlow),
                "Sticky scroll: control flow",
                "Also pin the if, else, loop, switch and case lines around the visible code, not only functions and types.",
            ),
            item(
                StickyKinds,
                "Sticky scroll: pinned lines",
                "Choose which kinds of lines are pinned: functions, types, if / else, loops, switch, case, try / catch. \
                 Also from the right-click menu of a pinned line.",
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
    (
        WINDOWS,
        &[item(
            ExplorerMenu,
            "Explorer right-click menu",
            "Add \"Edit with edittr\" to files and \"Open with edittr\" to folders in Windows Explorer, for this copy of edittr \
             (also the portable one). On Windows 11 they are under \"Show more options\".",
        )],
    ),
    (
        "About",
        &[item(
            About,
            "edittr",
            "The version of this copy of edittr. New versions are on the releases page.",
        )],
    ),
];

const RELEASES: &str = "https://github.com/term-guy/edittr/releases";

/// "0.4.2 (abc1234, 2026-11-25)": the version, and the commit it was built from when known.
pub(crate) fn version_text() -> String {
    let version = env!("CARGO_PKG_VERSION");
    match (env!("EDITTR_COMMIT"), env!("EDITTR_COMMIT_DATE")) {
        ("", _) => version.to_string(),
        (commit, "") => format!("{version} ({commit})"),
        (commit, date) => format!("{version} ({commit}, {date})"),
    }
}

#[derive(Default)]
pub(super) struct Page {
    pub(super) filter: String,
    focus: bool,
    font_search: [String; 2],
    /// The search box of a just-opened font picker still needs focus (not possible during the popup's invisible sizing pass).
    font_focus: [bool; 2],
    /// Frame each font picker's popup was last drawn in: a popup not drawn in the previous frame has just opened.
    font_shown: [u64; 2],
    /// Read from the registry when the Settings tab first shows it.
    explorer_menu: Option<crate::shell_menu::State>,
}

impl Page {
    pub(super) fn focus(&mut self) {
        self.focus = true;
    }
}

fn matches(filter: &str, section: &str, item: &Item) -> bool {
    // Every word of the filter must start a word of the title, description or section ("tab" finds "Tab size", not "database").
    let words = |text: &str| {
        text.split(|c: char| !c.is_alphanumeric())
            .map(str::to_lowercase)
            .collect::<Vec<_>>()
    };
    let haystack: Vec<String> = [section, item.title, item.detail].into_iter().flat_map(words).collect();
    filter
        .split_whitespace()
        .map(str::to_lowercase)
        .all(|word| haystack.iter().any(|w| w.starts_with(&word)))
}

fn theme_matches(filter: &str) -> bool {
    filter.split_whitespace().all(|word| {
        let word = word.to_ascii_lowercase();
        "theme editor palette color preset".contains(&word)
            || Part::ALL.iter().any(|(_, _, label)| label.to_ascii_lowercase().contains(&word))
            || PRESETS.iter().any(|preset| preset.name.to_ascii_lowercase().contains(&word))
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
        // Light mode's selection fill is too pale behind a white knob: use the selection text color instead.
        let fill = match (*on, ui.visuals().dark_mode) {
            (true, true) => ui.visuals().selection.bg_fill,
            (true, false) => ui.visuals().selection.stroke.color,
            (false, _) => visuals.bg_fill,
        };
        ui.painter().rect(rect, radius, fill, visuals.bg_stroke, egui::StrokeKind::Inside);
        let x = egui::lerp((rect.left() + radius)..=(rect.right() - radius), t);
        let knob = if *on { egui::Color32::WHITE } else { visuals.fg_stroke.color };
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
                        let section_size = egui::TextStyle::Body.resolve(ui.style()).size * (15.0 / 13.0);
                        let mut shown = 0;
                        for (section, items) in SECTIONS {
                            if *section == WINDOWS && !cfg!(windows) {
                                continue;
                            }
                            let items: Vec<_> = items.iter().filter(|item| matches(&filter, section, item)).collect();
                            if !items.is_empty() {
                                ui.add_space(18.0);
                                ui.label(RichText::new(*section).strong().size(section_size));
                                ui.add_space(2.0);
                                for item in items {
                                    shown += 1;
                                    ui.separator();
                                    self.settings_row(ui, item, &mut run, &mut tab_size);
                                }
                            }
                            if *section == "Appearance" && theme_matches(&filter) {
                                shown += 1;
                                ui.add_space(12.0);
                                egui::CollapsingHeader::new(RichText::new("Theme editor").strong().size(section_size))
                                    .open((!filter.trim().is_empty()).then_some(true))
                                    .show(ui, |ui| self.theme_editor(ui));
                            }
                        }
                        if shown == 0 {
                            ui.add_space(18.0);
                            ui.weak("No settings match your search.");
                        }
                        ui.add_space(24.0);
                        ui.weak(format!(
                            "Settings are saved automatically. Open the command palette with {}.",
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

    pub(super) fn set_theme_preference(&mut self, preference: egui::ThemePreference) {
        self.settings.themes.preference = preference;
        self.ctx.set_theme(preference);
        self.ctx.request_repaint();
        self.save_settings(&self.ctx.clone());
    }

    fn theme_editor(&mut self, ui: &mut egui::Ui) {
        let mut palette = *self.settings.themes.active(ui.ctx().theme() == egui::Theme::Dark);
        let before = palette;
        let mut preference = self.settings.themes.preference;
        let mut edited = false;
        ui.horizontal_wrapped(|ui| {
            ui.label("Preset");
            let preset = palette.preset();
            let label = if palette.is_custom() {
                format!("{} (custom)", preset.name).into()
            } else {
                std::borrow::Cow::Borrowed(preset.name)
            };
            egui::ComboBox::from_id_salt("editor-theme-preset")
                .selected_text(label.as_ref())
                .show_ui(ui, |ui| {
                    for (i, candidate) in PRESETS.iter().enumerate() {
                        let selected = !palette.is_custom() && candidate.id == preset.id;
                        if ui.selectable_label(selected, candidate.name).clicked() {
                            palette = Palette::new(i);
                            preference = candidate.base.into();
                        }
                    }
                    for custom in [self.settings.themes.custom_dark, self.settings.themes.custom_light]
                        .into_iter()
                        .flatten()
                    {
                        let name = format!("{} (custom)", custom.preset().name);
                        if ui.selectable_label(palette == custom, name).clicked() {
                            palette = custom;
                            preference = custom.preset().base.into();
                        }
                    }
                });
            if ui.add_enabled(palette.is_custom(), egui::Button::new("Reset to preset")).clicked() {
                palette.reset();
                edited = true;
            }
        });

        ui.add_space(6.0);
        theme_preview(ui, &palette);
        ui.add_space(6.0);
        let columns = if ui.available_width() >= 530.0 { 2 } else { 1 };
        egui::Grid::new("theme-colors")
            .num_columns(columns * 2)
            .spacing([12.0, 6.0])
            .show(ui, |ui| {
                for (i, (part, _, label)) in Part::ALL.into_iter().enumerate() {
                    let mut color = palette.color(part);
                    let alpha = if part == Part::Background {
                        egui::color_picker::Alpha::Opaque
                    } else {
                        egui::color_picker::Alpha::OnlyBlend
                    };
                    let response = ui
                        .push_id(part, |ui| egui::color_picker::color_edit_button_srgba(ui, &mut color, alpha))
                        .inner;
                    response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, ui.is_enabled(), label));
                    ui.label(label);
                    if response.changed() && color != palette.color(part) {
                        palette.set(part, color);
                        edited = true;
                    }
                    if (i + 1) % columns == 0 {
                        ui.end_row();
                    }
                }
            });
        if edited {
            self.settings
                .themes
                .set_custom(palette.preset().base, palette.is_custom().then_some(palette));
        }
        if edited || palette != before || preference != self.settings.themes.preference {
            self.settings.themes.set_palette(palette);
            self.set_theme_preference(preference);
        }
    }

    fn settings_row(&mut self, ui: &mut egui::Ui, item: &Item, run: &mut Option<Command>, tab_size: &mut Option<usize>) {
        let font_control = matches!(item.control, Font(_));
        let control_width = if font_control { 250.0 } else { 150.0 };
        let controls_below = font_control && ui.available_width() < 390.0;
        let layout = if controls_below {
            egui::Layout::top_down(egui::Align::Min)
        } else {
            egui::Layout::left_to_right(egui::Align::Center)
        };
        ui.with_layout(layout, |ui| {
            let text_width = if controls_below {
                ui.available_width()
            } else {
                (ui.available_width() - control_width - 12.0).max(120.0)
            };
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
                    let response = ui.add(egui::DragValue::new(&mut n).range(1..=16).speed(0.05).suffix(" spaces"));
                    if response.changed() && n != self.settings.tab_size {
                        *tab_size = Some(n);
                    }
                }
                Font(target) => self.font_controls(ui, target),
                ExplorerMenu => self.explorer_menu_switch(ui),
                StickyKinds => {
                    let shown = self.settings.sticky_shown;
                    let count = shown
                        .iter()
                        .filter(|kind| self.settings.sticky_control_flow || !kind.is_control_flow())
                        .count();
                    let label = format!("{count} of {} kinds", crate::editor::sticky::Kind::ALL.len());
                    ui.menu_button(label, |ui| {
                        if let Some(command) = crate::editor::sticky::kinds_menu(ui, shown, self.settings.sticky_control_flow) {
                            *run = Some(command);
                        }
                    });
                }
                BreadcrumbSize => {
                    let size = &mut self.settings.breadcrumb_font_size;
                    let response = ui
                        .add(egui::DragValue::new(size).range(FONT_SIZE_RANGE).suffix(" pt"))
                        .on_hover_text(format!("Default size is {DEFAULT_BREADCRUMB_FONT_SIZE} pt"));
                    if response.changed() {
                        let ctx = ui.ctx().clone();
                        self.fonts.apply_sizes(&self.settings, &ctx);
                        self.save_settings(&ctx);
                    }
                }
                Theme => {
                    let mut preference = self.settings.themes.preference;
                    let before = preference;
                    for (value, label) in [
                        (egui::ThemePreference::Light, "Light"),
                        (egui::ThemePreference::Dark, "Dark"),
                        (egui::ThemePreference::System, "System"),
                    ] {
                        ui.selectable_value(&mut preference, value, label);
                    }
                    if preference != before {
                        self.set_theme_preference(preference);
                    }
                }
                About => {
                    ui.hyperlink_to("Releases", RELEASES);
                    let version = version_text();
                    if ui
                        .add(egui::Label::new(RichText::new(format!("Version {version}")).strong()).sense(egui::Sense::click()))
                        .on_hover_text("Click to copy")
                        .clicked()
                    {
                        ui.ctx().copy_text(version);
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

    fn explorer_menu_switch(&mut self, ui: &mut egui::Ui) {
        use crate::shell_menu::{self, State};
        let state = self.settings_page.explorer_menu.get_or_insert_with(shell_menu::state).clone();
        let mut on = matches!(state, State::This | State::AllUsers);
        let hover = match &state {
            State::Off => "Not in the Explorer menu".to_string(),
            State::This => "In the Explorer menu for this copy of edittr".to_string(),
            State::Other(command) => format!("The Explorer menu opens another copy: {command}\nTurn on to use this one."),
            State::AllUsers => "Added for all users by the installer; change it by running the installer again".to_string(),
        };
        let response = ui.add_enabled_ui(state != State::AllUsers, |ui| switch(ui, &mut on)).inner;
        if response
            .on_hover_text(hover)
            .on_disabled_hover_text("Added for all users by the installer")
            .changed()
        {
            let result = if on { shell_menu::register() } else { shell_menu::unregister() };
            if let Err(err) = result {
                self.message = format!("Couldn't change the Explorer menu: {err}");
            }
            self.settings_page.explorer_menu = Some(shell_menu::state());
        }
    }

    fn font_controls(&mut self, ui: &mut egui::Ui, target: fonts::Target) {
        let ctx = ui.ctx().clone();
        let size = target.size(&mut self.settings);
        let mut size_changed = ui
            .add(egui::DragValue::new(size).range(FONT_SIZE_RANGE).suffix(" pt"))
            .on_hover_text(format!("Default size is {DEFAULT_FONT_SIZE} pt"))
            .changed();
        let saved = target.choice(&mut self.settings);
        let current = saved.as_ref().map_or("Default", |choice| choice.family.as_str());
        let salt = ("font-picker", target);
        // Not `ComboBox::is_open(make_persistent_id(salt))`: `from_id_salt` wraps the salt, so that id never matched
        // and the search text was cleared every frame.
        let frame = ctx.cumulative_frame_nr();
        let page = &mut self.settings_page;
        let i = target as usize;
        let (search, focus, shown) = (&mut page.font_search[i], &mut page.font_focus[i], &mut page.font_shown[i]);
        let picker = egui::ComboBox::from_id_salt(salt)
            .width(ui.available_width().clamp(60.0, 168.0))
            .height(340.0)
            .wrap_mode(egui::TextWrapMode::Truncate)
            .selected_text(current)
            .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
            .show_ui(ui, |ui| {
                self.fonts.request_list(&ctx);
                let was_open = *shown + 1 >= frame;
                *shown = frame;
                if !was_open {
                    search.clear();
                    *focus = true;
                }
                let response = ui.add(
                    egui::TextEdit::singleline(search)
                        .hint_text("Search fonts")
                        .desired_width(f32::INFINITY),
                );
                if *focus && !ui.is_sizing_pass() {
                    response.request_focus();
                    *focus = false;
                }
                if ui
                    .selectable_label(saved.is_none(), "Default")
                    .on_hover_text("Restore the bundled font and size")
                    .clicked()
                {
                    ui.close();
                    return Some(None);
                }
                // The list keeps one height while loading and filtering: the popup is sized when it opens and
                // later frames can't grow it, so a short first frame (fonts still loading) left room for one row.
                const LIST_HEIGHT: f32 = 250.0;
                let Some(families) = &self.fonts.families else {
                    ui.allocate_ui(egui::vec2(ui.available_width(), LIST_HEIGHT), |ui| {
                        ui.set_min_height(LIST_HEIGHT);
                        ui.spinner();
                    });
                    return None;
                };
                let query = search.to_lowercase();
                let mut matches: Vec<_> = families.iter().filter(|family| family.search_name.contains(query.trim())).collect();
                if matches!(target, fonts::Target::Editor) {
                    matches.sort_by_key(|family| !family.monospace);
                }
                let row_height = (ui.text_style_height(&egui::TextStyle::Button) + 2.0 * ui.spacing().button_padding.y)
                    .max(ui.spacing().interact_size.y);
                let mut selected = None;
                let mut scroll = egui::ScrollArea::vertical()
                    .id_salt("font-families")
                    .max_height(LIST_HEIGHT)
                    .auto_shrink([false, false]);
                if response.changed() {
                    scroll = scroll.vertical_scroll_offset(0.0);
                }
                if matches.is_empty() {
                    ui.allocate_ui(egui::vec2(ui.available_width(), LIST_HEIGHT), |ui| {
                        ui.set_min_height(LIST_HEIGHT);
                        ui.weak("No matching fonts");
                    });
                    return None;
                }
                scroll.show_rows(ui, row_height, matches.len(), |ui, range| {
                    for family in &matches[range] {
                        if ui
                            .add_sized(
                                [ui.available_width(), row_height],
                                egui::Button::selectable(saved.is_some() && current == family.name, &family.name).truncate(),
                            )
                            .on_hover_text(&family.name)
                            .clicked()
                        {
                            ui.close();
                            selected = Some(Some(FontChoice {
                                family: family.name.clone(),
                                file: family.file.clone(),
                            }));
                        }
                    }
                });
                selected
            });
        let mut family_changed = false;
        if let Some(choice) = picker.inner.flatten() {
            let reset = choice.is_none();
            if choice.as_ref().map(|font| &font.family) != saved.as_ref().map(|font| &font.family) {
                saved.clone_from(&choice);
                self.fonts.select(target, choice, &ctx);
                if reset {
                    self.fonts.install(&self.settings, &ctx);
                }
                family_changed = true;
            }
            if reset {
                let size = target.size(&mut self.settings);
                size_changed |= *size != DEFAULT_FONT_SIZE;
                *size = DEFAULT_FONT_SIZE;
            }
        }
        if size_changed || family_changed {
            self.fonts.apply_sizes(&self.settings, &ctx);
            self.save_settings(&ctx);
        }
    }
}

fn theme_preview(ui: &mut egui::Ui, palette: &Palette) {
    use Part::*;
    const LINES: &[&[(Part, &str)]] = &[
        &[(Comment, "// Print a scaled sample")],
        &[(Macro, "#define SCALE"), (Number, " 2")],
        &[(Disabled, "// Inactive preprocessor branch")],
        &[
            (Keyword, "static "),
            (Type, "Sample "),
            (Function, "scale"),
            (Bracket1, "("),
            (Type, "Sample "),
            (Variable, "value"),
            (Bracket1, ") {"),
        ],
        &[
            (Foreground, "    log"),
            (Bracket2, "("),
            (String, "\"ready\""),
            (Operator, ", "),
            (Bracket3, "("),
            (Number, "3"),
            (Bracket3, ")"),
            (Bracket2, ")"),
            (Operator, ";"),
        ],
        &[
            (Keyword, "    return "),
            (Bracket2, "("),
            (Variable, "value"),
            (Operator, " * "),
            (Macro, "SCALE"),
            (Bracket2, ")"),
            (Operator, " + "),
            (Constant, "LIMIT"),
            (Operator, ";"),
        ],
        &[(Bracket1, "}")],
    ];
    let font = fonts::editor_font(ui.style());
    let row = ui.fonts_mut(|fonts| fonts.row_height(&font));
    let height = row * LINES.len() as f32 + 12.0;
    let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), height), egui::Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 4.0, palette.color(Background));
    for (i, line) in LINES.iter().enumerate() {
        let y = rect.top() + 6.0 + i as f32 * row;
        if i == 5 {
            let band = egui::Rect::from_min_size(egui::pos2(rect.left(), y), egui::vec2(rect.width(), row));
            painter.rect_filled(band, 0.0, palette.color(CurrentLine));
        }
        painter.text(
            egui::pos2(rect.left() + 10.0, y),
            egui::Align2::LEFT_TOP,
            i + 1,
            font.clone(),
            palette.color(LineNumber),
        );
        let mut job = egui::text::LayoutJob::default();
        job.wrap.max_width = f32::INFINITY;
        for &(part, code) in *line {
            let mut format = egui::TextFormat::simple(font.clone(), palette.color(part));
            if i == 5 && part == Variable {
                format.background = palette.color(Selection).gamma_multiply(0.7);
            }
            job.append(code, 0.0, format);
        }
        let galley = ui.fonts_mut(|fonts| fonts.layout_job(job));
        painter.galley(egui::pos2(rect.left() + 36.0, y), galley, palette.color(Foreground));
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
        let database = &SECTIONS[4].1[1];
        assert!(!super::matches("tab", "C / C++", database));
        assert!(super::matches("comp", "C / C++", database));
    }

    #[test]
    fn typing_in_the_font_search_keeps_the_text() {
        let ctx = egui::Context::default();
        let cc = eframe::CreationContext::_new_kittest(ctx.clone());
        let (writer, worker) = crate::settings::SettingsWriter::start();
        let startup = Startup {
            root: None,
            file: None,
            restore: false,
            window: Default::default(),
        };
        let mut app = App::new(&cc, startup, Default::default(), writer, None);
        let frame = |app: &mut App, events: Vec<egui::Event>| {
            let input = egui::RawInput {
                events,
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1000.0, 800.0))),
                ..Default::default()
            };
            let mut out = ctx.run_ui(input, |ui| app.settings_tab(ui));
            out.textures_delta.clear();
            out.shapes
        };
        // The first font picker ("Default" family) opens on a click.
        let pos = frame(&mut app, Vec::new())
            .iter()
            .find_map(|shape| match &shape.shape {
                egui::Shape::Text(text) if text.galley.text() == "Default" => Some(text.pos + egui::vec2(3.0, 3.0)),
                _ => None,
            })
            .unwrap();
        let click = |pressed| egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: Default::default(),
        };
        frame(&mut app, vec![egui::Event::PointerMoved(pos)]);
        frame(&mut app, vec![click(true)]);
        frame(&mut app, vec![click(false)]);
        for _ in 0..3 {
            frame(&mut app, Vec::new());
        }
        frame(&mut app, vec![egui::Event::Text("mo".into())]);
        frame(&mut app, vec![egui::Event::Text("n".into())]);
        frame(&mut app, Vec::new());
        assert!(
            app.settings_page.font_search.contains(&"mon".to_string()),
            "{:?}",
            app.settings_page.font_search
        );
        drop(app);
        worker.join().unwrap();
    }
}
