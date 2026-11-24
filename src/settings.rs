use std::{
    fs,
    io::{self, Write},
    path::Path,
    sync::mpsc::{Receiver, Sender, channel},
    thread::{self, JoinHandle},
};

use eframe::egui;

use crate::outline::{CollapsedGroups, Group};

#[derive(Clone, Copy)]
pub(crate) struct Settings {
    pub(crate) changes_tree: bool,
    pub(crate) format_on_save: bool,
    pub(crate) highlight_line: bool,
    /// Show spaces as middle dots and tabs as arrows.
    pub(crate) show_whitespace: bool,
    pub(crate) diff_ignore_eol: bool,
    /// Dimmed "who changed this line" after the cursor's line.
    pub(crate) inline_blame: bool,
    /// Code overview on the right of the editor.
    pub(crate) minimap: bool,
    /// Color nested brackets by depth.
    pub(crate) bracket_colors: bool,
    pub(crate) show_outline: bool,
    pub(crate) outline_expanded: bool,
    pub(crate) outline_grouped: bool,
    pub(crate) outline_collapsed_groups: CollapsedGroups,
    pub(crate) restore_previous_session: bool,
    /// Start in the last folder instead of the start page (when no folder is given).
    pub(crate) reopen_last_folder: bool,
    /// Width of a tab character, and the spaces Tab inserts in files without an
    /// indentation style of their own.
    pub(crate) tab_size: usize,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            changes_tree: false,
            format_on_save: false,
            highlight_line: true,
            show_whitespace: false,
            diff_ignore_eol: true,
            inline_blame: true,
            minimap: true,
            bracket_colors: true,
            show_outline: true,
            outline_expanded: true,
            outline_grouped: true,
            outline_collapsed_groups: CollapsedGroups::default(),
            restore_previous_session: true,
            reopen_last_folder: false,
            tab_size: 4,
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
                    "changes_tree" => settings.changes_tree = value,
                    "format_on_save" => settings.format_on_save = value,
                    "highlight_line" => settings.highlight_line = value,
                    "show_whitespace" => settings.show_whitespace = value,
                    "diff_ignore_eol" => settings.diff_ignore_eol = value,
                    "inline_blame" => settings.inline_blame = value,
                    "minimap" => settings.minimap = value,
                    "bracket_colors" => settings.bracket_colors = value,
                    "show_outline" => settings.show_outline = value,
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
            if !matches!(
                entry(line),
                Some((
                    "changes_tree"
                        | "highlight_line"
                        | "show_whitespace"
                        | "diff_ignore_eol"
                        | "inline_blame"
                        | "minimap"
                        | "bracket_colors"
                        | "restore_previous_session"
                        | "reopen_last_folder"
                        | "tab_size"
                        | "format_on_save"
                        | "show_outline"
                        | "outline_expanded"
                        | "outline_grouped"
                        | "outline_collapsed_groups",
                    _
                ))
            ) {
                text.push_str(line);
                text.push('\n');
            }
        }
        text.push_str(&format!("changes_tree={}\n", self.changes_tree));
        text.push_str(&format!("highlight_line={}\n", self.highlight_line));
        text.push_str(&format!("show_whitespace={}\n", self.show_whitespace));
        text.push_str(&format!("diff_ignore_eol={}\n", self.diff_ignore_eol));
        text.push_str(&format!("inline_blame={}\n", self.inline_blame));
        text.push_str(&format!("minimap={}\n", self.minimap));
        text.push_str(&format!("bracket_colors={}\n", self.bracket_colors));
        text.push_str(&format!("restore_previous_session={}\n", self.restore_previous_session));
        text.push_str(&format!("reopen_last_folder={}\n", self.reopen_last_folder));
        text.push_str(&format!("tab_size={}\n", self.tab_size));
        text.push_str(&format!("format_on_save={}\n", self.format_on_save));
        text.push_str(&format!("show_outline={}\n", self.show_outline));
        text.push_str(&format!("outline_expanded={}\n", self.outline_expanded));
        text.push_str(&format!("outline_grouped={}\n", self.outline_grouped));
        let collapsed = Group::ALL
            .into_iter()
            .filter(|group| self.outline_collapsed_groups.contains(*group))
            .map(Group::label)
            .collect::<Vec<_>>()
            .join(",");
        text.push_str(&format!("outline_collapsed_groups={collapsed}\n"));
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
                    ..defaults
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
    fn preserves_unknown_entries_when_updating_settings() {
        let previous = "future_key=one=two\n changes_tree = true\n highlight_line = true\nfuture_key = three\n# A comment\n";
        let mut settings = Settings::parse(previous);
        assert!(settings.changes_tree);
        settings.changes_tree = false;
        settings.highlight_line = false;
        let text = settings.format(previous);
        assert_eq!(
            text,
            "future_key=one=two\nfuture_key = three\n# A comment\nchanges_tree=false\nhighlight_line=false\nshow_whitespace=false\ndiff_ignore_eol=true\ninline_blame=true\nminimap=true\nbracket_colors=true\nrestore_previous_session=true\nreopen_last_folder=false\ntab_size=4\nformat_on_save=false\nshow_outline=true\noutline_expanded=true\noutline_grouped=true\noutline_collapsed_groups=\n"
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
