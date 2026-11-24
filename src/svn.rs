//! Subversion CLI and its local cache. These operations belong on background threads.

use crate::vcs::{BlameLine, Commit, FileChange, Status};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

fn command(root: &Path, program: &str) -> Command {
    let mut cmd = Command::new(program);
    cmd.current_dir(root);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

fn svn(root: &Path, args: &[&str]) -> Command {
    let mut cmd = command(root, "svn");
    cmd.arg("--non-interactive").args(args);
    cmd
}

fn failed(command: &str, status: ExitStatus, stderr: &[u8]) -> String {
    let err = String::from_utf8_lossy(stderr);
    if err.trim().is_empty() {
        format!("svn {command} exited with {status}")
    } else {
        err.trim().into()
    }
}

fn run_bytes(root: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let output = svn(root, args).output().map_err(|e| format!("failed to run svn: {e}"))?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(failed(args[0], output.status, &output.stderr))
    }
}

fn run(root: &Path, args: &[&str]) -> Result<String, String> {
    run_bytes(root, args).map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
}

fn run_xml<T: DeserializeOwned>(root: &Path, args: &[&str]) -> Result<T, String> {
    let mut child = svn(root, args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| format!("failed to run svn: {err}"))?;
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut stderr = child.stderr.take().unwrap();
    std::thread::scope(|scope| {
        let errors = scope.spawn(move || {
            let mut bytes = Vec::new();
            let _ = stderr.read_to_end(&mut bytes);
            bytes
        });
        let result = quick_xml::de::from_reader(&mut stdout).map_err(|err| format!("Invalid svn XML: {err}"));
        // Even on a parse error, drain the pipe so SVN can exit and report diagnostics.
        let _ = io::copy(&mut stdout, &mut io::sink());
        let status = child.wait().map_err(|err| err.to_string())?;
        if status.success() {
            result
        } else {
            Err(failed(args[0], status, &errors.join().unwrap()))
        }
    })
}

/// Escape a literal @ in an SVN target (otherwise the suffix is a peg revision).
fn target(path: &str) -> String {
    if path.contains('@') { format!("{path}@") } else { path.into() }
}

/// The pristine (BASE) text of a file: read from the working copy, no server access.
pub fn cat_base(root: &Path, rel: &str) -> Result<String, String> {
    run(root, &["cat", "-r", "BASE", "--", &target(rel)])
}

pub fn repo_root(dir: &Path) -> Option<PathBuf> {
    // Since svn 1.7 a working copy has a single `.svn` at its root: no process needed.
    dir.ancestors().find(|p| p.join(".svn").is_dir()).map(Path::to_path_buf)
}

#[cfg(test)]
fn xml<T: DeserializeOwned>(output: &str) -> Result<T, String> {
    quick_xml::de::from_str(output).map_err(|e| format!("Invalid svn XML: {e}"))
}

#[derive(Deserialize)]
struct Info {
    entry: InfoEntry,
}

#[derive(Deserialize)]
#[serde(rename_all = "kebab-case")]
struct InfoEntry {
    url: String,
    #[serde(default)]
    relative_url: String,
    repository: Repository,
    commit: Option<Revision>,
}

#[derive(Deserialize)]
struct Repository {
    root: String,
    #[serde(default)]
    uuid: String,
}

impl InfoEntry {
    fn branch(&self) -> &str {
        let branch = self
            .relative_url
            .strip_prefix("^/")
            .or_else(|| self.url.strip_prefix(&self.repository.root).map(|p| p.trim_start_matches('/')));
        branch.filter(|p| !p.is_empty()).unwrap_or("/")
    }

    fn cache_dir(&self) -> Option<PathBuf> {
        let repo = &self.repository;
        let identity = if repo.uuid.is_empty() { &repo.root } else { &repo.uuid };
        Some(cache_root()?.join(cache_key(identity)))
    }

    fn history_file(&self) -> Option<PathBuf> {
        Some(self.cache_dir()?.join(format!("log-{}.json", cache_key(self.branch()))))
    }
}

fn info(root: &Path, rel: &str) -> Result<InfoEntry, String> {
    Ok(run_xml::<Info>(root, &["info", "--xml", "--", &target(rel)])?.entry)
}

#[derive(Deserialize)]
struct StatusXml {
    #[serde(default)]
    target: Vec<StatusTarget>,
    #[serde(default)]
    changelist: Vec<StatusTarget>,
}

#[derive(Deserialize)]
struct StatusTarget {
    #[serde(rename = "entry", default)]
    entries: Vec<StatusEntry>,
}

#[derive(Deserialize)]
struct StatusEntry {
    #[serde(rename = "@path")]
    path: String,
    #[serde(rename = "wc-status")]
    status: WcStatus,
}

#[derive(Deserialize)]
struct WcStatus {
    #[serde(rename = "@item")]
    item: String,
    #[serde(rename = "@props", default)]
    props: String,
    #[serde(rename = "@tree-conflicted", default)]
    tree_conflicted: bool,
    #[serde(rename = "@moved-from", default)]
    moved_from: Option<String>,
    #[serde(rename = "@moved-to", default)]
    moved_to: Option<String>,
    #[serde(rename = "@file-external", default)]
    file_external: bool,
}

impl WcStatus {
    fn code(&self) -> Option<char> {
        if self.file_external {
            return None;
        }
        if self.tree_conflicted || self.item == "conflicted" || self.props == "conflicted" {
            return Some('U');
        }
        match self.item.as_str() {
            "unversioned" => Some('?'),
            "added" => Some('A'),
            "deleted" | "missing" => Some('D'),
            "replaced" => Some('R'),
            "modified" | "incomplete" => Some('M'),
            "obstructed" => Some('U'),
            "normal" if self.props == "modified" => Some('M'),
            _ => None,
        }
    }
}

impl StatusXml {
    fn entries(self) -> impl Iterator<Item = StatusEntry> {
        self.target
            .into_iter()
            .chain(self.changelist)
            .flat_map(|group| group.entries)
            .map(|mut entry| {
                if cfg!(windows) {
                    entry.path = entry.path.replace('\\', "/");
                    for path in [&mut entry.status.moved_from, &mut entry.status.moved_to].into_iter().flatten() {
                        *path = path.replace('\\', "/");
                    }
                }
                entry
            })
    }
}

fn changes(root: &Path, rel: &str, depth: &str) -> Result<StatusXml, String> {
    run_xml(
        root,
        &["status", "--xml", "--ignore-externals", "--depth", depth, "--", &target(rel)],
    )
}

pub fn list_files(root: &Path) -> Option<Vec<String>> {
    let status: StatusXml = run_xml(root, &["status", "--xml", "--verbose", "--ignore-externals", "--depth", "infinity"]).ok()?;
    let mut files = Vec::new();
    let mut unversioned_dirs = Vec::new();
    for entry in status.entries() {
        if matches!(entry.status.item.as_str(), "deleted" | "missing" | "external") {
            continue;
        }
        let path = root.join(&entry.path);
        let Ok(meta) = path.symlink_metadata() else { continue };
        if !meta.is_dir() {
            files.push(entry.path);
        } else if entry.status.item == "unversioned" {
            unversioned_dirs.push(path);
        }
    }
    // SVN summarizes an unversioned directory with one entry. Visit its files locally.
    while let Some(dir) = unversioned_dirs.pop() {
        if dir
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| crate::watch::IGNORED_DIRS.contains(&name))
        {
            continue;
        }
        let Ok(entries) = fs::read_dir(dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                unversioned_dirs.push(path);
            } else if let Some(rel) = crate::workspace::relative_path(root, &path) {
                files.push(rel);
            }
        }
    }
    files.sort_unstable();
    files.dedup();
    Some(files)
}

pub fn status(root: &Path) -> Result<Status, String> {
    Ok(Status {
        branch: info(root, ".")?.branch().into(),
        unstaged: changes(root, ".", "infinity")?
            .entries()
            .filter_map(|e| {
                Some(FileChange {
                    code: e.status.code()?,
                    path: e.path,
                })
            })
            .collect(),
        ..Status::default()
    })
}

pub fn diff(root: &Path, change: &FileChange) -> String {
    let path = &change.path;
    if change.code == '?' {
        return new_file_diff(root, path).unwrap_or_else(|e| e.to_string());
    }
    // Working-copy diff uses literal paths and does not strip a peg suffix.
    let diff = run(
        root,
        &["diff", "--internal-diff", "--show-copies-as-adds", "-x", "-U1000000", "--", path],
    );
    // SVN omits files removed outside of `svn delete` from its working-copy diff.
    if change.code == 'D'
        && diff.as_ref().map_or(true, String::is_empty)
        && let Ok(base) = run_bytes(root, &["cat", "-r", "BASE", "--", &target(path)])
    {
        return one_sided_diff(path, &base, false);
    }
    diff.unwrap_or_else(|e| e)
}

fn new_file_diff(root: &Path, path: &str) -> std::io::Result<String> {
    let file = root.join(path);
    let meta = file.symlink_metadata()?;
    if meta.is_dir() {
        return Ok(format!("Unversioned directory: {path}"));
    }
    let bytes = if meta.is_symlink() {
        fs::read_link(&file)?.to_string_lossy().as_bytes().to_vec()
    } else {
        fs::read(file)?
    };
    Ok(one_sided_diff(path, &bytes, true))
}

fn one_sided_diff(path: &str, bytes: &[u8], added: bool) -> String {
    let (old, new, marker) = if added {
        ("/dev/null", path, '+')
    } else {
        (path, "/dev/null", '-')
    };
    let text = match std::str::from_utf8(bytes) {
        Ok(text) if !text.contains('\0') => text,
        _ => return format!("Binary files {old} and {new} differ"),
    };
    let mut diff = format!("--- {old}\n+++ {new}\n");
    if !text.is_empty() {
        let count = text.lines().count();
        if added {
            writeln!(diff, "@@ -0,0 +1,{count} @@").unwrap();
        } else {
            writeln!(diff, "@@ -1,{count} +0,0 @@").unwrap();
        }
        for line in text.lines() {
            diff.push(marker);
            diff.push_str(line);
            diff.push('\n');
        }
        if !text.ends_with('\n') {
            diff.push_str("\\ No newline at end of file\n");
        }
    }
    diff
}

pub fn discard(root: &Path, change: &FileChange) -> Result<String, String> {
    if change.code == '?' {
        let rel = Path::new(&change.path);
        if rel.as_os_str().is_empty() || !rel.components().all(|part| matches!(part, std::path::Component::Normal(_))) {
            return Err(format!("Invalid working-copy path: {}", change.path));
        }
        let path = root.join(rel);
        let canonical_root = root.canonicalize().map_err(|err| err.to_string())?;
        let parent = path.parent().unwrap().canonicalize().map_err(|err| err.to_string())?;
        if !parent.starts_with(&canonical_root) {
            return Err(format!("Path is outside the working copy: {}", change.path));
        }
        if !changes(root, &change.path, "empty")?
            .entries()
            .any(|entry| entry.status.item == "unversioned")
        {
            return Err(format!("Path is no longer unversioned: {}", change.path));
        }
        let result = if path.symlink_metadata().is_ok_and(|m| m.is_dir()) {
            fs::remove_dir_all(path)
        } else {
            fs::remove_file(path)
        };
        result.map(|()| String::new()).map_err(|e| e.to_string())
    } else {
        let mut args = vec!["revert"];
        if matches!(change.code, 'A' | 'D' | 'R') {
            args.extend(["--depth", "infinity"]);
        }
        let other = if matches!(change.code, 'A' | 'D') {
            changes(root, &change.path, "empty")?
                .entries()
                .find_map(|entry| entry.status.moved_from.or(entry.status.moved_to))
                .map(|path| {
                    // Status reports moves relative to the status target itself, even for a file.
                    let joined = Path::new(&change.path).join(path);
                    let mut path = PathBuf::new();
                    for part in joined.components() {
                        if part == std::path::Component::ParentDir {
                            path.pop();
                        } else {
                            path.push(part);
                        }
                    }
                    target(&path.to_string_lossy())
                })
        } else {
            None
        };
        let path = target(&change.path);
        args.extend(["--", &path]);
        if let Some(other) = &other {
            args.push(other);
        }
        run(root, &args)
    }
}

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> io::Result<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let builder = DirBuilder::new();
        #[cfg(unix)]
        let builder = {
            use std::os::unix::fs::DirBuilderExt;
            let mut builder = builder;
            builder.mode(0o700);
            builder
        };
        loop {
            let unique = NEXT.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!("edittr-svn-{}-{unique}", std::process::id()));
            match builder.create(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
                Err(err) => return Err(err),
            }
        }
    }

    fn targets<'a>(&self, name: &str, paths: impl IntoIterator<Item = &'a str>) -> Result<PathBuf, String> {
        let file = self.0.join(name);
        let mut out = BufWriter::new(File::create(&file).map_err(|err| err.to_string())?);
        for path in paths {
            if path.contains(['\r', '\n']) {
                return Err(format!("Subversion target contains a line break: {path:?}"));
            }
            // SVN trims each line. Protect leading whitespace with ./ and trailing
            // whitespace with an empty peg revision, which add/delete/commit strip.
            writeln!(out, "./{path}@").map_err(|err| err.to_string())?;
        }
        out.flush().map_err(|err| err.to_string())?;
        Ok(file)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

pub fn commit(root: &Path, msg: &str) -> Result<String, String> {
    let entries: Vec<_> = changes(root, ".", "infinity")?
        .entries()
        .filter(|entry| entry.status.code().is_some())
        .collect();
    if entries.iter().any(|e| e.status.code() == Some('U')) {
        return Err("Resolve Subversion conflicts before committing".into());
    }
    if entries.is_empty() {
        return Ok(String::new());
    }
    let scratch = Scratch::new().map_err(|err| err.to_string())?;
    let message = scratch.0.join("message");
    fs::write(&message, msg).map_err(|err| err.to_string())?;
    let targets = scratch.targets("commit", entries.iter().map(|entry| entry.path.as_str()))?;
    for (item, operation) in [("unversioned", "add"), ("missing", "delete")] {
        let mut paths = entries
            .iter()
            .filter(|entry| entry.status.item == item)
            .map(|entry| entry.path.as_str())
            .peekable();
        if paths.peek().is_some() {
            let file = scratch.targets(operation, paths)?;
            run(root, &[operation, "--quiet", "--targets", &file.to_string_lossy()])?;
        }
    }
    run(
        root,
        &[
            "commit",
            "--force-log",
            "--encoding",
            "UTF-8",
            "-F",
            &message.to_string_lossy(),
            "--targets",
            &targets.to_string_lossy(),
        ],
    )
}

#[derive(Clone, Default, Deserialize, Serialize)]
struct Revision {
    #[serde(rename = "@revision")]
    number: u64,
    #[serde(default)]
    author: String,
    #[serde(default)]
    date: String,
    #[serde(rename = "msg", default)]
    message: String,
}

impl Revision {
    fn subject(&self) -> &str {
        self.message.lines().next().unwrap_or("")
    }
}

/// SVN's UTC timestamp as local "YYYY-MM-DD HH:MM", like the Git history shows.
fn date_time(date: &str) -> String {
    match crate::blame::date_to_epoch(date) {
        Some(utc) if date.len() > 10 => crate::blame::local_date_time(utc),
        _ => date.get(..10).unwrap_or_default().to_owned(),
    }
}

#[derive(Deserialize)]
struct LogXml {
    #[serde(rename = "logentry", default)]
    entries: Vec<Revision>,
}

#[derive(Default, Deserialize, Serialize)]
struct History {
    entries: BTreeMap<u64, Revision>,
    /// Number requested by the largest initial (HEAD to oldest) fetch.
    backfill: usize,
}

impl History {
    fn merge(&mut self, entries: Vec<Revision>, backfill: usize) {
        for rev in entries {
            self.entries.entry(rev.number).or_insert(rev);
        }
        self.backfill = self.backfill.max(backfill);
    }

    fn commits(&self, limit: usize) -> Vec<Commit> {
        let mut entries = self.entries.values().rev().take(limit).peekable();
        let mut commits = Vec::new();
        while let Some(entry) = entries.next() {
            commits.push(Commit {
                hash: format!("r{}", entry.number),
                parents: entries.peek().map(|p| format!("r{}", p.number)).into_iter().collect(),
                author: entry.author.clone(),
                date: date_time(&entry.date),
                refs: Vec::new(),
                subject: entry.subject().into(),
            });
        }
        commits
    }
}

pub fn log(root: &Path, limit: usize, refresh: bool, cached: impl FnOnce(Vec<Commit>)) -> Result<Vec<Commit>, String> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let info = info(root, ".")?;
    let file = info.history_file();
    let mut history: History = file.as_deref().and_then(read_cache).unwrap_or_default();
    if !refresh && history.backfill >= limit && file.as_deref().is_some_and(|file| history_is_recent(root, file)) {
        return Ok(history.commits(limit));
    }
    if !history.entries.is_empty() {
        cached(history.commits(limit));
    }

    let mut args = vec!["log", "--xml", "-r"];
    let width = limit.to_string();
    let incremental = history.backfill >= limit && !history.entries.is_empty();
    let range = if incremental {
        let newest = *history.entries.last_key_value().unwrap().0;
        format!("HEAD:{newest}")
    } else {
        "HEAD:0".into()
    };
    args.push(&range);
    if !incremental {
        args.extend(["-l", &width]);
    }
    args.extend(["--", &info.url]);
    let mut entries = run_xml::<LogXml>(root, &args)?.entries;
    if incremental {
        let newest = *history.entries.last_key_value().unwrap().0;
        entries.retain(|r| r.number > newest);
    }
    let backfill = if incremental { 0 } else { limit };
    if let Some(file) = file.as_deref() {
        merge_cache(file, &entries, backfill);
    }
    history.merge(entries, backfill);
    Ok(history.commits(limit))
}

// A second request can use a recent result directly. An explicit refresh
// always checks HEAD, including commits made by other working copies.
fn history_is_recent(root: &Path, file: &Path) -> bool {
    let Ok(cached) = file.metadata().and_then(|meta| meta.modified()) else {
        return false;
    };
    let Ok(db) = root.join(".svn/wc.db").metadata().and_then(|meta| meta.modified()) else {
        return false;
    };
    db <= cached && cached.elapsed().is_ok_and(|age| age < Duration::from_secs(30))
}

pub fn show_commit(root: &Path, rev: &str) -> Result<String, String> {
    let number = rev.strip_prefix('r').unwrap_or(rev);
    let number = number
        .parse::<u64>()
        .map_err(|_| format!("Invalid Subversion revision: {rev}"))?
        .to_string();
    let info = info(root, ".")?;
    let file = info.cache_dir().map(|p| p.join(format!("show-{number}.json")));
    if let Some(cached) = file.as_deref().and_then(read_cache) {
        return Ok(cached);
    }
    let url = &info.repository.root;
    let log = run(root, &["log", "-r", &number, "-v", "--", url])?;
    let diff = run(root, &["diff", "--internal-diff", "-c", &number, "--", url])?;
    let output = format!("{log}\n{diff}");
    if let Some(file) = file.as_deref() {
        let _ = write_cache(file, &output);
    }
    Ok(output)
}

#[derive(Deserialize)]
struct BlameXml {
    target: BlameTarget,
}

#[derive(Deserialize)]
struct BlameTarget {
    #[serde(rename = "entry", default)]
    lines: Vec<BlameEntry>,
}

#[derive(Deserialize)]
struct BlameEntry {
    #[serde(rename = "@line-number")]
    line: usize,
    commit: Option<Revision>,
}

fn blame_lines(blame: BlameXml) -> Result<Vec<BlameLine>, String> {
    let mut lines = Vec::new();
    for entry in blame.target.lines {
        if entry.line != lines.len() + 1 {
            return Err("Invalid line numbering in svn blame XML".into());
        }
        lines.push(entry.commit.map_or_else(BlameLine::default, |r| BlameLine {
            rev: format!("r{}", r.number),
            author: r.author,
            date: r.date,
            summary: String::new(),
        }));
    }
    Ok(lines)
}

pub fn blame(root: &Path, rel: &str) -> Result<Vec<BlameLine>, String> {
    let status = changes(root, rel, "empty")?.entries().next();
    let file_info = info(root, rel);
    // Exact-target status can be empty for a file inside an unversioned directory.
    let unversioned = status.as_ref().is_some_and(|entry| entry.status.item == "unversioned")
        || (status.is_none() && file_info.is_err() && root.join(rel).is_file());
    if unversioned || file_info.as_ref().is_ok_and(|info| info.commit.is_none()) {
        let text = fs::read_to_string(root.join(rel)).map_err(|err| err.to_string())?;
        return Ok(vec![BlameLine::default(); text.lines().count()]);
    }
    let file_info = file_info?;
    let modified = status.is_some_and(|entry| entry.status.code().is_some());
    // The last-changed revision comes from local `svn info --xml`.
    let file = file_info.commit.as_ref().filter(|_| !modified).and_then(|rev| {
        Some(
            file_info
                .cache_dir()?
                .join(format!("blame-{}-{}.json", cache_key(&file_info.url), rev.number)),
        )
    });
    // With no explicit revision, SVN annotates the saved working copy.
    let mut lines = if let Some(cached) = file.as_deref().and_then(read_cache) {
        cached
    } else {
        let lines = blame_lines(run_xml(root, &["blame", "--xml", "--", &target(rel)])?)?;
        if let Some(file) = file.as_deref() {
            let _ = write_cache(file, &lines);
        }
        lines
    };
    if let Some(history) = info(root, ".")
        .ok()
        .and_then(|i| i.history_file())
        .as_deref()
        .and_then(read_cache::<History>)
    {
        for line in &mut lines {
            if let Some(rev) = line
                .rev
                .strip_prefix('r')
                .and_then(|r| r.parse().ok())
                .and_then(|r| history.entries.get(&r))
            {
                line.summary = rev.subject().into();
            }
        }
    }
    Ok(lines)
}

#[cfg(test)]
pub(crate) mod real_tests;

#[cfg(test)]
thread_local! {
    static TEST_CACHE: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

fn cache_root() -> Option<PathBuf> {
    #[cfg(test)]
    if let Some(path) = TEST_CACHE.with(|root| root.borrow().clone()) {
        return Some(path);
    }
    Some(dirs::cache_dir()?.join("edittr").join("svn"))
}

fn cache_key(key: &str) -> String {
    let mut hex = String::with_capacity(64);
    for byte in Sha256::digest(key.as_bytes()) {
        write!(hex, "{byte:02x}").unwrap();
    }
    hex
}

fn read_cache<T: DeserializeOwned>(path: &Path) -> Option<T> {
    serde_json::from_reader(BufReader::new(File::open(path).ok()?)).ok()
}

fn write_cache(path: &Path, value: &impl Serialize) -> std::io::Result<()> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!("{}.{}.tmp", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
    let file = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
    let result = (|| {
        let mut out = BufWriter::new(file);
        serde_json::to_writer(&mut out, value)?;
        out.flush()?;
        drop(out);
        fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

fn merge_cache(path: &Path, entries: &[Revision], backfill: usize) {
    let _ = (|| -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let lock = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(path.with_extension("lock"))?;
        lock.lock()?;
        let mut history: History = read_cache(path).unwrap_or_default();
        history.merge(entries.to_vec(), backfill);
        write_cache(path, &history)
    })();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::{Kind, side_by_side};

    // Fixtures follow the XML output examples in the SVN Book command reference.
    const INFO: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<info><entry kind="dir" path="." revision="17">
<url>https://example.org/svn/demo/branches/with%20space</url>
<relative-url>^/branches/with%20space</relative-url>
<repository><root>https://example.org/svn/demo</root><uuid>6e5e3e5b-ffe7-4f48-adc2-a757f368b897</uuid></repository>
<wc-info><wcroot-abspath>/projects/demo</wcroot-abspath><schedule>normal</schedule><depth>infinity</depth></wc-info>
<commit revision="14"><author>sally</author><date>2008-05-25T19:12:31.428953Z</date></commit>
</entry></info>"#;

    const STATUS: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<status><target path=".">
<entry path="new &amp; old.txt"><wc-status item="unversioned" props="none"/></entry>
<entry path="add.txt"><wc-status item="added" revision="-1" props="none"/></entry>
<entry path="lost.txt"><wc-status item="missing" revision="17" props="none"/></entry>
<entry path="delete.txt"><wc-status item="deleted" revision="17" props="none"/></entry>
<entry path="props.txt"><wc-status item="normal" revision="17" props="modified">
<commit revision="14"><author>sally</author><date>2008-05-25T19:12:31.428953Z</date></commit>
</wc-status></entry>
<entry path="tree.txt"><wc-status item="normal" props="none" tree-conflicted="true"/></entry>
<entry path="external"><wc-status item="external" props="none"/></entry>
</target>
<changelist name="feature">
<entry path="other.txt"><wc-status item="modified" props="none"/></entry>
</changelist>
<changelist name="bugfix">
<entry path="fix.txt"><wc-status item="replaced" props="none"/></entry>
</changelist>
</status>"#;

    const LOG: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<log>
<logentry revision="17">
<author>harry</author><date>2008-05-29T13:26:12.293121Z</date>
<paths><path kind="file" action="M">/branches/with space/readme.txt</path></paths>
<msg>One &amp; two
Body &lt;details&gt;</msg>
</logentry>
<logentry revision="14">
<date>2008-05-25T19:12:31.428953Z</date>
<paths><path kind="dir" action="A" copyfrom-path="/trunk" copyfrom-rev="13">/branches/with space</path></paths>
<msg>Create branch</msg>
</logentry>
</log>"#;

    const BLAME: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<blame><target path="readme.txt">
<entry line-number="1"><commit revision="3">
<author>sally &amp; bob</author><date>2008-05-25T19:12:31.428953Z</date>
</commit></entry>
<entry line-number="2"/>
<entry line-number="3"><commit revision="17">
<author>harry</author><date>2008-05-29T13:26:12.293121Z</date>
</commit></entry>
</target></blame>"#;

    #[test]
    fn parses_working_copy_info() {
        let info = xml::<Info>(INFO).unwrap().entry;
        assert_eq!(info.branch(), "branches/with%20space");
        assert_eq!(info.commit.unwrap().number, 14);
        let legacy = INFO.replace("<relative-url>^/branches/with%20space</relative-url>", "");
        assert_eq!(xml::<Info>(&legacy).unwrap().entry.branch(), "branches/with%20space");
    }

    #[test]
    fn parses_status_and_changelists() {
        let entries: Vec<_> = xml::<StatusXml>(STATUS).unwrap().entries().collect();
        let codes: Vec<_> = entries.iter().filter_map(|e| Some((e.path.as_str(), e.status.code()?))).collect();
        assert_eq!(
            codes,
            [
                ("new & old.txt", '?'),
                ("add.txt", 'A'),
                ("lost.txt", 'D'),
                ("delete.txt", 'D'),
                ("props.txt", 'M'),
                ("tree.txt", 'U'),
                ("other.txt", 'M'),
                ("fix.txt", 'R'),
            ]
        );
        assert!(
            xml::<StatusXml>("<status><target path=\".\"/></status>")
                .unwrap()
                .entries()
                .next()
                .is_none()
        );
        assert!(xml::<StatusXml>("<status><target><entry/></target></status>").is_err());
    }

    #[test]
    fn parses_log_and_merges_cached_revisions() {
        let mut history = History::default();
        history.merge(xml::<LogXml>(LOG).unwrap().entries, 2);
        let old = std::mem::take(&mut history);
        let mut history: History = serde_json::from_str(&serde_json::to_string(&old).unwrap()).unwrap();
        let rev = |number, message: &str| Revision {
            number,
            message: message.into(),
            ..Revision::default()
        };
        history.merge(vec![rev(17, "duplicate"), rev(25, "Newest"), rev(21, "Next")], 0);
        assert_eq!(history.backfill, 2);
        assert_eq!(history.entries.len(), 4);

        let commits = history.commits(3);
        assert_eq!(commits.iter().map(|c| c.hash.as_str()).collect::<Vec<_>>(), ["r25", "r21", "r17"]);
        assert_eq!(commits[0].parents, ["r21"]);
        assert_eq!(commits[1].parents, ["r17"]);
        assert!(commits[2].parents.is_empty());
        assert!(commits[2].refs.is_empty());
        assert_eq!(commits[2].subject, "One & two");
        assert_eq!(commits[2].author, "harry");
        // Shown in local time, like Git history.
        let utc = crate::blame::date_to_epoch("2008-05-29T13:26:12Z").unwrap();
        assert_eq!(commits[2].date, crate::blame::local_date_time(utc));
        assert!(history.commits(4)[3].author.is_empty());
    }

    #[test]
    fn parses_blame_with_unattributed_lines() {
        let lines = blame_lines(xml(BLAME).unwrap()).unwrap();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].rev, "r3");
        assert_eq!(lines[0].author, "sally & bob");
        assert_eq!(lines[0].date, "2008-05-25T19:12:31.428953Z");
        assert!(lines[1].rev.is_empty());
        assert_eq!(lines[2].rev, "r17");
        assert!(lines[2].summary.is_empty());
        assert!(blame_lines(xml(&BLAME.replace("line-number=\"2\"", "line-number=\"5\"")).unwrap()).is_err());
    }

    #[test]
    fn synthesizes_whole_file_changes() {
        for (added, kind) in [(true, Kind::Added), (false, Kind::Removed)] {
            let raw = one_sided_diff("hello.txt", b"one\ntwo", added);
            let diff = side_by_side(&raw);
            assert_eq!(diff.rows.iter().map(|r| r.kind).collect::<Vec<_>>(), [kind, kind]);
            assert!(raw.ends_with("\\ No newline at end of file\n"));
        }
    }

    #[test]
    fn parses_full_context_svn_diff() {
        let text = "Index: readme.txt\n===================================================================\n--- readme.txt\t(revision 17)\n+++ readme.txt\t(working copy)\n@@ -1,3 +1,3 @@\n a\n-before\n+after\n c\n";
        let diff = side_by_side(text);
        assert_eq!(
            diff.rows.iter().map(|r| r.kind).collect::<Vec<_>>(),
            [Kind::Same, Kind::Changed, Kind::Same]
        );
    }
}
