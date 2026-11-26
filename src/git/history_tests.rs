use super::*;
use crate::vcs::{HistoryAction as MenuAction, Vcs};
use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};

#[test]
fn parses_full_git_decorations() {
    assert_eq!(
        parse_ref("HEAD -> refs/heads/feature/login"),
        Some(CommitRef::Local {
            name: "feature/login".into(),
            head: true,
        })
    );
    assert_eq!(
        parse_ref("refs/heads/origin/main"),
        Some(CommitRef::Local {
            name: "origin/main".into(),
            head: false,
        })
    );
    assert_eq!(
        parse_ref("refs/remotes/origin/feature/login"),
        Some(CommitRef::Remote("origin/feature/login".into()))
    );
    assert_eq!(parse_ref("tag: refs/tags/release/1,2"), Some(CommitRef::Tag("release/1,2".into())));
    assert_eq!(parse_ref("HEAD"), Some(CommitRef::Head));
    assert_eq!(parse_ref("refs/remotes/origin/HEAD"), Some(CommitRef::Other("origin/HEAD".into())));
    assert_eq!(parse_ref("refs/stash"), Some(CommitRef::Other("refs/stash".into())));
}

#[test]
fn builds_history_commands_with_explicit_modes_and_ref_arguments() {
    use HistoryAction::*;
    let cases = [
        (
            Checkout("feature/login".into()),
            vec!["switch", "--no-guess", "--", "feature/login"],
        ),
        (
            Track {
                local: "topic".into(),
                remote: "refs/remotes/origin/topic".into(),
            },
            vec!["switch", "--track", "-c", "topic", "--", "refs/remotes/origin/topic"],
        ),
        (Detach("abc".into()), vec!["switch", "--detach", "--", "abc"]),
        (
            CreateBranch {
                name: "topic".into(),
                hash: "abc".into(),
                checkout: true,
            },
            vec!["switch", "--no-track", "-c", "topic", "--", "abc"],
        ),
        (
            CreateBranch {
                name: "topic".into(),
                hash: "abc".into(),
                checkout: false,
            },
            vec!["branch", "--no-track", "--", "topic", "abc"],
        ),
        (CherryPick("abc".into()), vec!["cherry-pick", "--", "abc"]),
        (Revert("abc".into()), vec!["revert", "--no-edit", "--", "abc"]),
        (
            Merge {
                branch: "main".into(),
                source: "refs/heads/topic".into(),
            },
            vec!["merge", "--no-edit", "--", "refs/heads/topic"],
        ),
        (
            CreateTag {
                name: "v1".into(),
                hash: "abc".into(),
            },
            vec!["tag", "--no-sign", "--", "v1", "abc"],
        ),
        (
            DeleteBranch {
                name: "topic".into(),
                force: false,
            },
            vec!["branch", "-d", "--", "topic"],
        ),
        (
            DeleteBranch {
                name: "topic".into(),
                force: true,
            },
            vec!["branch", "-D", "--", "topic"],
        ),
    ];
    for (action, expected) in cases {
        assert_eq!(action.args(), expected);
    }
    for (mode, flag) in [
        (ResetMode::Soft, "--soft"),
        (ResetMode::Mixed, "--mixed"),
        (ResetMode::Hard, "--hard"),
    ] {
        let action = Reset {
            branch: "main".into(),
            hash: "abc".into(),
            mode,
        };
        assert_eq!(action.args(), ["reset", flag, "abc", "--"]);
    }
}

struct TestRepo(PathBuf);

impl Drop for TestRepo {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn stash_workflow_keeps_conflicted_pops_and_rejects_stale_reflog_positions() {
    let unique = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let repo = TestRepo(std::env::temp_dir().join(format!("edittr-git-stash-{}-{unique}", std::process::id())));
    fs::create_dir(&repo.0).unwrap();
    let root = &repo.0;
    let git = |args: &[&str]| run(root, args).unwrap();
    git(&["init", "-b", "main"]);
    git(&["config", "user.name", "Test Author"]);
    git(&["config", "user.email", "author@example.invalid"]);
    git(&["config", "core.hooksPath", ".git/test-hooks"]);
    git(&["config", "core.autocrlf", "false"]);
    let tracked = root.join("tracked.txt");
    let untracked = root.join("new.txt");
    fs::write(&tracked, "base\n").unwrap();
    git(&["add", "tracked.txt"]);
    git(&["-c", "commit.gpgsign=false", "commit", "-m", "Initial"]);

    let vcs = Vcs::Git;
    assert!(vcs.stashes(root).unwrap().is_empty());
    fs::write(&tracked, "first change\n").unwrap();
    git(&["add", "tracked.txt"]);
    fs::write(&untracked, "new file\n").unwrap();
    vcs.stash_push(root, "", false).unwrap();
    let first = vcs.stashes(root).unwrap().remove(0);
    assert_eq!(first.selector, "stash@{0}");
    assert_eq!(first.commit.parents.len(), 2);
    assert_eq!(fs::read_to_string(&tracked).unwrap(), "base\n");
    assert!(untracked.exists());
    assert!(vcs.status(root).unwrap().staged.is_empty());

    vcs.stash_push(root, "--custom stash message", true).unwrap();
    let entries = vcs.stashes(root).unwrap();
    assert_eq!(entries.len(), 2);
    let newest = &entries[0];
    assert!(newest.commit.subject.ends_with("--custom stash message"));
    assert_eq!(newest.commit.parents.len(), 3);
    assert!(!untracked.exists());
    for action in [StashAction::Pop, StashAction::Drop] {
        assert!(vcs.stash_action(root, &first, action).unwrap_err().contains("has changed"));
        assert_eq!(vcs.stashes(root).unwrap().len(), 2);
    }

    vcs.stash_action(root, &first, StashAction::Apply).unwrap();
    assert_eq!(fs::read_to_string(&tracked).unwrap(), "first change\n");
    assert_eq!(vcs.stashes(root).unwrap().len(), 2);
    vcs.stash_action(root, newest, StashAction::Drop).unwrap();
    fs::write(&tracked, "conflicting change\n").unwrap();
    git(&["add", "tracked.txt"]);
    git(&["-c", "commit.gpgsign=false", "commit", "-m", "Conflicting change"]);
    assert!(vcs.stash_action(root, &first, StashAction::Pop).is_err());
    assert_eq!(vcs.stashes(root).unwrap()[0].commit.hash, first.commit.hash);
    assert_eq!(vcs.status(root).unwrap().unstaged[0].code, 'U');
    git(&["reset", "--hard", "HEAD^"]);
    vcs.stash_action(root, &first, StashAction::Pop).unwrap();
    assert!(vcs.stashes(root).unwrap().is_empty());
    assert_eq!(fs::read_to_string(&tracked).unwrap(), "first change\n");
}

#[test]
fn diff_can_ignore_crlf_only_changes_in_the_worktree_and_index() {
    let unique = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let repo = TestRepo(std::env::temp_dir().join(format!("edittr-git-eol-{}-{unique}", std::process::id())));
    fs::create_dir(&repo.0).unwrap();
    let git = |args: &[&str]| run(&repo.0, args).unwrap();
    git(&["init"]);
    git(&["config", "user.name", "Test Author"]);
    git(&["config", "user.email", "author@example.invalid"]);
    git(&["config", "core.autocrlf", "false"]);
    git(&["config", "core.hooksPath", ".git/test-hooks"]);
    fs::write(repo.0.join(".gitattributes"), "*.txt -text\n").unwrap();
    let path = repo.0.join("file.txt");
    fs::write(&path, "a\r\nb\r\n").unwrap();
    git(&["add", ".gitattributes", "file.txt"]);
    git(&["-c", "commit.gpgsign=false", "commit", "-m", "Initial"]);

    let change = FileChange {
        path: "file.txt".into(),
        code: 'M',
    };
    fs::write(&path, "a\nb").unwrap();
    let missing_newline = Vcs::Git.diff(&repo.0, &change, false, true);
    assert!(!missing_newline.only_line_endings);
    assert!(missing_newline.text.contains(" a\r\n-b\r\n+b\n\\ No newline at end of file"));
    fs::write(&path, "a\nB\nb").unwrap();
    let mixed = Vcs::Git.diff(&repo.0, &change, false, true);
    assert!(mixed.text.contains(" a\r\n"));
    assert!(mixed.text.contains("-b\r\n+B\n+b\n\\ No newline at end of file"));
    fs::write(&path, "a\nb\n").unwrap();
    for staged in [false, true] {
        if staged {
            git(&["add", "file.txt"]);
        }
        let filtered = Vcs::Git.diff(&repo.0, &change, staged, true);
        assert!(filtered.text.is_empty());
        assert!(filtered.only_line_endings);
        assert!(Vcs::Git.diff(&repo.0, &change, staged, false).text.contains("@@ "));
    }
}

#[test]
fn graph_history_actions_on_a_real_git_repository() {
    use HistoryAction::*;
    let unique = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let repo = TestRepo(std::env::temp_dir().join(format!("edittr-git-history-{}-{unique}", std::process::id())));
    fs::create_dir(&repo.0).unwrap();
    let git = |args: &[&str]| run(&repo.0, args).unwrap().trim().to_string();
    let action = |action| MenuAction::Git(action).run(Vcs::Git, &repo.0);
    let head = || git(&["rev-parse", "HEAD"]);
    let branch = || git(&["branch", "--show-current"]);
    let file = repo.0.join("tracked.txt");
    let write = |text| fs::write(&file, text).unwrap();
    let read = || fs::read_to_string(&file).unwrap();

    git(&["init", "-b", "main"]);
    git(&["config", "user.name", "Test Author"]);
    git(&["config", "user.email", "author@example.invalid"]);
    git(&["config", "commit.gpgsign", "false"]);
    git(&["config", "tag.gpgSign", "true"]);
    git(&["config", "core.autocrlf", "false"]);
    git(&["config", "core.hooksPath", ".git/test-hooks"]);
    write("base\n");
    git(&["add", "tracked.txt"]);
    git(&["commit", "-m", "Base"]);
    let base = head();

    action(CreateBranch {
        name: "feature/login".into(),
        hash: base.clone(),
        checkout: true,
    })
    .unwrap();
    assert_eq!(branch(), "feature/login");
    write("feature\n");
    git(&["commit", "-am", "Feature", "-m", "A multiline body.\nSecond line."]);
    let feature = head();

    git(&["remote", "add", "origin", "."]);
    git(&["update-ref", "refs/remotes/origin/new/topic", &feature]);
    git(&["symbolic-ref", "refs/remotes/origin/HEAD", "refs/remotes/origin/new/topic"]);
    action(Track {
        local: "new/topic".into(),
        remote: "refs/remotes/origin/new/topic".into(),
    })
    .unwrap();
    assert_eq!(branch(), "new/topic");
    assert_eq!(
        git(&["rev-parse", "--symbolic-full-name", "@{upstream}"]),
        "refs/remotes/origin/new/topic"
    );

    let release = "release/1,2";
    action(CreateTag {
        name: release.into(),
        hash: feature.clone(),
    })
    .unwrap();
    assert_eq!(git(&["rev-parse", release]), feature);
    assert_eq!(git(&["cat-file", "-t", release]), "commit");
    let history = log(&repo.0, 20).unwrap();
    let entry = history.iter().find(|commit| commit.hash == feature).unwrap();
    assert_eq!(entry.short_hash, git(&["rev-parse", "--short", &feature]));
    assert_eq!(entry.subject, "Feature");
    assert_eq!(entry.message, "Feature\n\nA multiline body.\nSecond line.");
    assert!(entry.refs.contains(&CommitRef::Local {
        name: "new/topic".into(),
        head: true
    }));
    assert!(entry.refs.contains(&CommitRef::Remote("origin/new/topic".into())));
    assert!(entry.refs.contains(&CommitRef::Tag(release.into())));
    assert!(!entry.refs.contains(&CommitRef::Remote("origin/HEAD".into())));

    action(Detach(base.clone())).unwrap();
    assert_eq!(head(), base);
    assert!(branch().is_empty());
    assert!(
        log(&repo.0, 20)
            .unwrap()
            .iter()
            .any(|entry| entry.hash == base && entry.refs.contains(&CommitRef::Head))
    );
    assert!(
        action(Reset {
            branch: "main".into(),
            hash: feature.clone(),
            mode: ResetMode::Hard
        })
        .is_err()
    );
    action(CreateBranch {
        name: "unmerged".into(),
        hash: feature.clone(),
        checkout: false,
    })
    .unwrap();
    assert!(branch().is_empty());
    action(Checkout("main".into())).unwrap();
    assert_eq!(branch(), "main");
    assert!(
        action(DeleteBranch {
            name: "unmerged".into(),
            force: false
        })
        .is_err()
    );
    assert_eq!(git(&["rev-parse", "refs/heads/unmerged"]), feature);
    action(DeleteBranch {
        name: "unmerged".into(),
        force: true,
    })
    .unwrap();
    assert!(!branches(&repo.0).contains(&"unmerged".into()));

    action(Merge {
        branch: "main".into(),
        source: "refs/heads/feature/login".into(),
    })
    .unwrap();
    assert_eq!(head(), feature);
    let reset = |mode| {
        action(Reset {
            branch: "main".into(),
            hash: base.clone(),
            mode,
        })
        .unwrap()
    };
    reset(ResetMode::Soft);
    assert_eq!(head(), base);
    assert_eq!(read(), "feature\n");
    assert_eq!(git(&["diff", "--cached", "--name-only"]), "tracked.txt");
    reset(ResetMode::Mixed);
    assert_eq!(read(), "feature\n");
    assert!(git(&["diff", "--cached", "--name-only"]).is_empty());
    assert_eq!(git(&["diff", "--name-only"]), "tracked.txt");
    reset(ResetMode::Hard);
    assert_eq!(read(), "base\n");
    assert!(git(&["status", "--porcelain"]).is_empty());

    action(CherryPick(feature.clone())).unwrap();
    assert_eq!(git(&["log", "-1", "--format=%s"]), "Feature");
    assert_eq!(read(), "feature\n");
    action(Revert(head())).unwrap();
    assert_eq!(git(&["log", "-1", "--format=%s"]), "Revert \"Feature\"");
    assert_eq!(read(), "base\n");
    action(CreateBranch {
        name: "merged".into(),
        hash: base,
        checkout: false,
    })
    .unwrap();
    action(DeleteBranch {
        name: "merged".into(),
        force: false,
    })
    .unwrap();
    assert!(!branches(&repo.0).contains(&"merged".into()));

    write("conflicting change\n");
    git(&["commit", "-am", "Conflicting change"]);
    let error = action(CherryPick(feature.clone())).unwrap_err();
    assert!(error.contains("CONFLICT") && error.contains("tracked.txt"), "{error}");
    assert_eq!(git(&["rev-parse", "CHERRY_PICK_HEAD"]), feature);
    assert_eq!(status(&repo.0).unwrap().unstaged[0].code, 'U');
}

#[test]
fn discards_a_folder_in_one_batch() {
    let repo = std::env::temp_dir().join(format!("edittr-discard-folder-{}", std::process::id()));
    let _ = fs::remove_dir_all(&repo);
    fs::create_dir_all(repo.join("src/ui")).unwrap();
    let git = |args: &[&str]| run(&repo, args).unwrap();
    git(&["init", "-q"]);
    git(&["config", "user.email", "t@t"]);
    git(&["config", "user.name", "t"]);
    for (path, text) in [("src/a.rs", "a\n"), ("src/ui/b.rs", "b\n"), ("keep.rs", "k\n")] {
        fs::write(repo.join(path), text).unwrap();
    }
    git(&["add", "."]);
    git(&["commit", "-qm", "init"]);
    fs::write(repo.join("src/a.rs"), "changed\n").unwrap();
    fs::write(repo.join("src/ui/b.rs"), "changed\n").unwrap();
    fs::write(repo.join("src/ui/new.rs"), "new\n").unwrap();
    fs::write(repo.join("keep.rs"), "outside the folder\n").unwrap();
    let changes =
        [('M', "src/a.rs"), ('M', "src/ui/b.rs"), ('?', "src/ui/new.rs")].map(|(code, path)| FileChange { code, path: path.into() });
    Vcs::Git.discard_many(&repo, &changes).unwrap();
    assert_eq!(fs::read_to_string(repo.join("src/a.rs")).unwrap(), "a\n");
    assert_eq!(fs::read_to_string(repo.join("src/ui/b.rs")).unwrap(), "b\n");
    assert!(!repo.join("src/ui/new.rs").exists());
    assert_eq!(fs::read_to_string(repo.join("keep.rs")).unwrap(), "outside the folder\n");
    fs::remove_dir_all(&repo).ok();
}
