use std::{
    fs,
    io::{self, Write},
    path::Path,
    sync::mpsc::{Receiver, Sender, channel},
    thread::{self, JoinHandle},
};

use eframe::egui;

use crate::outline::{CollapsedGroups, Group};
use crate::theme::{Palette, Themes};

pub(crate) const DEFAULT_FONT_SIZE: u8 = 13;
/// Size of the file path above the editor (it used egui's 9 pt `Small` style before).
pub(crate) const DEFAULT_BREADCRUMB_FONT_SIZE: u8 = 11;
pub(crate) const FONT_SIZE_RANGE: std::ops::RangeInclusive<u8> = 8..=32;

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct FontChoice {
    pub(crate) family: String,
    pub(crate) file: Option<FontFile>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct FontFile {
    pub(crate) path: String,
    pub(crate) index: u32,
}

#[derive(Clone)]
pub(crate) struct Settings {
    pub(crate) themes: Themes,
    pub(crate) color_window: bool,
    pub(crate) changes_tree: bool,
    pub(crate) format_on_save: bool,
    pub(crate) trim_trailing_whitespace_on_save: bool,
    pub(crate) insert_final_newline_on_save: bool,
    pub(crate) highlight_line: bool,
    /// Show spaces as middle dots and tabs as arrows.
    pub(crate) show_whitespace: bool,
    pub(crate) diff_ignore_eol: bool,
    /// Opening a diff scrolls to (and, in the editable view, puts the caret on) its first change.
    pub(crate) diff_first_change: bool,
    /// Dimmed "who changed this line" after the cursor's line.
    pub(crate) inline_blame: bool,
    /// Code overview on the right of the editor.
    pub(crate) minimap: bool,
    /// Color nested brackets by depth.
    pub(crate) bracket_colors: bool,
    /// Colors from the language server (macros, types, functions, disabled code).
    pub(crate) semantic_highlighting: bool,
    pub(crate) sticky_scroll: bool,
    /// Sticky scroll also pins if / else / loops / switch / case blocks, not only declarations.
    pub(crate) sticky_control_flow: bool,
    /// Kinds of lines sticky scroll pins (saved as the hidden ones).
    pub(crate) sticky_shown: crate::editor::sticky::Kinds,
    pub(crate) show_outline: bool,
    /// Keyboard shortcuts changed from the defaults (by command title).
    pub(crate) shortcuts: crate::commands::Overrides,
    /// Wrap long lines at the editor's width.
    pub(crate) word_wrap: bool,
    /// Frames per second and frame time in the status bar.
    pub(crate) show_fps: bool,
    /// Divider lines under the breadcrumbs and the sticky scroll lines.
    pub(crate) header_lines: bool,
    pub(crate) outline_expanded: bool,
    pub(crate) outline_grouped: bool,
    pub(crate) outline_collapsed_groups: CollapsedGroups,
    pub(crate) restore_previous_session: bool,
    /// Start in the last folder instead of the start page (when no folder is given).
    pub(crate) reopen_last_folder: bool,
    /// Width of a tab character, and the spaces Tab inserts in files without an
    /// indentation style of their own.
    pub(crate) tab_size: usize,
    pub(crate) editor_font: Option<FontChoice>,
    pub(crate) editor_font_size: u8,
    pub(crate) ui_font: Option<FontChoice>,
    pub(crate) ui_font_size: u8,
    pub(crate) breadcrumb_font_size: u8,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            themes: Themes::default(),
            color_window: true,
            changes_tree: false,
            format_on_save: false,
            trim_trailing_whitespace_on_save: false,
            insert_final_newline_on_save: false,
            highlight_line: true,
            show_whitespace: false,
            diff_ignore_eol: true,
            diff_first_change: true,
            inline_blame: true,
            minimap: true,
            show_fps: false,
            word_wrap: false,
            shortcuts: Default::default(),
            header_lines: true,
            bracket_colors: true,
            semantic_highlighting: true,
            sticky_scroll: true,
            sticky_control_flow: true,
            sticky_shown: crate::editor::sticky::Kinds::ALL,
            show_outline: true,
            outline_expanded: true,
            outline_grouped: true,
            outline_collapsed_groups: CollapsedGroups::default(),
            restore_previous_session: true,
            reopen_last_folder: false,
            tab_size: 4,
            editor_font: None,
            editor_font_size: DEFAULT_FONT_SIZE,
            ui_font: None,
            ui_font_size: DEFAULT_FONT_SIZE,
            breadcrumb_font_size: DEFAULT_BREADCRUMB_FONT_SIZE,
        }
    }
}

impl Settings {
    pub(crate) fn load() -> Self {
        dirs::config_dir().map(|dir| Self::load_from(&dir)).unwrap_or_default()
    }

    fn save(&self) -> io::Result<()> {
        if cfg!(feature = "tour") && std::env::var_os("EDITTR_TOUR_OUT").is_some() {
            return Ok(());
        }
        if let Some(dir) = dirs::config_dir() {
            self.save_to(&dir)?;
        }
        Ok(())
    }

    fn load_from(config_dir: &Path) -> Self {
        fs::read_to_string(config_dir.join("edittr/settings.txt"))
            .map(|text| Self::parse(&text))
            .unwrap_or_default()
    }

    fn save_to(&self, config_dir: &Path) -> io::Result<()> {
        let dir = config_dir.join("edittr");
        let file = dir.join("settings.txt");
        let previous = match fs::read_to_string(&file) {
            Ok(text) => text,
            Err(err) if err.kind() == io::ErrorKind::NotFound => String::new(),
            Err(err) => return Err(err),
        };
        fs::create_dir_all(dir)?;

        let temp = file.with_extension(format!("{}.tmp", std::process::id()));
        let mut pending = fs::OpenOptions::new().write(true).create_new(true).open(&temp)?;
        let written = pending.write_all(self.format(&previous).as_bytes());
        drop(pending);
        let result = written.and_then(|()| fs::rename(&temp, file));
        if result.is_err() {
            let _ = fs::remove_file(temp);
        }
        result
    }

    fn parse(text: &str) -> Self {
        let mut settings = Self::default();
        for (key, value) in text.lines().filter_map(entry) {
            if let Some(title) = key.strip_prefix("shortcut.") {
                let list: Vec<_> = value
                    .split(';')
                    .filter(|spec| !spec.trim().is_empty())
                    .filter_map(crate::commands::parse_shortcut)
                    .collect();
                // An empty value removes the shortcuts; one we can't read keeps the defaults.
                if value.is_empty() || !list.is_empty() {
                    settings.shortcuts.insert(title.to_string(), list);
                }
                continue;
            }
            match key {
                "editor_font" | "ui_font" => {
                    let font = serde_json::from_str(value).ok();
                    if key == "editor_font" {
                        settings.editor_font = font;
                    } else {
                        settings.ui_font = font;
                    }
                    continue;
                }
                "editor_font_size" | "ui_font_size" | "breadcrumb_font_size" => {
                    if let Ok(size) = value.parse()
                        && FONT_SIZE_RANGE.contains(&size)
                    {
                        match key {
                            "editor_font_size" => settings.editor_font_size = size,
                            "ui_font_size" => settings.ui_font_size = size,
                            _ => settings.breadcrumb_font_size = size,
                        }
                    }
                    continue;
                }
                "theme_preference" => {
                    settings.themes.preference = match value {
                        "dark" => egui::ThemePreference::Dark,
                        "light" => egui::ThemePreference::Light,
                        _ => egui::ThemePreference::System,
                    };
                    continue;
                }
                "theme_dark" | "theme_light" => {
                    let base = if key == "theme_dark" {
                        egui::Theme::Dark
                    } else {
                        egui::Theme::Light
                    };
                    if let Some(palette) = Palette::parse(value, base) {
                        settings.themes.set_palette(palette);
                    }
                    continue;
                }
                "theme_custom_dark" | "theme_custom_light" => {
                    let base = if key == "theme_custom_dark" {
                        egui::Theme::Dark
                    } else {
                        egui::Theme::Light
                    };
                    settings
                        .themes
                        .set_custom(base, Palette::parse(value, base).filter(Palette::is_custom));
                    continue;
                }
                _ => {}
            }
            if key == "sticky_hidden" {
                use crate::editor::sticky::{Kind, Kinds};
                let hidden: Vec<_> = value.split(',').filter_map(|key| Kind::from_key(key.trim())).collect();
                settings.sticky_shown = Kinds::ALL.iter().filter(|kind| !hidden.contains(kind)).collect();
                continue;
            }
            if key == "tab_size" {
                if let Ok(n) = value.parse::<usize>()
                    && (1..=16).contains(&n)
                {
                    settings.tab_size = n;
                }
                continue;
            }
            if key == "outline_collapsed_groups" {
                let mut collapsed = CollapsedGroups::default();
                for group in Group::ALL {
                    if value.split(',').any(|name| name.trim() == group.label()) {
                        collapsed.toggle(group);
                    }
                }
                settings.outline_collapsed_groups = collapsed;
                continue;
            }
            if let Ok(value) = value.parse() {
                match key {
                    "color_window" => settings.color_window = value,
                    "changes_tree" => settings.changes_tree = value,
                    "format_on_save" => settings.format_on_save = value,
                    "trim_trailing_whitespace_on_save" => settings.trim_trailing_whitespace_on_save = value,
                    "insert_final_newline_on_save" => settings.insert_final_newline_on_save = value,
                    "highlight_line" => settings.highlight_line = value,
                    "show_whitespace" => settings.show_whitespace = value,
                    "diff_ignore_eol" => settings.diff_ignore_eol = value,
                    "diff_first_change" => settings.diff_first_change = value,
                    "inline_blame" => settings.inline_blame = value,
                    "minimap" => settings.minimap = value,
                    "bracket_colors" => settings.bracket_colors = value,
                    "semantic_highlighting" => settings.semantic_highlighting = value,
                    "sticky_scroll" => settings.sticky_scroll = value,
                    "sticky_control_flow" => settings.sticky_control_flow = value,
                    "show_outline" => settings.show_outline = value,
                    "show_fps" => settings.show_fps = value,
                    "word_wrap" => settings.word_wrap = value,
                    "header_lines" => settings.header_lines = value,
                    "outline_expanded" => settings.outline_expanded = value,
                    "outline_grouped" => settings.outline_grouped = value,
                    "restore_previous_session" => settings.restore_previous_session = value,
                    "reopen_last_folder" => settings.reopen_last_folder = value,
                    _ => {}
                }
            }
        }
        settings
    }

    fn format(&self, previous: &str) -> String {
        let mut text = String::new();
        for line in previous.lines() {
            if !entry(line).is_some_and(|(key, _)| key.starts_with("shortcut."))
                && !matches!(
                    entry(line),
                    Some((
                        "changes_tree"
                            | "highlight_line"
                            | "show_whitespace"
                            | "diff_ignore_eol"
                            | "diff_first_change"
                            | "inline_blame"
                            | "minimap"
                            | "bracket_colors"
                            | "semantic_highlighting"
                            | "sticky_scroll"
                            | "sticky_control_flow"
                            | "sticky_hidden"
                            | "restore_previous_session"
                            | "reopen_last_folder"
                            | "tab_size"
                            | "editor_font"
                            | "editor_font_size"
                            | "ui_font"
                            | "ui_font_size"
                            | "breadcrumb_font_size"
                            | "format_on_save"
                            | "trim_trailing_whitespace_on_save"
                            | "insert_final_newline_on_save"
                            | "show_outline"
                            | "show_fps"
                            | "word_wrap"
                            | "header_lines"
                            | "outline_expanded"
                            | "outline_grouped"
                            | "outline_collapsed_groups"
                            | "color_window"
                            | "theme_preference"
                            | "theme_dark"
                            | "theme_light"
                            | "theme_custom_dark"
                            | "theme_custom_light",
                        _
                    ))
                )
            {
                text.push_str(line);
                text.push('\n');
            }
        }
        text.push_str(&format!("changes_tree={}\n", self.changes_tree));
        text.push_str(&format!("highlight_line={}\n", self.highlight_line));
        text.push_str(&format!("show_whitespace={}\n", self.show_whitespace));
        text.push_str(&format!("diff_ignore_eol={}\n", self.diff_ignore_eol));
        text.push_str(&format!("diff_first_change={}\n", self.diff_first_change));
        text.push_str(&format!("inline_blame={}\n", self.inline_blame));
        text.push_str(&format!("minimap={}\n", self.minimap));
        text.push_str(&format!("bracket_colors={}\n", self.bracket_colors));
        text.push_str(&format!("semantic_highlighting={}\n", self.semantic_highlighting));
        text.push_str(&format!("sticky_scroll={}\n", self.sticky_scroll));
        text.push_str(&format!("sticky_control_flow={}\n", self.sticky_control_flow));
        let hidden: Vec<_> = crate::editor::sticky::Kind::ALL
            .into_iter()
            .filter(|kind| !self.sticky_shown.contains(*kind))
            .map(|kind| kind.key())
            .collect();
        text.push_str(&format!("sticky_hidden={}\n", hidden.join(",")));
        text.push_str(&format!("restore_previous_session={}\n", self.restore_previous_session));
        text.push_str(&format!("reopen_last_folder={}\n", self.reopen_last_folder));
        text.push_str(&format!("tab_size={}\n", self.tab_size));
        for (key, font, size) in [
            ("editor_font", &self.editor_font, self.editor_font_size),
            ("ui_font", &self.ui_font, self.ui_font_size),
        ] {
            if let Some(font) = font {
                text.push_str(&format!(
                    "{key}={}\n",
                    serde_json::to_string(font).expect("font selection is serializable")
                ));
            }
            text.push_str(&format!("{key}_size={size}\n"));
        }
        text.push_str(&format!("breadcrumb_font_size={}\n", self.breadcrumb_font_size));
        text.push_str(&format!("format_on_save={}\n", self.format_on_save));
        text.push_str(&format!(
            "trim_trailing_whitespace_on_save={}\n",
            self.trim_trailing_whitespace_on_save
        ));
        text.push_str(&format!("insert_final_newline_on_save={}\n", self.insert_final_newline_on_save));
        text.push_str(&format!("show_outline={}\n", self.show_outline));
        text.push_str(&format!("show_fps={}\n", self.show_fps));
        text.push_str(&format!("word_wrap={}\n", self.word_wrap));
        for (title, list) in &self.shortcuts {
            let specs: Vec<String> = list.iter().map(crate::commands::shortcut_spec).collect();
            text.push_str(&format!("shortcut.{title}={}\n", specs.join(";")));
        }
        text.push_str(&format!("header_lines={}\n", self.header_lines));
        text.push_str(&format!("outline_expanded={}\n", self.outline_expanded));
        text.push_str(&format!("outline_grouped={}\n", self.outline_grouped));
        let collapsed = Group::ALL
            .into_iter()
            .filter(|group| self.outline_collapsed_groups.contains(*group))
            .map(Group::label)
            .collect::<Vec<_>>()
            .join(",");
        text.push_str(&format!("outline_collapsed_groups={collapsed}\n"));
        text.push_str(&format!("color_window={}\n", self.color_window));
        text.push_str(match self.themes.preference {
            egui::ThemePreference::Dark => "theme_preference=dark\n",
            egui::ThemePreference::Light => "theme_preference=light\n",
            egui::ThemePreference::System => "theme_preference=system\n",
        });
        text.push_str("theme_dark=");
        self.themes.dark.write(&mut text);
        text.push_str("\ntheme_light=");
        self.themes.light.write(&mut text);
        text.push('\n');
        for (key, custom) in [
            ("theme_custom_dark=", self.themes.custom_dark),
            ("theme_custom_light=", self.themes.custom_light),
        ] {
            if let Some(palette) = custom {
                text.push_str(key);
                palette.write(&mut text);
                text.push('\n');
            }
        }
        text
    }
}

pub(crate) struct SettingsWriter {
    requests: Sender<(Settings, egui::Context)>,
    pub(crate) errors: Receiver<io::Error>,
}

impl SettingsWriter {
    pub(crate) fn start() -> (Self, JoinHandle<()>) {
        let (requests, queue) = channel::<(Settings, egui::Context)>();
        let (report, errors) = channel();
        let worker = thread::spawn(move || {
            for (settings, ctx) in queue {
                if let Err(err) = settings.save()
                    && report.send(err).is_ok()
                {
                    ctx.request_repaint();
                }
            }
        });
        (Self { requests, errors }, worker)
    }

    pub(crate) fn save(&self, settings: Settings, ctx: &egui::Context) -> io::Result<()> {
        self.requests
            .send((settings, ctx.clone()))
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "settings writer stopped"))
    }
}

fn entry(line: &str) -> Option<(&str, &str)> {
    let (key, value) = line.split_once('=')?;
    Some((key.trim(), value.trim()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn font_settings_round_trip_with_collection_indices_and_unknown_keys() {
        let previous = concat!(
            " editor_font = {\"family\":\"Example Mono\",\"file\":{\"path\":\"C:\\\\Fonts\\\\Fonts=Mono.ttc\",\"index\":3}}\n",
            "ui_font={\"family\":\"Example Sans\",\"file\":null}\n",
            "editor_font_size=19\n",
            "ui_font_size=16\n",
            "future_option=true\n"
        );
        let settings = Settings::parse(previous);
        let parsed = Settings::parse(&settings.format(previous));
        let editor = parsed.editor_font.as_ref().unwrap();
        assert_eq!(editor.family, "Example Mono");
        assert_eq!(editor.file.as_ref().unwrap().path, r"C:\Fonts\Fonts=Mono.ttc");
        assert_eq!(editor.file.as_ref().unwrap().index, 3);
        assert_eq!(parsed.editor_font, settings.editor_font);
        assert_eq!(parsed.ui_font, settings.ui_font);
        assert_eq!((parsed.editor_font_size, parsed.ui_font_size), (19, 16));

        let text = settings.format(previous);
        assert_eq!(text.matches("editor_font=").count(), 1);
        assert!(text.contains("future_option=true\n"));
        let defaults = Settings::parse("editor_font_size=7\nui_font_size=33\n");
        assert!(defaults.editor_font.is_none() && defaults.ui_font.is_none());
        assert_eq!(
            (defaults.editor_font_size, defaults.ui_font_size),
            (DEFAULT_FONT_SIZE, DEFAULT_FONT_SIZE)
        );
        let text = defaults.format(&text);
        assert!(!text.contains("editor_font=") && !text.contains("ui_font="));
        assert!(text.contains("future_option=true\n"));
    }

    #[test]
    fn color_window_defaults_on_and_round_trips() {
        assert!(Settings::parse("").color_window);
        assert!(Settings::parse("color_window=maybe").color_window);
        let text = Settings::parse("color_window=false").format(" color_window = true\n");
        assert_eq!(text.matches("color_window=").count(), 1);
        assert!(!Settings::parse(&text).color_window);
    }

    #[test]
    fn theme_presets_and_custom_colors_round_trip() {
        use crate::theme::{PRESETS, Part};
        for (i, preset) in PRESETS.iter().enumerate() {
            let mut settings = Settings::default();
            let mut palette = Palette::new(i);
            settings.themes.preference = preset.base.into();
            settings.themes.set_palette(palette);
            let saved = settings.format("");
            assert_eq!(Settings::parse(&saved).themes, settings.themes);

            palette.set(Part::Comment, egui::Color32::from_rgba_unmultiplied(135, 83, 117, 142));
            palette.set(Part::CurrentLine, egui::Color32::TRANSPARENT);
            settings.themes.set_custom(preset.base, Some(palette));
            settings.themes.set_palette(palette);
            let saved = settings.format(&saved);
            assert_eq!(Settings::parse(&saved).themes, settings.themes);
            settings.themes.set_palette(Palette::new(i));
            assert_eq!(Settings::parse(&settings.format(&saved)).themes, settings.themes);
        }
        let settings = Settings::parse("theme_dark=oat-paper\ntheme_light=edittr-light,comment:invalid");
        assert_eq!(settings.themes, Themes::default());
    }

    #[test]
    fn outline_group_layout_round_trips() {
        let defaults = Settings::parse("");
        assert!(defaults.outline_grouped);
        assert_eq!(defaults.outline_collapsed_groups, CollapsedGroups::default());
        let previous = "outline_grouped=false\n outline_collapsed_groups = Variables, Unknown, Macros, Variables\n";
        let settings = Settings::parse(previous);
        let text = settings.format(previous);
        assert_eq!(text.matches("outline_collapsed_groups=").count(), 1);
        assert_eq!(text.matches("outline_grouped=").count(), 1);
        assert!(text.contains("outline_collapsed_groups=Macros,Variables\n"));
        let parsed = Settings::parse(&text);
        assert!(!parsed.outline_grouped);
        assert_eq!(parsed.outline_collapsed_groups, settings.outline_collapsed_groups);
    }

    #[test]
    fn outline_visibility_and_expansion_round_trip_independently() {
        let defaults = Settings::parse("show_outline=maybe\noutline_expanded=maybe");
        assert!(defaults.show_outline && defaults.outline_expanded);
        for show_outline in [false, true] {
            for outline_expanded in [false, true] {
                let settings = Settings {
                    show_outline,
                    outline_expanded,
                    ..defaults.clone()
                };
                let text = settings.format(" show_outline = false\noutline_expanded=false\n");
                assert_eq!(text.matches("show_outline=").count(), 1);
                assert_eq!(text.matches("outline_expanded=").count(), 1);
                let parsed = Settings::parse(&text);
                assert_eq!((parsed.show_outline, parsed.outline_expanded), (show_outline, outline_expanded));
            }
        }
    }

    #[test]
    fn save_whitespace_settings_round_trip() {
        let defaults = Settings::parse("");
        assert!(!defaults.trim_trailing_whitespace_on_save);
        assert!(!defaults.insert_final_newline_on_save);
        let input = "trim_trailing_whitespace_on_save=true\ninsert_final_newline_on_save=true\n";
        let formatted = Settings::parse(input).format(input);
        assert_eq!(formatted.matches(input).count(), 1);
        let parsed = Settings::parse(&formatted);
        assert!(parsed.trim_trailing_whitespace_on_save);
        assert!(parsed.insert_final_newline_on_save);
    }

    #[test]
    fn format_on_save_round_trip_and_default_off() {
        assert!(!Settings::parse("").format_on_save);
        let settings = Settings {
            format_on_save: true,
            ..Settings::default()
        };
        let text = settings.format("format_on_save=false\n");
        assert_eq!(text.matches("format_on_save=").count(), 1);
        assert!(Settings::parse(&text).format_on_save);
    }

    #[test]
    fn changes_tree_round_trip() {
        for changes_tree in [false, true] {
            let text = Settings {
                changes_tree,
                ..Settings::default()
            }
            .format("");
            assert_eq!(Settings::parse(&text).changes_tree, changes_tree);
        }
    }

    #[test]
    fn highlight_line_round_trip() {
        for highlight_line in [false, true] {
            let text = Settings {
                highlight_line,
                ..Settings::default()
            }
            .format("");
            assert_eq!(Settings::parse(&text).highlight_line, highlight_line);
        }
    }

    #[test]
    fn shortcut_changes_round_trip() {
        use eframe::egui::{Key, KeyboardShortcut, Modifiers};
        let mut settings = Settings::default();
        let cmd_shift_k = KeyboardShortcut::new(Modifiers::COMMAND.plus(Modifiers::SHIFT), Key::K);
        let ctrl_minus = KeyboardShortcut::new(Modifiers::CTRL, Key::Minus);
        settings.shortcuts.insert("Delete line".into(), vec![cmd_shift_k, ctrl_minus]);
        settings.shortcuts.insert("Join lines".into(), Vec::new());
        let text = settings.format("shortcut.Old=Cmd+Q\n");
        assert!(text.contains("shortcut.Delete line=Cmd+Shift+K;Ctrl+Minus\n"), "{text}");
        assert!(text.contains("shortcut.Join lines=\n"));
        assert!(!text.contains("shortcut.Old"));
        assert_eq!(Settings::parse(&text).shortcuts, settings.shortcuts);
        // Unreadable specs keep the defaults.
        assert!(Settings::parse("shortcut.Save=Cmd+Nonsense").shortcuts.is_empty());
    }

    #[test]
    fn preserves_unknown_entries_when_updating_settings() {
        let previous = "future_key=one=two\n changes_tree = true\n highlight_line = true\nfuture_key = three\n# A comment\n";
        let mut settings = Settings::parse(previous);
        assert!(settings.changes_tree);
        settings.changes_tree = false;
        settings.highlight_line = false;
        let text = settings.format(previous);
        assert_eq!(
            text,
            "future_key=one=two\nfuture_key = three\n# A comment\nchanges_tree=false\nhighlight_line=false\nshow_whitespace=false\ndiff_ignore_eol=true\ndiff_first_change=true\ninline_blame=true\nminimap=true\nbracket_colors=true\nsemantic_highlighting=true\nsticky_scroll=true\nsticky_control_flow=true\nsticky_hidden=\nrestore_previous_session=true\nreopen_last_folder=false\ntab_size=4\neditor_font_size=13\nui_font_size=13\nbreadcrumb_font_size=11\nformat_on_save=false\ntrim_trailing_whitespace_on_save=false\ninsert_final_newline_on_save=false\nshow_outline=true\nshow_fps=false\nword_wrap=false\nheader_lines=true\noutline_expanded=true\noutline_grouped=true\noutline_collapsed_groups=\ncolor_window=true\ntheme_preference=system\ntheme_dark=edittr-dark\ntheme_light=edittr-light\n"
        );
        assert!(!Settings::parse(&text).changes_tree);
    }

    #[test]
    fn missing_or_invalid_value_defaults_to_list_view() {
        assert!(!Settings::parse("").changes_tree);
        assert!(!Settings::parse("changes_tree=maybe").changes_tree);
    }

    #[test]
    fn saving_twice_keeps_one_line_per_key() {
        let s = Settings::default();
        let once = s.format("");
        assert_eq!(s.format(&once), once);
    }

    #[test]
    fn sticky_hidden_kinds_round_trip() {
        use crate::editor::sticky::{Kind, Kinds};
        assert_eq!(Settings::parse("").sticky_shown, Kinds::ALL);
        let settings = Settings::parse("sticky_hidden=loop, case,unknown\n");
        assert!(!settings.sticky_shown.contains(Kind::Loop) && !settings.sticky_shown.contains(Kind::Case));
        assert!(settings.sticky_shown.contains(Kind::Conditional));
        let text = settings.format("");
        assert!(text.contains("sticky_hidden=loop,case\n"));
        assert_eq!(Settings::parse(&text).sticky_shown, settings.sticky_shown);
    }

    #[test]
    fn sticky_scroll_defaults_on_and_round_trips() {
        assert!(Settings::parse("").sticky_scroll);
        assert!(Settings::parse("sticky_scroll=maybe").sticky_scroll);
        let settings = Settings::parse("sticky_scroll=false");
        let text = settings.format(" sticky_scroll = true\n");
        assert_eq!(text.matches("sticky_scroll=").count(), 1);
        assert!(!Settings::parse(&text).sticky_scroll);
    }

    #[test]
    fn show_whitespace_round_trip_and_default_off() {
        assert!(!Settings::parse("").show_whitespace);
        let s = Settings {
            show_whitespace: true,
            ..Settings::default()
        };
        assert!(Settings::parse(&s.format("")).show_whitespace);
    }

    #[test]
    fn diff_ignore_eol_round_trip_and_default_on() {
        assert!(Settings::parse("").diff_ignore_eol);
        for diff_ignore_eol in [false, true] {
            let settings = Settings {
                diff_ignore_eol,
                ..Settings::default()
            };
            let text = settings.format("diff_ignore_eol=maybe\n");
            assert_eq!(text.matches("diff_ignore_eol=").count(), 1);
            assert_eq!(Settings::parse(&text).diff_ignore_eol, diff_ignore_eol);
        }
    }

    #[test]
    fn missing_or_invalid_value_enables_line_highlight() {
        assert!(Settings::parse("changes_tree=true").highlight_line);
        assert!(Settings::parse("highlight_line=maybe").highlight_line);
    }

    #[test]
    fn tab_size_defaults_to_four_and_rejects_nonsense() {
        assert_eq!(Settings::parse("").tab_size, 4);
        for (text, n) in [
            ("tab_size=2", 2),
            ("tab_size=8", 8),
            ("tab_size=0", 4),
            ("tab_size=99", 4),
            ("tab_size=x", 4),
        ] {
            assert_eq!(Settings::parse(text).tab_size, n, "{text}");
        }
        let settings = Settings {
            tab_size: 2,
            ..Settings::default()
        };
        assert_eq!(Settings::parse(&settings.format("")).tab_size, 2);
    }

    #[test]
    fn reopen_last_folder_defaults_off_and_round_trips() {
        assert!(!Settings::parse("").reopen_last_folder);
        let settings = Settings {
            reopen_last_folder: true,
            ..Settings::default()
        };
        assert!(Settings::parse(&settings.format("")).reopen_last_folder);
    }

    #[test]
    fn restore_previous_session_round_trip_and_default_on() {
        assert!(Settings::parse("").restore_previous_session);
        assert!(Settings::parse("restore_previous_session=maybe").restore_previous_session);
        for restore_previous_session in [false, true] {
            let settings = Settings {
                restore_previous_session,
                ..Settings::default()
            };
            let text = settings.format("restore_previous_session=false");
            assert_eq!(text.matches("restore_previous_session=").count(), 1);
            assert_eq!(Settings::parse(&text).restore_previous_session, restore_previous_session);
        }
    }
}
