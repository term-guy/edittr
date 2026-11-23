//! Thin wrapper over the system `git` CLI. Shelling out keeps the binary small
//! (no libgit2) and always matches the user's git config, hooks and credentials.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn cmd(repo: &Path) -> Command {
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
    run(dir, &["rev-parse", "--show-toplevel"])
        .ok()
        .map(|s| PathBuf::from(s.trim()))
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
    let out = run(repo, &["status", "--porcelain=v1", "-b", "-z", "-uall"])?;
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
            st.staged.push(FileChange { path: path.clone(), code: x });
        }
        if y != ' ' {
            st.unstaged.push(FileChange { path, code: y });
        }
    }
    Ok(st)
}

pub fn diff(repo: &Path, change: &FileChange, staged: bool) -> String {
    let res = if change.code == '?' {
        // Untracked: diff against nothing. Exit code 1 is expected here.
        run_raw(repo, &["diff", "--no-color", "--no-index", "--", "/dev/null", &change.path])
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .map_err(|e| e.to_string())
    } else if staged {
        run(repo, &["diff", "--no-color", "--cached", "--", &change.path])
    } else {
        run(repo, &["diff", "--no-color", "--", &change.path])
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
    run(repo, &["restore", "--staged", "--", path])
        .or_else(|_| run(repo, &["rm", "--cached", "-q", "--", path]))
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
pub fn commit(repo: &Path, msg: &str, amend: bool) -> Result<String, String> {
    let mut args = vec!["commit", "-m", msg];
    if amend {
        args.push("--amend");
    }
    run(repo, &args)
}

pub fn branches(repo: &Path) -> Vec<String> {
    run(repo, &["branch", "--format=%(refname:short)"])
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

/// Files tracked or untracked-but-not-ignored, relative to the repo root.
pub fn list_files(repo: &Path) -> Option<Vec<String>> {
    run(repo, &["ls-files", "-co", "--exclude-standard"])
        .ok()
        .map(|s| s.lines().map(str::to_string).collect())
}

#[derive(Clone, Debug)]
pub struct Commit {
    pub hash: String,
    pub parents: Vec<String>,
    pub author: String,
    pub date: String,
    pub refs: Vec<String>,
    pub subject: String,
}

pub fn log(repo: &Path, limit: usize) -> Result<Vec<Commit>, String> {
    let n = format!("-n{limit}");
    let out = run(
        repo,
        &[
            "log",
            "--all",
            "--topo-order",
            &n,
            "--format=%H%x1f%P%x1f%an%x1f%ad%x1f%D%x1f%s%x1e",
            "--date=format:%Y-%m-%d %H:%M",
        ],
    )?;
    Ok(out
        .split('\x1e')
        .filter_map(|rec| {
            let rec = rec.trim_start_matches('\n');
            let f: Vec<&str> = rec.split('\x1f').collect();
            if f.len() < 6 {
                return None;
            }
            Some(Commit {
                hash: f[0].to_string(),
                parents: f[1].split_whitespace().map(str::to_string).collect(),
                author: f[2].to_string(),
                date: f[3].to_string(),
                refs: f[4]
                    .split(", ")
                    .filter(|r| !r.is_empty())
                    .map(str::to_string)
                    .collect(),
                subject: f[5].to_string(),
            })
        })
        .collect())
}
