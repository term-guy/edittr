//! Thin wrapper over the system `git` CLI. Shelling out keeps the binary small
//! (no libgit2) and always matches the user's git config, hooks and credentials.

use crate::diff::{FileDiff, unified_hunks};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

pub(crate) use hunks::{BufferBase, apply_diff_hunk, stage_buffer_hunk};
mod hunks;

pub(crate) fn cmd(repo: &Path) -> Command {
    let mut c = Command::new("git");
    c.current_dir(repo).env("GIT_TERMINAL_PROMPT", "0");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        c.creation_flags(CREATE_NO_WINDOW);
    }
    c
}

fn run_raw(repo: &Path, args: &[&str]) -> std::io::Result<Output> {
    cmd(repo).args(args).output()
}

/// Run git, returning stdout on success or stderr (trimmed) as the error.
pub fn run(repo: &Path, args: &[&str]) -> Result<String, String> {
    match run_raw(repo, args) {
        Ok(o) if o.status.success() => Ok(String::from_utf8_lossy(&o.stdout).into_owned()),
        Ok(o) => {
            let err = String::from_utf8_lossy(&o.stderr).trim().to_string();
            let out = String::from_utf8_lossy(&o.stdout).trim().to_string();
            Err(if err.is_empty() { out } else { err })
        }
        Err(e) => Err(format!("failed to run git: {e}")),
    }
}

/// Returns the repository top-level directory containing `dir`, if any.
pub fn repo_root(dir: &Path) -> Option<PathBuf> {
    run(dir, &["rev-parse", "--show-toplevel"]).ok().map(|s| PathBuf::from(s.trim()))
}

#[derive(Clone, Debug)]
pub struct FileChange {
    pub path: String,
    /// Single status letter shown in the UI (M, A, D, R, U, ?).
    pub code: char,
}

#[derive(Default, Clone, Debug)]
pub struct Status {
    pub branch: String,
    pub ahead_behind: String,
    pub staged: Vec<FileChange>,
    pub unstaged: Vec<FileChange>,
}

pub fn status(repo: &Path) -> Result<Status, String> {
    // Status runs in response to filesystem events. Avoid writing the index stat cache.
    let out = run(repo, &["--no-optional-locks", "status", "--porcelain=v1", "-b", "-z", "-uall"])?;
    let mut st = Status::default();
    let mut parts = out.split('\0').filter(|s| !s.is_empty());
    while let Some(entry) = parts.next() {
        if let Some(head) = entry.strip_prefix("## ") {
            // "main...origin/main [ahead 1, behind 2]" or "No commits yet on main"
            let (name, rest) = head.split_once(' ').unwrap_or((head, ""));
            st.branch = name.split("...").next().unwrap_or(name).to_string();
            if let Some(s) = rest.find('[') {
                st.ahead_behind = rest[s..].to_string();
            }
            if head.starts_with("No commits yet on ") {
                st.branch = head.trim_start_matches("No commits yet on ").to_string();
            }
            continue;
        }
        if entry.len() < 4 {
            continue;
        }
        let x = entry.as_bytes()[0] as char;
        let y = entry.as_bytes()[1] as char;
        let path = entry[3..].to_string();
        if x == 'R' || x == 'C' {
            parts.next(); // original path of a rename/copy
        }
        if x == '?' {
            st.unstaged.push(FileChange { path, code: '?' });
            continue;
        }
        let conflict = x == 'U' || y == 'U' || (x == 'A' && y == 'A') || (x == 'D' && y == 'D');
        if conflict {
            st.unstaged.push(FileChange { path, code: 'U' });
            continue;
        }
        if x != ' ' {
            st.staged.push(FileChange {
                path: path.clone(),
                code: x,
            });
        }
        if y != ' ' {
            st.unstaged.push(FileChange { path, code: y });
        }
    }
    Ok(st)
}

/// Enough context that the diff contains the whole file (for the side-by-side view).
const FULL_CONTEXT: &str = "--unified=1000000";

pub fn diff(repo: &Path, change: &FileChange, staged: bool, ignore_eol: bool) -> FileDiff {
    let text = diff_patch(repo, change, staged, ignore_eol, FULL_CONTEXT);
    if !ignore_eol || change.code == '?' {
        return FileDiff::new(text, None);
    }
    // Git's CR filter can also suppress a final LF. Look for a real change
    // to the final newline in the strict patch before accepting the filtered one.
    let strict = diff_patch(repo, change, staged, false, "--unified=0");
    if changes_final_newline(&strict) && !changes_final_newline(&text) {
        let strict = diff_patch(repo, change, staged, false, FULL_CONTEXT);
        return FileDiff::new(filter_crlf_hunks(&strict), None);
    }
    FileDiff::new(text, Some(&strict))
}

fn changes_final_newline(patch: &str) -> bool {
    let mut changed = patch
        .lines()
        .zip(patch.lines().skip(1))
        .filter(|(line, next)| line.starts_with(['-', '+']) && next.starts_with("\\ "));
    changed.next().is_some() && changed.next().is_none()
}

fn filter_crlf_hunks(patch: &str) -> String {
    let mut output = String::new();
    let mut lines = patch.split_inclusive('\n').peekable();
    while let Some(header) = lines.next() {
        if !header.starts_with("@@ ") {
            output.push_str(header);
            continue;
        }
        let mut old = String::new();
        let mut new = String::new();
        while let Some(line) = lines.next_if(|line| line.starts_with([' ', '-', '+'])) {
            let (tag, content) = line.split_at(1);
            let content = if lines.next_if(|next| next.starts_with("\\ ")).is_some() {
                content.strip_suffix('\n').unwrap_or(content)
            } else {
                content
            };
            if matches!(tag, " " | "-") {
                old.push_str(content);
            }
            if matches!(tag, " " | "+") {
                new.push_str(content);
            }
        }
        // Keep the original hunk's coordinates by including its full context.
        output.push_str(&unified_hunks(
            &old,
            &new,
            true,
            old.lines().count().max(new.lines().count()),
            |_| header.trim_end_matches('\n').to_owned(),
        ));
    }
    output
}

fn diff_patch(repo: &Path, change: &FileChange, staged: bool, ignore_eol: bool, context: &str) -> String {
    let mut args = vec![
        "--literal-pathspecs",
        "diff",
        "--no-color",
        "--no-ext-diff",
        "--no-textconv",
        context,
    ];
    if ignore_eol {
        args.push("--ignore-cr-at-eol");
    }
    let res = if change.code == '?' {
        // Untracked: diff against nothing. Exit code 1 is expected here.
        args.extend(["--no-index", "--", "/dev/null", &change.path]);
        run_raw(repo, &args)
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .map_err(|e| e.to_string())
    } else {
        if staged {
            args.push("--cached");
        }
        args.extend(["--", &change.path]);
        run(repo, &args)
    };
    res.unwrap_or_else(|e| e)
}

pub fn stage(repo: &Path, path: &str) -> Result<String, String> {
    run(repo, &["add", "--", path])
}
pub fn stage_all(repo: &Path) -> Result<String, String> {
    run(repo, &["add", "-A"])
}
pub fn unstage(repo: &Path, path: &str) -> Result<String, String> {
    // `restore --staged` fails before the first commit; `rm --cached` works there.
    run(repo, &["restore", "--staged", "--", path]).or_else(|_| run(repo, &["rm", "--cached", "-r", "-q", "--", path]))
}
pub fn unstage_all(repo: &Path) -> Result<String, String> {
    run(repo, &["reset", "-q"])
}
pub fn discard(repo: &Path, change: &FileChange) -> Result<String, String> {
    if change.code == '?' {
        std::fs::remove_file(repo.join(&change.path))
            .map(|_| String::new())
            .map_err(|e| e.to_string())
    } else {
        run(repo, &["restore", "--", &change.path])
    }
}
/// Discard many changes: new (untracked) files are deleted, the rest restored with
/// one `git restore` per 500 paths.
pub fn discard_many(repo: &Path, changes: &[FileChange]) -> Result<String, String> {
    let mut errors = Vec::new();
    for change in changes.iter().filter(|c| c.code == '?') {
        if let Err(e) = std::fs::remove_file(repo.join(&change.path)) {
            errors.push(format!("{}: {e}", change.path));
        }
    }
    let tracked: Vec<&str> = changes.iter().filter(|c| c.code != '?').map(|c| c.path.as_str()).collect();
    for chunk in tracked.chunks(500) {
        let mut args = vec!["restore", "--"];
        args.extend(chunk);
        if let Err(e) = run(repo, &args) {
            errors.push(e);
        }
    }
    if errors.is_empty() {
        Ok(String::new())
    } else {
        Err(errors.join("\n"))
    }
}

pub fn commit(repo: &Path, msg: &str, amend: bool) -> Result<String, String> {
    let mut args = vec!["commit", "-m", msg];
    if amend {
        args.push("--amend");
    }
    run(repo, &args)
}

#[derive(Clone)]
pub struct Stash {
    pub selector: String,
    pub commit: Commit,
}

impl Stash {
    pub fn title(&self) -> String {
        format!("{}  {}", self.selector, self.commit.subject)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum StashAction {
    Apply,
    Pop,
    Drop,
}

impl StashAction {
    pub fn label(self) -> &'static str {
        match self {
            Self::Apply => "Apply stash",
            Self::Pop => "Pop stash",
            Self::Drop => "Drop stash",
        }
    }
}

pub fn stashes(repo: &Path) -> Result<Vec<Stash>, String> {
    let out = run(
        repo,
        &[
            "stash",
            "list",
            "-z",
            "--no-show-signature",
            "--format=%gd%x00%H%x00%h%x00%P%x00%an%x00%as%x00%gs",
        ],
    )?;
    let fields: Vec<_> = out.split_terminator('\0').collect();
    let (entries, rest) = fields.as_chunks::<7>();
    if !rest.is_empty() {
        return Err("Invalid Git stash list".into());
    }
    Ok(entries
        .iter()
        .map(|&[selector, hash, short_hash, parents, author, date, subject]| Stash {
            selector: selector.into(),
            commit: Commit {
                hash: hash.into(),
                short_hash: short_hash.into(),
                parents: parents.split_whitespace().map(str::to_string).collect(),
                author: author.into(),
                date: date.into(),
                refs: Vec::new(),
                subject: subject.into(),
                message: subject.into(),
            },
        })
        .collect())
}

pub fn stash_push(repo: &Path, message: &str, include_untracked: bool) -> Result<String, String> {
    let mut args = vec!["stash", "push"];
    if include_untracked {
        args.push("--include-untracked");
    }
    if !message.is_empty() {
        args.extend(["--message", message]);
    }
    run(repo, &args)
}

pub fn stash_action(repo: &Path, stash: &Stash, action: StashAction) -> Result<String, String> {
    let selector = &stash.selector;
    let hash = &stash.commit.hash;
    if action == StashAction::Apply {
        run(repo, &["stash", "apply", "--", hash])?;
        return Ok(format!("Applied stash {}", stash.commit.short_hash));
    }
    // Reflog positions can shift while a menu or confirmation is open.
    let current = run(repo, &["rev-parse", "--verify", "--end-of-options", selector]);
    if !matches!(current.as_deref(), Ok(current) if current.trim_end() == hash) {
        return Err(format!("{selector} has changed. Select the stash again from the refreshed list."));
    }
    let (verb, completed) = match action {
        StashAction::Pop => ("pop", "Popped"),
        StashAction::Drop => ("drop", "Dropped"),
        StashAction::Apply => unreachable!(),
    };
    run(repo, &["stash", verb, "--", selector])?;
    Ok(format!("{completed} {selector}"))
}

pub fn branches(repo: &Path) -> Vec<String> {
    run(repo, &["branch", "--format=%(refname:lstrip=2)"])
        .map(|s| s.lines().map(str::to_string).collect())
        .unwrap_or_default()
}
pub fn switch(repo: &Path, branch: &str) -> Result<String, String> {
    run(repo, &["switch", branch])
}

pub fn show_commit(repo: &Path, hash: &str) -> String {
    run(
        repo,
        &[
            "show",
            "--no-color",
            "--stat",
            "--patch",
            "--format=commit %H%nAuthor: %an <%ae>%nDate:   %ad%n%n%w(0,4,4)%B",
            hash,
        ],
    )
    .unwrap_or_else(|e| e)
}

/// Files tracked or untracked-but-not-ignored, relative to `dir`.
pub fn list_files(dir: &Path) -> Option<Vec<String>> {
    run(dir, &["ls-files", "-z", "-co", "--exclude-standard"])
        .ok()
        .map(|s| s.split_terminator('\0').map(str::to_string).collect())
}

#[derive(Clone, Debug)]
pub struct Commit {
    pub hash: String,
    pub short_hash: String,
    pub parents: Vec<String>,
    pub author: String,
    pub date: String,
    pub refs: Vec<CommitRef>,
    pub subject: String,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommitRef {
    Local { name: String, head: bool },
    Remote(String),
    Tag(String),
    Head,
    SvnBase,
    Other(String),
}

fn parse_ref(decoration: &str) -> Option<CommitRef> {
    if decoration == "HEAD" {
        return Some(CommitRef::Head);
    }
    let (name, head) = match decoration.strip_prefix("HEAD -> ") {
        Some(name) => (name, true),
        None => (decoration, false),
    };
    if let Some(name) = name.strip_prefix("refs/heads/") {
        Some(CommitRef::Local { name: name.into(), head })
    } else if let Some(name) = name.strip_prefix("refs/remotes/") {
        // The remote's symbolic default-branch pointer is an alias for a branch already listed.
        let (_, branch) = name.split_once('/')?;
        Some(if branch == "HEAD" {
            CommitRef::Other(name.into())
        } else {
            CommitRef::Remote(name.into())
        })
    } else if name == "refs/stash" {
        Some(CommitRef::Other(name.into()))
    } else {
        name.strip_prefix("tag: refs/tags/").map(|name| CommitRef::Tag(name.into()))
    }
}

pub fn log(repo: &Path, limit: usize) -> Result<Vec<Commit>, String> {
    let n = format!("-n{limit}");
    let out = run(
        repo,
        &[
            "log",
            "--all",
            "--topo-order",
            "--decorate=full",
            "--decorate-refs=HEAD",
            "--decorate-refs=refs/heads/",
            "--decorate-refs=refs/remotes/",
            "--decorate-refs=refs/tags/",
            "--decorate-refs=refs/stash",
            "-z",
            &n,
            "--format=%H%x00%h%x00%P%x00%an%x00%ad%x00%D%x00%s%x00%B",
            "--date=format:%Y-%m-%d %H:%M",
        ],
    )?;
    let fields: Vec<_> = out.split_terminator('\0').collect();
    Ok(fields
        .as_chunks::<8>()
        .0
        .iter()
        .map(|&[hash, short_hash, parents, author, date, refs, subject, message]| Commit {
            hash: hash.into(),
            short_hash: short_hash.into(),
            parents: parents.split_whitespace().map(str::to_string).collect(),
            author: author.into(),
            date: date.into(),
            refs: refs.split(", ").filter_map(parse_ref).collect(),
            subject: subject.into(),
            message: message.trim_end_matches('\n').into(),
        })
        .collect())
}

#[derive(Clone, Copy, Debug)]
pub enum ResetMode {
    Soft,
    Mixed,
    Hard,
}

impl ResetMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Soft => "Soft",
            Self::Mixed => "Mixed",
            Self::Hard => "Hard",
        }
    }
}

#[derive(Clone, Debug)]
pub enum HistoryAction {
    Checkout(String),
    Track { local: String, remote: String },
    Detach(String),
    CreateBranch { name: String, hash: String, checkout: bool },
    Reset { branch: String, hash: String, mode: ResetMode },
    CherryPick(String),
    Revert(String),
    Merge { branch: String, source: String },
    CreateTag { name: String, hash: String },
    DeleteBranch { name: String, force: bool },
    DeleteTag(String),
}

impl HistoryAction {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Checkout(_) | Self::Track { .. } | Self::Detach(_) => "Checkout",
            Self::CreateBranch { .. } => "Create branch",
            Self::Reset { .. } => "Reset",
            Self::CherryPick(_) => "Cherry-pick",
            Self::Revert(_) => "Revert",
            Self::Merge { .. } => "Merge",
            Self::CreateTag { .. } => "Create tag",
            Self::DeleteBranch { .. } => "Delete branch",
            Self::DeleteTag(_) => "Delete tag",
        }
    }

    fn args(&self) -> Vec<&str> {
        match self {
            Self::Checkout(name) => vec!["switch", "--no-guess", "--", name],
            Self::Track { local, remote } => vec!["switch", "--track", "-c", local, "--", remote],
            Self::Detach(hash) => vec!["switch", "--detach", "--", hash],
            Self::CreateBranch {
                name,
                hash,
                checkout: true,
            } => vec!["switch", "--no-track", "-c", name, "--", hash],
            Self::CreateBranch {
                name,
                hash,
                checkout: false,
            } => vec!["branch", "--no-track", "--", name, hash],
            Self::Reset { hash, mode, .. } => {
                let flag = match mode {
                    ResetMode::Soft => "--soft",
                    ResetMode::Mixed => "--mixed",
                    ResetMode::Hard => "--hard",
                };
                vec!["reset", flag, hash, "--"]
            }
            Self::CherryPick(hash) => vec!["cherry-pick", "--", hash],
            Self::Revert(hash) => vec!["revert", "--no-edit", "--", hash],
            Self::Merge { source, .. } => vec!["merge", "--no-edit", "--", source],
            Self::CreateTag { name, hash } => vec!["tag", "--no-sign", "--", name, hash],
            Self::DeleteBranch { name, force } => vec!["branch", if *force { "-D" } else { "-d" }, "--", name],
            Self::DeleteTag(name) => vec!["tag", "-d", "--", name],
        }
    }

    pub fn run(&self, repo: &Path) -> Result<String, String> {
        if let Self::Reset { branch, .. } | Self::Merge { branch, .. } = self
            && run(repo, &["symbolic-ref", "--quiet", "HEAD"]).ok().as_deref().map(str::trim_end)
                != Some(format!("refs/heads/{branch}").as_str())
        {
            return Err(format!("The current branch changed. Check out {branch} and try again."));
        }

        // Git prints conflict paths on stdout and recovery hints on stderr.
        let output = run_raw(repo, &self.args()).map_err(|e| format!("failed to run git: {e}"))?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        if output.status.success() {
            return Ok(stdout.into_owned());
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        let message = [stdout.trim(), stderr.trim()]
            .into_iter()
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        Err(if message.is_empty() {
            format!("git exited with {}", output.status)
        } else {
            message
        })
    }
}

#[cfg(test)]
mod history_tests;
