use std::io::{self, ErrorKind, Write};
use std::path::{Component, Path, PathBuf};
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
    if cfg!(feature = "tour") && std::env::var_os("EDITTR_TOUR_OUT").is_some() {
        return;
    }
    let root = root.to_string_lossy();
    let Some(path) = config_file(&root) else { return };
    let existing = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == ErrorKind::NotFound => String::new(),
        Err(_) => return,
    };
    let Some(config) = parse(&root, &existing) else { return };
    let _ = write_config(&path, &format_config(&root, list, &config.unknown));
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
}
