//! Source control operations. Callers run CLI and cache work off the UI thread.

use crate::{git, svn};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub use git::{Commit, FileChange, Status};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Vcs {
    Git,
    Svn,
}

pub fn detect(dir: &Path) -> Option<(PathBuf, Vcs)> {
    let git = git::repo_root(dir).map(|p| p.canonicalize().unwrap_or(p));
    let svn = svn::repo_root(dir).map(|p| p.canonicalize().unwrap_or(p));
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

    pub fn diff(self, root: &Path, change: &FileChange, staged: bool) -> String {
        match self {
            Self::Git => git::diff(root, change, staged),
            Self::Svn => svn::diff(root, change),
        }
    }

    pub fn discard(self, root: &Path, change: &FileChange) -> Result<String, String> {
        match self {
            Self::Git => git::discard(root, change),
            Self::Svn => svn::discard(root, change),
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
}
