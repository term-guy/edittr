use super::*;
use crate::diff::{Kind, side_by_side};
use crate::vcs::{self, Vcs};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Output};
use std::thread;
use std::time::Instant;

const SVN: Vcs = Vcs::Svn;

struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

pub(crate) struct Fixture {
    server: Option<Server>,
    scratch: Scratch,
    pub(crate) wc: PathBuf,
    url: String,
}

impl Fixture {
    pub(crate) fn new() -> Self {
        let scratch = Scratch::new().unwrap();
        let base = scratch.0.canonicalize().unwrap();
        for program in ["svn", "svnadmin", "svnserve"] {
            assert!(
                command(&base, program)
                    .arg("--version")
                    .output()
                    .is_ok_and(|out| out.status.success()),
                "SVN integration tests require {program} on PATH"
            );
        }
        checked(
            command(&base, "svnadmin").args(["create", "repo"]).output().unwrap(),
            "svnadmin create",
        );
        fs::write(base.join("repo/conf/svnserve.conf"), "[general]\nanon-access = write\n").unwrap();

        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("SVN integration tests require loopback TCP");
        let address = listener.local_addr().unwrap();
        let port = address.port().to_string();
        drop(listener);
        let mut server = Server(
            command(&base, "svnserve")
                .args(["-d", "--foreground", "--threads", "-r"])
                .arg(&base)
                .args(["--listen-host", "127.0.0.1", "--listen-port", &port])
                .spawn()
                .unwrap(),
        );
        let start = Instant::now();
        while TcpStream::connect(address).is_err() {
            assert!(server.0.try_wait().unwrap().is_none(), "svnserve exited during startup");
            assert!(start.elapsed() < Duration::from_secs(5), "svnserve did not start");
            thread::sleep(Duration::from_millis(15));
        }

        TEST_CACHE.with(|cache| cache.replace(Some(base.join("cache"))));
        let mut fixture = Self {
            server: Some(server),
            scratch,
            wc: base.join("wc"),
            url: format!("svn://{address}/repo"),
        };
        fixture.wc = fixture.checkout("wc");
        fixture
    }

    pub(crate) fn checkout(&self, name: &str) -> PathBuf {
        let base = &self.scratch.0;
        let output = svn(base, &["checkout", "--quiet", &self.url, name]).output().unwrap();
        checked(output, "svn checkout");
        base.join(name).canonicalize().unwrap()
    }

    fn stop(&mut self) {
        drop(self.server.take());
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        TEST_CACHE.with(|cache| cache.replace(None));
    }
}

fn checked(output: Output, operation: &str) -> String {
    assert!(
        output.status.success(),
        "{operation} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

pub(crate) fn cli(root: &Path, args: &[&str]) -> String {
    checked(svn(root, args).output().unwrap(), &format!("svn {args:?}"))
}

pub(crate) fn write(root: &Path, path: &str, bytes: impl AsRef<[u8]>) {
    let file = root.join(path);
    if let Some(parent) = file.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(file, bytes).unwrap();
}

fn status(root: &Path) -> BTreeMap<String, char> {
    SVN.status(root).unwrap().unstaged.into_iter().map(|c| (c.path, c.code)).collect()
}

fn change(path: &str, code: char) -> FileChange {
    FileChange { path: path.into(), code }
}

fn timed<T>(label: &str, f: impl FnOnce() -> T) -> (T, Duration) {
    let start = Instant::now();
    let result = f();
    let elapsed = start.elapsed();
    println!("svn {label}: {elapsed:.3?}");
    (result, elapsed)
}

#[test]
#[ignore = "requires svn and svnserve"]
fn svn_real_working_copy_operations() {
    let mut fixture = Fixture::new();
    let wc = fixture.wc.clone();
    let nested = wc.join("nested/deep");
    let text = "nested/deep/中文 @ & spaces.txt";
    let crlf = "nested/deep/crlf.txt";
    let binary = "nested/deep/data.bin";
    let missing = "nested/missing.txt";
    let deleted = "nested/delete.txt";
    let moved = "nested/move old.txt";
    let renamed = "nested/中文@ renamed.txt";
    let props = "nested/property.txt";
    let missing_dir = "nested/removed dir";
    for path in [text, crlf, missing, deleted, moved, props, "nested/removed dir/sub/file.txt"] {
        write(&wc, path, "one\r\ntwo\r\n");
    }
    write(&wc, binary, [0, 255, 1, 2]);
    let message = "Initial 中文 & setup\n\nSecond line <details>";
    assert!(SVN.commit(&wc, message, false).unwrap().contains("Committed revision 1"));
    assert!(vcs::detect(&nested).is_some_and(|(root, vcs)| root == wc && vcs == SVN));
    assert_eq!(repo_root(&nested), Some(wc.clone()));
    assert_eq!(SVN.name(), "Subversion");
    assert_eq!(SVN.history_title(), "History");
    assert!(!SVN.has_index());
    assert!(SVN.branches(&nested).is_empty());
    assert!(SVN.stage(&wc, None, true).is_err());
    assert!(SVN.switch(&wc, "other").is_err());
    assert!(SVN.remote(&wc, &["fetch"]).is_err());
    assert!(SVN.commit(&wc, "amend", true).is_err());
    let clean = SVN.status(&wc).unwrap();
    assert_eq!(clean.branch, "/");
    assert!(clean.staged.is_empty() && clean.unstaged.is_empty());
    assert_eq!(SVN.base_text(&wc, text).as_deref(), Some("one\r\ntwo\r\n"));
    assert_eq!(SVN.base_text(&wc, binary).unwrap().as_bytes()[0], 0);

    write(&wc, text, "new\r\none\r\nchanged\r\n");
    write(&wc, crlf, "one\r\nchanged\r\n");
    write(&wc, binary, [0, 255, 3, 4]);
    write(&wc, "new added.txt", "scheduled\n");
    cli(&wc, &["add", "--quiet", "new added.txt"]);
    write(&wc, "new & loose.txt", "unscheduled\n");
    write(&wc, "new dir/sub/file.txt", "nested unversioned\n");
    if cfg!(unix) {
        write(&wc, " leading and trailing ", "spaces\n");
    }
    fs::remove_file(wc.join(missing)).unwrap();
    fs::remove_dir_all(wc.join(missing_dir)).unwrap();
    cli(&wc, &["delete", "--quiet", deleted]);
    cli(&wc, &["move", "--quiet", moved, renamed]);
    cli(&wc, &["propset", "test:prop", "value", props]);
    cli(&wc, &["propset", "svn:ignore", "ignore.me", "nested"]);
    write(&wc, "nested/ignore.me", "ignored\n");
    cli(&wc, &["changelist", "work", crlf]);

    let entries = status(&wc);
    for (path, expected) in [
        (text, 'M'),
        (crlf, 'M'),
        (binary, 'M'),
        (props, 'M'),
        ("nested", 'M'),
        (missing, 'D'),
        (missing_dir, 'D'),
        ("nested/removed dir/sub/file.txt", 'D'),
        (deleted, 'D'),
        (moved, 'D'),
        (renamed, 'A'),
        ("new added.txt", 'A'),
        ("new & loose.txt", '?'),
        ("new dir", '?'),
    ] {
        assert_eq!(entries.get(path), Some(&expected), "{path}");
    }
    assert!(!entries.contains_key("nested/ignore.me"));

    let listed = vcs::list_files(&wc, Some(SVN)).unwrap();
    for path in [text, binary, renamed, "new added.txt", "new & loose.txt", "new dir/sub/file.txt"] {
        assert!(listed.iter().any(|item| item == path), "{path}: {listed:?}");
    }
    for path in [missing, deleted, moved, "nested/ignore.me", "new dir", ".svn/wc.db"] {
        assert!(!listed.iter().any(|item| item == path), "{path}: {listed:?}");
    }
    let listed = vcs::list_files(&nested, Some(SVN)).unwrap();
    assert_eq!(listed, ["crlf.txt", "data.bin", "中文 @ & spaces.txt"]);

    assert!(SVN.diff(&wc, &change(text, 'M'), false).contains("+changed"));
    let crlf_diff = side_by_side(&SVN.diff(&wc, &change(crlf, 'M'), false));
    assert_eq!(
        crlf_diff.rows.iter().map(|row| row.kind).collect::<Vec<_>>(),
        [Kind::Same, Kind::Changed]
    );
    assert!(SVN.diff(&wc, &change(binary, 'M'), false).contains("binary"));
    assert!(SVN.diff(&wc, &change(props, 'M'), false).contains("Added: test:prop"));
    assert!(SVN.diff(&wc, &change(missing, 'D'), false).contains("-two"));
    assert!(SVN.diff(&wc, &change(deleted, 'D'), false).contains("-two"));
    assert!(SVN.diff(&wc, &change(renamed, 'A'), false).contains("+one"));
    assert!(SVN.diff(&wc, &change("new added.txt", 'A'), false).contains("+scheduled"));
    assert!(SVN.diff(&wc, &change("new & loose.txt", '?'), false).contains("+unscheduled"));
    assert!(SVN.base_text(&wc, "new & loose.txt").is_none());

    let log = SVN.log(&wc, 20, |_| {}).unwrap();
    assert_eq!(log.iter().map(|entry| entry.hash.as_str()).collect::<Vec<_>>(), ["r1"]);
    assert_eq!(log[0].subject, "Initial 中文 & setup");
    let detail = SVN.show_commit(&wc, "r1");
    assert!(detail.contains(message) && detail.contains("+two"), "{detail}");
    assert!(SVN.show_commit(&wc, "not-a-revision").contains("Invalid Subversion revision"));
    let blamed = vcs::blame(SVN, &wc, text).unwrap();
    assert_eq!(blamed.iter().map(|line| line.rev.as_str()).collect::<Vec<_>>(), ["", "r1", ""]);
    assert_eq!(blamed[1].summary, "Initial 中文 & setup");
    for path in ["new added.txt", "new & loose.txt", "new dir/sub/file.txt"] {
        let lines = vcs::blame(SVN, &wc, path).unwrap();
        assert_eq!(lines.len(), 1);
        assert!(crate::blame::from_svn(&lines).line(0).unwrap().uncommitted());
    }

    SVN.discard(&wc, &change(text, 'M')).unwrap();
    SVN.discard(&wc, &change("nested", 'M')).unwrap();
    assert_eq!(status(&wc).get(props), Some(&'M'));
    // Revert both endpoints so SVN restores the source and removes the destination.
    SVN.discard(&wc, &change(renamed, 'A')).unwrap();
    let reverted = status(&wc);
    assert!(!reverted.contains_key(moved));
    assert!(!reverted.contains_key(renamed));
    assert!(!wc.join(renamed).exists());
    cli(&wc, &["move", "--quiet", moved, renamed]);
    SVN.discard(&wc, &change("new & loose.txt", '?')).unwrap();
    write(&wc, "discard dir/sub/file.txt", "throwaway\n");
    SVN.discard(&wc, &change("discard dir", '?')).unwrap();
    assert!(!wc.join("discard dir").exists());
    assert!(!wc.join("new & loose.txt").exists());
    SVN.discard(&wc, &change(missing_dir, 'D')).unwrap();
    assert!(wc.join("nested/removed dir/sub/file.txt").exists());
    fs::remove_dir_all(wc.join(missing_dir)).unwrap();
    fs::remove_file(wc.join("nested/ignore.me")).unwrap();

    let message = "Second revision 中文\n\nKeep the body intact";
    assert!(SVN.commit(&wc, message, false).unwrap().contains("Committed revision 2"));
    assert!(status(&wc).is_empty());
    assert!(!wc.join(missing_dir).exists());
    if cfg!(unix) {
        assert_eq!(SVN.base_text(&wc, " leading and trailing ").as_deref(), Some("spaces\n"));
    }
    let log = SVN.refresh_log(&wc, 20, |_| {}).unwrap();
    assert_eq!(log[0].hash, "r2");
    assert_eq!(log[0].parents, ["r1"]);
    assert!(SVN.show_commit(&wc, "2").contains(message));
    let cached_blame = vcs::blame(SVN, &wc, text).unwrap();

    fixture.stop();
    assert_eq!(SVN.show_commit(&wc, "r1"), detail);
    assert_eq!(SVN.base_text(&wc, text).as_deref(), Some("one\r\ntwo\r\n"));
    assert_eq!(vcs::blame(SVN, &wc, text).unwrap()[0].rev, cached_blame[0].rev);
}

#[test]
#[ignore = "requires svn and svnserve"]
fn svn_real_conflict_rejects_commit_until_discarded() {
    let fixture = Fixture::new();
    let wc = &fixture.wc;
    write(wc, "conflict.txt", "base\n");
    SVN.commit(wc, "Base", false).unwrap();
    let other = fixture.checkout("other");
    write(&other, "conflict.txt", "incoming\n");
    SVN.commit(&other, "Remote edit", false).unwrap();
    write(wc, "conflict.txt", "local\n");
    write(wc, "unversioned.txt", "leave scheduling alone\n");
    cli(wc, &["update", "--quiet"]);
    assert_eq!(status(wc)["conflict.txt"], 'U');
    assert!(SVN.commit(wc, "must fail", false).unwrap_err().contains("conflicts"));
    assert_eq!(status(wc)["unversioned.txt"], '?');
    SVN.discard(wc, &change("conflict.txt", 'U')).unwrap();
    assert_eq!(fs::read_to_string(wc.join("conflict.txt")).unwrap(), "incoming\n");
    assert_eq!(status(wc), BTreeMap::from([("unversioned.txt".to_owned(), '?')]));
}

#[test]
#[ignore = "requires svn and svnserve"]
fn svn_real_large_changes() {
    let fixture = Fixture::new();
    let wc = &fixture.wc;
    let path = |i: usize| format!("batch-{}/directory-{:04}/file-{:05}.txt", i / 1000, i / 10, i);
    for i in 0..20_000 {
        write(wc, &path(i), "first\n");
    }
    let (out, _) = timed("commit 20,000 initial files", || {
        SVN.commit(wc, "Seed 20,000 files", false).unwrap()
    });
    assert!(out.contains("Committed revision 1"));
    assert!(status(wc).is_empty());
    assert_eq!(vcs::list_files(wc, Some(SVN)).unwrap().len(), 20_000);

    for i in 0..5_000 {
        write(wc, &path(i), "changed\n");
    }
    for i in 5_000..6_000 {
        fs::remove_file(wc.join(path(i))).unwrap();
    }
    for i in 6_000..8_000 {
        // Keep additions in existing directories so status reports all 2,000.
        write(wc, &path(i).replace(".txt", " added 中文.txt"), "added\n");
    }

    let (entries, elapsed) = timed("status of 8,000 changes", || status(wc));
    assert!(
        elapsed < Duration::from_secs(if cfg!(debug_assertions) { 5 } else { 2 }),
        "slow status: {elapsed:?}"
    );
    assert_eq!(entries.len(), 8_000);
    for i in 0..8_000 {
        let path = if i < 6_000 {
            path(i)
        } else {
            path(i).replace(".txt", " added 中文.txt")
        };
        let expected = if i < 5_000 {
            'M'
        } else if i < 6_000 {
            'D'
        } else {
            '?'
        };
        assert_eq!(entries[&path], expected);
    }

    timed("working-copy diffs (modified, missing, unversioned)", || {
        assert!(SVN.diff(wc, &change(&path(0), 'M'), false).contains("+changed"));
        assert!(SVN.diff(wc, &change(&path(5_000), 'D'), false).contains("-first"));
        assert!(
            SVN.diff(wc, &change(&path(6_000).replace(".txt", " added 中文.txt"), '?'), false)
                .contains("+added")
        );
    });

    let message = "Bulk changes 中文\n\n5,000 modified, 2,000 added, 1,000 removed";
    let (out, _) = timed("commit all 8,000 changes", || SVN.commit(wc, message, false).unwrap());
    assert!(out.contains("Committed revision 2"));
    assert!(status(wc).is_empty());
    let (log, _) = timed("bulk history", || SVN.log(wc, 10, |_| {}).unwrap());
    assert_eq!(log.iter().map(|entry| entry.hash.as_str()).collect::<Vec<_>>(), ["r2", "r1"]);
    assert_eq!(log[0].subject, "Bulk changes 中文");
    let (diff, _) = timed("show revision with 8,000 changes", || SVN.show_commit(wc, "r2"));
    assert!(diff.contains(message));
    assert_eq!(diff.lines().filter(|line| line.starts_with("Index: ")).count(), 8_000);
    for (line, count) in [("+changed", 5_000), ("+added", 2_000), ("-first", 6_000)] {
        assert_eq!(diff.lines().filter(|candidate| *candidate == line).count(), count, "{line}");
    }
}

#[test]
#[ignore = "requires svn and svnserve"]
fn svn_real_history_pages_and_offline_cache() {
    let mut fixture = Fixture::new();
    let wc = fixture.wc.clone();
    write(&wc, "history/file.txt", "1\n");
    SVN.commit(&wc, "Revision 1", false).unwrap();
    timed("seed 300 revisions", || {
        for revision in 2..=300 {
            let url = format!("{}/history/rev-{revision}", fixture.url);
            cli(&wc, &["mkdir", "--quiet", "-m", &format!("Revision {revision}"), &url]);
        }
    });

    assert!(SVN.log(&wc, 0, |_| panic!("zero limit uses no history")).unwrap().is_empty());
    for limit in [17, 130, 300, 350] {
        let (log, _) = timed(&format!("history limit {limit}"), || SVN.log(&wc, limit, |_| {}).unwrap());
        assert_eq!(log.len(), limit.min(300));
        for (entry, revision) in log.iter().zip((1..=300).rev()) {
            assert_eq!(entry.hash, format!("r{revision}"));
            assert_eq!(entry.subject, format!("Revision {revision}"));
        }
    }

    fixture.stop();
    let log = timed("history cache with svnserve stopped", || {
        SVN.log(&wc, 300, |_| panic!("a cache hit should return before starting a server fetch"))
            .unwrap()
    })
    .0;
    assert_eq!(log.len(), 300);
    assert_eq!(log[299].hash, "r1");
}
