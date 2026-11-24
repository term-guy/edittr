use std::{
    fs,
    io::{self, Write},
    path::Path,
    sync::mpsc::{Receiver, Sender, channel},
    thread::{self, JoinHandle},
};

use eframe::egui;

#[derive(Clone, Copy)]
pub(crate) struct Settings {
    pub(crate) changes_tree: bool,
    pub(crate) highlight_line: bool,
    /// Show spaces as middle dots and tabs as arrows.
    pub(crate) show_whitespace: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            changes_tree: false,
            highlight_line: true,
            show_whitespace: false,
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
            if let Ok(value) = value.parse() {
                match key {
                    "changes_tree" => settings.changes_tree = value,
                    "highlight_line" => settings.highlight_line = value,
                    "show_whitespace" => settings.show_whitespace = value,
                    _ => {}
                }
            }
        }
        settings
    }

    fn format(&self, previous: &str) -> String {
        let mut text = String::new();
        for line in previous.lines() {
            if !matches!(entry(line), Some(("changes_tree" | "highlight_line" | "show_whitespace", _))) {
                text.push_str(line);
                text.push('\n');
            }
        }
        text.push_str(&format!("changes_tree={}\n", self.changes_tree));
        text.push_str(&format!("highlight_line={}\n", self.highlight_line));
        text.push_str(&format!("show_whitespace={}\n", self.show_whitespace));
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
            "future_key=one=two\nfuture_key = three\n# A comment\nchanges_tree=false\nhighlight_line=false\nshow_whitespace=false\n"
        );
        assert!(!Settings::parse(&text).changes_tree);
    }

    #[test]
    fn missing_or_invalid_value_defaults_to_list_view() {
        assert!(!Settings::parse("").changes_tree);
        assert!(!Settings::parse("changes_tree=maybe").changes_tree);
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
    fn missing_or_invalid_value_enables_line_highlight() {
        assert!(Settings::parse("changes_tree=true").highlight_line);
        assert!(Settings::parse("highlight_line=maybe").highlight_line);
    }
}
