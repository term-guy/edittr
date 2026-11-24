//! EditorConfig discovery, matching and save-time whitespace rules.
use regex::Regex;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

const NAME: &str = ".editorconfig";
const DEFAULT_TAB_WIDTH: usize = 4;

#[derive(Default)]
pub(crate) struct Properties {
    values: HashMap<String, String>,
}

impl Properties {
    fn get(&self, key: &str) -> Option<&str> {
        self.values.get(key).map(String::as_str)
    }

    /// The indent unit, only when the .editorconfig sets `indent_style`.
    pub(crate) fn explicit_indent(&self) -> Option<String> {
        self.get("indent_style").is_some().then(|| self.indent_unit())
    }

    pub(crate) fn indent_unit(&self) -> String {
        let width = |key| {
            self.get(key)
                .and_then(|value| value.parse::<usize>().ok())
                .filter(|n| (1..=1024).contains(n))
        };
        let size = width("indent_size");
        let tab_width = width("tab_width").or(size).unwrap_or(DEFAULT_TAB_WIDTH);
        let size = size.unwrap_or(tab_width);
        match self.get("indent_style") {
            Some("space") => " ".repeat(size),
            Some("tab") => "\t".repeat(size / tab_width) + &" ".repeat(size % tab_width),
            _ => "\t".into(),
        }
    }

    pub(crate) fn transform(&self, mut text: String) -> String {
        const BOM: char = '\u{feff}';
        if self.get("charset") == Some("utf-8") && text.starts_with(BOM) {
            text.drain(..BOM.len_utf8());
        }
        if text.contains("\r\n") {
            text = text.replace("\r\n", "\n");
        }
        if self.get("trim_trailing_whitespace") == Some("true") {
            let mut trimmed = String::with_capacity(text.len());
            for line in text.split_inclusive('\n') {
                trimmed.push_str(line.trim_end_matches(char::is_whitespace));
                if line.ends_with('\n') {
                    trimmed.push('\n');
                }
            }
            text = trimmed;
        }
        match self.get("insert_final_newline") {
            Some("true") if !text.is_empty() && !text.ends_with('\n') => text.push('\n'),
            Some("false") => text.truncate(text.trim_end_matches('\n').len()),
            _ => {}
        }
        text
    }

    pub(crate) fn crlf(&self, original_crlf: bool) -> bool {
        original_crlf || self.get("end_of_line") == Some("crlf")
    }
}

#[derive(Default)]
struct Config {
    root: bool,
    sections: Vec<Section>,
}

struct Section {
    glob: Option<Regex>,
    pairs: Vec<(String, String)>,
}

impl Config {
    fn parse(text: &str) -> Self {
        let mut config = Self::default();
        for line in text.trim_start_matches('\u{feff}').lines().map(str::trim) {
            if line.starts_with(['#', ';']) {
                continue;
            }
            if let Some(glob) = line.strip_prefix('[').and_then(|line| line.strip_suffix(']')) {
                config.sections.push(Section {
                    glob: compile_glob(glob),
                    pairs: Vec::new(),
                });
            } else if let Some((key, value)) = line.split_once('=') {
                let key = key.trim().to_ascii_lowercase();
                let value = value.trim().to_ascii_lowercase();
                if let Some(section) = config.sections.last_mut() {
                    section.pairs.push((key, value));
                } else if key == "root" {
                    config.root = value == "true";
                }
            }
        }
        config
    }

    fn apply(&self, relative: &str, properties: &mut Properties) {
        for Section { glob, pairs } in &self.sections {
            if glob.as_ref().is_some_and(|glob| glob.is_match(relative)) {
                for (key, value) in pairs {
                    if value == "unset" {
                        properties.values.remove(key);
                    } else {
                        properties.values.insert(key.clone(), value.clone());
                    }
                }
            }
        }
    }
}

#[derive(Clone, Default)]
pub(crate) struct Cache(Arc<Mutex<Entries>>);

#[derive(Default)]
struct Entries {
    generation: u64,
    dirs: HashMap<PathBuf, Cached>,
}

struct Cached {
    stamp: (SystemTime, u64),
    config: Arc<Config>,
}

impl Cache {
    /// Background only. Check metadata to cover files above or outside the watched project.
    pub(crate) fn load(&self, path: &Path) -> Properties {
        resolve(path, |dir| self.load_dir(dir))
    }

    /// Use the already loaded configs without file I/O on the UI thread.
    pub(crate) fn cached(&self, path: &Path) -> Properties {
        resolve(path, |dir| self.0.lock().unwrap().dirs.get(dir).map(|cached| cached.config.clone()))
    }

    pub(crate) fn invalidate<'a>(&self, paths: impl IntoIterator<Item = &'a PathBuf>, rescan: bool) -> bool {
        let dirs: Vec<_> = paths
            .into_iter()
            .filter(|path| path.file_name().is_some_and(|name| name == NAME))
            .filter_map(|path| path.parent())
            .collect();
        if !rescan && dirs.is_empty() {
            return false;
        }
        let mut entries = self.0.lock().unwrap();
        entries.generation += 1;
        if rescan {
            entries.dirs.clear();
        } else {
            for dir in dirs {
                entries.dirs.remove(dir);
            }
        }
        true
    }

    fn load_dir(&self, dir: &Path) -> Option<Arc<Config>> {
        let generation = self.0.lock().unwrap().generation;
        let path = dir.join(NAME);
        let stamp = fs::metadata(&path)
            .ok()
            .filter(|metadata| metadata.is_file())
            .and_then(|metadata| metadata.modified().ok().map(|time| (time, metadata.len())));
        let Some(stamp) = stamp else {
            self.remove_dir(dir, generation);
            return None;
        };
        if let Some(cached) = self.0.lock().unwrap().dirs.get(dir)
            && cached.stamp == stamp
        {
            return Some(cached.config.clone());
        }
        let Ok(text) = fs::read_to_string(path) else {
            self.remove_dir(dir, generation);
            return None;
        };
        let config = Arc::new(Config::parse(&text));
        let mut entries = self.0.lock().unwrap();
        if entries.generation == generation {
            entries.dirs.insert(
                dir.to_owned(),
                Cached {
                    stamp,
                    config: config.clone(),
                },
            );
        }
        Some(config)
    }

    fn remove_dir(&self, dir: &Path, generation: u64) {
        let mut entries = self.0.lock().unwrap();
        if entries.generation == generation {
            entries.dirs.remove(dir);
        }
    }
}

fn resolve(path: &Path, mut get: impl FnMut(&Path) -> Option<Arc<Config>>) -> Properties {
    let mut configs = Vec::new();
    if let Some(folder) = path.parent() {
        for dir in folder.ancestors() {
            if let Some(config) = get(dir) {
                let root = config.root;
                configs.push((dir, config));
                if root {
                    break;
                }
            }
        }
    }
    let mut properties = Properties::default();
    for (dir, config) in configs.into_iter().rev() {
        if let Ok(relative) = path.strip_prefix(dir) {
            let relative = relative.iter().map(|part| part.to_string_lossy()).collect::<Vec<_>>().join("/");
            config.apply(&relative, &mut properties);
        }
    }
    properties
}

fn compile_glob(glob: &str) -> Option<Regex> {
    if glob.is_empty() {
        return None;
    }
    let mut pattern = String::from(r"(?s)\A");
    if !glob.contains('/') {
        pattern.push_str("(?:.*/)?");
    }
    let glob: Vec<_> = glob.strip_prefix('/').unwrap_or(glob).chars().collect();
    translate(&glob, &mut pattern, 0);
    pattern.push_str(r"\z");
    Regex::new(&pattern).ok()
}

fn translate(mut glob: &[char], pattern: &mut String, depth: usize) {
    while let Some((&ch, rest)) = glob.split_first() {
        glob = rest;
        match ch {
            '*' if glob.starts_with(&['*', '/']) => {
                pattern.push_str("(?:.*/)?");
                glob = &glob[2..];
            }
            '*' if glob.starts_with(&['*']) => {
                pattern.push_str(".*");
                glob = &glob[1..];
            }
            '*' => pattern.push_str("[^/]*"),
            '?' => pattern.push_str("[^/]"),
            '\\' if !glob.is_empty() => {
                literal(pattern, glob[0]);
                glob = &glob[1..];
            }
            '[' if let Some(end) = class_end(glob) => {
                let class = &glob[..end];
                let (negation, class) = class.strip_prefix(&['!']).map_or(("", class), |class| ("^", class));
                pattern.push_str("[[^/]&&[");
                pattern.push_str(negation);
                let mut iter = class.iter().copied();
                while let Some(ch) = iter.next() {
                    if ch == '\\' {
                        literal(pattern, iter.next().unwrap_or(ch));
                    } else if ch == '-' {
                        pattern.push(ch);
                    } else {
                        literal(pattern, ch);
                    }
                }
                pattern.push_str("]]");
                glob = &glob[end + 1..];
            }
            '{' if depth < 32
                && let Some((choices, end)) = alternatives(glob) =>
            {
                pattern.push_str("(?:");
                for (index, choice) in choices.into_iter().enumerate() {
                    if index > 0 {
                        pattern.push('|');
                    }
                    translate(choice, pattern, depth + 1);
                }
                pattern.push(')');
                glob = &glob[end + 1..];
            }
            _ => literal(pattern, ch),
        }
    }
}

fn literal(pattern: &mut String, ch: char) {
    if r"\.+*?()|[]{}^$#&-~".contains(ch) {
        pattern.push('\\');
    }
    pattern.push(ch);
}

fn class_end(glob: &[char]) -> Option<usize> {
    let mut iter = glob.iter().enumerate();
    while let Some((i, &ch)) = iter.next() {
        match ch {
            '\\' => {
                iter.next();
            }
            ']' => return (i > 0 && glob[..i] != ['!']).then_some(i),
            '/' => return None,
            _ => {}
        }
    }
    None
}

fn alternatives(glob: &[char]) -> Option<(Vec<&[char]>, usize)> {
    let mut branches = Vec::new();
    let (mut start, mut index, mut level) = (0, 0, 0);
    while let Some(&ch) = glob.get(index) {
        match ch {
            '\\' => index += 1,
            '[' if let Some(end) = class_end(&glob[index + 1..]) => index += end + 1,
            '{' => level += 1,
            '}' if level == 0 => {
                if branches.is_empty() {
                    return None;
                }
                branches.push(&glob[start..index]);
                return Some((branches, index));
            }
            '}' => level -= 1,
            ',' if level == 0 => {
                branches.push(&glob[start..index]);
                start = index + 1;
            }
            _ => {}
        }
        index += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globs_match_paths_and_basenames() {
        for (pattern, yes, no) in [
            ("*.rs", "src/deep/lib.rs", "src/lib.rs.txt"),
            ("/Makefile", "Makefile", "src/Makefile"),
            ("src/*.rs", "src/lib.rs", "src/deep/lib.rs"),
            ("src/**/lib.rs", "src/lib.rs", "test/lib.rs"),
            ("src/**/lib.rs", "src/a/b/lib.rs", "src/a/b/main.rs"),
            ("*.{js,{ts,tsx}}", "src/a.tsx", "a.jsx"),
            ("[a-c]?[!x].txt", "sub/b3z.txt", "b3x.txt"),
            (r"file\[1\].txt", "file[1].txt", "file11.txt"),
        ] {
            let glob = compile_glob(pattern).unwrap();
            assert!(glob.is_match(yes), "{pattern} should match {yes}");
            assert!(!glob.is_match(no), "{pattern} should not match {no}");
        }
    }

    #[test]
    fn nearer_files_and_later_sections_win_until_root() {
        let configs = [
            ("/", "[*]\ninsert_final_newline=true"),
            (
                "/project",
                "ROOT = TRUE\n[*]\nindent_style=space\nindent_size=8\nend_of_line=crlf\n[*.rs]\nindent_size=4",
            ),
            ("/project/src", "[*.rs]\nindent_size=tab\ntab_width=2\nend_of_line=unset"),
        ]
        .map(|(dir, text)| (Path::new(dir), Arc::new(Config::parse(text))));
        let get = |dir: &Path| configs.iter().find(|(path, _)| *path == dir).map(|(_, config)| config.clone());
        let src = resolve(Path::new("/project/src/lib.rs"), get);
        assert_eq!(src.indent_unit(), "  ");
        assert!(!src.crlf(false));
        assert_eq!(src.get("insert_final_newline"), None);
        let project = resolve(Path::new("/project/lib.rs"), get);
        assert_eq!(project.indent_unit(), "    ");
        assert!(project.crlf(false));
        assert_eq!(resolve(Path::new("/project/test.py"), get).indent_unit(), "        ");
    }

    #[test]
    fn transforms_whitespace_and_preserves_original_crlf() {
        let mut properties = Properties::default();
        let config = Config::parse("[*]\ntrim_trailing_whitespace=true\ninsert_final_newline=true\nend_of_line=lf\ncharset=utf-8");
        config.apply("test.txt", &mut properties);
        assert_eq!(properties.transform("\u{feff}hi \t\r\n  \r\nbye\t".into()), "hi\n\nbye\n");
        assert_eq!(properties.transform(String::new()), "");
        assert!(properties.crlf(true));
        assert!(!properties.crlf(false));
        Config::parse("[*]\ninsert_final_newline=false").apply("test.txt", &mut properties);
        assert_eq!(properties.transform("hi \n\n".into()), "hi");
        assert_eq!(Properties::default().transform("hi  \n\n".into()), "hi  \n\n");
    }
}
