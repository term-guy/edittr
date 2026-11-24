//! Inline blame ("who changed this line"), shown dimmed after the cursor's line.
//!
//! Git: `git blame --porcelain --contents -` on the current buffer, so line numbers
//! match even with unsaved edits. Runs on a background thread per request.

use std::io::Write;
use std::path::Path;
use std::process::Stdio;

#[derive(Clone, Debug, PartialEq)]
pub struct BlameLine {
    pub rev: String,
    pub author: String,
    /// Commit time, seconds since the epoch (0 = unknown / not committed).
    pub time: i64,
    pub summary: String,
}

impl BlameLine {
    pub fn uncommitted(&self) -> bool {
        self.rev.is_empty() || self.rev.bytes().all(|b| b == b'0')
    }

    /// e.g. "Ada Lovelace, 3 months ago · Fix the parser".
    pub fn describe(&self, now: i64) -> String {
        if self.uncommitted() {
            return "You · Not committed yet".into();
        }
        let mut s = self.author.clone();
        if self.time > 0 {
            if !s.is_empty() {
                s.push_str(", ");
            }
            s.push_str(&ago(now - self.time));
        }
        if !self.summary.is_empty() {
            s.push_str(" · ");
            s.push_str(&self.summary);
        }
        s
    }
}

/// Per line of the blamed text: an index into `commits`.
#[derive(Default)]
pub struct Blame {
    pub lines: Vec<u32>,
    pub commits: Vec<BlameLine>,
}

impl Blame {
    pub fn line(&self, line: usize) -> Option<&BlameLine> {
        self.commits.get(*self.lines.get(line)? as usize)
    }
}

pub fn ago(secs: i64) -> String {
    let secs = secs.max(0);
    let (n, unit) = match secs {
        0..60 => return "just now".into(),
        60..3600 => (secs / 60, "minute"),
        3600..86_400 => (secs / 3600, "hour"),
        86_400..2_592_000 => (secs / 86_400, "day"),
        2_592_000..31_536_000 => (secs / 2_592_000, "month"),
        _ => (secs / 31_536_000, "year"),
    };
    format!("{n} {unit}{} ago", if n == 1 { "" } else { "s" })
}

pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// Like `git`, for editor text that had CRLF line endings on disk (the editor keeps
/// LF): git compares the given contents byte for byte with the committed file, so
/// CRLF must be restored or every line reads "Not committed yet".
pub fn git_with_endings(repo: &Path, rel: &str, text: &str, crlf: bool) -> Result<Blame, String> {
    if crlf {
        git(repo, rel, &text.replace('\n', "\r\n"))
    } else {
        git(repo, rel, text)
    }
}

/// Blame `rel` (relative to `repo`) as it is in `text` (the editor buffer).
pub fn git(repo: &Path, rel: &str, text: &str) -> Result<Blame, String> {
    let mut child = crate::git::cmd(repo)
        .args(["blame", "--porcelain", "--contents", "-", "--", rel])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    let mut stdin = child.stdin.take().ok_or("no stdin")?;
    let input = text.to_string();
    // Write from another thread: git starts printing before it has read everything.
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(input.as_bytes());
    });
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    let _ = writer.join();
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        if err.contains("no such path") {
            // Untracked (or new) file: every line is uncommitted.
            return Ok(Blame {
                lines: vec![0; text.split('\n').count()],
                commits: vec![BlameLine {
                    rev: String::new(),
                    author: String::new(),
                    time: 0,
                    summary: String::new(),
                }],
            });
        }
        return Err(err);
    }
    Ok(parse_porcelain(&String::from_utf8_lossy(&out.stdout)))
}

/// From `svn blame` lines (one per line).
pub fn from_svn(lines: &[crate::vcs::BlameLine]) -> Blame {
    let mut blame = Blame::default();
    let mut index: std::collections::HashMap<&str, u32> = std::collections::HashMap::new();
    for l in lines {
        let i = *index.entry(&l.rev).or_insert_with(|| {
            blame.commits.push(BlameLine {
                rev: l.rev.clone(),
                author: l.author.clone(),
                time: date_to_epoch(&l.date).unwrap_or(0),
                summary: l.summary.clone(),
            });
            (blame.commits.len() - 1) as u32
        });
        blame.lines.push(i);
    }
    blame
}

/// An SVN UTC timestamp or a cached YYYY-MM-DD date to seconds since the epoch.
pub(crate) fn date_to_epoch(date: &str) -> Option<i64> {
    let y: i64 = date.get(0..4)?.parse().ok()?;
    let m: i64 = date.get(5..7)?.parse().ok()?;
    let d: i64 = date.get(8..10)?.parse().ok()?;
    // Days from civil (Howard Hinnant's algorithm).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let time = if date.len() == 10 {
        0
    } else {
        let hours: i64 = date.get(11..13)?.parse().ok()?;
        let minutes: i64 = date.get(14..16)?.parse().ok()?;
        let seconds: i64 = date.get(17..19)?.parse().ok()?;
        hours * 3600 + minutes * 60 + seconds
    };
    Some(days * 86_400 + time)
}

/// Seconds east of UTC of the local time zone at `utc` (seconds since the epoch).
pub(crate) fn local_offset(utc: i64) -> i64 {
    #[cfg(unix)]
    {
        let t = utc as libc::time_t;
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        // SAFETY: localtime_r only writes to `tm`.
        if unsafe { libc::localtime_r(&t, &mut tm) }.is_null() {
            return 0;
        }
        tm.tm_gmtoff as i64
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{FILETIME, SYSTEMTIME};
        use windows_sys::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToFileTime, SystemTimeToTzSpecificLocalTime};
        let ticks = (utc + 11_644_473_600) * 10_000_000;
        let ft = FILETIME {
            dwLowDateTime: ticks as u32,
            dwHighDateTime: (ticks >> 32) as u32,
        };
        // SAFETY: plain conversions between stack values.
        unsafe {
            let (mut st, mut local, mut out): (SYSTEMTIME, SYSTEMTIME, FILETIME) = std::mem::zeroed();
            if FileTimeToSystemTime(&ft, &mut st) == 0
                || SystemTimeToTzSpecificLocalTime(std::ptr::null(), &st, &mut local) == 0
                || SystemTimeToFileTime(&local, &mut out) == 0
            {
                return 0;
            }
            let local_ticks = ((out.dwHighDateTime as i64) << 32) | out.dwLowDateTime as i64;
            (local_ticks - ticks) / 10_000_000
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = utc;
        0
    }
}

/// "YYYY-MM-DD HH:MM" in local time for seconds since the epoch.
pub(crate) fn local_date_time(utc: i64) -> String {
    let t = utc + local_offset(utc);
    let (days, secs) = (t.div_euclid(86_400), t.rem_euclid(86_400));
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}", secs / 3600, secs % 3600 / 60)
}

/// Parse `git blame --porcelain`.
pub fn parse_porcelain(out: &str) -> Blame {
    let mut blame = Blame::default();
    let mut index: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
    let mut current: Option<u32> = None;
    for line in out.lines() {
        if line.starts_with('\t') {
            // The line's content: ends this line's record.
            if let Some(c) = current {
                blame.lines.push(c);
            }
            continue;
        }
        let mut parts = line.split(' ');
        let first = parts.next().unwrap_or_default();
        if first.len() == 40 && first.bytes().all(|b| b.is_ascii_hexdigit()) {
            let i = *index.entry(first.to_string()).or_insert_with(|| {
                blame.commits.push(BlameLine {
                    rev: first.to_string(),
                    author: String::new(),
                    time: 0,
                    summary: String::new(),
                });
                (blame.commits.len() - 1) as u32
            });
            current = Some(i);
            continue;
        }
        let Some(c) = current.and_then(|i| blame.commits.get_mut(i as usize)) else {
            continue;
        };
        let value = line.split_once(' ').map_or("", |(_, v)| v);
        match first {
            "author" => c.author = value.to_string(),
            "author-time" => c.time = value.parse().unwrap_or(0),
            "summary" => c.summary = value.to_string(),
            _ => {}
        }
    }
    blame
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
1111111111111111111111111111111111111111 1 1 2
author Ada
author-mail <ada@x>
author-time 1700000000
author-tz +0000
committer Ada
summary First commit
filename a.rs
\tfn main() {
1111111111111111111111111111111111111111 2 2
\t    x();
0000000000000000000000000000000000000000 3 3 1
author Not Committed Yet
author-time 1800000000
summary Version of a.rs from a.rs
filename a.rs
\t    y();
1111111111111111111111111111111111111111 3 4 1
\t}
";

    #[test]
    fn parses_porcelain_and_describes() {
        let b = parse_porcelain(SAMPLE);
        assert_eq!(b.lines, vec![0, 0, 1, 0]);
        let first = b.line(0).unwrap();
        assert_eq!(
            (first.author.as_str(), first.time, first.summary.as_str()),
            ("Ada", 1700000000, "First commit")
        );
        assert_eq!(first.describe(1700000000 + 3 * 86_400), "Ada, 3 days ago · First commit");
        assert_eq!(b.line(2).unwrap().describe(0), "You · Not committed yet");
        assert!(b.line(9).is_none());
    }

    #[test]
    fn blames_this_repo_with_unsaved_edits() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        if crate::git::repo_root(root).is_none() {
            return; // e.g. a source tarball
        }
        let text = std::fs::read_to_string(root.join("src/main.rs")).unwrap();
        let edited = format!("// new first line\n{text}");
        let b = git(root, "src/main.rs", &edited).unwrap();
        assert_eq!(b.lines.len(), edited.lines().count());
        assert!(b.line(0).unwrap().uncommitted());
        let second = b.line(1).unwrap();
        assert!(!second.uncommitted() && !second.author.is_empty() && second.time > 0);
        let new = git(root, "no/such/file.rs", "a\nb").unwrap();
        assert!(new.line(1).unwrap().uncommitted());
    }

    #[test]
    fn local_dates_round_trip_the_offset() {
        let utc = 1_700_000_000; // 2023-11-14 22:13:20 UTC
        let shown = local_date_time(utc);
        let back = date_to_epoch(&format!("{}T{}:00Z", &shown[..10], &shown[11..])).unwrap();
        assert_eq!(back - local_offset(utc), utc - 20);
        assert_eq!(local_date_time(0).len(), 16);
    }

    #[test]
    fn svn_lines_and_dates() {
        assert_eq!(date_to_epoch("1970-01-01"), Some(0));
        assert_eq!(date_to_epoch("2023-11-14T22:13:20.000000Z"), Some(1_700_000_000));
        let l = |rev: &str| crate::vcs::BlameLine {
            rev: rev.into(),
            author: "bob".into(),
            date: "2024-02-29".into(),
            summary: "s".into(),
        };
        let b = from_svn(&[l("r3"), l("r5"), l("r3")]);
        assert_eq!(b.lines, vec![0, 1, 0]);
        assert_eq!(b.line(1).unwrap().rev, "r5");
        assert_eq!(b.line(0).unwrap().time, date_to_epoch("2024-02-29").unwrap());
    }

    #[test]
    fn relative_times() {
        assert_eq!(ago(5), "just now");
        assert_eq!(ago(60), "1 minute ago");
        assert_eq!(ago(7200), "2 hours ago");
        assert_eq!(ago(40 * 86_400), "1 month ago");
        assert_eq!(ago(800 * 86_400), "2 years ago");
    }

    #[test]
    fn blames_crlf_files_from_lf_editor_text() {
        let repo = std::env::temp_dir().join(format!("edittr-blame-crlf-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&repo);
        std::fs::create_dir_all(&repo).unwrap();
        let git = |args: &[&str]| crate::git::run(&repo, args).unwrap();
        git(&["init", "-q"]);
        git(&["config", "user.email", "t@t"]);
        git(&["config", "user.name", "Tester"]);
        git(&["config", "core.autocrlf", "false"]);
        std::fs::write(repo.join("a.c"), "int a;\r\nint b;\r\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-qm", "init"]);
        // The editor holds LF text and remembers the file was CRLF.
        let blame = super::git_with_endings(&repo, "a.c", "int a;\nint b;\n", true).unwrap();
        assert!(blame.commits.iter().all(|c| !c.uncommitted()), "every line should be committed");
        assert_eq!(blame.line(0).unwrap().author, "Tester");
        std::fs::remove_dir_all(&repo).ok();
    }
}
