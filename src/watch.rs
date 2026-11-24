//! Publish filesystem changes after a quiet period, with no timer when idle.
use crate::clangd;
use eframe::egui;
use notify::{Event, EventKind, RecursiveMode};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::time::{Duration, Instant};

pub const IGNORED_DIRS: &[&str] = &[".git", ".svn", "node_modules", "target", ".venv", "__pycache__", "dist", "build"];
const QUIET: Duration = Duration::from_millis(300);
const MAX_DELAY: Duration = Duration::from_secs(1);

#[derive(Default)]
pub struct Changes {
    pub vcs: bool,
    pub rediscover: bool,
    pub rescan: bool,
    pub paths: HashSet<PathBuf>,
    pub dirs: HashSet<PathBuf>,
}

impl Changes {
    pub fn merge(&mut self, other: Self) {
        self.vcs |= other.vcs;
        self.rediscover |= other.rediscover;
        self.rescan |= other.rescan;
        self.paths.extend(other.paths);
        self.dirs.extend(other.dirs);
    }

    pub fn affects(&self, path: &Path) -> bool {
        path.ancestors().any(|p| self.paths.contains(p))
    }
}

enum Input {
    Event(notify::Result<Event>),
    CompilationDatabase(Vec<PathBuf>),
    Stop,
}

pub struct Watcher {
    input: Sender<Input>,
    output: Receiver<Changes>,
    errors: Receiver<String>,
}

impl Watcher {
    pub fn start(root: PathBuf, ctx: egui::Context) -> Self {
        Self::start_with::<notify::RecommendedWatcher>(root, ctx, notify::Config::default().with_follow_symlinks(false))
    }

    fn start_with<W: notify::Watcher>(root: PathBuf, ctx: egui::Context, config: notify::Config) -> Self {
        let (input, events) = channel();
        let (output, batches) = channel();
        let (errors, failures) = channel();
        let callback = input.clone();
        std::thread::spawn(move || {
            let watcher = W::new(
                move |event| {
                    let _ = callback.send(Input::Event(event));
                },
                config,
            );
            if let Err(err) = watcher.and_then(|watcher| run(root, watcher, events, &output, &ctx))
                && errors.send(err.to_string()).is_ok()
            {
                ctx.request_repaint();
            }
        });
        Self {
            input,
            output: batches,
            errors: failures,
        }
    }

    pub fn watch_compilation_database(&self, paths: Vec<PathBuf>) {
        let _ = self.input.send(Input::CompilationDatabase(paths));
    }

    pub fn take(&self) -> Changes {
        let mut changes = Changes::default();
        for batch in self.output.try_iter() {
            changes.merge(batch);
        }
        changes
    }

    pub fn error(&self) -> Option<String> {
        self.errors.try_recv().ok()
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        let _ = self.input.send(Input::Stop);
    }
}

#[derive(Default)]
struct GitDirs {
    admin: Option<PathBuf>,
    common: Option<PathBuf>,
}

impl GitDirs {
    fn resolve(root: &Path) -> Self {
        let admin = root.ancestors().find_map(|dir| {
            let marker = dir.join(".git");
            if marker.is_dir() {
                marker.canonicalize().ok()
            } else {
                let pointer = std::fs::read_to_string(marker).ok()?;
                dir.join(pointer.strip_prefix("gitdir:")?.trim()).canonicalize().ok()
            }
        });
        let common = admin.as_ref().and_then(|dir| {
            let pointer = std::fs::read_to_string(dir.join("commondir")).ok()?;
            dir.join(pointer.trim()).canonicalize().ok()
        });
        Self { admin, common }
    }

    fn paths(&self) -> impl Iterator<Item = &PathBuf> {
        self.admin.iter().chain(&self.common)
    }

    fn update_watches(&self, root: &Path, watcher: &mut impl notify::Watcher, watching: &mut HashSet<PathBuf>) -> notify::Result<()> {
        let wanted: HashSet<_> = self
            .paths()
            .filter(|dir| !dir.starts_with(root) && !self.paths().any(|other| other != *dir && dir.starts_with(other)))
            .cloned()
            .collect();
        for dir in watching.difference(&wanted) {
            // A backend can remove a watch itself when its directory is deleted.
            let _ = watcher.unwatch(dir);
        }
        for dir in wanted.difference(watching) {
            watcher.watch(dir, RecursiveMode::Recursive)?;
        }
        *watching = wanted;
        Ok(())
    }
}

fn git_metadata(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    if name.ends_with(".lock") {
        return false;
    }
    let top = path.iter().next().and_then(|part| part.to_str()).unwrap_or_default();
    match top {
        "refs" | "reftable" | "rebase-apply" | "rebase-merge" | "sequencer" => true,
        "index" | "HEAD" | "packed-refs" | "MERGE_HEAD" | "MERGE_MSG" | "CHERRY_PICK_HEAD" | "REVERT_HEAD" | "REBASE_HEAD"
        | "ORIG_HEAD" | "FETCH_HEAD" | "BISECT_LOG" | "config" | "config.worktree" | "commondir" => path == Path::new(top),
        _ => false,
    }
}

fn svn_metadata(path: &Path, structural: bool, changes: &mut Changes) -> bool {
    if path.as_os_str().is_empty() && structural {
        changes.rediscover = true;
    } else if path == Path::new("wc.db") || path == Path::new("entries") {
        changes.vcs = true;
    } else {
        return false;
    }
    true
}

fn watch_parent_svn(root: &Path, watcher: &mut impl notify::Watcher, current: &mut Option<PathBuf>) -> notify::Result<()> {
    let wanted = crate::svn::repo_root(root).filter(|dir| dir != root).map(|dir| dir.join(".svn"));
    if *current != wanted {
        if let Some(dir) = current.take() {
            let _ = watcher.unwatch(&dir);
        }
        if let Some(dir) = &wanted {
            watcher.watch(dir, RecursiveMode::NonRecursive)?;
        }
        *current = wanted;
    }
    Ok(())
}

fn classify(root: &Path, git: &GitDirs, parent_svn: Option<&Path>, event: Event, changes: &mut Changes) -> bool {
    if event.need_rescan() {
        changes.rescan = true;
        changes.rediscover = true;
        return true;
    }
    if matches!(event.kind, EventKind::Access(_)) {
        return false;
    }
    let structural = matches!(
        event.kind,
        EventKind::Any | EventKind::Create(_) | EventKind::Remove(_) | EventKind::Modify(notify::event::ModifyKind::Name(_))
    );
    let mut relevant = false;
    'paths: for path in event.paths {
        if let Some(relative) = parent_svn.and_then(|dir| path.strip_prefix(dir).ok()) {
            relevant |= svn_metadata(relative, structural, changes);
            continue;
        }
        if let Some(relative) = git.paths().find_map(|dir| path.strip_prefix(dir).ok()) {
            if (relative.as_os_str().is_empty() && structural) || relative == Path::new("commondir") {
                changes.rediscover = true;
                relevant = true;
            } else if git_metadata(relative) {
                changes.vcs = true;
                relevant = true;
            }
            continue;
        }
        let Ok(relative) = path.strip_prefix(root) else { continue };
        if clangd::is_index_path(relative) {
            continue;
        }
        // Build directories are normally quiet, but clangd needs database changes.
        let database = path.file_name().is_some_and(|name| name == clangd::DATABASE || name == ".clangd");
        let mut parts = relative.iter();
        while let Some(part) = parts.next() {
            if part == ".git" {
                let rest = parts.as_path();
                if relative == Path::new(".git") {
                    changes.rediscover = true;
                    relevant = true;
                } else if rest.as_os_str().is_empty() || git_metadata(rest) {
                    changes.vcs = true;
                    relevant = true;
                }
                continue 'paths;
            }
            if part == ".svn" {
                relevant |= svn_metadata(parts.as_path(), structural, changes);
                continue 'paths;
            }
            if !database && part.to_str().is_some_and(|name| IGNORED_DIRS.contains(&name)) {
                continue 'paths;
            }
        }
        changes.rediscover |= structural && path == root;
        changes
            .dirs
            .insert(path.parent().filter(|p| p.starts_with(root)).unwrap_or(root).to_path_buf());
        changes.paths.insert(path);
        relevant = true;
    }
    relevant
}

fn deadline(start: Instant, last: Instant) -> Instant {
    (last + QUIET).min(start + MAX_DELAY)
}

fn run(
    root: PathBuf,
    mut watcher: impl notify::Watcher,
    events: Receiver<Input>,
    output: &Sender<Changes>,
    ctx: &egui::Context,
) -> notify::Result<()> {
    watcher.watch(&root, RecursiveMode::Recursive)?;
    let mut git = GitDirs::resolve(&root);
    let mut watching = HashSet::new();
    git.update_watches(&root, &mut watcher, &mut watching)?;
    let mut parent_svn = None;
    watch_parent_svn(&root, &mut watcher, &mut parent_svn)?;

    let mut changes = Changes::default();
    let mut window = None;
    let mut databases = Vec::<PathBuf>::new();
    let mut database_watches = HashSet::<PathBuf>::new();
    loop {
        let input = match window {
            None => events.recv().ok(),
            Some((start, last)) => {
                let remaining = deadline(start, last).saturating_duration_since(Instant::now());
                match if remaining.is_zero() {
                    Err(RecvTimeoutError::Timeout)
                } else {
                    events.recv_timeout(remaining)
                } {
                    Ok(input) => Some(input),
                    Err(RecvTimeoutError::Disconnected) => None,
                    Err(RecvTimeoutError::Timeout) => {
                        if changes.rediscover {
                            git = GitDirs::resolve(&root);
                            git.update_watches(&root, &mut watcher, &mut watching)?;
                            watch_parent_svn(&root, &mut watcher, &mut parent_svn)?;
                        }
                        if output.send(std::mem::take(&mut changes)).is_err() {
                            return Ok(());
                        }
                        ctx.request_repaint();
                        window = None;
                        continue;
                    }
                }
            }
        };
        match input {
            Some(Input::Event(event)) => {
                let event = event?;
                let mut database_change = false;
                if !matches!(event.kind, EventKind::Access(_)) {
                    for path in &event.paths {
                        if databases.iter().any(|db| db.starts_with(path)) {
                            database_change |= changes.paths.insert(path.clone());
                        }
                    }
                }
                if classify(&root, &git, parent_svn.as_deref(), event, &mut changes) || database_change {
                    let now = Instant::now();
                    window = Some((window.map_or(now, |(start, _)| start), now));
                }
            }
            Some(Input::CompilationDatabase(paths)) => {
                let wanted: HashSet<_> = paths
                    .iter()
                    .filter(|path| !path.starts_with(&root))
                    .filter_map(|path| path.ancestors().skip(1).find(|dir| dir.is_dir()))
                    .map(Path::to_path_buf)
                    .collect();
                for dir in database_watches.difference(&wanted) {
                    let _ = watcher.unwatch(dir);
                }
                database_watches.retain(|dir| wanted.contains(dir));
                for dir in wanted.difference(&database_watches).cloned().collect::<Vec<_>>() {
                    if watcher.watch(&dir, RecursiveMode::NonRecursive).is_ok() {
                        database_watches.insert(dir);
                    }
                }
                databases = paths;
            }
            Some(Input::Stop) | None => return Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::SystemTime;

    #[test]
    fn debounce_waits_for_quiet_up_to_the_maximum_delay() {
        let start = Instant::now();
        assert_eq!(deadline(start, start), start + QUIET);
        let last = start + Duration::from_millis(200);
        assert_eq!(deadline(start, last), last + QUIET);
        let last = start + Duration::from_millis(900);
        assert_eq!(deadline(start, last), start + MAX_DELAY);
    }

    #[test]
    fn filesystem_changes_wake_once_and_noise_leaves_the_watcher_idle() {
        struct Scratch(PathBuf);
        impl Drop for Scratch {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }
        let unique = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_nanos();
        let base = Scratch(std::env::temp_dir().join(format!("edittr-watch-{}-{unique}", std::process::id())));
        let root = base.0.join("project");
        let common = base.0.join("admin");
        let admin = common.join("worktrees/test");
        fs::create_dir_all(root.join("src/.svn")).unwrap();
        fs::create_dir_all(base.0.join(".svn")).unwrap();
        fs::create_dir_all(root.join("target")).unwrap();
        fs::create_dir_all(common.join("refs/heads")).unwrap();
        fs::create_dir_all(common.join("objects")).unwrap();
        fs::create_dir_all(&admin).unwrap();
        fs::write(root.join(".git"), "gitdir: ../admin/worktrees/test\n").unwrap();
        fs::write(admin.join("commondir"), "../..\n").unwrap();
        fs::write(admin.join("index"), "index").unwrap();
        let root = root.canonicalize().unwrap();

        let ctx = egui::Context::default();
        let (repaints, requests) = channel();
        ctx.set_repaint_observer(move |_| {
            let _ = repaints.send(Instant::now());
        });
        // The polling backend gives this headless test filesystem events even in
        // sandboxes that deny access to the native notification service.
        let config = notify::Config::default()
            .with_poll_interval(Duration::from_millis(20))
            .with_compare_contents(true);
        let watcher = Watcher::start_with::<notify::PollWatcher>(root.clone(), ctx, config);
        let receive = || {
            let changes = watcher.output.recv_timeout(Duration::from_secs(5)).unwrap_or_else(|err| {
                panic!("watcher did not publish a batch: {err}; setup error: {:?}", watcher.error());
            });
            let when = requests.recv_timeout(Duration::from_secs(1)).unwrap();
            (changes, when)
        };

        // The worker processes this marker once the native watches have been installed.
        watcher
            .input
            .send(Input::Event(Ok(Event::new(EventKind::Any).add_path(root.join("ready")))))
            .unwrap();
        assert!(receive().0.affects(&root.join("ready")));

        let first = root.join("src/first.rs");
        let second = root.join("second.rs");
        fs::write(&first, "first").unwrap();
        fs::write(&first, "updated").unwrap();
        fs::write(&second, "second").unwrap();
        let (changes, _) = receive();
        assert!(changes.affects(&first));
        assert!(changes.affects(&second));
        assert!(changes.dirs.contains(&root.join("src")));
        assert!(changes.dirs.contains(&root));
        assert!(!changes.vcs);

        fs::write(&first, "modified after creation").unwrap();
        assert!(receive().0.affects(&first));

        let index = fs::OpenOptions::new().write(true).open(admin.join("index")).unwrap();
        let touched = Instant::now();
        index.set_modified(SystemTime::now() + Duration::from_secs(60)).unwrap();
        drop(index);
        let (changes, published) = receive();
        assert!(changes.vcs);
        assert!(changes.dirs.is_empty());
        assert!(published.duration_since(touched) >= QUIET);

        fs::write(common.join("refs/heads/main"), "ref").unwrap();
        assert!(receive().0.vcs);
        for db in [root.join("src/.svn/wc.db"), base.0.join(".svn/wc.db")] {
            fs::write(db, "svn").unwrap();
            let (changes, _) = receive();
            assert!(changes.vcs);
            assert!(changes.paths.is_empty());
        }

        fs::create_dir_all(root.join("build")).unwrap();
        let database = root.join("build/compile_commands.json");
        fs::write(&database, "[]").unwrap();
        assert!(receive().0.affects(&database));

        fs::create_dir_all(root.join(".cache/clangd/index")).unwrap();
        let _ = receive(); // The new .cache directory itself is visible.
        fs::write(root.join(".cache/clangd/index/entry.idx"), "cached").unwrap();
        fs::write(admin.join("index.lock"), "lock").unwrap();
        fs::write(common.join("objects/ignored"), "object").unwrap();
        fs::write(base.0.join(".svn/wc.db-journal"), "journal").unwrap();
        fs::write(root.join("target/ignored"), "build").unwrap();
        assert_eq!(requests.recv_timeout(MAX_DELAY + QUIET), Err(RecvTimeoutError::Timeout));
        assert!(matches!(watcher.output.try_recv(), Err(std::sync::mpsc::TryRecvError::Empty)));

        fs::remove_file(root.join(".git")).unwrap();
        assert!(receive().0.rediscover);
    }
}
