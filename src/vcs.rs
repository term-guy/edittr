//! Source control operations. Callers run CLI and cache work off the UI thread.

use crate::{diff::FileDiff, git, svn};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub use git::{Commit, FileChange, Stash, StashAction, Status};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Vcs {
    Git,
    Svn,
}

#[derive(Clone, Debug)]
pub enum HistoryAction {
    Git(git::HistoryAction),
    UpdateRevision(u64),
}

impl HistoryAction {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Git(action) => action.label(),
            Self::UpdateRevision(_) => "Update working copy",
        }
    }

    pub fn run(&self, vcs: Vcs, root: &Path) -> Result<String, String> {
        match (vcs, self) {
            (Vcs::Git, Self::Git(action)) => action.run(root),
            (Vcs::Svn, Self::UpdateRevision(revision)) => svn::update_revision(root, *revision),
            _ => Err("This action is unavailable for the current repository".into()),
        }
    }
}

pub fn detect(dir: &Path) -> Option<(PathBuf, Vcs)> {
    let git = git::repo_root(dir).map(|p| crate::workspace::canonical(&p).unwrap_or(p));
    let svn = svn::repo_root(dir).map(|p| crate::workspace::canonical(&p).unwrap_or(p));
    nearest(git, svn)
}

fn nearest(git: Option<PathBuf>, svn: Option<PathBuf>) -> Option<(PathBuf, Vcs)> {
    match (git, svn) {
        (Some(git), Some(svn)) if svn != git && svn.starts_with(&git) => Some((svn, Vcs::Svn)),
        (Some(git), _) => Some((git, Vcs::Git)),
        (None, Some(svn)) => Some((svn, Vcs::Svn)),
        (None, None) => None,
    }
}

/// Folders searched below the project for nested repositories (submodules, independent clones).
const DISCOVER_DEPTH: usize = 8;
const DISCOVER_DIRS: usize = 20_000;
/// Repositories shown at most (status runs for each on every refresh).
pub const MAX_REPOSITORIES: usize = 64;

/// The repositories of a project folder: the one containing `root` (if any) first, then
/// those nested inside it, by path. `ignored` holds root-relative folders to skip.
pub fn discover(root: &Path, ignored: &[String]) -> Vec<(PathBuf, Vcs)> {
    let mut found: Vec<(PathBuf, Vcs)> = detect(root).into_iter().collect();
    let mut nested = nested_repositories(root, ignored);
    nested.sort_by(|a, b| a.0.cmp(&b.0));
    for repo in nested {
        if found.len() >= MAX_REPOSITORIES {
            break;
        }
        if !found.iter().any(|(path, _)| *path == repo.0) {
            found.push(repo);
        }
    }
    found
}

/// Folders below `root` (not `root` itself) holding a `.git` (folder, or file for submodules
/// and worktrees) or the top `.svn` of a working copy.
fn nested_repositories(root: &Path, ignored: &[String]) -> Vec<(PathBuf, Vcs)> {
    let mut out = Vec::new();
    // (folder, depth, inside a Subversion working copy: pre-1.7 copies have `.svn` in every folder)
    let mut stack = vec![(root.to_path_buf(), 0, false)];
    let mut visited = 0;
    while let Some((dir, depth, in_svn)) = stack.pop() {
        visited += 1;
        if visited > DISCOVER_DIRS {
            break;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        let (mut git, mut svn) = (false, false);
        let mut subdirs = Vec::new();
        for entry in entries.flatten() {
            // Symbolic links are not followed: they can loop or leave the project.
            let Ok(kind) = entry.file_type() else { continue };
            let name = entry.file_name();
            if name == ".git" {
                git |= kind.is_dir() || kind.is_file();
            } else if name == ".svn" {
                svn |= kind.is_dir();
            } else if kind.is_dir() && depth < DISCOVER_DEPTH {
                let skip = name.to_str().is_none_or(|name| crate::watch::IGNORED_DIRS.contains(&name));
                let path = entry.path();
                if !skip && crate::workspace::relative_path(root, &path).is_none_or(|rel| !crate::workspace::is_ignored(&rel, ignored)) {
                    subdirs.push(path);
                }
            }
        }
        if dir != root {
            let vcs = if git {
                Some(Vcs::Git)
            } else {
                (svn && !in_svn).then_some(Vcs::Svn)
            };
            if let Some(vcs) = vcs {
                out.push((crate::workspace::canonical(&dir).unwrap_or(dir.clone()), vcs));
            }
        }
        let in_svn = svn && !git;
        stack.extend(subdirs.into_iter().map(|path| (path, depth + 1, in_svn)));
    }
    out
}

pub fn init_git(dir: &Path) -> Result<String, String> {
    git::run(dir, &["init"])
}

impl Vcs {
    pub fn name(self) -> &'static str {
        match self {
            Self::Git => "Git",
            Self::Svn => "Subversion",
        }
    }

    pub fn history_title(self) -> &'static str {
        match self {
            Self::Git => "Git Graph",
            Self::Svn => "History",
        }
    }

    pub fn has_index(self) -> bool {
        self == Self::Git
    }

    pub fn status(self, root: &Path) -> Result<Status, String> {
        let mut status = match self {
            Self::Git => git::status(root),
            Self::Svn => svn::status(root),
        }?;
        status
            .unstaged
            .retain(|change| change.code != '?' || !crate::clangd::is_index_path(Path::new(&change.path)));
        Ok(status)
    }

    pub fn stashes(self, root: &Path) -> Result<Vec<Stash>, String> {
        match self {
            Self::Git => git::stashes(root),
            Self::Svn => Ok(Vec::new()),
        }
    }

    pub fn stash_push(self, root: &Path, message: &str, include_untracked: bool) -> Result<String, String> {
        match self {
            Self::Git => git::stash_push(root, message, include_untracked),
            Self::Svn => Err("Stashes are available only in Git repositories".into()),
        }
    }

    pub fn stash_action(self, root: &Path, stash: &Stash, action: StashAction) -> Result<String, String> {
        match self {
            Self::Git => git::stash_action(root, stash, action),
            Self::Svn => Err("Stashes are available only in Git repositories".into()),
        }
    }

    pub fn diff(self, root: &Path, change: &FileChange, staged: bool, ignore_eol: bool) -> FileDiff {
        match self {
            Self::Git => git::diff(root, change, staged, ignore_eol),
            Self::Svn => {
                let text = svn::diff(root, change, ignore_eol);
                let unfiltered = (ignore_eol && text.is_empty()).then(|| svn::diff(root, change, false));
                FileDiff::new(text, unfiltered.as_deref())
            }
        }
    }

    pub fn discard(self, root: &Path, change: &FileChange) -> Result<String, String> {
        match self {
            Self::Git => git::discard(root, change),
            Self::Svn => svn::discard(root, change),
        }
    }

    /// Discard several changes at once (a folder): batched where the VCS allows it.
    pub fn discard_many(self, root: &Path, changes: &[FileChange]) -> Result<String, String> {
        match self {
            Self::Git => git::discard_many(root, changes),
            Self::Svn => {
                let errors: Vec<String> = changes.iter().filter_map(|c| svn::discard(root, c).err()).collect();
                if errors.is_empty() {
                    Ok(String::new())
                } else {
                    Err(errors.join("\n"))
                }
            }
        }
    }

    pub fn commit(self, root: &Path, msg: &str, amend: bool) -> Result<String, String> {
        match self {
            Self::Git => git::commit(root, msg, amend),
            Self::Svn if !amend => svn::commit(root, msg),
            Self::Svn => Err("Subversion does not support amending commits".into()),
        }
    }

    pub fn stage(self, root: &Path, path: Option<&str>, staged: bool) -> Result<String, String> {
        match (self, path, staged) {
            (Self::Git, Some(path), true) => git::stage(root, path),
            (Self::Git, Some(path), false) => git::unstage(root, path),
            (Self::Git, None, true) => git::stage_all(root),
            (Self::Git, None, false) => git::unstage_all(root),
            (Self::Svn, _, _) => Err("Subversion has no staging area".into()),
        }
    }

    pub(crate) fn stage_buffer_hunk(
        self,
        root: &Path,
        path: &str,
        base: git::BufferBase,
        text: &str,
        crlf: bool,
        hunk: crate::diff::Hunk,
    ) -> Result<String, String> {
        match self {
            Self::Git => git::stage_buffer_hunk(root, path, base, text, crlf, hunk),
            Self::Svn => Err("Subversion has no staging area".into()),
        }
    }

    pub(crate) fn apply_diff_hunk(
        self,
        root: &Path,
        change: &FileChange,
        staged: bool,
        ignore_eol: bool,
        snapshot: &str,
        hunk: crate::diff::Hunk,
    ) -> Result<String, String> {
        match self {
            Self::Git => git::apply_diff_hunk(root, change, staged, ignore_eol, snapshot, hunk),
            Self::Svn => Err("Subversion has no staging area".into()),
        }
    }

    pub fn branches(self, root: &Path) -> Vec<String> {
        match self {
            Self::Git => git::branches(root),
            Self::Svn => Vec::new(),
        }
    }

    pub fn switch(self, root: &Path, branch: &str) -> Result<String, String> {
        match self {
            Self::Git => git::switch(root, branch),
            Self::Svn => Err("Switching Subversion branches is unavailable".into()),
        }
    }

    pub fn remote(self, root: &Path, args: &[&str]) -> Result<String, String> {
        match self {
            Self::Git => git::run(root, args),
            Self::Svn => Err("This remote operation is only available for Git".into()),
        }
    }

    pub fn log(self, root: &Path, limit: usize, cached: impl FnOnce(Vec<Commit>)) -> Result<Vec<Commit>, String> {
        match self {
            Self::Git => git::log(root, limit),
            Self::Svn => svn::log(root, limit, false, cached),
        }
    }

    pub(crate) fn path_history(
        self,
        root: &Path,
        path: &str,
        directory: bool,
        lines: Option<std::ops::RangeInclusive<usize>>,
    ) -> Result<Vec<crate::file_history::Entry>, String> {
        crate::file_history::log(self, root, path, directory, lines)
    }

    pub(crate) fn path_history_patch(self, root: &Path, path: &str, entry: &crate::file_history::Entry) -> Result<String, String> {
        crate::file_history::patch(root, path, entry)
    }

    pub(crate) fn path_history_binary_sides(
        self,
        root: &Path,
        path: &str,
        entry: &crate::file_history::Entry,
    ) -> Result<crate::hex::Pair, String> {
        crate::file_history::binary_patch_sides(root, path, entry)
    }

    pub fn refresh_log(self, root: &Path, limit: usize, cached: impl FnOnce(Vec<Commit>)) -> Result<Vec<Commit>, String> {
        match self {
            Self::Git => self.log(root, limit, cached),
            Self::Svn => svn::log(root, limit, true, cached),
        }
    }

    /// The committed version of `rel` (git: HEAD, svn: the local pristine BASE copy),
    /// or None when the file isn't versioned. Both are local operations.
    pub fn base_text(self, root: &Path, rel: &str) -> Option<String> {
        match self {
            Self::Git => git::run(root, &["show", &format!("HEAD:{rel}")]).ok(),
            Self::Svn => svn::cat_base(root, rel).ok(),
        }
    }

    /// What a working-copy diff compares against (git: the index, svn: BASE); local.
    pub fn diff_base(self, root: &Path, rel: &str) -> Option<String> {
        match self {
            Self::Git => git::run(root, &["show", &format!(":{rel}")]).ok(),
            Self::Svn => svn::cat_base(root, rel).ok(),
        }
    }

    pub fn show_commit(self, root: &Path, rev: &str) -> String {
        match self {
            Self::Git => git::show_commit(root, rev),
            Self::Svn => svn::show_commit(root, rev).unwrap_or_else(|e| e),
        }
    }
}

pub fn list_files(dir: &Path, vcs: Option<Vcs>) -> Option<Vec<String>> {
    match vcs {
        Some(Vcs::Git) => git::list_files(dir),
        Some(Vcs::Svn) => svn::list_files(dir),
        None => None,
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct BlameLine {
    pub rev: String,
    pub author: String,
    /// UTC timestamp. Older cached blame entries contain only YYYY-MM-DD.
    pub date: String,
    pub summary: String,
}

pub fn blame(vcs: Vcs, root: &Path, rel: &str) -> Result<Vec<BlameLine>, String> {
    match vcs {
        Vcs::Svn => svn::blame(root, rel),
        Vcs::Git => Err("use blame::git for Git (it blames the live buffer)".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn innermost_root_wins_with_git_preferred_at_the_same_root() {
        let outer = PathBuf::from("project");
        let inner = outer.join("nested");
        let git = |path: &Path| Some((path.to_path_buf(), Vcs::Git));
        let svn = |path: &Path| Some((path.to_path_buf(), Vcs::Svn));
        assert!(nearest(Some(outer.clone()), Some(outer.clone())) == git(&outer));
        assert!(nearest(Some(outer.clone()), Some(inner.clone())) == svn(&inner));
        assert!(nearest(Some(inner.clone()), Some(outer.clone())) == git(&inner));
        assert!(nearest(None, Some(outer.clone())) == svn(&outer));
        assert!(nearest(Some(outer.clone()), None) == git(&outer));
        assert!(nearest(None, None).is_none());
    }

    #[test]
    fn discover_finds_nested_repositories_and_skips_ignored_folders() {
        let root = std::env::temp_dir().join(format!("edittr-discover-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for dir in [
            "app/.git",
            "libs/core/.git",
            "libs/core/vendor/x/.git",
            "node_modules/dep/.git",
            "skipped/inner/.git",
        ] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        // A submodule's `.git` is a file.
        std::fs::create_dir_all(root.join("libs/sub")).unwrap();
        std::fs::write(root.join("libs/sub/.git"), "gitdir: ../../.git/modules/sub\n").unwrap();
        std::fs::create_dir_all(root.join("docs/.svn")).unwrap();
        let mut found = nested_repositories(&root, &["skipped".to_string()]);
        found.sort_by(|a, b| a.0.cmp(&b.0));
        let root = crate::workspace::canonical(&root).unwrap();
        let rel: Vec<_> = found
            .iter()
            .map(|(path, vcs)| {
                (
                    path.strip_prefix(&root).unwrap().to_string_lossy().replace('\\', "/"),
                    *vcs == Vcs::Git,
                )
            })
            .collect();
        let expected = [
            ("app", true),
            ("docs", false),
            ("libs/core", true),
            ("libs/core/vendor/x", true),
            ("libs/sub", true),
        ];
        assert_eq!(rel, expected.map(|(p, git)| (p.to_string(), git)));
        std::fs::remove_dir_all(&root).unwrap();
    }
}
