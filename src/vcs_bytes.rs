//! The same repository sides used by the Changes diff, read without text conversion.

use crate::{
    git,
    hex::Source,
    svn,
    vcs::{FileChange, Vcs},
};
use std::path::Path;

impl Vcs {
    pub(crate) fn binary_sides(self, root: &Path, change: &FileChange, staged: bool) -> Result<crate::hex::Pair, String> {
        let path = &change.path;
        let left = if matches!(change.code, 'A' | '?') {
            Source::empty()
        } else if self == Self::Git {
            let old = if staged && matches!(change.code, 'R' | 'C') {
                staged_old_path(root, path)?
            } else {
                None
            };
            git_blob(
                root,
                &format!("{}:{}", if staged { "HEAD" } else { "" }, old.as_deref().unwrap_or(path)),
            )?
        } else {
            svn_blob(root, "BASE", path)?
        };
        let right = if change.code == 'D' {
            Source::empty()
        } else if staged {
            git_blob(root, &format!(":{path}"))?
        } else {
            Source::open(&root.join(path)).map_err(|err| err.to_string())?
        };
        Ok(crate::hex::Pair { left, right })
    }
}

pub(crate) fn svn_blob(root: &Path, revision: &str, path: &str) -> Result<Source, String> {
    let mut command = svn::command(root, "svn");
    command.args(["--non-interactive", "cat", "-r", revision, "--", &svn::target(path)]);
    Source::from_command(command)
}

pub(crate) fn git_blob(root: &Path, revision: &str) -> Result<Source, String> {
    let mut command = git::cmd(root);
    command.args(["show", "--no-textconv", revision]);
    Source::from_command(command)
}

fn staged_old_path(root: &Path, path: &str) -> Result<Option<String>, String> {
    let output = git::run(root, &["diff", "--cached", "--name-status", "-z", "-M", "-C"])?;
    let mut fields = output.split_terminator('\0');
    while let Some(status) = fields.next() {
        let Some(first) = fields.next() else { break };
        if status.starts_with(['R', 'C']) && fields.next() == Some(path) {
            return Ok(Some(first.to_owned()));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_binary_diff_reads_bytes_from_head_index_and_worktree() {
        let root = std::env::temp_dir().join(format!("edittr-vcs-bytes-{}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        let run = |args: &[&str]| git::run(&root, args).unwrap();
        run(&["init"]);
        run(&["config", "user.email", "edittr@example.com"]);
        run(&["config", "user.name", "Edittr"]);
        let file = root.join("binary");
        let head = [0, 255, 1];
        let index = [0, 254, 1];
        let work = [0, 253, 1, 2];
        std::fs::write(&file, head).unwrap();
        run(&["add", "."]);
        run(&["commit", "-m", "Binary"]);
        std::fs::write(&file, index).unwrap();
        run(&["add", "."]);
        std::fs::write(&file, work).unwrap();
        let change = FileChange {
            path: "binary".into(),
            code: 'M',
        };
        let staged = Vcs::Git.binary_sides(&root, &change, true).unwrap();
        assert_eq!(staged.left.read(0..8).unwrap(), head);
        assert_eq!(staged.right.read(0..8).unwrap(), index);
        let unstaged = Vcs::Git.binary_sides(&root, &change, false).unwrap();
        assert_eq!(unstaged.left.read(0..8).unwrap(), index);
        assert_eq!(unstaged.right.read(0..8).unwrap(), work);
        drop(unstaged);
        drop(staged);
        std::fs::remove_dir_all(root).unwrap();
    }
}
