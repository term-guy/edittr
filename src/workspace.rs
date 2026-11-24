use std::io::{self, ErrorKind, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

fn config_file(root: &str) -> Option<PathBuf> {
    let hash = root_hash(Path::new(root));
    Some(dirs::config_dir()?.join("edittr").join("workspaces").join(format!("{hash}.txt")))
}

pub(crate) fn root_hash(root: &Path) -> String {
    let hash = root.to_string_lossy().bytes().fold(0xcbf29ce484222325_u64, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
    });
    format!("{hash:016x}")
}

pub fn load_ignored(root: &Path) -> Vec<String> {
    let root = root.to_string_lossy();
    config_file(&root)
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|text| parse(&root, &text).map(|config| config.ignored))
        .unwrap_or_default()
}

pub fn save_ignored(root: &Path, list: &[String]) {
    let _ = update_config(root, |root, config| format_config(root, list, &config.unknown));
}

fn update_config(root: &Path, format: impl FnOnce(&str, &Config<'_>) -> String) -> io::Result<()> {
    if cfg!(feature = "tour") && std::env::var_os("EDITTR_TOUR_OUT").is_some() {
        return Ok(());
    }
    let root = root.to_string_lossy();
    let Some(path) = config_file(&root) else { return Ok(()) };
    // Keep read-modify-write updates ordered across the UI and session writer.
    static CONFIG_UPDATE: Mutex<()> = Mutex::new(());
    let _guard = CONFIG_UPDATE.lock().unwrap();
    let existing = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == ErrorKind::NotFound => String::new(),
        Err(err) => return Err(err),
    };
    let Some(config) = parse(&root, &existing) else { return Ok(()) };
    write_config(&path, &format(&root, &config))
}

/// Per-workspace C/C++ settings (stored as `c.<key>=` lines in the workspace file).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct CSettings {
    /// Extra compiler flags added to every file, e.g. `-DDEBUG -Iinclude`.
    pub flags: String,
    /// The compiler the project builds with (for clangd's --query-driver), e.g. `arm-none-eabi-gcc`.
    pub compiler: String,
    /// Build command used by "Generate compile database", e.g. `make -C firmware all`.
    pub build: String,
}

const C_KEYS: [&str; 3] = ["c.flags=", "c.compiler=", "c.build="];

pub(crate) fn load_c_settings(root: &Path) -> CSettings {
    let root = root.to_string_lossy();
    let text = config_file(&root)
        .and_then(|path| std::fs::read_to_string(path).ok())
        .unwrap_or_default();
    let mut settings = CSettings::default();
    let Some(config) = parse(&root, &text) else { return settings };
    for line in config.unknown {
        if let Some(v) = line.strip_prefix(C_KEYS[0]) {
            settings.flags = v.to_owned();
        } else if let Some(v) = line.strip_prefix(C_KEYS[1]) {
            settings.compiler = v.to_owned();
        } else if let Some(v) = line.strip_prefix(C_KEYS[2]) {
            settings.build = v.to_owned();
        }
    }
    settings
}

pub(crate) fn save_c_settings(root: &Path, settings: &CSettings) {
    let _ = update_config(root, |root, config| format_with_c(root, config, settings));
}

fn format_with_c(root: &str, config: &Config, settings: &CSettings) -> String {
    format_with_settings(root, config, C_KEYS, [&settings.flags, &settings.compiler, &settings.build])
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct SearchFilters {
    pub include: String,
    pub exclude: String,
}

const SEARCH_KEYS: [&str; 2] = ["search.include=", "search.exclude="];

pub(crate) fn load_search_filters(root: &Path) -> SearchFilters {
    let root = root.to_string_lossy();
    let text = config_file(&root)
        .and_then(|path| std::fs::read_to_string(path).ok())
        .unwrap_or_default();
    parse(&root, &text).map(|config| parse_search_filters(&config)).unwrap_or_default()
}

fn parse_search_filters(config: &Config<'_>) -> SearchFilters {
    let mut filters = SearchFilters::default();
    for line in &config.unknown {
        if let Some(value) = line.strip_prefix(SEARCH_KEYS[0]) {
            filters.include = value.to_owned();
        } else if let Some(value) = line.strip_prefix(SEARCH_KEYS[1]) {
            filters.exclude = value.to_owned();
        }
    }
    filters
}

pub(crate) fn save_search_filters(root: &Path, filters: &SearchFilters) -> io::Result<()> {
    update_config(root, |root, config| format_with_search_filters(root, config, filters))
}

fn format_with_search_filters(root: &str, config: &Config, filters: &SearchFilters) -> String {
    format_with_settings(root, config, SEARCH_KEYS, [&filters.include, &filters.exclude])
}

fn format_with_settings<const N: usize>(root: &str, config: &Config, keys: [&str; N], values: [&str; N]) -> String {
    let lines: Vec<_> = keys
        .iter()
        .zip(values)
        .filter_map(|(key, value)| {
            let one_line = value.replace(['\r', '\n'], " ");
            let value = one_line.trim();
            (!value.is_empty()).then(|| format!("{key}{value}"))
        })
        .collect();
    let mut unknown: Vec<&str> = config
        .unknown
        .iter()
        .copied()
        .filter(|line| !keys.iter().any(|key| line.starts_with(key)))
        .collect();
    unknown.extend(lines.iter().map(String::as_str));
    format_config(root, &config.ignored, &unknown)
}

pub(crate) fn write_config(path: &Path, text: &str) -> io::Result<()> {
    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);
    std::fs::create_dir_all(path.parent().unwrap())?;
    let temp = path.with_extension(format!("{}.{}.tmp", std::process::id(), NEXT_TEMP.fetch_add(1, Ordering::Relaxed)));
    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(&temp)?;
    let written = file.write_all(text.as_bytes());
    drop(file);
    let result = written.and_then(|()| std::fs::rename(&temp, path));
    if result.is_err() {
        let _ = std::fs::remove_file(temp);
    }
    result
}

pub fn is_ignored(rel: &str, ignored: &[String]) -> bool {
    ignored
        .iter()
        .any(|dir| rel == dir || rel.strip_prefix(dir).is_some_and(|rest| rest.starts_with('/')))
}

pub fn relative_path(root: &Path, path: &Path) -> Option<String> {
    let rel = path.strip_prefix(root).ok()?.to_string_lossy();
    Some(if cfg!(windows) { rel.replace('\\', "/") } else { rel.into_owned() })
}

pub fn valid_folder(rel: &str) -> bool {
    !rel.contains(['\\', '\r', '\n'])
        && rel.split('/').all(|part| !matches!(part, "" | "." | ".."))
        && Path::new(rel)
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

#[derive(Default)]
struct Config<'a> {
    ignored: Vec<String>,
    unknown: Vec<&'a str>,
}

fn parse<'a>(root: &str, text: &'a str) -> Option<Config<'a>> {
    let mut config = Config::default();
    let mut lines = text.lines().peekable();
    if let Some(saved_root) = lines.peek().and_then(|line| line.strip_prefix("root=")) {
        if saved_root != root {
            return None;
        }
        lines.next();
    }
    for line in lines {
        if let Some(rel) = line.strip_prefix("ignore=") {
            if valid_folder(rel) && !config.ignored.iter().any(|entry| entry == rel) {
                config.ignored.push(rel.to_owned());
            }
        } else {
            config.unknown.push(line);
        }
    }
    Some(config)
}

fn format_config(root: &str, ignored: &[String], unknown: &[&str]) -> String {
    let mut text = format!("root={root}\n");
    for rel in ignored.iter().filter(|rel| valid_folder(rel)) {
        text.push_str("ignore=");
        text.push_str(rel);
        text.push('\n');
    }
    for line in unknown {
        text.push_str(line);
        text.push('\n');
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_whole_folder_components() {
        let ignored = ["vend".into(), "web/node_modules".into()];
        for rel in ["vend", "vend/foo.rs", "web/node_modules/pkg/file.js"] {
            assert!(is_ignored(rel, &ignored), "{rel}");
        }
        for rel in ["vendor/x", "web/node_modules-old/x", "node_modules/x", "web", "nested/vend/x"] {
            assert!(!is_ignored(rel, &ignored), "{rel}");
        }
        assert!(!is_ignored("vend/a", &[]));
    }

    #[test]
    fn round_trips_config_and_keeps_unknown_lines() {
        let root = "/home/user/project";
        let original = "root=/home/user/project\nignore=vendor\n# comment\nfuture=1\n\nignore=web/node_modules\n";
        let mut config = parse(root, original).unwrap();
        assert_eq!(config.ignored, ["vendor", "web/node_modules"]);
        assert_eq!(config.unknown, ["# comment", "future=1", ""]);
        assert!(parse("/other/project", original).is_none());

        config.ignored.remove(0);
        config.ignored.push("build/output".into());
        let formatted = format_config(root, &config.ignored, &config.unknown);
        let reloaded = parse(root, &formatted).unwrap();
        assert_eq!(reloaded.ignored, config.ignored);
        assert_eq!(reloaded.unknown, config.unknown);
    }

    #[test]
    fn c_settings_round_trip_next_to_ignored_folders() {
        let root = "/home/user/project";
        let config = parse(root, "root=/home/user/project\nignore=vendor\nc.flags=-DOLD\nfuture=1\n").unwrap();
        let settings = CSettings {
            flags: "-DDEBUG -Iinc".into(),
            compiler: "arm-none-eabi-gcc".into(),
            build: "make -C fw\nall".into(),
        };
        let text = format_with_c(root, &config, &settings);
        assert_eq!(
            text,
            "root=/home/user/project\nignore=vendor\nfuture=1\nc.flags=-DDEBUG -Iinc\nc.compiler=arm-none-eabi-gcc\nc.build=make -C fw all\n"
        );
        let cleared = format_with_c(root, &parse(root, &text).unwrap(), &CSettings::default());
        assert_eq!(cleared, "root=/home/user/project\nignore=vendor\nfuture=1\n");
    }

    #[test]
    fn search_filters_round_trip_and_clear_next_to_other_settings() {
        let root = "/home/user/project";
        let other = concat!(
            "root=/home/user/project\n",
            "ignore=vendor\n",
            "c.flags=-DDEBUG\n",
            "c.compiler=clang\n",
            "c.build=make all\n",
            "# comment\n",
            "search.future=1\n",
        );
        let original = format!("{other}search.include=*.old\nsearch.exclude=tmp/**\n");
        let filters = SearchFilters {
            include: "*.c, src/**/*.h".into(),
            exclude: "*.min.js, path with spaces/**".into(),
        };
        let saved = format_with_search_filters(root, &parse(root, &original).unwrap(), &filters);
        assert_eq!(
            saved,
            format!("{other}search.include=*.c, src/**/*.h\nsearch.exclude=*.min.js, path with spaces/**\n")
        );
        let config = parse(root, &saved).unwrap();
        assert_eq!(parse_search_filters(&config), filters);
        let cleared = format_with_search_filters(root, &config, &SearchFilters::default());
        assert_eq!(cleared, other);
        assert_eq!(parse_search_filters(&parse(root, &cleared).unwrap()), SearchFilters::default());
    }

    #[test]
    fn search_filter_values_stay_on_one_line() {
        let root = "/home/user/project";
        let filters = SearchFilters {
            include: "*.c,\nsrc/**/*.h".into(),
            exclude: "\r\n".into(),
        };
        let text = format_with_search_filters(root, &Config::default(), &filters);
        assert_eq!(text, "root=/home/user/project\nsearch.include=*.c, src/**/*.h\n");
    }
}
