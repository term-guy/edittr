use std::{
    fs,
    io::{self, Write},
    path::Path,
};

#[derive(Default)]
pub(crate) struct Settings {
    pub(crate) changes_tree: bool,
}

impl Settings {
    pub(crate) fn load() -> Self {
        dirs::config_dir().map(|dir| Self::load_from(&dir)).unwrap_or_default()
    }

    pub(crate) fn save(&self) -> io::Result<()> {
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
            if key == "changes_tree"
                && let Ok(value) = value.parse()
            {
                settings.changes_tree = value;
            }
        }
        settings
    }

    fn format(&self, previous: &str) -> String {
        let mut text = String::new();
        for line in previous.lines() {
            if !matches!(entry(line), Some(("changes_tree", _))) {
                text.push_str(line);
                text.push('\n');
            }
        }
        text.push_str(&format!("changes_tree={}\n", self.changes_tree));
        text
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
            let text = Settings { changes_tree }.format("");
            assert_eq!(Settings::parse(&text).changes_tree, changes_tree);
        }
    }

    #[test]
    fn preserves_unknown_entries_when_updating_settings() {
        let previous = "future_key=one=two\n changes_tree = true\nfuture_key = three\n# A comment\n";
        let mut settings = Settings::parse(previous);
        assert!(settings.changes_tree);
        settings.changes_tree = false;
        let text = settings.format(previous);
        assert_eq!(text, "future_key=one=two\nfuture_key = three\n# A comment\nchanges_tree=false\n");
        assert!(!Settings::parse(&text).changes_tree);
    }

    #[test]
    fn missing_or_invalid_value_defaults_to_list_view() {
        assert!(!Settings::parse("").changes_tree);
        assert!(!Settings::parse("changes_tree=maybe").changes_tree);
    }
}
