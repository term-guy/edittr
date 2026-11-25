//! Path and line history, including the historical names on either side of a rename.

use crate::{
    blame, git,
    hex::{Pair, Source},
    svn,
    vcs::Vcs,
    vcs_bytes::{git_blob, svn_blob},
};
use std::ops::RangeInclusive;
use std::path::Path;

const FORMAT: &str = "--format=%x00%H%x00%h%x00%an%x00%at%x00%s";
const CONTEXT: &str = "--unified=1000000";

pub(crate) struct Entry {
    pub hash: String,
    pub short_hash: String,
    pub author: String,
    pub date: String,
    pub subject: String,
    change: Change,
}

enum Change {
    Git {
        path: String,
        old_path: Option<String>,
        code: Option<char>,
    },
    Lines(String),
    Svn,
}

impl Entry {
    pub(crate) fn svn(number: u64, author: String, date: String, subject: String) -> Self {
        let hash = format!("r{number}");
        Self {
            short_hash: hash.clone(),
            hash,
            author,
            date,
            subject,
            change: Change::Svn,
        }
    }
}

pub(crate) fn log(vcs: Vcs, root: &Path, path: &str, directory: bool, lines: Option<RangeInclusive<usize>>) -> Result<Vec<Entry>, String> {
    match (vcs, lines) {
        (Vcs::Git, Some(lines)) => {
            let range = format!("-L{},{}:{path}", lines.start(), lines.end());
            let format = format!("{FORMAT}%x00");
            git::run(root, &["log", "--no-color", "--no-ext-diff", "--no-textconv", &format, &range]).map(|output| parse_lines(&output))
        }
        (Vcs::Git, None) => {
            let mut args = vec!["--literal-pathspecs", "log", "--no-color", FORMAT, "-z", "--name-status"];
            if !directory {
                args.push("--follow");
            }
            args.extend(["--", path]);
            git::run(root, &args).map(|output| parse_paths(&output, path, directory))
        }
        (Vcs::Svn, None) => svn::path_history(root, path),
        (Vcs::Svn, Some(_)) => Err("History for selected lines is available in Git repositories".into()),
    }
}

pub(crate) fn patch(root: &Path, path: &str, entry: &Entry) -> Result<String, String> {
    match &entry.change {
        Change::Git { path, old_path, .. } => {
            let mut args = vec![
                "--literal-pathspecs",
                "show",
                "--first-parent",
                "--diff-merges=first-parent",
                "--format=",
                "--no-color",
                "--no-ext-diff",
                "--no-textconv",
                CONTEXT,
                "-M",
                &entry.hash,
                "--",
                path,
            ];
            args.extend(old_path.as_deref());
            git::run(root, &args)
        }
        Change::Lines(patch) => Ok(patch.clone()),
        Change::Svn => svn::path_history_patch(root, path, entry.hash.trim_start_matches('r')),
    }
}

pub(crate) fn binary_patch_sides(root: &Path, path: &str, entry: &Entry) -> Result<Pair, String> {
    let (left, right) = match &entry.change {
        Change::Git { path, old_path, code } => {
            let hash = &entry.hash;
            let left = if *code == Some('A') {
                Source::empty()
            } else {
                let old = old_path.as_deref().unwrap_or(path);
                git_blob(root, &format!("{hash}^:{old}"))?
            };
            let right = if *code == Some('D') {
                Source::empty()
            } else {
                git_blob(root, &format!("{hash}:{path}"))?
            };
            (left, right)
        }
        Change::Svn => {
            let revision = entry.hash[1..].parse::<u64>().map_err(|err| err.to_string())?;
            let cat = |rev: u64| match svn_blob(root, &rev.to_string(), path) {
                Err(err) if err.contains("E195012:") || err.contains("E160013:") => Ok(Source::empty()),
                result => result,
            };
            (cat(revision.saturating_sub(1))?, cat(revision)?)
        }
        Change::Lines(_) => return Err("Line history has no binary revision data".into()),
    };
    Ok(Pair { left, right })
}

fn metadata<'a>(fields: &mut impl Iterator<Item = &'a str>, change: Change) -> Option<Entry> {
    let hash = fields.next()?;
    if !matches!(hash.len(), 40 | 64) || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    Some(Entry {
        hash: hash.into(),
        short_hash: fields.next()?.into(),
        author: fields.next()?.into(),
        date: blame::local_date_time(fields.next()?.parse().ok()?),
        subject: fields.next()?.into(),
        change,
    })
}

fn parse_paths(output: &str, path: &str, directory: bool) -> Vec<Entry> {
    let mut current_path = path.to_owned();
    let mut fields = output.split('\0');
    fields.next(); // leading NUL
    let mut entries = Vec::new();
    while let Some(mut entry) = metadata(&mut fields, Change::Svn) {
        let mut old_path = None;
        let mut code = None;
        while let Some(status) = fields.next().map(str::trim).filter(|s| !s.is_empty()) {
            let Some(first) = fields.next() else { break };
            let (previous, changed) = if status.starts_with(['R', 'C']) {
                (Some(first), fields.next().unwrap_or_default())
            } else {
                (None, first)
            };
            if !directory && changed == current_path {
                old_path = previous.map(str::to_owned);
                code = status.chars().next();
            }
        }
        entry.change = Change::Git {
            path: current_path.clone(),
            old_path: old_path.clone(),
            code,
        };
        if let Some(old) = old_path {
            current_path = old;
        }
        entries.push(entry);
    }
    entries
}

fn parse_lines(output: &str) -> Vec<Entry> {
    let mut fields = output.split('\0');
    fields.next();
    std::iter::from_fn(|| {
        let mut entry = metadata(&mut fields, Change::Svn)?;
        entry.change = Change::Lines(fields.next()?.trim_start_matches('\n').to_owned());
        Some(entry)
    })
    .collect()
}

/// Convert the editor's half-open byte selection to Git's inclusive 1-based lines.
pub(crate) fn selected_lines(range: std::ops::Range<usize>, line_at: impl Fn(usize) -> usize) -> Option<RangeInclusive<usize>> {
    (!range.is_empty()).then(|| (line_at(range.start) + 1)..=(line_at(range.end - 1) + 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rename_and_older_path_from_git() {
        let output = concat!(
            "\0aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\0aaaaaaa\0A U Thor\x001700000000\0Rename\0\nR100\0old name\0new name\0",
            "\0bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\0bbbbbbb\0Someone\x001600000000\0Create\0\nA\0old name\0",
        );
        let rows = parse_paths(output, "new name", false);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].author, "A U Thor");
        assert_eq!(rows[0].date, blame::local_date_time(1700000000));
        assert!(matches!(&rows[0].change, Change::Git { path, old_path: Some(old), .. } if path == "new name" && old == "old name"));
        assert!(matches!(&rows[1].change, Change::Git { path, old_path: None, .. } if path == "old name"));
    }

    #[test]
    fn parses_line_patches_and_excludes_unselected_end_line() {
        let rows = parse_lines(concat!(
            "\0aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\0aaaaaaa\0Author\x001700000000\0Edit\0\n\n@@ -1 +1 @@\n-old\n+new\n",
            "\0bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\0bbbbbbb\0Author\x001600000000\0Create\0\n\n@@ -0,0 +1 @@\n+old\n",
        ));
        assert_eq!(rows.len(), 2);
        assert!(matches!(&rows[0].change, Change::Lines(patch) if patch.starts_with("@@")));
        let text = "é\nsecond\nthird";
        let line_at = |end| text.as_bytes()[..end].iter().filter(|&&byte| byte == b'\n').count();
        assert_eq!(selected_lines(0..10, line_at), Some(1..=2));
        assert_eq!(selected_lines(0..11, line_at), Some(1..=3));
        assert_eq!(selected_lines(2..2, line_at), None);
    }
}
