//! Subversion CLI and its local cache. These operations belong on background threads.

use crate::vcs::{BlameLine, Commit, FileChange, Status};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

fn run(root: &Path, args: &[&str]) -> Result<String, String> {
    let mut cmd = Command::new("svn");
    cmd.current_dir(root).arg("--non-interactive").args(args);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let output = cmd.output().map_err(|e| format!("failed to run svn: {e}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        let err = String::from_utf8_lossy(&output.stderr);
        Err(if err.trim().is_empty() {
            format!("svn {} exited with {}", args[0], output.status)
        } else {
            err.trim().into()
        })
    }
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
        Some(dirs::cache_dir()?.join("edittr").join("svn").join(cache_key(identity)))
    }

    fn history_file(&self) -> Option<PathBuf> {
        Some(self.cache_dir()?.join(format!("log-{}.json", cache_key(self.branch()))))
    }
}

fn info(root: &Path, rel: &str) -> Result<InfoEntry, String> {
    Ok(xml::<Info>(&run(root, &["info", "--xml", "--", &target(rel)])?)?.entry)
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
}

impl WcStatus {
    fn code(&self) -> Option<char> {
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

fn parse_status(output: &str) -> Result<Vec<StatusEntry>, String> {
    let status = xml::<StatusXml>(output)?;
    let entries: Vec<_> = status
        .target
        .into_iter()
        .chain(status.changelist)
        .flat_map(|group| group.entries)
        .collect();
    #[cfg(windows)]
    let entries = entries
        .into_iter()
        .map(|mut entry| {
            entry.path = entry.path.replace('\\', "/");
            entry
        })
        .collect();
    Ok(entries)
}

fn changes(root: &Path, rel: &str) -> Result<Vec<StatusEntry>, String> {
    parse_status(&run(root, &["status", "--xml", "--ignore-externals", "--", &target(rel)])?)
}

pub fn status(root: &Path) -> Result<Status, String> {
    Ok(Status {
        branch: info(root, ".")?.branch().into(),
        unstaged: changes(root, ".")?
            .into_iter()
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
    let target = target(path);
    let diff = run(
        root,
        &["diff", "--internal-diff", "--show-copies-as-adds", "-x", "-U1000000", "--", &target],
    );
    // SVN omits files removed outside of `svn delete` from its working-copy diff.
    if change.code == 'D'
        && diff.as_ref().is_ok_and(String::is_empty)
        && let Ok(base) = run(root, &["cat", "-r", "BASE", "--", &target])
    {
        return one_sided_diff(path, base.as_bytes(), false);
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
    if bytes.contains(&0) {
        return format!("Binary files {old} and {new} differ");
    }
    let text = String::from_utf8_lossy(bytes);
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
        let path = root.join(&change.path);
        let result = if path.symlink_metadata().is_ok_and(|m| m.is_dir()) {
            fs::remove_dir(path)
        } else {
            fs::remove_file(path)
        };
        result.map(|()| String::new()).map_err(|e| e.to_string())
    } else {
        run(root, &["revert", "--", &target(&change.path)])
    }
}

pub fn commit(root: &Path, msg: &str) -> Result<String, String> {
    let entries = changes(root, ".")?;
    if entries.iter().any(|e| e.status.code() == Some('U')) {
        return Err("Resolve Subversion conflicts before committing".into());
    }
    // SVN only commits versioned changes, so schedule new and missing paths first.
    for entry in entries {
        let path = target(&entry.path);
        match entry.status.item.as_str() {
            "unversioned" => {
                run(root, &["add", "--", &path])?;
            }
            "missing" => {
                run(root, &["delete", "--", &path])?;
            }
            _ => {}
        }
    }
    run(root, &["commit", "--force-log", "-m", msg, "--", "."])
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

fn date_time(date: &str) -> String {
    match (date.get(..10), date.get(11..16)) {
        (Some(day), Some(time)) => format!("{day} {time}"),
        _ => String::new(),
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

pub fn log(root: &Path, limit: usize, cached: impl FnOnce(Vec<Commit>)) -> Result<Vec<Commit>, String> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let info = info(root, ".")?;
    let file = info.history_file();
    let mut history: History = file.as_deref().and_then(read_cache).unwrap_or_default();
    if !history.entries.is_empty() {
        cached(history.commits(limit));
    }

    let mut args = vec!["log", "--xml", "-v", "-r"];
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
    let mut entries = xml::<LogXml>(&run(root, &args)?)?.entries;
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

fn parse_blame(output: &str) -> Result<Vec<BlameLine>, String> {
    let mut lines = Vec::new();
    for entry in xml::<BlameXml>(output)?.target.lines {
        if entry.line != lines.len() + 1 {
            return Err("Invalid line numbering in svn blame XML".into());
        }
        lines.push(entry.commit.map_or_else(BlameLine::default, |r| BlameLine {
            rev: format!("r{}", r.number),
            author: r.author,
            date: r.date.get(..10).unwrap_or("").into(),
            summary: String::new(),
        }));
    }
    Ok(lines)
}

pub fn blame(root: &Path, rel: &str) -> Result<Vec<BlameLine>, String> {
    let file_info = info(root, rel)?;
    let modified = changes(root, rel)?.iter().any(|entry| entry.status.code().is_some());
    // The last-changed revision comes from local `svn info --xml`.
    let file = file_info.commit.as_ref().filter(|_| !modified).and_then(|rev| {
        Some(
            file_info
                .cache_dir()?
                .join(format!("blame-{}-{}.json", cache_key(&file_info.url), rev.number)),
        )
    });
    // BASE describes the pristine file, so locally edited lines may be offset.
    let mut lines = if let Some(cached) = file.as_deref().and_then(read_cache) {
        cached
    } else {
        let lines = parse_blame(&run(root, &["blame", "--xml", "-r", "1:BASE", "--", &target(rel)])?)?;
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
        let entries = parse_status(STATUS).unwrap();
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
        assert!(parse_status("<status><target path=\".\"/></status>").unwrap().is_empty());
        assert!(parse_status("<status><target><entry/></target></status>").is_err());
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
        assert_eq!(commits[2].date, "2008-05-29 13:26");
        assert!(history.commits(4)[3].author.is_empty());
    }

    #[test]
    fn parses_blame_with_unattributed_lines() {
        let lines = parse_blame(BLAME).unwrap();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].rev, "r3");
        assert_eq!(lines[0].author, "sally & bob");
        assert_eq!(lines[0].date, "2008-05-25");
        assert!(lines[1].rev.is_empty());
        assert_eq!(lines[2].rev, "r17");
        assert!(lines[2].summary.is_empty());
        assert!(parse_blame(&BLAME.replace("line-number=\"2\"", "line-number=\"5\"")).is_err());
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
