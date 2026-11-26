//! Partial index updates. All Git commands and patch preparation run on the VCS worker.

use super::{FileChange, cmd, diff, run};
use crate::diff::{Hunk, changed_blocks, patch_block, with_editor_eol, without_cr_at_eol};
use std::fmt::Write as _;
use std::io::Write as _;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

const STALE: &str = "The comparison changed. Refresh it and try again.";
const OVERLAP: &str = "This hunk overlaps another staged edit. Stage it from the working-copy diff.";

pub(crate) enum BufferBase {
    Committed { text: Arc<str>, full_new: Range<usize> },
    Index(String),
}

struct Blob {
    mode: String,
    text: String,
}

impl Blob {
    fn from_oid(repo: &Path, mode: &str, oid: &str) -> Result<Self, String> {
        if !matches!(mode, "100644" | "100755") {
            return Err("Hunk actions require a regular text file".into());
        }
        Ok(Self {
            mode: mode.into(),
            text: text_output(cmd(repo).args(["cat-file", "blob", oid]))?,
        })
    }
}

fn text_output(command: &mut Command) -> Result<String, String> {
    output_text(command.output().map_err(|err| err.to_string())?)
}

fn output_text(output: std::process::Output) -> Result<String, String> {
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_owned());
    }
    String::from_utf8(output.stdout).map_err(|_| "Hunk actions require UTF-8 text".into())
}

fn with_input(command: &mut Command, input: &str) -> Result<String, String> {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| err.to_string())?;
    let mut stdin = child.stdin.take().unwrap();
    std::thread::scope(|scope| {
        let writer = scope.spawn(move || stdin.write_all(input.as_bytes()));
        let output = child.wait_with_output().map_err(|err| err.to_string());
        let written = writer.join().unwrap();
        output
            .and_then(output_text)
            .and_then(|out| written.map(|()| out).map_err(|err| err.to_string()))
    })
}

fn index_entry(repo: &Path, path: &str) -> Result<Option<(String, String)>, String> {
    let listing = run(repo, &["--literal-pathspecs", "ls-files", "--stage", "-z", "--", path])?;
    let mut entries = listing.split_terminator('\0');
    let Some(entry) = entries.next() else { return Ok(None) };
    let (meta, listed_path) = entry.split_once('\t').ok_or(STALE)?;
    let mut fields = meta.split_whitespace();
    let (Some(mode), Some(oid), Some("0")) = (fields.next(), fields.next(), fields.next()) else {
        return Err("Resolve the merge conflict before applying a hunk".into());
    };
    if listed_path != path || entries.next().is_some() {
        return Err(STALE.into());
    }
    Ok(Some((mode.into(), oid.into())))
}

fn head_blob(repo: &Path, path: &str) -> Result<Option<Blob>, String> {
    let listing = run(repo, &["--literal-pathspecs", "ls-tree", "-z", "HEAD", "--", path])?;
    let Some((meta, listed_path)) = listing.trim_end_matches('\0').split_once('\t') else {
        return Ok(None);
    };
    let mut fields = meta.split_whitespace();
    let (Some(mode), Some("blob"), Some(oid)) = (fields.next(), fields.next(), fields.next()) else {
        return Err("Hunk actions require a regular text file".into());
    };
    if listed_path != path {
        return Ok(None);
    }
    Blob::from_oid(repo, mode, oid).map(Some)
}

/// Apply Git's clean and EOL conversion in a temporary object directory. Only the actual
/// partial patch reaches the repository, even if the buffer contains other unsaved edits.
fn clean_text(repo: &Path, path: &str, text: &str) -> Result<String, String> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    struct Objects(PathBuf);
    impl Drop for Objects {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    let objects = loop {
        let path = std::env::temp_dir().join(format!(
            "edittr-hunk-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        match builder.create(&path) {
            Ok(()) => break Objects(path),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err.to_string()),
        }
    };
    let oid = with_input(
        cmd(repo)
            .env("GIT_OBJECT_DIRECTORY", &objects.0)
            .args(["hash-object", "-w", "--stdin", "--path", path]),
        text,
    )?;
    text_output(
        cmd(repo)
            .env("GIT_OBJECT_DIRECTORY", &objects.0)
            .args(["cat-file", "blob", oid.trim()]),
    )
}

/// Translate a committed range through the already-staged changes, keeping neighbouring
/// staged inserts outside the replacement. An overlapping partial replacement is ambiguous.
fn index_range(base: &str, index: &str, target: &Range<usize>, full: &[&str], selected: Range<usize>) -> Result<Range<usize>, String> {
    let mut old_end = 0;
    let mut new_end = 0;
    let mut start = None;
    let mut end = None;
    for h in changed_blocks(base, index) {
        if h.old.is_empty() && target.is_empty() && h.old.start == target.start {
            // Staging another portion of an addition is safe when the index lines appear
            // in the incoming addition in the same order.
            let index_lines: Vec<_> = index.split_inclusive('\n').collect();
            let mut incoming = full.iter().enumerate();
            let (mut start, mut end) = (h.new.start, h.new.start);
            for line in &index_lines[h.new] {
                let Some((i, _)) = incoming.find(|(_, candidate)| candidate == &line) else {
                    return Err(OVERLAP.into());
                };
                start += usize::from(i < selected.start);
                end += usize::from(i < selected.end);
            }
            return Ok(start..end);
        }
        if target.start < h.old.start || (target.start == h.old.start && !h.old.is_empty()) {
            start.get_or_insert_with(|| new_end + (target.start - old_end));
        } else if start.is_none() && target.start < h.old.end {
            if h.old.len() != h.new.len() {
                return Err(OVERLAP.into());
            }
            start = Some(h.new.start + target.start - h.old.start);
        }
        if target.end <= h.old.start {
            end = Some(new_end + (target.end - old_end));
            break;
        }
        if target.end < h.old.end {
            if h.old.len() != h.new.len() {
                return Err(OVERLAP.into());
            }
            end = Some(h.new.start + target.end - h.old.start);
            break;
        }
        (old_end, new_end) = (h.old.end, h.new.end);
    }
    let end = end.unwrap_or_else(|| new_end + (target.end - old_end));
    let start = start.unwrap_or_else(|| new_end + (target.start - old_end));
    Ok(start..end)
}

/// Quote both patch paths in Git's C-style syntax, including whitespace and literal backslashes.
fn patch_path(side: char, path: &str) -> String {
    let mut quoted = format!("\"{side}/");
    for byte in path.bytes() {
        match byte {
            b'"' | b'\\' => {
                quoted.push('\\');
                quoted.push(char::from(byte));
            }
            0x20..=0x7e => quoted.push(char::from(byte)),
            _ => {
                let _ = write!(quoted, "\\{byte:03o}");
            }
        }
    }
    quoted.push('"');
    quoted
}

fn apply(
    repo: &Path,
    path: &str,
    expected: Option<(String, String)>,
    old: &[&str],
    target: Range<usize>,
    insert: &[&str],
    mode: Option<(&str, bool)>,
) -> Result<String, String> {
    if target.start > target.end || target.end > old.len() {
        return Err(STALE.into());
    }
    // A partial patch must never join a non-terminated line to a neighbouring
    // indexed line, even though git apply will accept such a patch.
    if !insert.is_empty()
        && ((target.start > 0 && !old[target.start - 1].ends_with('\n'))
            || (target.end < old.len() && insert.last().is_some_and(|line| !line.ends_with('\n'))))
    {
        return Err(OVERLAP.into());
    }
    if old[target.clone()] == *insert && mode.is_none() {
        return Ok("Hunk is already staged".into());
    }
    let a = patch_path('a', path);
    let b = patch_path('b', path);
    let mut patch = format!("diff --git {a} {b}\n");
    let (mut from, mut to) = (a.as_str(), b.as_str());
    if let Some((mode, create)) = mode {
        if create {
            let _ = writeln!(patch, "new file mode {mode}");
            from = "/dev/null";
        } else {
            let _ = writeln!(patch, "deleted file mode {mode}");
            to = "/dev/null";
        }
    }
    let _ = writeln!(patch, "--- {from}\n+++ {to}");
    patch.push_str(&patch_block(old, target.start, target.end, insert));
    if index_entry(repo, path)? != expected {
        return Err(STALE.into());
    }
    let mut git = cmd(repo);
    git.args(["-c", "apply.ignoreWhitespace=no", "apply", "--cached", "--whitespace=nowarn"]);
    if old.is_empty() {
        git.arg("--unidiff-zero");
    }
    with_input(git.arg("-"), &patch)
}

pub(crate) fn stage_buffer_hunk(repo: &Path, path: &str, base: BufferBase, text: &str, crlf: bool, hunk: Hunk) -> Result<String, String> {
    let entry = index_entry(repo, path)?;
    let index = entry.as_ref().map(|(mode, oid)| Blob::from_oid(repo, mode, oid)).transpose()?;
    let indexed = index.as_ref().map_or("", |index| index.text.as_str());
    let cleaned = clean_text(repo, path, &with_editor_eol(text, crlf))?;
    if cleaned.contains('\0') || without_cr_at_eol(&cleaned, true) != text {
        return Err("Git's clean filter changes the buffer's line content".into());
    }
    let lines: Vec<_> = cleaned.split_inclusive('\n').collect();
    let insert = lines.get(hunk.new.clone()).ok_or(STALE)?;
    let mut create_mode = "100644".to_owned();
    let target = match base {
        BufferBase::Committed { text: base, full_new } => {
            let head = head_blob(repo, path)?.ok_or(STALE)?;
            if without_cr_at_eol(&head.text, true) != base.as_ref() {
                return Err(STALE.into());
            }
            create_mode = head.mode;
            let editor_lines: Vec<_> = text.split_inclusive('\n').collect();
            let offset = full_new.start;
            let selection = hunk.new.start - offset..hunk.new.end - offset;
            index_range(
                &base,
                &without_cr_at_eol(indexed, true),
                &hunk.old,
                editor_lines.get(full_new).ok_or(STALE)?,
                selection,
            )?
        }
        BufferBase::Index(base) => {
            if base != indexed {
                return Err(STALE.into());
            }
            if index.is_none() {
                create_mode = worktree_mode(repo, path)?;
            }
            hunk.old
        }
    };
    let old: Vec<_> = indexed.split_inclusive('\n').collect();
    apply(
        repo,
        path,
        entry,
        &old,
        target,
        insert,
        index.is_none().then_some((create_mode.as_str(), true)),
    )
}

fn worktree_mode(repo: &Path, path: &str) -> Result<String, String> {
    let meta = std::fs::symlink_metadata(repo.join(path)).map_err(|err| err.to_string())?;
    if !meta.is_file() {
        return Err("Hunk actions require a regular text file".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o100 != 0 && run(repo, &["config", "--bool", "core.filemode"]).is_ok_and(|v| v.trim() == "true") {
            return Ok("100755".into());
        }
    }
    Ok("100644".into())
}

pub(crate) fn apply_diff_hunk(
    repo: &Path,
    change: &FileChange,
    staged: bool,
    ignore_eol: bool,
    snapshot: &str,
    hunk: Hunk,
) -> Result<String, String> {
    let path = &change.path;
    let entry = index_entry(repo, path)?;
    let index = entry.as_ref().map(|(mode, oid)| Blob::from_oid(repo, mode, oid)).transpose()?;
    let other = if staged {
        if change.code == 'A' { None } else { head_blob(repo, path)? }
    } else if change.code == 'D' {
        None
    } else {
        let mode = worktree_mode(repo, path)?;
        let text = std::fs::read_to_string(repo.join(path)).map_err(|err| err.to_string())?;
        Some(Blob {
            mode,
            text: clean_text(repo, path, &text)?,
        })
    };
    if diff(repo, change, staged, ignore_eol).text != snapshot {
        return Err(STALE.into());
    }
    let old: Vec<_> = index.as_ref().map_or("", |blob| &blob.text).split_inclusive('\n').collect();
    let new: Vec<_> = other.as_ref().map_or("", |blob| &blob.text).split_inclusive('\n').collect();
    let (target, source) = if staged { (hunk.new, hunk.old) } else { (hunk.old, hunk.new) };
    let insert = new.get(source).ok_or(STALE)?;
    let mode = match (&index, &other) {
        (None, Some(other)) => Some((other.mode.as_str(), true)),
        (Some(index), None) if target.len() == old.len() => Some((index.mode.as_str(), false)),
        _ => None,
    };
    apply(repo, path, entry, &old, target, insert, mode)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::side_by_side;

    struct Repo(PathBuf);

    impl Repo {
        fn new() -> Self {
            let unique = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!("edittr-hunks-{}-{unique}", std::process::id()));
            std::fs::create_dir(&path).unwrap();
            let repo = Self(path);
            repo.git(&["init"]);
            repo.git(&["config", "user.email", "edittr@example.invalid"]);
            repo.git(&["config", "user.name", "Edittr"]);
            repo.git(&["config", "core.hooksPath", ".git/test-hooks"]);
            repo.git(&["config", "commit.gpgsign", "false"]);
            repo.git(&["config", "core.autocrlf", "false"]);
            repo
        }

        fn git(&self, args: &[&str]) -> String {
            run(&self.0, args).unwrap()
        }

        fn write(&self, path: &str, text: &str) {
            std::fs::write(self.0.join(path), text).unwrap();
        }

        fn apply(&self, path: &str, code: char, staged: bool, block: usize) {
            let change = FileChange { path: path.into(), code };
            let patch = diff(&self.0, &change, staged, true).text;
            let hunk = side_by_side(&patch).blocks[block].1.clone();
            apply_diff_hunk(&self.0, &change, staged, true, &patch, hunk).unwrap();
        }
    }

    impl Drop for Repo {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn stages_crlf_from_the_buffer_without_touching_other_staged_edits_or_disk() {
        for (attribute, stored_crlf) in [("-text", true), ("text eol=crlf", false)] {
            let repo = Repo::new();
            let path = "file [test] é.txt";
            let base = "a\nb\nc\nd\ne\nlast\n";
            repo.write(".gitattributes", &format!("*.txt {attribute}\n"));
            repo.write(path, &with_editor_eol(base, true));
            repo.git(&["add", "--", ".gitattributes", path]);
            repo.git(&["commit", "-m", "Base"]);
            let disk = "staged\na\nb\nc\nd\ne\nlast\n";
            repo.write(path, &with_editor_eol(disk, true));
            repo.git(&["add", "--", path]);
            let editor = "a\nB\nc\nd\ne\nLAST";
            let blocks = changed_blocks(base, editor);
            stage_buffer_hunk(
                &repo.0,
                path,
                BufferBase::Committed {
                    text: base.into(),
                    full_new: blocks[1].new.clone(),
                },
                editor,
                true,
                blocks[1].clone(),
            )
            .unwrap();
            let index = format!(":{path}");
            assert_eq!(
                repo.git(&["show", &index]),
                with_editor_eol("staged\na\nb\nc\nd\ne\nLAST", stored_crlf)
            );
            assert_eq!(std::fs::read_to_string(repo.0.join(path)).unwrap(), with_editor_eol(disk, true));
            repo.apply(path, 'M', true, 1);
            assert_eq!(repo.git(&["show", &index]), with_editor_eol(disk, stored_crlf));
        }
    }

    #[test]
    fn partial_replacements_stage_independently() {
        let repo = Repo::new();
        let path = "x.txt";
        let base = "a\nb\nc\nd\n";
        let edited = "a\nB\nC\nd\n";
        repo.write(path, base);
        repo.git(&["add", "--", path]);
        repo.git(&["commit", "-m", "Base"]);
        let block = changed_blocks(base, edited).remove(0);
        for lines in [2..3, 1..2] {
            let partial = block.selected(&lines).unwrap();
            stage_buffer_hunk(
                &repo.0,
                path,
                BufferBase::Committed {
                    text: base.into(),
                    full_new: block.new.clone(),
                },
                edited,
                false,
                partial,
            )
            .unwrap();
            let expected = if lines.start == 2 { "a\nb\nC\nd\n" } else { edited };
            assert_eq!(repo.git(&["show", ":x.txt"]), expected);
        }
        assert_eq!(std::fs::read_to_string(repo.0.join(path)).unwrap(), base);
    }

    #[test]
    fn stages_selected_insertions_in_their_original_order() {
        let repo = Repo::new();
        let path = "x.txt";
        let base = "a\nb\n";
        let edited = "a\nfirst\nmiddle\nlast\nb\n";
        repo.write(path, base);
        repo.git(&["add", "--", path]);
        repo.git(&["commit", "-m", "Base"]);
        let block = changed_blocks(base, edited).remove(0);
        for (lines, expected) in [(3..4, "a\nlast\nb\n"), (1..2, "a\nfirst\nlast\nb\n"), (2..3, edited)] {
            let hunk = block.selected(&lines).unwrap();
            let base = BufferBase::Committed {
                text: base.into(),
                full_new: block.new.clone(),
            };
            stage_buffer_hunk(&repo.0, path, base, edited, false, hunk).unwrap();
            assert_eq!(repo.git(&["show", ":x.txt"]), expected);
        }
    }

    #[test]
    fn stages_and_unstages_added_and_deleted_paths() {
        let repo = Repo::new();
        let path = "new.txt";
        repo.write(path, "one\ntwo");
        let hunk = Hunk { old: 0..0, new: 0..1 };
        stage_buffer_hunk(&repo.0, path, BufferBase::Index(String::new()), "one\ntwo", false, hunk).unwrap();
        assert_eq!(repo.git(&["show", ":new.txt"]), "one\n");
        repo.apply(path, 'A', true, 0);
        assert!(index_entry(&repo.0, path).unwrap().is_none());
        repo.git(&["add", "--", path]);
        repo.git(&["commit", "-m", "Add"]);
        std::fs::remove_file(repo.0.join(path)).unwrap();
        repo.apply(path, 'D', false, 0);
        assert!(index_entry(&repo.0, path).unwrap().is_none());
        repo.apply(path, 'D', true, 0);
        assert_eq!(repo.git(&["show", ":new.txt"]), "one\ntwo");
        assert!(!repo.0.join(path).exists());
    }

    #[test]
    fn rejects_a_stale_index_snapshot() {
        let repo = Repo::new();
        let path = "x.txt";
        repo.write(path, "index\n");
        repo.git(&["add", "--", path]);
        let hunk = Hunk { old: 0..1, new: 0..1 };
        let err = stage_buffer_hunk(&repo.0, path, BufferBase::Index("previous\n".into()), "new\n", false, hunk).unwrap_err();
        assert_eq!(err, STALE);
        assert_eq!(repo.git(&["show", ":x.txt"]), "index\n");
    }

    #[test]
    fn missing_final_newline_cannot_join_an_unrelated_staged_line() {
        let repo = Repo::new();
        let path = "x.txt";
        let base = "a\nlast\n";
        let indexed = "a\nlast\nstaged-tail\n";
        repo.write(path, base);
        repo.git(&["add", "--", path]);
        repo.git(&["commit", "-m", "Base"]);
        repo.write(path, indexed);
        repo.git(&["add", "--", path]);
        let edited = "a\nLAST";
        let hunk = changed_blocks(base, edited).remove(0);
        let base = BufferBase::Committed {
            text: base.into(),
            full_new: hunk.new.clone(),
        };
        assert_eq!(stage_buffer_hunk(&repo.0, path, base, edited, false, hunk).unwrap_err(), OVERLAP);
        assert_eq!(repo.git(&["show", ":x.txt"]), indexed);
    }
}
