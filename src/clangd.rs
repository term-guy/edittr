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

/// A Makefile or included .mk file (flags may be inferred from these).
pub(crate) fn is_build_file(path: &Path) -> bool {
    match path.file_name().and_then(OsStr::to_str) {
        Some("Makefile" | "makefile" | "GNUmakefile") => true,
        Some(name) => name.ends_with(".mk"),
        None => false,
    }
}

pub(crate) fn database_event(root: &Path, path: &Path) -> bool {
    match path.file_name().and_then(OsStr::to_str) {
        Some(DATABASE | ".clangd") => true,
        Some(name) if matches!(name, "Makefile" | "makefile" | "GNUmakefile" | "compile_flags.txt") || name.ends_with(".mk") => {
            path.strip_prefix(root).ok().and_then(Path::parent).is_some_and(|parent| {
                parent.iter().all(|part| {
                    part != ".."
                        && part.to_str().is_none_or(|name| {
                            !matches!(name, ".git" | ".svn" | ".cache" | "node_modules" | "target") && !build_directory(name)
                        })
                })
            })
        }
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

/// Rewrite commands of compilers clangd can't use (IAR, TI, Keil, ...) as clang ones,
/// and append the workspace's extra flags.
/// Returns the compiler family when its command was converted.
fn translate_record(record: &mut Value, extra: &[String]) -> Option<crate::ccompilers::Family> {
    let args: Vec<String> = match (&record["arguments"], &record["command"]) {
        (Value::Array(a), _) => a.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect(),
        (_, Value::String(c)) => crate::compdb::split_words(c),
        _ => return None,
    };
    let file = record["file"].as_str().unwrap_or_default().to_owned();
    let translated = crate::ccompilers::translate(&args, &file);
    let family = translated
        .as_ref()
        .and_then(|_| args.iter().find_map(|a| crate::ccompilers::family(a)));
    if translated.is_none() && extra.is_empty() {
        return None;
    }
    let mut args = translated.unwrap_or(args);
    // Extra flags go before the source file / -c at the end when possible.
    let at = args.iter().rposition(|a| a == "-c").unwrap_or(args.len());
    args.splice(at..at, extra.iter().cloned());
    if let Some(map) = record.as_object_mut() {
        map.remove("command");
        map.insert("arguments".into(), Value::from(args));
    }
    family
}

#[cfg(test)]
fn filter_database(source: &Path, root: &Path, ignored: &[String], extra: &[String], records: Vec<Value>) -> Vec<Value> {
    filter_database_with(source, root, ignored, extra, records, &mut None)
}

fn filter_database_with(
    source: &Path,
    root: &Path,
    ignored: &[String],
    extra: &[String],
    mut records: Vec<Value>,
    converted: &mut Option<crate::ccompilers::Family>,
) -> Vec<Value> {
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
        if let Some(family) = translate_record(record, extra) {
            converted.get_or_insert(family);
        }
        true
    });
    records
}

/// Where clangd's compile flags come from, as shown in the status bar.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) enum FlagSource {
    /// No flags known: clangd guesses per open file.
    #[default]
    None,
    /// The project's compile_commands.json (filtered copy).
    Database(PathBuf),
    /// "Generate compile database" output, and whether build files changed since.
    Generated { stale: bool },
    /// clangd finds compile_flags.txt, nested databases or `.clangd` settings itself.
    Native(&'static str),
    /// Read from the Makefiles and #include paths.
    Inferred { partial: bool, makefiles: usize, guessed: usize },
}

impl FlagSource {
    pub(crate) fn label(&self, root: &Path) -> String {
        match self {
            Self::None => "no compile flags".into(),
            Self::Database(path) => {
                let rel = workspace::relative_path(root, path).unwrap_or_else(|| path.display().to_string());
                format!("flags: {rel}")
            }
            Self::Generated { stale: false } => "flags: generated database".into(),
            Self::Generated { stale: true } => "flags: generated database (build files changed)".into(),
            Self::Native(what) => format!("flags: {what}"),
            Self::Inferred {
                partial,
                makefiles,
                guessed,
            } => {
                let from = match (makefiles, guessed) {
                    (0, _) => "#include paths".to_owned(),
                    (_, 0) => "Makefiles".to_owned(),
                    _ => "Makefiles + #include paths".to_owned(),
                };
                format!("flags: inferred from {from}{}", if *partial { " (partial)" } else { "" })
            }
        }
    }
}

#[derive(Default)]
pub(crate) struct CompilationDatabase {
    pub(crate) source: Option<PathBuf>,
    pub(crate) target: Option<PathBuf>,
    /// Makefiles read for inferred flags: changes to them restart clangd.
    pub(crate) inputs: Vec<PathBuf>,
    pub(crate) flags: FlagSource,
    /// The compiler family whose commands were converted for clangd (IAR, TI, ...).
    pub(crate) converted: Option<crate::ccompilers::Family>,
}

impl CompilationDatabase {
    pub(crate) fn label(&self, root: &Path) -> String {
        let label = self.flags.label(root);
        match self.converted {
            Some(family) => format!("{label} · {}, converted for clangd", family.name()),
            None => label,
        }
    }
}

/// What a clangd launch needs from `CompilationDatabase::prepare`.
#[derive(Default)]
pub(crate) struct Prepared {
    /// `--compile-commands-dir`, when edittr wrote a database for clangd.
    pub(crate) dir: Option<PathBuf>,
    /// `--query-driver` for a GCC-compatible cross compiler.
    pub(crate) query_driver: Option<String>,
}

fn has_file(files: &[String], name: &str) -> bool {
    files.iter().any(|f| f == name || f.ends_with(&format!("/{name}")))
}

impl CompilationDatabase {
    pub(crate) fn find(root: &Path) -> Self {
        let mut source = database(root);
        let generated = source.is_none() && database_setting_disabled(root).is_none();
        if generated {
            source = crate::compdb::generated_database(root).filter(|p| p.is_file());
        }
        let target = source
            .as_ref()
            .and_then(|path| {
                path.canonicalize().ok().or_else(|| {
                    let target = path.read_link().ok()?;
                    Some(absolute(path.parent()?, &target))
                })
            })
            .filter(|path| Some(path) != source.as_ref());
        let flags = match &source {
            Some(_) if generated => FlagSource::Generated { stale: false },
            Some(path) => FlagSource::Database(path.clone()),
            None => FlagSource::None,
        };
        Self {
            source,
            target,
            inputs: Vec::new(),
            flags,
            converted: None,
        }
    }

    pub(crate) fn paths(&self) -> impl Iterator<Item = &PathBuf> {
        self.source.iter().chain(&self.target).chain(&self.inputs)
    }

    /// Decide clangd's flags (database, generated, clangd's own discovery, or
    /// inferred from the Makefiles) and write the database clangd should use.
    pub(crate) fn prepare(
        &mut self,
        root: &Path,
        ignored: &[String],
        settings: &workspace::CSettings,
        files: impl FnOnce() -> Vec<String>,
        cancelled: impl Fn() -> bool,
    ) -> io::Result<Prepared> {
        // A superseded launch must never overwrite the new launch's copy.
        static PREPARING: Mutex<()> = Mutex::new(());
        let _preparing = PREPARING.lock().unwrap();
        let mut prepared = Prepared::default();
        if cancelled() {
            return Ok(prepared);
        }
        let dir =
            crate::compdb::cache_dir(root).ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "edittr cache directory unavailable"))?;
        let extra = crate::compdb::split_words(&settings.flags);
        let mut compiler = None;
        if let Some(source) = self.source.clone() {
            if let Some((written, records, converted)) = copy_database(&source, root, ignored, &extra, &dir)? {
                prepared.dir = Some(written);
                compiler = crate::makeflags::database_compiler(&records);
                self.converted = converted;
            }
        } else if database_setting_disabled(root).is_some() {
            self.flags = FlagSource::Native(".clangd settings");
        } else {
            let files = files();
            let native = if has_file(&files, "compile_flags.txt") || root.join("compile_flags.txt").is_file() {
                Some("compile_flags.txt")
            } else if has_file(&files, DATABASE) {
                Some("nested compile_commands.json")
            } else {
                None
            };
            if let Some(what) = native {
                // clangd looks these up next to each file; --compile-commands-dir would hide them.
                self.flags = FlagSource::Native(what);
            } else if let Some(inferred) = crate::makeflags::infer(root, ignored, &files, &cancelled) {
                let mut records = crate::makeflags::synthesize_database(root, &inferred, &files, ignored);
                if !records.is_empty() && !cancelled() {
                    for record in &mut records {
                        if let Some(family) = translate_record(record, &extra) {
                            self.converted.get_or_insert(family);
                        }
                    }
                    write_database(&dir, &records)?;
                    prepared.dir = Some(dir.clone());
                    self.flags = FlagSource::Inferred {
                        partial: inferred.partial,
                        makefiles: inferred.inputs.len(),
                        guessed: inferred.guessed_includes,
                    };
                    self.inputs = inferred.inputs.clone();
                    compiler = inferred.compiler.clone().or_else(|| inferred.cxx_compiler.clone());
                }
            }
        }
        let compiler = Some(settings.compiler.trim())
            .filter(|c| !c.is_empty())
            .map(str::to_owned)
            .or(compiler);
        prepared.query_driver = compiler
            .filter(|c| crate::ccompilers::family(c) == Some(crate::ccompilers::Family::Gcc))
            .and_then(|c| crate::makeflags::query_driver(&c));
        Ok(prepared)
    }
}

fn database_setting_disabled(root: &Path) -> Option<()> {
    matches!(root.ancestors().find_map(database_setting), Some(DatabaseSetting::Disabled)).then_some(())
}

fn write_database(dir: &Path, records: &[Value]) -> io::Result<()> {
    let path = dir.join(DATABASE);
    let json = serde_json::to_string(records)?;
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
    Ok(())
}

/// The folder clangd reads, the records written there, and the converted compiler family.
type Copied = (PathBuf, Vec<Value>, Option<crate::ccompilers::Family>);

/// Filtered (and translated) copy of `source` in `dir`.
fn copy_database(source: &Path, root: &Path, ignored: &[String], extra: &[String], dir: &Path) -> io::Result<Option<Copied>> {
    let input = match fs::read_to_string(source) {
        Ok(input) => input,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    let mut converted = None;
    let records = filter_database_with(source, root, ignored, extra, serde_json::from_str(&input)?, &mut converted);
    write_database(dir, &records)?;
    Ok(Some((dir.to_path_buf(), records, converted)))
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
    fn recognizes_make_inputs_outside_generated_and_metadata_directories() {
        let root = Path::new("project");
        for path in [
            "Makefile",
            "sub/makefile",
            "sub/GNUmakefile",
            "config/options.mk",
            "sub/compile_flags.txt",
        ] {
            assert!(database_event(root, &root.join(path)), "{path}");
        }
        for dir in [
            ".git",
            ".svn",
            ".cache",
            "node_modules",
            "target",
            "build-debug",
            "cmake-build-release",
            "out",
        ] {
            let path = root.join("nested").join(dir).join("flags.mk");
            assert!(!database_event(root, &path), "{path:?}");
        }
        assert!(!database_event(root, Path::new("other/Makefile")));
        assert!(!database_event(root, &root.join("../other/Makefile")));
        assert!(!database_event(root, &root.join("src/main.c")));
        assert!(database_event(root, &root.join("build/compile_commands.json")));
    }

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
        let filtered = filter_database(&source, &root, &["vendor".into()], &[], records.clone());
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
        assert_eq!(
            copy_database(&original, &root, &ignored, &[], &cache).unwrap().map(|(dir, ..)| dir),
            Some(cache.clone())
        );
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

    /// Prepare flags the way the app does and start clangd with them.
    fn start_prepared(
        root: &Path,
        files: Vec<String>,
        settings: &workspace::CSettings,
    ) -> (
        CompilationDatabase,
        std::sync::Arc<crate::lsp::Client>,
        std::sync::mpsc::Receiver<()>,
    ) {
        use crate::lsp;
        let mut database = CompilationDatabase::find(root);
        let prepared = database.prepare(root, &[], settings, || files, || false).unwrap();
        let spec = lsp::servers_for("c").remove(0);
        let mut launch = lsp::resolve(spec.command).expect("clangd is not installed");
        if let Some(dir) = prepared.dir {
            launch.args.push(format!("--compile-commands-dir={}", dir.display()));
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let client = lsp::Client::start(
            &spec,
            launch,
            root,
            &[],
            Some(std::sync::Arc::new(move || {
                let _ = tx.send(());
            })),
        )
        .unwrap();
        client.wait_ready(std::time::Duration::from_secs(10)).unwrap();
        (database, client, rx)
    }

    fn errors_after_open(client: &crate::lsp::Client, rx: &std::sync::mpsc::Receiver<()>, path: &Path, text: &str) -> Vec<String> {
        let start = std::time::Instant::now();
        client.sync_ready(path, text, (0, 0));
        while client.diagnostics_generation() == 0 {
            rx.recv_timeout(std::time::Duration::from_secs(15).saturating_sub(start.elapsed()))
                .expect("no diagnostics");
        }
        client
            .diagnostics_snapshot()
            .get(path)
            .map(|list| {
                list.iter()
                    .filter(|d| d.severity == crate::lsp::Severity::Error)
                    .map(|d| d.message.clone())
                    // Each fixture ends with one deliberate error, so diagnostics are always published.
                    .filter(|m| !m.contains("edittr_marker"))
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    #[ignore = "requires clangd on PATH"]
    fn lsp_round_trip_clangd_flags_inferred_from_makefile() {
        use std::time::{Duration, Instant};
        let root = std::env::temp_dir().join(format!("edittr-clangd-make-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        for dir in ["inc", "src", "drivers/uart"] {
            fs::create_dir_all(root.join(dir)).unwrap();
        }
        let root = root.canonicalize().unwrap();
        fs::write(
            root.join("Makefile"),
            "include config.mk\nCFLAGS += -Iinc -DFEATURE=1\nall:\n\t$(CC) $(CFLAGS) -c src/main.c\n",
        )
        .unwrap();
        fs::write(root.join("config.mk"), "CPPFLAGS := -DBOARD_REV=3\n").unwrap();
        fs::write(root.join("inc/util.h"), "int helper(void);\n").unwrap();
        // Only found through the #include heuristic: "uart.h" lives in drivers/uart.
        fs::write(root.join("drivers/uart/uart.h"), "int uart_send(int);\n").unwrap();
        fs::write(
            root.join("src/util.c"),
            "#include \"util.h\"\nint helper(void) { return 1; }\nint not_open_anywhere(void) { return 2; }\n",
        )
        .unwrap();
        let main = "#include \"util.h\"\n#include \"uart.h\"\n#if FEATURE && BOARD_REV == 3\nint main(void) { return helper() + uart_send(1); }\n#endif\nint m = edittr_marker;\n";
        fs::write(root.join("src/main.c"), main).unwrap();
        let files = [
            "Makefile",
            "config.mk",
            "inc/util.h",
            "drivers/uart/uart.h",
            "src/util.c",
            "src/main.c",
        ]
        .map(String::from)
        .to_vec();

        let (database, client, rx) = start_prepared(&root, files, &workspace::CSettings::default());
        assert!(
            matches!(
                database.flags,
                FlagSource::Inferred {
                    makefiles: 2,
                    guessed: 1,
                    ..
                }
            ),
            "{:?}",
            database.flags
        );
        assert_eq!(database.inputs.len(), 2);
        let errors = errors_after_open(&client, &rx, &root.join("src/main.c"), main);
        assert!(errors.is_empty(), "{errors:?}");
        // Background index over the synthesized database: a function in a file never opened.
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let found = client
                .workspace_symbols("not_open_anywhere", Duration::from_secs(3))
                .unwrap_or_default();
            if found.iter().any(|s| s.name == "not_open_anywhere") {
                break;
            }
            assert!(Instant::now() < deadline, "clangd did not index the inferred project");
            std::thread::sleep(Duration::from_millis(200));
        }
        drop(client);
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    #[ignore = "requires clangd on PATH"]
    fn lsp_round_trip_clangd_iar_project_is_converted() {
        let base = std::env::temp_dir().join(format!("edittr-clangd-iar-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let iar = base.join("IAR/arm");
        let root = base.join("fw");
        for dir in [iar.join("bin"), iar.join("inc/c"), root.join("inc")] {
            fs::create_dir_all(dir).unwrap();
        }
        let root = root.canonicalize().unwrap();
        let iccarm = iar.join("bin/iccarm");
        fs::write(&iccarm, "").unwrap();
        // IAR's own C library header, which the host libc doesn't have.
        fs::write(iar.join("inc/c/intrinsics.h"), "void __enable_interrupt(void);\n").unwrap();
        fs::write(root.join("inc/board.h"), "#define LED 5\n").unwrap();
        let main = "#include <intrinsics.h>\n#include \"board.h\"\n#ifndef __ICCARM__\n#error not IAR\n#endif\n\
                    __no_init int keep;\n__ramfunc void fast(void) {}\n__interrupt void irq(void) { __enable_interrupt(); }\n\
                    int main(void) { return LED + PART; }\nint m = edittr_marker;\n";
        fs::write(root.join("main.c"), main).unwrap();
        let commands = json!([{
            "directory": root,
            "file": root.join("main.c"),
            "command": format!("{} main.c -e --cpu=Cortex-M4 --dlib_config normal.h -I inc -DPART=7 -o obj/", iccarm.display()),
        }]);
        fs::write(root.join(DATABASE), commands.to_string()).unwrap();

        let (database, client, rx) = start_prepared(&root, vec!["main.c".into(), "inc/board.h".into()], &workspace::CSettings::default());
        assert_eq!(database.converted, Some(crate::ccompilers::Family::Iar));
        let errors = errors_after_open(&client, &rx, &root.join("main.c"), main);
        assert!(errors.is_empty(), "{errors:?}");
        drop(client);
        fs::remove_dir_all(&base).ok();
    }
}
