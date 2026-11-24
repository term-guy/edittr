use crate::workspace;
use serde_json::Value;
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;

pub(crate) const ARGS: &[&str] = &[
    "--background-index",
    "--header-insertion=never",
    "--pch-storage=memory",
    "--clang-tidy=false",
];
pub(crate) const DATABASE: &str = "compile_commands.json";
const SOURCES: &[&str] = &["c", "cc", "cpp", "cxx", "m", "mm"];
const HEADERS: &[&str] = &["h", "hh", "hpp", "hxx", "inl"];

pub(crate) fn language(ext: &str) -> Option<&'static str> {
    match ext {
        "c" | "h" => Some("c"),
        "m" => Some("objective-c"),
        "mm" => Some("objective-cpp"),
        ext if SOURCES.contains(&ext) || HEADERS.contains(&ext) => Some("cpp"),
        _ => None,
    }
}

fn absolute(base: &Path, path: &Path) -> PathBuf {
    let joined = base.join(path);
    let mut normalized = PathBuf::new();
    for part in joined.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir if normalized.file_name().is_some() => {
                normalized.pop();
            }
            Component::ParentDir if normalized.has_root() => {}
            _ => normalized.push(part),
        }
    }
    normalized
}

enum DatabaseSetting {
    Disabled,
    Ancestors,
    Directory(PathBuf),
}

fn database_setting(root: &Path) -> Option<DatabaseSetting> {
    let config = fs::read_to_string(root.join(".clangd")).ok()?;
    let fragments = yaml_rust::YamlLoader::load_from_str(&config).ok()?;
    fragments
        .iter()
        .rev()
        .filter(|fragment| fragment["If"].is_badvalue() || fragment["If"].as_hash().is_some_and(|map| map.is_empty()))
        .find_map(|fragment| fragment["CompileFlags"]["CompilationDatabase"].as_str())
        .map(|dir| match dir {
            "None" => DatabaseSetting::Disabled,
            "Ancestors" => DatabaseSetting::Ancestors,
            dir => DatabaseSetting::Directory(absolute(root, Path::new(dir))),
        })
}

fn build_directory(name: &str) -> bool {
    name == "out" || name.starts_with("build") || name.starts_with("cmake-build-")
}

pub(crate) fn database_event(root: &Path, path: &Path) -> bool {
    match path.file_name().and_then(OsStr::to_str) {
        Some(DATABASE | ".clangd") => true,
        Some(name) if path.parent() == Some(root) => build_directory(name),
        _ => false,
    }
}

fn database_candidate(path: &Path) -> bool {
    path.is_file() || path.is_symlink()
}

/// Returns both the selected database and the configured path to monitor if it is missing.
fn database(root: &Path) -> Option<PathBuf> {
    match root.ancestors().find_map(database_setting) {
        Some(DatabaseSetting::Disabled) => return None,
        Some(DatabaseSetting::Directory(dir)) => return Some(dir.join(DATABASE)),
        Some(DatabaseSetting::Ancestors) | None => {}
    }
    for dir in root.ancestors() {
        for candidate in [dir.join(DATABASE), dir.join("build").join(DATABASE)] {
            if database_candidate(&candidate) {
                return Some(candidate);
            }
        }
        let found = fs::read_dir(dir)
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_str().is_some_and(build_directory))
            .map(|entry| entry.path().join(DATABASE))
            .filter(|path| database_candidate(path))
            .min();
        if found.is_some() {
            return found;
        }
    }
    None
}

fn filter_database(source: &Path, root: &Path, ignored: &[String], mut records: Vec<Value>) -> Vec<Value> {
    let canonical_root = (!ignored.is_empty()).then(|| root.canonicalize().ok()).flatten();
    records.retain_mut(|record| {
        let Some(directory) = record["directory"].as_str().map(Path::new) else {
            return true;
        };
        let Some(file) = record["file"].as_str() else { return true };
        let relative_directory = !directory.is_absolute();
        let directory = absolute(source.parent().unwrap_or(root), directory);
        let file = absolute(&directory, Path::new(file));
        let excluded = |root, path| workspace::relative_path(root, path).is_some_and(|rel| workspace::is_ignored(&rel, ignored));
        if excluded(root, &file)
            || canonical_root
                .as_deref()
                .zip(file.canonicalize().ok().as_deref())
                .is_some_and(|(root, path)| excluded(root, path))
        {
            return false;
        }
        // Keep command arguments unchanged, but make their working directory
        // independent of where the copied database lives.
        if relative_directory {
            record["directory"] = Value::String(directory.to_string_lossy().into_owned());
        }
        true
    });
    records
}

#[derive(Default)]
pub(crate) struct CompilationDatabase {
    pub(crate) source: Option<PathBuf>,
    pub(crate) target: Option<PathBuf>,
}

impl CompilationDatabase {
    pub(crate) fn find(root: &Path) -> Self {
        let source = database(root);
        let target = source
            .as_ref()
            .and_then(|path| {
                path.canonicalize().ok().or_else(|| {
                    let target = path.read_link().ok()?;
                    Some(absolute(path.parent()?, &target))
                })
            })
            .filter(|path| Some(path) != source.as_ref());
        Self { source, target }
    }

    pub(crate) fn paths(&self) -> impl Iterator<Item = &PathBuf> {
        self.source.iter().chain(&self.target)
    }

    pub(crate) fn prepare(&self, root: &Path, ignored: &[String], cancelled: impl FnOnce() -> bool) -> io::Result<Option<PathBuf>> {
        let Some(source) = &self.source else { return Ok(None) };
        // A superseded launch must never overwrite the new launch's copy.
        static PREPARING: Mutex<()> = Mutex::new(());
        let _preparing = PREPARING.lock().unwrap();
        if cancelled() {
            return Ok(None);
        }
        let dir = dirs::cache_dir()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "edittr cache directory unavailable"))?
            .join("edittr/clangd")
            .join(workspace::root_hash(root));
        copy_database(source, root, ignored, &dir)
    }
}

fn copy_database(source: &Path, root: &Path, ignored: &[String], dir: &Path) -> io::Result<Option<PathBuf>> {
    let input = match fs::read_to_string(source) {
        Ok(input) => input,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    let records = filter_database(source, root, ignored, serde_json::from_str(&input)?);
    let path = dir.join(DATABASE);
    let json = serde_json::to_string(&records)?;
    if fs::read_to_string(&path).ok().as_deref() != Some(&json) {
        let mut directory = fs::DirBuilder::new();
        directory.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            directory.mode(0o700);
        }
        directory.create(dir)?;
        workspace::write_config(&path, &json)?;
    }
    Ok(Some(dir.to_path_buf()))
}

pub(crate) fn counterpart(path: &Path, root: Option<&Path>, ignored: &[String], files: impl FnOnce() -> Vec<String>) -> Option<PathBuf> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    let (extensions, sibling) = if SOURCES.contains(&ext.as_str()) {
        (HEADERS, "include")
    } else if HEADERS.contains(&ext.as_str()) {
        (SOURCES, "src")
    } else {
        return None;
    };
    let visible = |candidate: &Path| {
        !root
            .and_then(|root| workspace::relative_path(root, candidate))
            .is_some_and(|rel| workspace::is_ignored(&rel, ignored))
            && candidate.is_file()
    };
    let parent = path.parent()?;
    let name = path.file_name()?;
    let sibling_dirs = parent
        .ancestors()
        .skip(1)
        .take(if root.is_some() { usize::MAX } else { 1 })
        .take_while(|dir| root.is_none_or(|root| dir.starts_with(root)))
        .map(|dir| dir.join(sibling))
        .chain(std::iter::once(parent.join(sibling)));
    for dir in std::iter::once(parent.to_owned()).chain(sibling_dirs) {
        for extension in extensions {
            let candidate = dir.join(name).with_extension(extension);
            if visible(&candidate) {
                return Some(candidate);
            }
        }
    }
    let root = root?;
    let mut files = files();
    files.sort_unstable();
    files.into_iter().map(|rel| root.join(rel)).find(|candidate| {
        candidate.file_stem() == path.file_stem()
            && candidate
                .extension()
                .and_then(OsStr::to_str)
                .is_some_and(|ext| extensions.contains(&ext.to_ascii_lowercase().as_str()))
            && visible(candidate)
    })
}

pub(crate) fn is_index_path(path: &Path) -> bool {
    let mut previous = None;
    for component in path.iter() {
        if component == "clangd" && previous == Some(OsStr::new(".cache")) {
            return true;
        }
        previous = Some(component);
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn filters_compilation_database_paths_and_preserves_commands() {
        let root = std::env::temp_dir().join("edittr-compilation-database-fixture");
        let source = root.join("build").join(DATABASE);
        let records = vec![
            json!({"directory": root, "file": root.join("vendor/one.cc"), "command": "clang++ -c vendor/one.cc"}),
            json!({"directory": "../src", "file": "../vendor/two.c", "arguments": ["clang", "-c", "../vendor/two.c"]}),
            json!({"directory": root, "file": "vendorized/main.cc", "arguments": ["clang++", "-c", "vendorized/main.cc"]}),
            json!({"directory": "../src", "file": "./main.cc", "arguments": ["clang++", "-c", "./main.cc"], "output": "main.o"}),
            json!({"directory": root.parent().unwrap(), "file": "vendor/external.cc", "command": "clang++ -c vendor/external.cc"}),
        ];
        let filtered = filter_database(&source, &root, &["vendor".into()], records.clone());
        assert_eq!(filtered.len(), 3);
        assert_eq!(filtered[0], records[2]);
        assert_eq!(
            filtered[1],
            json!({"directory": root.join("src"), "file": "./main.cc", "arguments": ["clang++", "-c", "./main.cc"], "output": "main.o"})
        );
        assert_eq!(filtered[2], records[4]);
    }

    #[test]
    fn discovers_build_directories_and_configured_database() {
        let root = std::env::temp_dir().join(format!("edittr-clangd-discovery-{}", std::process::id()));
        for name in ["build", "build-debug", "out", "cmake-build-release"] {
            let dir = root.join(name);
            fs::create_dir_all(&dir).unwrap();
            let path = dir.join(DATABASE);
            fs::write(&path, "[]").unwrap();
            assert_eq!(database(&root), Some(path));
            fs::remove_dir_all(dir).unwrap();
        }
        let direct = root.join(DATABASE);
        fs::write(&direct, "[]").unwrap();
        assert_eq!(database(&root), Some(direct));
        fs::write(
            root.join(".clangd"),
            "CompileFlags: { CompilationDatabase: 'custom build' }\n---\nIndex: { Background: Build }\n",
        )
        .unwrap();
        assert_eq!(database(&root), Some(root.join("custom build").join(DATABASE)));
        fs::write(root.join(".clangd"), "CompileFlags: { CompilationDatabase: None }\n").unwrap();
        assert_eq!(database(&root), None);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn counterpart_prefers_local_then_sibling_then_project_files() {
        let root = std::env::temp_dir().join(format!("edittr-counterpart-{}", std::process::id()));
        for dir in ["src", "include", "other"] {
            fs::create_dir_all(root.join(dir)).unwrap();
        }
        let source = root.join("src/item.mm");
        let local = root.join("src/item.hh");
        let sibling = root.join("include/item.hpp");
        let distant = root.join("other/item.inl");
        for path in [&source, &local, &sibling, &distant] {
            fs::write(path, "").unwrap();
        }
        let files = || vec!["other/item.inl".to_owned()];
        assert_eq!(counterpart(&source, Some(&root), &[], files), Some(local.clone()));
        fs::remove_file(local).unwrap();
        assert_eq!(counterpart(&source, Some(&root), &[], files), Some(sibling.clone()));
        assert_eq!(counterpart(&sibling, Some(&root), &[], files), Some(source.clone()));
        assert_eq!(counterpart(&source, Some(&root), &["include".into()], files), Some(distant));
        assert_eq!(counterpart(&source, Some(&root), &["include".into(), "other".into()], files), None);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[ignore = "requires clangd on PATH"]
    fn lsp_round_trip_clangd_filtered_database_and_counterpart() {
        use crate::lsp;
        use std::time::{Duration, Instant};

        let fixture = std::env::temp_dir().join(format!("edittr-clangd-filtered-{}", std::process::id()));
        let root = fixture.join("project");
        let cache = fixture.join("cache");
        fs::create_dir_all(root.join("vendor")).unwrap();
        let root = root.canonicalize().unwrap();
        let source = root.join("main.cpp");
        let header = root.join("main.h");
        let skipped = root.join("vendor/ignored.cpp");
        let text = "#include \"main.h\"\nint main() { return kept_function(); }\n";
        fs::write(&source, text).unwrap();
        fs::write(&header, "inline int kept_function() { return 42; }\n").unwrap();
        fs::write(&skipped, "int ignored_function() { return 0; }\n").unwrap();
        let original = root.join(DATABASE);
        let commands = json!([
            {"directory": root, "file": source, "arguments": ["clang++", "-c", source]},
            {"directory": root, "file": skipped, "arguments": ["clang++", "-c", skipped]},
        ]);
        fs::write(&original, commands.to_string()).unwrap();
        let ignored = ["vendor".into()];
        assert_eq!(copy_database(&original, &root, &ignored, &cache).unwrap(), Some(cache.clone()));
        let spec = lsp::servers_for("cpp").remove(0);
        let mut launch = lsp::resolve(spec.command).expect("clangd is not installed");
        launch.args.push(format!("--compile-commands-dir={}", cache.display()));
        let client = lsp::Client::start(&spec, launch, &root, &ignored, None).unwrap();
        let timeout = Duration::from_secs(3);
        assert_eq!(client.switch_source_header(&source, text, (1, 0), timeout), Some(header.clone()));
        let index = cache.join(".cache/clangd/index");
        let deadline = Instant::now() + timeout;
        while !fs::read_dir(&index).is_ok_and(|mut files| files.any(|entry| entry.is_ok())) {
            assert!(Instant::now() < deadline, "clangd did not persist its background index");
            std::thread::sleep(Duration::from_millis(25));
        }
        let symbols = client.workspace_symbols("kept_function", timeout).unwrap();
        assert!(symbols.iter().any(|symbol| symbol.name == "kept_function"));
        assert!(client.workspace_symbols("ignored_function", timeout).unwrap().is_empty());
        assert!(!root.join(".cache/clangd").exists());
        assert_eq!(fs::read_to_string(&original).unwrap(), commands.to_string());
        drop(client);
        fs::remove_dir_all(fixture).unwrap();
    }
}
