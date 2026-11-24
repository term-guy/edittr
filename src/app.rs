mod document_save;
mod find;
mod gutter;
mod navigation;
mod outline;
mod problems;
mod search_replace;
mod session;
#[cfg(feature = "tour")]
mod tour;

use crate::commands::{self, Command, Setting};
use crate::editor;
use crate::graph::Graph;
use crate::icons as ic;
use crate::search::{self, FileHits};
use crate::session::{Side, Startup, Writer};
use crate::settings::{Settings, SettingsWriter};
use crate::vcs::{self, FileChange, Status, Vcs};
use crate::watch::{Changes, IGNORED_DIRS, Watcher};
use crate::{blame, highlight};
use crate::{goto, installer, lsp, symbols, workspace};
use eframe::egui::{self, Color32, Key, Modifiers, RichText};
use egui_extras::syntax_highlighting::{self, CodeTheme};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::time::{Duration, Instant};

const SEARCH_DEBOUNCE: Duration = Duration::from_millis(250);
const SAFETY_POLL: Duration = Duration::from_secs(30);
const FALLBACK_POLL: Duration = Duration::from_secs(3);
const ACTIVITY_POLL: Duration = Duration::from_millis(250);
const HIGHLIGHT_LIMIT: usize = 1_000_000; // bytes; bigger files are shown as plain text
type DirEntries = Vec<(PathBuf, bool)>;

struct Doc {
    id: u64,
    path: PathBuf,
    text: String,
    saved: String,
    crlf: bool,
    lang: String,
    /// Pending selection (char range) to reveal on the next frame, e.g. from a search hit.
    goto: Option<(usize, usize)>,
    /// Char index of the text cursor (last frame).
    cursor: usize,
    /// Other end of the selection (== cursor when nothing is selected).
    anchor: usize,
    /// Editor widget state: selections (multi-cursor), undo history.
    ed: editor::State,
    /// Text Tab inserts (detected from the file), and the line comment token.
    indent: String,
    comment: Option<&'static str>,
    /// Bumped on every edit; `edited_at` is when the last one happened.
    version: u64,
    edited_at: Option<Instant>,
    /// Inline blame of `blame_version` of the text (None: none / not in a repo).
    blame: Option<blame::Blame>,
    blame_version: Option<u64>,
    blame_rx: Option<Receiver<(u64, Result<blame::Blame, String>)>>,
    /// Incremental syntax highlighting + cached layout.
    hl: highlight::Cache,
    /// Gutter change markers vs. the committed file.
    markers: gutter::Markers,
    /// Per-frame values cached by `version` (they are O(file size) to compute).
    dirty_cache: std::cell::Cell<(u64, bool)>,
    line_col_cache: std::cell::Cell<(u64, usize, (usize, usize))>,
}

impl Doc {
    /// The selected text, if any and if it fits on one line.
    fn selected_text(&self) -> Option<String> {
        let (a, b) = (self.cursor.min(self.anchor), self.cursor.max(self.anchor));
        if a == b {
            return None;
        }
        let sel: String = self.text.chars().skip(a).take(b - a).collect();
        (!sel.contains('\n') && !sel.trim().is_empty()).then_some(sel)
    }

    fn ext(&self) -> String {
        self.path
            .extension()
            .map(|e| e.to_string_lossy().to_lowercase())
            .unwrap_or_default()
    }
}

struct InstallJob {
    command: &'static str,
    progress: Arc<installer::Progress>,
    rx: Receiver<Result<PathBuf, String>>,
}

struct VcsRefresh {
    repository: DetectedRepo,
    status: Option<Result<Status, String>>,
    branches: Vec<String>,
    refresh_git_history: bool,
    files: Vec<(u64, u64, DiskText)>,
}

impl Doc {
    fn dirty(&self) -> bool {
        let (v, dirty) = self.dirty_cache.get();
        if v == self.version {
            return dirty;
        }
        let dirty = self.text != self.saved;
        self.dirty_cache.set((self.version, dirty));
        dirty
    }

    fn mark_saved(&mut self) {
        self.saved = self.text.clone();
        self.dirty_cache.set((self.version, false));
    }

    fn mark_saved_snapshot(&mut self, text: String) {
        self.saved = text;
        self.dirty_cache.set((self.version, self.text != self.saved));
    }

    /// 0-based line and UTF-16 column of the cursor (cached: it scans the text).
    fn line_col(&self) -> (usize, usize) {
        let (v, c, lc) = self.line_col_cache.get();
        if v == self.version && c == self.cursor {
            return lc;
        }
        let lc = goto::line_col16(&self.text, self.cursor);
        self.line_col_cache.set((self.version, self.cursor, lc));
        lc
    }
    fn name(&self) -> String {
        self.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
    }

    /// Replace a clean buffer with the latest text from disk.
    fn reload(&mut self, text: String, _ctx: &egui::Context) {
        let len = text.chars().count();
        self.cursor = self.cursor.min(len);
        self.anchor = self.anchor.min(len);
        self.goto = self.goto.map(|(a, b)| (a.min(len), b.min(len)));
        self.saved = text.clone();
        self.text = text;
        self.version += 1; // the editor clamps its selections to the new text
        self.clear_blame();
    }

    fn clear_blame(&mut self) {
        self.blame = None;
        self.blame_version = None;
        self.blame_rx = None;
    }
}

// File tabs are the common case; boxing them would only add an indirection.
#[allow(clippy::large_enum_variant)]
enum Tab {
    File(Doc),
    Diff(Box<DiffTab>),
    Graph,
}

struct DiffTab {
    title: String,
    path: PathBuf,
    lang: String,
    lines: Vec<String>,
    sbs: crate::diff::SideBySide,
    inline: bool,
    added: usize,
    removed: usize,
    /// Row index of the first row of each change block.
    blocks: Vec<usize>,
    /// Row to scroll to on the next frame.
    jump: Option<usize>,
    /// First visible row (last frame).
    top_row: usize,
    hscroll: f32,
}

impl DiffTab {
    fn new(title: String, path: PathBuf, text: &str) -> Self {
        use crate::diff::Kind;
        let sbs = crate::diff::side_by_side(text);
        let changed = |k: Kind| matches!(k, Kind::Changed | Kind::Added | Kind::Removed);
        let blocks: Vec<usize> = (0..sbs.rows.len())
            .filter(|&i| changed(sbs.rows[i].kind) && (i == 0 || !changed(sbs.rows[i - 1].kind)))
            .collect();
        let added = sbs
            .rows
            .iter()
            .filter(|r| matches!(r.kind, Kind::Added | Kind::Changed) && r.right.is_some())
            .count();
        let removed = sbs
            .rows
            .iter()
            .filter(|r| matches!(r.kind, Kind::Removed | Kind::Changed) && r.left.is_some())
            .count();
        let lang = path.extension().map(|e| e.to_string_lossy().into_owned()).unwrap_or_default();
        let jump = blocks.first().copied();
        DiffTab {
            title,
            path,
            lang,
            lines: text.lines().map(str::to_string).collect(),
            sbs,
            inline: false,
            added,
            removed,
            blocks,
            jump,
            top_row: 0,
            hscroll: 0.0,
        }
    }
}

#[derive(Default)]
struct SearchState {
    opts: search::Options,
    /// Options of the search currently shown / running.
    searched: search::Options,
    edited: Option<Instant>,
    results: Vec<FileHits>,
    total: usize,
    collapsed: HashSet<usize>,
    rx: Option<Receiver<search::Msg>>,
    cancel: Arc<AtomicBool>,
    summary: String,
    error: Option<String>,
    focus: bool,
    /// The "files to include / exclude" fields are shown.
    show_filters: bool,
}

enum SearchRow {
    File(usize),
    Hit(usize, usize),
}

struct QuickOpen {
    query: String,
    files: Vec<String>,
    /// `files`, lowercased once (matching is case-insensitive).
    lower: Vec<String>,
    files_rx: Option<Receiver<(Vec<String>, Vec<String>)>>,
    selected: usize,
    /// Indices into `files` of the best matches, for `matched_query`.
    matches: Vec<usize>,
    matched_query: Option<String>,
}

enum Confirm {
    CloseTabs(Vec<usize>),
    SwitchWorkspace(PathBuf),
    Discard(FileChange),
}

type DetectedRepo = Option<(PathBuf, Vcs)>;

enum VcsOperation {
    Change(&'static str),
    History(&'static str),
    Commit(String, bool),
    Discard(PathBuf),
}

impl VcsOperation {
    fn label(&self) -> &'static str {
        match self {
            Self::Change(label) | Self::History(label) => label,
            Self::Commit(..) => "Commit",
            Self::Discard(_) => "Discard",
        }
    }
}

struct VcsJob {
    operation: VcsOperation,
    rx: Receiver<Result<String, String>>,
}

pub struct App {
    ctx: egui::Context,
    /// Window title last sent (sending it every frame costs a platform call).
    title: String,
    /// Open project folder; None = empty project.
    root: Option<PathBuf>,
    watcher: Option<Watcher>,
    ignored: Vec<String>,
    /// Recently opened folders, most recent first.
    recent: Vec<PathBuf>,
    repo: Option<PathBuf>,
    vcs: Option<Vcs>,
    repo_rx: Option<Receiver<Result<DetectedRepo, String>>>,
    side: Side,
    side_open: bool,
    tabs: Vec<Tab>,
    active: usize,
    next_doc_id: u64,

    tree: HashMap<PathBuf, DirEntries>,
    tree_loading: HashSet<PathBuf>,
    tree_requested: HashSet<PathBuf>,
    tree_rx: Option<Receiver<HashMap<PathBuf, DirEntries>>>,
    expanded: HashSet<PathBuf>,
    outline: outline::Outline,

    status: Status,
    status_rx: Option<Receiver<VcsRefresh>>,
    refresh_pending: bool,
    pending_changes: Changes,
    last_poll: Instant,
    was_focused: Option<bool>,
    branches: Vec<String>,
    commit_msg: String,
    amend: bool,

    graph: Option<Graph>,
    graph_sel: Option<usize>,
    graph_detail: Vec<String>,
    graph_rx: Option<Receiver<Result<Graph, String>>>,
    graph_detail_rx: Option<Receiver<Vec<String>>>,
    diff_rx: Option<Receiver<DiffTab>>,

    bg_task: Option<VcsJob>,
    message: String,
    quick_open: Option<QuickOpen>,
    picker: Option<navigation::Picker>,
    /// Tab shown in the side editor (Open to the Side).
    split: Option<usize>,
    confirm: Option<Confirm>,
    open_folder_requested: bool,
    open_folder: Option<String>,
    search: SearchState,
    find: find::Find,
    search_replace: search_replace::SearchReplace,

    lsp: problems::LanguageServers,
    problems: problems::Problems,
    goto_rx: Option<Receiver<goto::Outcome>>,
    /// Where we jumped from, for "go back".
    nav_back: Vec<(PathBuf, usize)>,
    install: Option<InstallJob>,
    install_prompt: Option<&'static str>,
    install_declined: HashSet<&'static str>,
    settings: Settings,
    document_save: document_save::DocumentSave,
    settings_writer: SettingsWriter,
    sessions: session::Sessions,
    /// Collapsed folders in the Changes tree, keyed "S:<path>" (staged) or "U:<path>".
    changes_collapsed: HashSet<String>,
    #[cfg(feature = "tour")]
    tour: Option<tour::Tour>,
}

impl App {
    pub fn new(
        cc: &eframe::CreationContext,
        startup: Startup,
        settings: Settings,
        settings_writer: SettingsWriter,
        session_writer: Option<Writer>,
    ) -> Self {
        ic::install(&cc.egui_ctx);
        highlight::warm_up();
        cc.egui_ctx.global_style_mut(|s| {
            s.spacing.item_spacing.y = 3.0;
        });
        let mut app = App {
            ctx: cc.egui_ctx.clone(),
            title: String::new(),
            repo: None,
            vcs: None,
            repo_rx: None,
            root: None,
            watcher: None,
            ignored: Vec::new(),
            recent: load_recent(),
            side: Side::Explorer,
            side_open: true,
            tabs: Vec::new(),
            active: 0,
            next_doc_id: 0,
            document_save: Default::default(),
            tree: HashMap::new(),
            tree_loading: HashSet::new(),
            tree_requested: HashSet::new(),
            tree_rx: None,
            expanded: HashSet::new(),
            outline: Default::default(),
            status: Status::default(),
            status_rx: None,
            refresh_pending: false,
            pending_changes: Changes::default(),
            last_poll: Instant::now(),
            was_focused: None,
            branches: Vec::new(),
            commit_msg: String::new(),
            amend: false,
            graph: None,
            graph_sel: None,
            graph_detail: Vec::new(),
            graph_rx: None,
            graph_detail_rx: None,
            diff_rx: None,
            bg_task: None,
            message: String::new(),
            quick_open: None,
            picker: None,
            split: None,
            confirm: None,
            open_folder_requested: false,
            open_folder: None,
            search: SearchState::default(),
            lsp: problems::LanguageServers::default(),
            problems: problems::Problems::default(),
            find: find::Find::default(),
            search_replace: search_replace::SearchReplace::default(),
            goto_rx: None,
            nav_back: Vec::new(),
            install: None,
            install_prompt: None,
            install_declined: HashSet::new(),
            settings,
            settings_writer,
            sessions: session::Sessions::new(session_writer, startup.window.clone()),
            changes_collapsed: HashSet::new(),
            #[cfg(feature = "tour")]
            tour: tour::Tour::from_env(),
        };
        app.start_session(startup);
        app
    }

    fn activate_root(&mut self, root: PathBuf, ignored: Vec<String>) {
        self.clear_project();
        self.watcher = Some(Watcher::start(root.clone(), self.ctx.clone()));
        self.ignored = ignored;
        self.root = Some(root);
        self.search.cancel.store(true, Ordering::Relaxed);
        let opts = std::mem::take(&mut self.search.opts);
        self.search = SearchState {
            edited: (!opts.query.is_empty()).then(Instant::now),
            opts,
            ..SearchState::default()
        };
        self.reset_lsp();
        self.goto_rx = None;
        self.nav_back.clear();
        self.last_poll = Instant::now();
        self.pending_changes.rediscover = true;
        self.refresh_vcs();
    }

    fn clear_project(&mut self) {
        self.root = None;
        self.watcher = None;
        self.update_repository(None);
        self.repo_rx = None;
        self.ignored.clear();
        self.expanded.clear();
        self.tree.clear();
        self.tree_loading.clear();
        self.tree_requested.clear();
        self.tree_rx = None;
        self.status_rx = None;
        self.refresh_pending = false;
        self.pending_changes = Changes::default();
        self.bg_task = None;
        self.amend = false;
        self.quick_open = None;
        self.picker = None;
        self.confirm = None;
    }

    /// Folder to run tools in: the project, else the active file's folder.
    fn work_dir(&self) -> PathBuf {
        if let Some(r) = self.repo.clone().or_else(|| self.root.clone()) {
            return r;
        }
        match self.tabs.get(self.active) {
            Some(Tab::File(d)) => d.path.parent().map(PathBuf::from),
            _ => None,
        }
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("."))
    }

    fn request_open_folder(&mut self) {
        self.open_folder_requested = true;
    }

    fn open_requested_folder(&mut self, frame: &eframe::Frame) {
        if !std::mem::take(&mut self.open_folder_requested)
            || self.quick_open.is_some()
            || self.picker.is_some()
            || self.confirm.is_some()
            || self.install_prompt.is_some()
            || self.open_folder.is_some()
            || self.search_replace.confirming()
        {
            return;
        }

        let mut picker = rfd::FileDialog::new().set_title("Open folder").set_parent(frame);
        if let Some(root) = &self.root {
            picker = picker.set_directory(root);
        }
        #[cfg(target_os = "linux")]
        let (path, failed) = folder_picker::pick_folder(picker);
        #[cfg(not(target_os = "linux"))]
        let path = picker.pick_folder();

        if let Some(path) = path {
            match path.canonicalize() {
                Ok(path) => self.set_root(path),
                Err(err) => self.message = format!("Could not open {}: {err}", path.display()),
            }
        }

        #[cfg(target_os = "linux")]
        if failed {
            self.open_folder = Some(self.root.as_ref().map(|r| r.display().to_string()).unwrap_or_default());
        }
    }

    /// Path shown relative to the project when possible.
    fn display_path(&self, p: &Path) -> String {
        self.root
            .as_ref()
            .and_then(|r| p.strip_prefix(r).ok())
            .unwrap_or(p)
            .display()
            .to_string()
    }

    // --------------------------------------------------------- source control

    fn repository(&self) -> Option<(PathBuf, Vcs)> {
        Some((self.repo.clone()?, self.vcs?))
    }

    fn refresh_vcs(&mut self) {
        self.refresh_pending = true;
        self.start_refresh();
    }

    fn start_refresh(&mut self) {
        if self.status_rx.is_some() || self.repo_rx.is_some() || self.bg_task.is_some() || !self.refresh_pending {
            return;
        }
        self.refresh_pending = false;
        let changes = std::mem::take(&mut self.pending_changes);
        let files: Vec<_> = self
            .tabs
            .iter()
            .filter_map(|tab| match tab {
                Tab::File(doc) if (changes.rescan || changes.affects(&doc.path)) && !doc.dirty() && !self.writing_document(&doc.path) => {
                    Some((doc.path.clone(), doc.id, doc.version))
                }
                _ => None,
            })
            .collect();
        let root = self
            .root
            .clone()
            .filter(|_| changes.rediscover || (self.repo.is_none() && changes.vcs));
        let repository = self.repository();
        if root.is_none() && repository.is_none() && files.is_empty() {
            return;
        }
        self.status_rx = Some(background(&self.ctx, move || {
            let repository = match root {
                Some(root) => vcs::detect(&root),
                None => repository,
            };
            let (status, branches) = repository
                .as_ref()
                .map_or_else(|| (None, Vec::new()), |(repo, vcs)| (Some(vcs.status(repo)), vcs.branches(repo)));
            let files = files
                .into_iter()
                .filter_map(|(path, id, version)| read_doc_text(&path).ok().flatten().map(|text| (id, version, text)))
                .collect();
            VcsRefresh {
                repository,
                status,
                branches,
                refresh_git_history: changes.vcs || changes.rediscover,
                files,
            }
        }));
    }

    fn update_repository(&mut self, repository: DetectedRepo) -> bool {
        if self.repository() == repository {
            return false;
        }
        (self.repo, self.vcs) = match repository {
            Some((root, vcs)) => (Some(root), Some(vcs)),
            None => (None, None),
        };
        self.status = Status::default();
        self.branches.clear();
        self.graph = None;
        self.graph_sel = None;
        self.graph_detail.clear();
        self.graph_rx = None;
        self.graph_detail_rx = None;
        self.diff_rx = None;
        for tab in &mut self.tabs {
            if let Tab::File(doc) = tab {
                doc.clear_blame();
            }
        }
        true
    }

    fn refresh_history(&mut self) {
        if self.tabs.iter().any(|t| matches!(t, Tab::Graph)) {
            self.load_graph();
        }
    }

    fn load_graph(&mut self) {
        let Some((repo, vcs)) = self.repository() else { return };
        let (tx, rx) = channel();
        let ctx = self.ctx.clone();
        std::thread::spawn(move || {
            let result = vcs.log(&repo, 2000, |cached| {
                let _ = tx.send(Ok(Graph::build(cached)));
                ctx.request_repaint();
            });
            let _ = tx.send(result.map(Graph::build));
            drop(tx);
            ctx.request_repaint();
        });
        self.graph_rx = Some(rx);
    }

    fn load_graph_detail(&mut self, row: usize) {
        let Some((repo, vcs)) = self.repository() else { return };
        let Some(commit) = self.graph.as_ref().and_then(|g| g.commits.get(row)) else {
            return;
        };
        let hash = commit.hash.clone();
        self.graph_sel = Some(row);
        self.graph_detail.clear();
        self.graph_detail_rx = Some(background(&self.ctx, move || {
            vcs.show_commit(&repo, &hash).lines().map(str::to_string).collect()
        }));
    }

    fn spawn_vcs(&mut self, operation: VcsOperation, work: impl FnOnce(Vcs, &Path) -> Result<String, String> + Send + 'static) {
        let Some((repo, vcs)) = self.repository() else { return };
        if self.bg_task.is_some() {
            return;
        }
        let rx = background(&self.ctx, move || work(vcs, &repo));
        self.message = format!("{}…", operation.label());
        self.bg_task = Some(VcsJob { operation, rx });
    }

    fn stage_change(&mut self, path: Option<String>, stage: bool) {
        let label = if stage { "Stage" } else { "Unstage" };
        self.spawn_vcs(VcsOperation::Change(label), move |vcs, repo| {
            vcs.stage(repo, path.as_deref(), stage)
        });
    }

    /// Keep the active file's inline blame current: rerun after edits pause.
    fn poll_blame(&mut self, ctx: &egui::Context) {
        const SETTLE: Duration = Duration::from_millis(700);
        if !self.settings.inline_blame {
            return;
        }
        let (Some(repo), Some(vcs)) = (self.repo.clone(), self.vcs) else {
            return;
        };
        let Some(Tab::File(doc)) = self.tabs.get_mut(self.active) else {
            return;
        };
        if let Some(rx) = &doc.blame_rx
            && let Ok((version, res)) = rx.try_recv()
        {
            doc.blame_rx = None;
            doc.blame = res.ok();
            doc.blame_version = Some(version);
        }
        // Git blames the live buffer, so it follows edits. Svn blames the saved file
        // (it may need the server): once per open and after each save.
        let fresh = match vcs {
            Vcs::Git => doc.blame_version == Some(doc.version),
            Vcs::Svn => doc.blame_version.is_some(),
        };
        if doc.blame_rx.is_some() || fresh || doc.text.len() > 2_000_000 {
            return;
        }
        if let Some(t) = doc.edited_at
            && t.elapsed() < SETTLE
        {
            ctx.request_repaint_after(SETTLE - t.elapsed());
            return;
        }
        let Ok(rel) = doc.path.strip_prefix(&repo) else { return };
        let rel = rel.to_string_lossy().replace('\\', "/");
        let (text, version, ctx) = (doc.text.clone(), doc.version, ctx.clone());
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let res = match vcs {
                Vcs::Git => blame::git(&repo, &rel, &text),
                Vcs::Svn => vcs::blame(vcs, &repo, &rel).map(|lines| blame::from_svn(&lines)),
            };
            let _ = tx.send((version, res));
            ctx.request_repaint();
        });
        doc.blame_rx = Some(rx);
    }

    fn poll_background(&mut self, ctx: &egui::Context) {
        self.poll_goto();
        self.poll_problems();
        self.poll_picker();
        self.poll_outline();
        self.poll_search(ctx);
        self.poll_project_replace(ctx);
        self.poll_find_edit();
        if let Some(err) = self.settings_writer.errors.try_iter().last() {
            self.message = format!("Could not save settings: {err}");
        }
        self.poll_project_changes(ctx);
        self.poll_document_saves();
        self.poll_vcs(ctx);
        self.poll_blame(ctx);
        self.poll_markers();
        self.start_refresh();
        if let Some(entries) = receive(&mut self.tree_rx) {
            self.tree
                .extend(entries.into_iter().filter(|(dir, _)| self.tree_loading.contains(dir)));
        }
        if self.tree_rx.is_none() {
            self.tree_loading.clear();
        }
        if let Some(q) = &mut self.quick_open
            && let Some((files, lower)) = receive(&mut q.files_rx)
        {
            q.files = files;
            q.lower = lower;
            q.matched_query = None; // re-rank
        }
    }

    fn poll_vcs(&mut self, ctx: &egui::Context) {
        if let Some(detected) = receive(&mut self.repo_rx) {
            match detected {
                Ok(repository) => {
                    if self.update_repository(repository) {
                        self.refresh_history();
                    }
                    self.refresh_vcs();
                }
                Err(e) => self.message = e,
            }
        }
        if let Some(refresh) = receive(&mut self.status_rx) {
            if self.bg_task.is_none() && self.repo_rx.is_none() {
                let repo_changed = self.update_repository(refresh.repository);
                if let Some(status) = refresh.status {
                    match status {
                        Ok(status) => self.status = status,
                        Err(e) => self.message = e,
                    }
                }
                self.branches = refresh.branches;
                if repo_changed || (refresh.refresh_git_history && self.vcs == Some(Vcs::Git)) {
                    self.refresh_history();
                }
            }
            for (id, version, DiskText { text, crlf }) in refresh.files {
                if let Some(doc) = self.tabs.iter_mut().find_map(|tab| match tab {
                    Tab::File(doc) if doc.id == id => Some(doc),
                    _ => None,
                }) && doc.version == version
                    && !doc.dirty()
                    && doc.text != text
                {
                    doc.crlf = crlf;
                    doc.reload(text, ctx);
                }
            }
        }
        if let Some(job) = &self.bg_task {
            let result = match job.rx.try_recv() {
                Ok(result) => Some(result),
                Err(TryRecvError::Disconnected) => Some(Err("Source control worker stopped".into())),
                Err(TryRecvError::Empty) => None,
            };
            if let Some(result) = result {
                let operation = self.bg_task.take().unwrap().operation;
                let label = operation.label();
                match result {
                    Ok(out) => {
                        self.message = out
                            .lines()
                            .last()
                            .filter(|s| !s.is_empty())
                            .map_or_else(|| format!("{label} done"), str::to_string);
                        match operation {
                            VcsOperation::Commit(draft, amend) => {
                                if self.commit_msg == draft && self.amend == amend {
                                    self.commit_msg.clear();
                                    self.amend = false;
                                }
                                self.refresh_history();
                            }
                            VcsOperation::Discard(path) => {
                                if let Some(parent) = path.parent() {
                                    self.invalidate_tree(|dir| dir == parent);
                                }
                                self.pending_changes.paths.insert(path);
                            }
                            VcsOperation::History(_) => self.refresh_history(),
                            VcsOperation::Change(_) => {}
                        }
                    }
                    Err(e) => self.message = format!("{label} failed: {e}"),
                }
                self.refresh_vcs();
            }
        }
        if let Some(rx) = &self.graph_rx {
            loop {
                match rx.try_recv() {
                    Ok(Ok(graph)) => {
                        self.graph_sel = self.graph_sel.and_then(|row| {
                            let selected = &self.graph.as_ref()?.commits.get(row)?.hash;
                            graph.commits.iter().position(|c| &c.hash == selected)
                        });
                        if self.graph_sel.is_none() {
                            self.graph_detail.clear();
                            self.graph_detail_rx = None;
                        }
                        self.graph = Some(graph);
                    }
                    Ok(Err(e)) => self.message = e,
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        self.graph_rx = None;
                        break;
                    }
                }
            }
        }
        if let Some(detail) = receive(&mut self.graph_detail_rx) {
            self.graph_detail = detail;
        }
        if let Some(mut tab) = receive(&mut self.diff_rx) {
            tab.inline = matches!(self.tabs.iter().find(|t| matches!(t, Tab::Diff(_))), Some(Tab::Diff(d)) if d.inline);
            self.show_tab(Tab::Diff(Box::new(tab)));
        }
    }

    fn poll_project_changes(&mut self, ctx: &egui::Context) {
        let mut changes = self.watcher.as_ref().map_or_else(Changes::default, Watcher::take);
        let watcher_rescan = changes.rescan;
        if let Some(error) = self.watcher.as_ref().and_then(Watcher::error) {
            self.watcher = None;
            self.message = format!("File watcher unavailable. Checking for changes while focused: {error}");
        }
        let focused = ctx.input(|i| i.viewport().focused.unwrap_or(i.focused));
        let gained_focus = self.was_focused.replace(focused) == Some(false) && focused;
        if self.root.is_some() && focused {
            let interval = if self.watcher.is_some() { SAFETY_POLL } else { FALLBACK_POLL };
            if gained_focus || self.last_poll.elapsed() >= interval {
                self.last_poll = Instant::now();
                changes.rescan = true;
                changes.rediscover = true;
            }
            ctx.request_repaint_after(interval.saturating_sub(self.last_poll.elapsed()));
        } else if gained_focus {
            changes.rescan = true;
        }
        self.invalidate_editorconfigs(&changes, watcher_rescan);
        if changes.rescan || !changes.dirs.is_empty() {
            self.invalidate_tree(|dir| changes.rescan || changes.dirs.contains(dir) || changes.affects(dir));
        }
        if changes.vcs || changes.rescan || changes.rediscover {
            // A commit, checkout or revert changes the committed text the markers diff against.
            self.reset_markers();
        }
        if changes.rescan || changes.rediscover || changes.vcs || !changes.paths.is_empty() {
            self.pending_changes.merge(changes);
            self.refresh_pending = true;
        }
    }

    // --------------------------------------------------------------- tabs

    /// Diagnostics of tab `i` as (line, col, end line, end col, color).
    fn squiggles_for(&self, i: usize, v: &egui::Visuals) -> Vec<(usize, usize, usize, usize, Color32)> {
        let Some(Tab::File(doc)) = self.tabs.get(i) else {
            return Vec::new();
        };
        self.diagnostics_for(&doc.path)
            .iter()
            .map(|d| {
                let color = match d.severity {
                    crate::diagnostics::Severity::Error => v.error_fg_color,
                    crate::diagnostics::Severity::Warning => v.warn_fg_color,
                    _ => v.hyperlink_color.gamma_multiply(0.7),
                };
                (d.line, d.col_start, d.end_line, d.col_end, color)
            })
            .collect()
    }

    /// The second editor (Open to the Side), on the right of the main one.
    fn split_pane(&mut self, ui: &mut egui::Ui) {
        let Some(si) = self.split else { return };
        if !matches!(self.tabs.get(si), Some(Tab::File(_))) {
            self.split = None;
            return;
        }
        let diags = self.squiggles_for(si, ui.visuals());
        let settings = self.settings;
        let (mut close, mut goto) = (false, None);
        egui::Panel::right("split-pane")
            .resizable(true)
            .default_size(ui.available_width() / 2.0)
            .min_size(200.0)
            .show(ui, |ui| {
                let Some(Tab::File(doc)) = self.tabs.get_mut(si) else { return };
                ui.horizontal(|ui| {
                    ui.label(RichText::new(doc.name()).strong());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if icon_button(ui, ic::X).on_hover_text("Close the split").clicked() {
                            close = true;
                        }
                    });
                });
                ui.separator();
                goto = editor(ui, doc, settings, None, &diags, &[], 1);
            });
        if close {
            self.split = None;
        }
        if let Some(idx) = goto {
            // Go to definition from the side editor: make it the active one first.
            self.active = si;
            self.go_to_definition(idx, &self.ctx.clone());
        }
    }

    fn open_file(&mut self, path: &Path) {
        if let Some(i) = self.tabs.iter().position(|t| matches!(t, Tab::File(d) if d.path == path)) {
            self.active = i;
            return;
        }
        if self.search_replace.is_writing(path) {
            self.message = "Wait for replacements to finish before opening this file".into();
            return;
        }
        let DiskText { text, crlf } = match read_doc_text(path) {
            Ok(Some(text)) => text,
            Ok(None) => {
                self.message = format!("{} looks binary; not opening", path.display());
                return;
            }
            Err(e) => {
                self.message = format!("{}: {e}", path.display());
                return;
            }
        };
        // The active document's sync starts its language server (see sync_active_document).
        self.add_file_tab(path, text, crlf);
        self.active = self.tabs.len() - 1;
    }

    fn add_file_tab(&mut self, path: &Path, text: String, crlf: bool) {
        let lang = path
            .extension()
            .or_else(|| path.file_name())
            .map(|e| e.to_string_lossy().into_owned())
            .unwrap_or_default();
        let indent = editor::detect_indent(&text);
        let comment = editor::comment_token(&lang.to_lowercase());
        self.tabs.push(Tab::File(Doc {
            id: self.next_doc_id,
            path: path.to_path_buf(),
            saved: text.clone(),
            crlf,
            text,
            lang,
            goto: None,
            cursor: 0,
            anchor: 0,
            ed: Default::default(),
            indent,
            comment,
            version: 0,
            edited_at: None,
            blame: None,
            blame_version: None,
            blame_rx: None,
            hl: Default::default(),
            markers: Default::default(),
            dirty_cache: Default::default(),
            line_col_cache: std::cell::Cell::new((u64::MAX, 0, (0, 0))),
        }));
        self.next_doc_id += 1;
        self.prime_editorconfig(path.to_owned());
    }

    fn show_tab(&mut self, tab: Tab) {
        // Reuse the existing diff / graph tab rather than piling up new ones.
        let same_kind = |t: &Tab| matches!((t, &tab), (Tab::Diff(_), Tab::Diff(_)) | (Tab::Graph, Tab::Graph));
        if let Some(i) = self.tabs.iter().position(same_kind) {
            self.tabs[i] = tab;
            self.active = i;
        } else {
            self.tabs.push(tab);
            self.active = self.tabs.len() - 1;
        }
    }

    fn open_diff(&mut self, change: &FileChange, staged: bool) {
        let Some((repo, vcs)) = self.repository() else { return };
        let change = change.clone();
        self.diff_rx = Some(background(&self.ctx, move || {
            let text = vcs.diff(&repo, &change, staged);
            let kind = if staged {
                "staged"
            } else if vcs.has_index() {
                "working tree"
            } else {
                "working copy"
            };
            let title = format!("{} ({kind})", file_name(&change.path));
            DiffTab::new(title, repo.join(&change.path), &text)
        }));
    }

    fn open_graph(&mut self) {
        if self.graph_rx.is_none() {
            self.load_graph();
        }
        self.show_tab(Tab::Graph);
    }

    fn close_tab(&mut self, i: usize, force: bool) {
        self.close_tabs(vec![i], force);
    }

    /// Close several tabs at once, asking first if any has unsaved changes.
    fn close_tabs(&mut self, mut idx: Vec<usize>, force: bool) {
        idx.retain(|&i| i < self.tabs.len());
        idx.sort_unstable();
        idx.dedup();
        if idx.is_empty() {
            return;
        }
        if !force && idx.iter().any(|&i| matches!(&self.tabs[i], Tab::File(d) if d.dirty())) {
            self.confirm = Some(Confirm::CloseTabs(idx));
            return;
        }
        if let Some(si) = self.split {
            self.split = (!idx.contains(&si)).then(|| si - idx.iter().filter(|&&i| i < si).count());
        }
        let active_closed = idx.contains(&self.active);
        let removed_before_active = idx.iter().filter(|&&i| i < self.active).count();
        for &i in idx.iter().rev() {
            self.tabs.remove(i);
        }
        self.active = if active_closed {
            // Activate the tab that slid into the first closed position (or the last tab).
            idx[0]
                .saturating_sub(removed_before_active.min(idx[0]))
                .min(self.tabs.len().saturating_sub(1))
        } else {
            self.active - removed_before_active
        };
    }

    fn show_search(&mut self) {
        self.side = Side::Search;
        self.side_open = true;
        self.search.focus = true;
    }

    fn start_quick_open(&mut self) {
        let Some(root) = self.root.clone() else {
            self.message = "Open a folder to use Go to file".into();
            return;
        };
        let vcs = self.vcs;
        let ignored = self.ignored.clone();
        let files_rx = background(&self.ctx, move || {
            let files = project_files(&root, vcs, &ignored);
            let lower = files.iter().map(|f| f.to_lowercase()).collect();
            (files, lower)
        });
        self.picker = None;
        self.quick_open = Some(QuickOpen {
            query: String::new(),
            files: Vec::new(),
            lower: Vec::new(),
            files_rx: Some(files_rx),
            selected: 0,
            matches: Vec::new(),
            matched_query: None,
        });
    }

    // ------------------------------------------------------------------ UI

    // ------------------------------------------------- code intelligence

    fn go_to_definition(&mut self, char_idx: usize, ctx: &egui::Context) {
        let Some(Tab::File(doc)) = self.tabs.get(self.active) else { return };
        let (path, text, ext, from) = (doc.path.clone(), doc.text.clone(), doc.ext(), doc.cursor);
        let revision = (doc.id, doc.version);
        let Some((_, word)) = goto::word_at(&text, char_idx) else { return };
        // Ignored folders get no language server and are never a source of definitions.
        let client = (!self.path_is_ignored(&path)).then(|| self.launch_server(&ext, true)).flatten();
        self.lsp.install_check = client.as_ref().map(|client| (ext, client.clone()));
        // Without a project, the text-search fallback only looks at the current file.
        let project = self.root.clone();
        let ignored = self.ignored.clone();
        let ignored_for_goto = ignored.clone();
        let vcs = self.vcs;
        let root = project.clone().unwrap_or_else(|| self.work_dir());
        let req = goto::Request {
            root,
            files: Box::new(move || project.map(|r| project_files(&r, vcs, &ignored)).unwrap_or_default()),
            path: path.clone(),
            text,
            ignored: if self.root.is_some() { ignored_for_goto } else { Vec::new() },
            char_idx,
            client,
            revision,
        };
        let (tx, rx) = channel();
        ctx.request_repaint();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(goto::run(req));
            ctx.request_repaint();
        });
        self.goto_rx = Some(rx);
        self.nav_back.push((path, from));
        self.message = format!("Finding definition of `{word}`…");
    }

    fn poll_goto(&mut self) {
        if let Some(job) = &self.install
            && let Ok(res) = job.rx.try_recv()
        {
            let command = job.command;
            self.install = None;
            match res {
                Ok(path) => {
                    self.message = format!("Installed {command} at {}", path.display());
                    self.lsp.retry_missing();
                }
                Err(e) => self.message = format!("Could not install {command}: {e}"),
            }
        }
        let Some(rx) = &self.goto_rx else { return };
        let Ok(outcome) = rx.try_recv() else { return };
        self.goto_rx = None;
        match outcome {
            goto::Outcome::Found { target, via } => {
                let rel = self.display_path(&target.path);
                self.message = format!("{rel}:{} (via {via})", target.line + 1);
                self.open_at(&target.path, target.line, target.cols);
            }
            goto::Outcome::NotFound(msg) => {
                self.nav_back.pop();
                self.message = msg;
            }
        }
    }

    fn go_back(&mut self) {
        let Some((path, idx)) = self.nav_back.pop() else { return };
        self.open_file(&path);
        if let Some(Tab::File(d)) = self.tabs.get_mut(self.active)
            && d.path == path
        {
            d.goto = Some((idx, idx));
        }
    }

    fn offer_install(&mut self, ext: &str) {
        if self.install.is_some() || self.install_prompt.is_some() {
            return;
        }
        let candidate = lsp::servers_for(ext)
            .into_iter()
            .find(|s| installer::installable(s.command).is_some() && !self.install_declined.contains(s.command));
        if let Some(spec) = candidate {
            self.install_prompt = Some(spec.command);
        }
    }

    fn start_install(&mut self, command: &'static str, ctx: &egui::Context) {
        let Some(inst) = installer::installable(command) else { return };
        let progress = Arc::new(installer::Progress::default());
        let (tx, rx) = channel();
        let p = progress.clone();
        ctx.request_repaint();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(installer::install(&inst, &p));
            ctx.request_repaint();
        });
        self.install = Some(InstallJob { command, progress, rx });
    }

    // -------------------------------------------------------------- search

    fn path_is_ignored(&self, path: &Path) -> bool {
        !self.ignored.is_empty()
            && self
                .root
                .as_deref()
                .and_then(|root| workspace::relative_path(root, path))
                .is_some_and(|rel| workspace::is_ignored(&rel, &self.ignored))
    }

    fn ignore_folder(&mut self, rel: String, ctx: &egui::Context) {
        if workspace::valid_folder(&rel) && !workspace::is_ignored(&rel, &self.ignored) {
            self.ignored.push(rel);
            self.ignored_changed(ctx);
        }
    }

    fn ignored_changed(&mut self, ctx: &egui::Context) {
        if let Some(root) = &self.root {
            workspace::save_ignored(root, &self.ignored);
        }
        self.reset_lsp();
        if self.goto_rx.take().is_some() {
            self.nav_back.pop();
        }
        self.start_search(ctx);
    }

    fn start_search(&mut self, ctx: &egui::Context) {
        let st = &mut self.search;
        st.cancel.store(true, Ordering::Relaxed);
        st.results.clear();
        st.collapsed.clear();
        st.total = 0;
        st.error = None;
        st.summary.clear();
        st.searched = st.opts.clone();
        st.edited = None;
        if st.opts.query.is_empty() {
            st.rx = None;
            return;
        }
        let Some(root) = self.root.clone() else {
            st.rx = None;
            st.error = Some("Open a folder to search in files".into());
            return;
        };
        st.cancel = Arc::new(AtomicBool::new(false));
        let vcs = self.vcs;
        let ignored = self.ignored.clone();
        let list_root = root.clone();
        // Open files are searched first, from their (possibly unsaved) text; active tab first.
        let order = std::iter::once(self.active).chain((0..self.tabs.len()).filter(|&i| i != self.active));
        let open = order
            .filter_map(|i| match self.tabs.get(i) {
                Some(Tab::File(d)) => {
                    let rel = d.path.strip_prefix(&root).ok()?.to_string_lossy().replace('\\', "/");
                    (!workspace::is_ignored(&rel, &self.ignored)).then(|| (rel, d.text.clone()))
                }
                _ => None,
            })
            .collect();
        st.rx = Some(search::spawn(
            root,
            move || project_files(&list_root, vcs, &ignored),
            open,
            st.opts.clone(),
            st.cancel.clone(),
            ctx.clone(),
        ));
        ctx.request_repaint();
    }

    fn poll_search(&mut self, ctx: &egui::Context) {
        if let Some(t) = self.search.edited {
            if t.elapsed() >= SEARCH_DEBOUNCE {
                self.start_search(ctx);
            } else {
                ctx.request_repaint_after(SEARCH_DEBOUNCE - t.elapsed());
            }
        }
        let st = &mut self.search;
        let Some(rx) = &st.rx else { return };
        while let Ok(msg) = rx.try_recv() {
            match msg {
                search::Msg::File(f) => {
                    st.total += f.hits.len();
                    st.results.push(f);
                }
                search::Msg::Done { files, truncated } => {
                    let more = if truncated { " (stopped early, too many results)" } else { "" };
                    st.summary = match st.total {
                        0 => format!("No results in {files} files"),
                        n => format!("{n} results in {} of {files} files{more}", st.results.len()),
                    };
                    st.rx = None;
                    break;
                }
                search::Msg::Error(e) => {
                    st.error = Some(e);
                    st.rx = None;
                    break;
                }
            }
        }
    }

    fn open_at(&mut self, path: &Path, line: usize, cols: impl Into<symbols::Columns>) {
        self.open_file(path);
        if let Some(Tab::File(doc)) = self.tabs.get_mut(self.active)
            && doc.path == path
        {
            doc.goto = Some(symbols::selection(&doc.text, line, cols.into()));
        }
    }

    fn search_panel(&mut self, ui: &mut egui::Ui) {
        ui.label(RichText::new("SEARCH").small().strong());
        ui.separator();
        let st = &mut self.search;
        let r = ui.add(
            egui::TextEdit::singleline(&mut st.opts.query)
                .hint_text("Search in files")
                .desired_width(f32::INFINITY),
        );
        if std::mem::take(&mut st.focus) {
            r.request_focus();
        }
        let enter = r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
        if self.search_replace.shown {
            ui.add(
                egui::TextEdit::singleline(&mut self.search_replace.text)
                    .hint_text("Replace in files")
                    .desired_width(f32::INFINITY),
            );
        }
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            let caret = if self.search_replace.shown {
                ic::CARET_DOWN
            } else {
                ic::CARET_RIGHT
            };
            toggle(ui, &mut self.search_replace.shown, caret, "Toggle replace");
            toggle(ui, &mut st.opts.match_case, "Aa", "Match case");
            toggle(ui, &mut st.opts.whole_word, "ab", "Match whole word");
            toggle(ui, &mut st.opts.regex, ".*", "Use regular expression");
            let filtered = !st.opts.include.trim().is_empty() || !st.opts.exclude.trim().is_empty();
            let label = if filtered && !st.show_filters { "...*" } else { "..." };
            toggle(ui, &mut st.show_filters, label, "Files to include / exclude");
            if st.rx.is_some() {
                ui.add_space(6.0);
                ui.spinner();
            }
        });
        let mut filter_enter = false;
        if st.show_filters {
            for (text, hint) in [
                (&mut st.opts.include, "files to include, e.g. *.c, *.h, src/**"),
                (&mut st.opts.exclude, "files to exclude, e.g. *.min.js, vendor"),
            ] {
                let r = ui.add(egui::TextEdit::singleline(text).hint_text(hint).desired_width(f32::INFINITY));
                filter_enter |= r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
            }
        }
        if filter_enter {
            self.start_search(ui.ctx());
        }
        let st = &mut self.search;
        if st.opts != st.searched && st.edited.is_none() {
            st.edited = Some(Instant::now());
        }
        if enter {
            self.start_search(ui.ctx());
            self.search.focus = true;
        }
        if !self.ignored.is_empty() {
            let mut remove = None;
            egui::CollapsingHeader::new(format!("Ignored ({})", self.ignored.len()))
                .id_salt("ignored-folders")
                .show(ui, |ui| {
                    for (index, rel) in self.ignored.iter().enumerate() {
                        ui.horizontal(|ui| {
                            if icon_button(ui, ic::X).on_hover_text("Remove from ignored folders").clicked() {
                                remove = Some(index);
                            }
                            ui.add(egui::Label::new(rel).truncate()).on_hover_text(rel);
                        });
                    }
                });
            if let Some(index) = remove {
                self.ignored.remove(index);
                self.ignored_changed(ui.ctx());
            }
        }
        let show_replace = self.search_replace.shown;
        let can_replace = self.can_replace_in_files();
        let replace_all = show_replace && ui.add_enabled(can_replace, egui::Button::new("Replace all")).clicked();
        self.search_replace.status(ui);
        let st = &mut self.search;
        if let Some(e) = &st.error {
            ui.colored_label(ui.visuals().error_fg_color, e);
        } else if !st.summary.is_empty() {
            ui.weak(&st.summary);
        } else if st.rx.is_some() {
            ui.weak(format!("{} results so far…", st.total));
        }
        if !st.results.is_empty() {
            ui.horizontal(|ui| {
                if ui.small_button("Collapse all").clicked() {
                    st.collapsed = (0..st.results.len()).collect();
                }
                if ui.small_button("Expand all").clicked() {
                    st.collapsed.clear();
                }
            });
        }
        ui.add_space(2.0);

        let mut rows = Vec::with_capacity(st.total + st.results.len());
        for (fi, f) in st.results.iter().enumerate() {
            rows.push(SearchRow::File(fi));
            if !st.collapsed.contains(&fi) {
                rows.extend((0..f.hits.len()).map(|hi| SearchRow::Hit(fi, hi)));
            }
        }
        let mut toggle_file = None;
        let mut replace_file = None;
        let mut open = None;
        let mut ignore = None;
        let row_h = 20.0;
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show_rows(ui, row_h, rows.len(), |ui, range| {
                for row in &rows[range] {
                    let (mut rect, row_response) = ui.allocate_exact_size(egui::vec2(ui.available_width(), row_h), egui::Sense::hover());
                    if show_replace && let SearchRow::File(fi) = *row {
                        let mut button_rect = rect;
                        button_rect.set_left(rect.right() - 58.0);
                        rect.set_right(button_rect.left() - 2.0);
                        let mut buttons = ui.new_child(egui::UiBuilder::new().id_salt(row_response.id).max_rect(button_rect));
                        if buttons
                            .add_enabled(can_replace, egui::Button::new("Replace").small())
                            .on_hover_text("Replace all matches in this file")
                            .clicked()
                        {
                            replace_file = Some(st.results[fi].rel.clone());
                        }
                    }
                    let resp = ui.interact(rect, row_response.id.with("content"), egui::Sense::click());
                    let painter = ui.painter_at(rect);
                    let vis = ui.visuals();
                    if resp.hovered() {
                        painter.rect_filled(rect, 2.0, vis.widgets.hovered.weak_bg_fill);
                    }
                    let y = rect.center().y;
                    let font = egui::FontId::proportional(13.0);
                    match *row {
                        SearchRow::File(fi) => {
                            let f = &st.results[fi];
                            let caret = if st.collapsed.contains(&fi) {
                                ic::CARET_RIGHT
                            } else {
                                ic::CARET_DOWN
                            };
                            let dir = Path::new(&f.rel).parent().map(|p| p.display().to_string()).unwrap_or_default();
                            let mut job = egui::text::LayoutJob::default();
                            let fmt = |c: Color32| egui::TextFormat {
                                font_id: font.clone(),
                                color: c,
                                ..Default::default()
                            };
                            job.append(&format!("{caret} {} ", ic::FILE), 0.0, fmt(vis.weak_text_color()));
                            job.append(&file_name(&f.rel), 0.0, fmt(vis.strong_text_color()));
                            job.append(&format!("  {dir}"), 0.0, fmt(vis.weak_text_color()));
                            let g = painter.layout_job(job);
                            painter.galley(egui::pos2(rect.left() + 2.0, y - g.size().y / 2.0), g, vis.text_color());
                            let count = f.hits.len().to_string();
                            let cg = painter.layout_no_wrap(count, egui::FontId::proportional(11.0), vis.text_color());
                            let pill = egui::Rect::from_center_size(
                                egui::pos2(rect.right() - 6.0 - cg.size().x / 2.0 - 4.0, y),
                                cg.size() + egui::vec2(10.0, 2.0),
                            );
                            painter.rect_filled(pill, 8.0, vis.widgets.inactive.bg_fill);
                            painter.galley(pill.center() - cg.size() / 2.0, cg, vis.text_color());
                            if resp.clicked() {
                                toggle_file = Some(fi);
                            }
                            if f.rel.contains('/') {
                                resp.context_menu(|ui| {
                                    let mut path = f.rel.as_str();
                                    while let Some((parent, _)) = path.rsplit_once('/') {
                                        if workspace::valid_folder(parent) && ui.button(format!("Ignore folder: {parent}")).clicked() {
                                            ignore = Some(parent.to_owned());
                                            ui.close();
                                        }
                                        path = parent;
                                    }
                                });
                            }
                            resp.on_hover_text(&f.rel);
                        }
                        SearchRow::Hit(fi, hi) => {
                            let h = &st.results[fi].hits[hi];
                            let mono = egui::FontId::monospace(12.0);
                            let mut job = egui::text::LayoutJob::default();
                            let fmt = |c: Color32, bg: Color32| egui::TextFormat {
                                font_id: mono.clone(),
                                color: c,
                                background: bg,
                                ..Default::default()
                            };
                            let (text, weak) = (vis.text_color(), vis.weak_text_color());
                            let hl_bg = Color32::from_rgba_unmultiplied(0xe2, 0xb3, 0x40, if vis.dark_mode { 90 } else { 140 });
                            job.append(&format!("{:>5}  ", h.line + 1), 0.0, fmt(weak, Color32::TRANSPARENT));
                            job.append(&h.snippet[..h.highlight.start], 0.0, fmt(text, Color32::TRANSPARENT));
                            job.append(&h.snippet[h.highlight.clone()], 0.0, fmt(vis.strong_text_color(), hl_bg));
                            job.append(&h.snippet[h.highlight.end..], 0.0, fmt(text, Color32::TRANSPARENT));
                            let g = painter.layout_job(job);
                            painter.galley(egui::pos2(rect.left() + 14.0, y - g.size().y / 2.0), g, text);
                            if resp.clicked() {
                                open = Some((fi, hi));
                            }
                        }
                    }
                }
            });
        if let Some(fi) = toggle_file
            && !self.search.collapsed.remove(&fi)
        {
            self.search.collapsed.insert(fi);
        }
        if let (Some((fi, hi)), Some(root)) = (open, &self.root) {
            let f = &self.search.results[fi];
            let (path, line, cols) = (root.join(&f.rel), f.hits[hi].line, f.hits[hi].cols.clone());
            self.open_at(&path, line, cols);
        }
        if let Some(rel) = ignore {
            self.ignore_folder(rel, ui.ctx());
        }
        if replace_all {
            self.start_project_replace(None);
        } else if let Some(rel) = replace_file {
            self.start_project_replace(Some(rel));
        }
    }

    fn activity_bar(&mut self, ui: &mut egui::Ui) {
        ui.add_space(6.0);
        ui.vertical_centered(|ui| {
            ui.spacing_mut().item_spacing.y = 6.0;
            let n = self.status.staged.len() + self.status.unstaged.len();
            let title = if self.vcs == Some(Vcs::Svn) {
                "Subversion"
            } else {
                "Source control"
            };
            let changes_tip = if n > 0 { format!("{title} ({n} changes)") } else { title.into() };
            for (side, icon, tip) in [
                (Side::Explorer, ic::FILES, "Explorer".to_string()),
                (Side::Search, ic::MAGNIFYING_GLASS, "Search in files (Ctrl/Cmd+Shift+F)".to_string()),
                (Side::Changes, ic::GIT_BRANCH, changes_tip),
                (Side::Problems, ic::WARNING, "Problems".to_string()),
            ] {
                let on = self.side_open && self.side == side;
                if side == Side::Search && !on {
                    self.search.focus = true;
                }
                let r = activity_button(ui, icon, on).on_hover_text(tip);
                let count = match side {
                    Side::Changes => n,
                    Side::Problems => self.problems.badge_count(),
                    _ => 0,
                };
                if count > 0 {
                    badge(ui, r.rect, count);
                }
                if r.clicked() {
                    // Clicking the active view collapses the sidebar.
                    self.side_open = !on;
                    self.side = side;
                }
            }
            let quick_open = commands::entry(Command::QuickOpen);
            if activity_button(ui, ic::FILE_SEARCH, false)
                .on_hover_text(format!("{} ({})", quick_open.title, quick_open.shortcut_text(ui.ctx())))
                .clicked()
            {
                self.run_command(Command::QuickOpen);
            }
            ui.with_layout(egui::Layout::bottom_up(egui::Align::Center), |ui| {
                ui.add_space(4.0);
                let dark = ui.visuals().dark_mode;
                if activity_button(ui, if dark { ic::SUN } else { ic::MOON }, false)
                    .on_hover_text("Toggle theme")
                    .clicked()
                {
                    self.run_command(Command::ToggleTheme);
                }
                if activity_button(ui, ic::FOLDER_OPEN, false).on_hover_text("Open folder").clicked() {
                    self.run_command(Command::OpenFolder);
                }
                self.settings_menu(ui);
            });
        });
    }

    fn settings_menu(&mut self, ui: &mut egui::Ui) {
        let gear = activity_button(ui, ic::GEAR, false).on_hover_text("Settings");
        egui::Popup::menu(&gear)
            .align(egui::RectAlign::RIGHT_END)
            .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
            .accessible_name("Settings")
            .show(|ui| {
                for entry in commands::COMMANDS {
                    let Command::ToggleSetting(setting) = entry.id else { continue };
                    let mut enabled = *setting.value(&mut self.settings);
                    if ui.checkbox(&mut enabled, setting.label()).changed() {
                        self.run_command(entry.id);
                    }
                }
            });
    }

    fn save_settings(&mut self, ctx: &egui::Context) {
        if let Err(err) = self.settings_writer.save(self.settings, ctx) {
            self.message = format!("Could not save settings: {err}");
        }
    }

    fn explorer(&mut self, ui: &mut egui::Ui) {
        self.show_outline(ui);
        let Some(root) = self.root.clone() else {
            egui::ScrollArea::vertical().show(ui, |ui| self.welcome(ui));
            return;
        };
        ui.horizontal(|ui| {
            ui.label(RichText::new(dir_name(&root).to_uppercase()).small().strong());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if icon_button(ui, ic::ARROWS_CLOCKWISE).on_hover_text("Refresh").clicked() {
                    self.invalidate_tree(|_| true);
                    self.refresh_vcs();
                }
            });
        });
        ui.separator();
        let decorations: HashMap<PathBuf, char> = self
            .repo
            .as_ref()
            .map(|r| {
                self.status
                    .unstaged
                    .iter()
                    .chain(&self.status.staged)
                    .map(|c| (r.join(&c.path), c.code))
                    .collect()
            })
            .unwrap_or_default();
        let mut open = None;
        egui::ScrollArea::both().auto_shrink([false, false]).show(ui, |ui| {
            self.tree_dir(ui, &root, 0, &decorations, &mut open);
        });
        self.read_tree_entries();
        if let Some(p) = open {
            self.open_file(&p);
        }
    }

    /// Explorer content when no folder is open.
    fn welcome(&mut self, ui: &mut egui::Ui) {
        ui.label(RichText::new("NO FOLDER OPENED").small().strong());
        ui.separator();
        ui.add_space(6.0);
        ui.weak("Open a folder to browse its files, search it and use source control.");
        ui.add_space(8.0);
        if ui
            .add(egui::Button::new(format!("{} Open Folder…", ic::FOLDER_OPEN)).min_size(egui::vec2(ui.available_width(), 28.0)))
            .clicked()
        {
            self.request_open_folder();
        }
        if self.recent.is_empty() {
            return;
        }
        ui.add_space(14.0);
        ui.label(RichText::new("RECENT").small().strong());
        let mut open = None;
        let mut forget = None;
        for (i, dir) in self.recent.iter().enumerate() {
            ui.horizontal(|ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if icon_button(ui, ic::X).on_hover_text("Remove from list").clicked() {
                        forget = Some(i);
                    }
                    ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                        let r = ui
                            .add(egui::Button::selectable(false, format!("{} {}", ic::FOLDER, dir_name(dir))).truncate())
                            .on_hover_text(dir.display().to_string());
                        if r.clicked() {
                            open = Some(dir.clone());
                        }
                    });
                });
            });
        }
        if let Some(i) = forget {
            self.recent.remove(i);
            save_recent(&self.recent);
        }
        if let Some(dir) = open {
            if dir.is_dir() {
                self.set_root(dir);
            } else {
                self.message = format!("{} no longer exists", dir.display());
                self.recent.retain(|r| r != &dir);
                save_recent(&self.recent);
            }
        }
    }

    fn tree_dir(&mut self, ui: &mut egui::Ui, dir: &Path, depth: usize, deco: &HashMap<PathBuf, char>, open: &mut Option<PathBuf>) {
        let Some(entries) = self.tree.get(dir).cloned() else {
            if !self.tree_loading.contains(dir) {
                self.tree_requested.insert(dir.to_path_buf());
            }
            return;
        };
        let active_path = match self.tabs.get(self.active) {
            Some(Tab::File(d)) => Some(d.path.clone()),
            _ => None,
        };
        for (path, is_dir) in entries {
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            ui.horizontal(|ui| {
                ui.add_space(depth as f32 * 12.0);
                if is_dir {
                    let expanded = self.expanded.contains(&path);
                    let (caret, folder) = if expanded {
                        (ic::CARET_DOWN, ic::FOLDER_OPEN)
                    } else {
                        (ic::CARET_RIGHT, ic::FOLDER)
                    };
                    let row = ui.selectable_label(false, format!("{caret} {folder} {name}"));
                    if row.clicked() {
                        if expanded {
                            self.expanded.remove(&path);
                        } else {
                            self.expanded.insert(path.clone());
                        }
                    }
                    row.context_menu(|ui| {
                        if let Some(rel) = self.root.as_deref().and_then(|root| workspace::relative_path(root, &path)) {
                            let enabled = workspace::valid_folder(&rel) && !workspace::is_ignored(&rel, &self.ignored);
                            if ui.add_enabled(enabled, egui::Button::new("Ignore folder in search")).clicked() {
                                self.ignore_folder(rel, ui.ctx());
                                ui.close();
                            }
                        }
                    });
                } else {
                    let mut text = RichText::new(format!("     {} {name}", ic::FILE));
                    if let Some(&code) = deco.get(&path) {
                        text = text.color(status_color(code));
                    }
                    if ui.selectable_label(active_path.as_deref() == Some(&path), text).clicked() {
                        *open = Some(path.clone());
                    }
                }
            });
            if is_dir && self.expanded.contains(&path) {
                self.tree_dir(ui, &path, depth + 1, deco, open);
            }
        }
    }

    fn invalidate_tree(&mut self, invalid: impl Fn(&Path) -> bool) {
        self.tree_requested.extend(self.tree.keys().filter(|dir| invalid(dir)).cloned());
        self.tree_loading.retain(|dir| {
            if invalid(dir) {
                self.tree_requested.insert(dir.clone());
                false
            } else {
                true
            }
        });
    }

    fn read_tree_entries(&mut self) {
        if self.tree_rx.is_some() || self.tree_requested.is_empty() {
            return;
        }
        let dirs = std::mem::take(&mut self.tree_requested);
        self.tree_loading.clone_from(&dirs);
        self.tree_rx = Some(background(&self.ctx, move || {
            dirs.into_iter()
                .map(|dir| {
                    let entries = read_dir_sorted(&dir);
                    (dir, entries)
                })
                .collect()
        }));
    }

    fn changes(&mut self, ui: &mut egui::Ui) {
        let heading = match self.vcs {
            Some(Vcs::Svn) => "SOURCE CONTROL · SUBVERSION",
            _ => "SOURCE CONTROL",
        };
        ui.label(RichText::new(heading).small().strong());
        ui.separator();
        let Some(root) = self.root.clone() else {
            ui.weak("Open a folder to see its changes.");
            ui.add_space(6.0);
            if ui.button(format!("{} Open Folder…", ic::FOLDER_OPEN)).clicked() {
                self.request_open_folder();
            }
            return;
        };
        if self.repo_rx.is_some() || (self.status_rx.is_some() && self.repo.is_none()) {
            ui.weak("Detecting source control…");
            return;
        }
        let Some((repo, vcs)) = self.repository() else {
            ui.label("No Git or Subversion repository found.");
            if ui.button("Initialize Git repository").clicked() {
                self.repo_rx = Some(background(&self.ctx, move || {
                    vcs::init_git(&root)?;
                    Ok(vcs::detect(&root))
                }));
            }
            return;
        };
        let busy = self.bg_task.is_some();
        let has_index = vcs.has_index();

        ui.horizontal(|ui| {
            if has_index {
                let current = self.status.branch.clone();
                ui.add_enabled_ui(!busy, |ui| {
                    egui::ComboBox::from_id_salt("branch")
                        .selected_text(format!("{} {current}", ic::GIT_BRANCH))
                        .width(140.0)
                        .show_ui(ui, |ui| {
                            for b in self.branches.clone() {
                                if ui.selectable_label(b == current, &b).clicked() && b != current {
                                    self.spawn_vcs(VcsOperation::History("Switch branch"), move |vcs, repo| vcs.switch(repo, &b));
                                }
                            }
                        });
                });
                ui.weak(&self.status.ahead_behind);
            } else {
                ui.label(format!("{} {}", ic::GIT_BRANCH, self.status.branch));
            }
        });
        ui.horizontal(|ui| {
            if has_index {
                ui.add_enabled_ui(!busy, |ui| {
                    for (icon, tip, label, args) in [
                        (ic::CLOUD_ARROW_DOWN, "Fetch all", "Fetch", &["fetch", "--all", "--prune"][..]),
                        (ic::ARROW_DOWN, "Pull (fast-forward only)", "Pull", &["pull", "--ff-only"][..]),
                        (ic::ARROW_UP, "Push", "Push", &["push"][..]),
                    ] {
                        if icon_button(ui, icon).on_hover_text(tip).clicked() {
                            self.spawn_vcs(VcsOperation::History(label), move |vcs, repo| vcs.remote(repo, args));
                        }
                    }
                });
            }
            if busy {
                ui.spinner();
            }
            if icon_button(ui, ic::ARROWS_CLOCKWISE)
                .on_hover_text(format!("Refresh {}", vcs.name()))
                .clicked()
            {
                self.refresh_vcs();
                self.refresh_history();
            }
            let (icon, tip) = if self.settings.changes_tree {
                (ic::LIST, "View as list")
            } else {
                (ic::TREE_VIEW, "View as tree (grouped by folder)")
            };
            if icon_button(ui, icon).on_hover_text(tip).clicked() {
                self.run_command(Command::ToggleSetting(Setting::ChangesTree));
            }
            if icon_button(ui, ic::TREE_STRUCTURE).on_hover_text(vcs.history_title()).clicked() {
                self.run_command(Command::History);
            }
        });
        ui.add_space(4.0);
        ui.add(
            egui::TextEdit::multiline(&mut self.commit_msg)
                .hint_text("Commit message (Ctrl/Cmd+Enter)")
                .desired_rows(3)
                .desired_width(f32::INFINITY),
        );
        let commit_key = ui.input_mut(|i| commands::entry(Command::Commit).consume(i));
        ui.horizontal(|ui| {
            let can = self.can_commit();
            let commit = ui.add_enabled(can, egui::Button::new(format!("{} Commit", ic::CHECK)));
            if has_index {
                ui.checkbox(&mut self.amend, "Amend");
            }
            if can && (commit.clicked() || commit_key) {
                self.run_command(Command::Commit);
            }
        });
        ui.separator();

        let staged = self.status.staged.clone();
        let unstaged = self.status.unstaged.clone();
        let mut actions = Vec::new();
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            if has_index {
                ui.horizontal(|ui| {
                    ui.strong(format!("Staged ({})", staged.len()));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if !busy && !staged.is_empty() && icon_button(ui, ic::MINUS).on_hover_text("Unstage all").clicked() {
                            self.stage_change(None, false);
                        }
                    });
                });
                if let Some(a) = change_section(ui, &staged, true, !busy, self.settings.changes_tree, &self.changes_collapsed) {
                    actions.push(a);
                }
                ui.add_space(8.0);
            }
            ui.horizontal(|ui| {
                ui.strong(format!("Changes ({})", unstaged.len()));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if has_index && !busy && !unstaged.is_empty() && icon_button(ui, ic::PLUS).on_hover_text("Stage all").clicked() {
                        self.stage_change(None, true);
                    }
                });
            });
            if let Some(a) = change_section(
                ui,
                &unstaged,
                false,
                has_index && !busy,
                self.settings.changes_tree,
                &self.changes_collapsed,
            ) {
                actions.push(a);
            }
        });
        for action in actions {
            match action {
                ChangeAction::OpenDiff(c, staged) => self.open_diff(&c, staged),
                ChangeAction::OpenFile(path) => self.open_file(&repo.join(path)),
                ChangeAction::Stage(path) => self.stage_change(Some(path), true),
                ChangeAction::Unstage(path) => self.stage_change(Some(path), false),
                ChangeAction::Discard(c) if !busy => self.confirm = Some(Confirm::Discard(c)),
                ChangeAction::Discard(_) => {}
                ChangeAction::ToggleFolder(key) => {
                    if !self.changes_collapsed.remove(&key) {
                        self.changes_collapsed.insert(key);
                    }
                }
            }
        }
    }

    fn tab_strip(&mut self, ui: &mut egui::Ui) {
        let mut close = None;
        let mut menu: Option<TabMenu> = None;
        #[cfg(feature = "tour")]
        let mut tab_rects = Vec::new();
        egui::ScrollArea::horizontal().id_salt("tabs").show(ui, |ui| {
            ui.horizontal(|ui| {
                for (i, tab) in self.tabs.iter().enumerate() {
                    let title = match tab {
                        Tab::File(d) => format!("{}{}", d.name(), if d.dirty() { " ●" } else { "" }),
                        Tab::Diff(d) => format!("{} {}", ic::GIT_DIFF, d.title),
                        Tab::Graph => format!("{} {}", ic::TREE_STRUCTURE, self.vcs.unwrap_or(Vcs::Git).history_title()),
                    };
                    let r = ui.selectable_label(i == self.active, title);
                    #[cfg(feature = "tour")]
                    tab_rects.push(r.rect);
                    if r.clicked() {
                        self.active = i;
                    }
                    if r.middle_clicked() {
                        close = Some(i);
                    }
                    let file_path = match tab {
                        Tab::File(d) => Some(d.path.clone()),
                        Tab::Diff(d) => Some(d.path.clone()),
                        Tab::Graph => None,
                    };
                    let r = match &file_path {
                        Some(p) => r.on_hover_text(p.display().to_string()),
                        None => r,
                    };
                    let n = self.tabs.len();
                    r.context_menu(|ui| {
                        let mut item = |ui: &mut egui::Ui, label: &str, enabled: bool, action: TabMenu| {
                            if ui.add_enabled(enabled, egui::Button::new(label)).clicked() {
                                menu = Some(action);
                                ui.close();
                            }
                        };
                        item(ui, "Close", true, TabMenu::Close(i));
                        item(ui, "Close Others", n > 1, TabMenu::CloseOthers(i));
                        item(ui, "Close to the Right", i + 1 < n, TabMenu::CloseRight(i));
                        item(ui, "Close All", true, TabMenu::CloseAll);
                        if let Some(p) = &file_path {
                            ui.separator();
                            if matches!(tab, Tab::File(_)) {
                                item(ui, "Open to the Side", true, TabMenu::Split(i));
                                item(ui, "Format document (Shift+Alt+F)", true, TabMenu::Format(i));
                            }
                            item(ui, "Copy Path", true, TabMenu::Copy(p.display().to_string()));
                            let rel = self.display_path(p);
                            item(ui, "Copy Relative Path", true, TabMenu::Copy(rel));
                        }
                    });
                    if icon_button(ui, ic::X).on_hover_text("Close (Ctrl/Cmd+W)").clicked() {
                        close = Some(i);
                    }
                    ui.add_space(6.0);
                }
            });
        });
        #[cfg(feature = "tour")]
        if let Some(t) = &mut self.tour {
            t.tab_rects = tab_rects;
        }
        if let Some(i) = close {
            self.close_tab(i, false);
        }
        match menu {
            Some(TabMenu::Close(i)) => self.close_tabs(vec![i], false),
            Some(TabMenu::CloseOthers(i)) => self.close_tabs((0..self.tabs.len()).filter(|&j| j != i).collect(), false),
            Some(TabMenu::CloseRight(i)) => self.close_tabs((i + 1..self.tabs.len()).collect(), false),
            Some(TabMenu::CloseAll) => self.run_command(Command::CloseAllTabs),
            Some(TabMenu::Copy(text)) => ui.ctx().copy_text(text),
            Some(TabMenu::Format(i)) => self.format_document(i),
            Some(TabMenu::Split(i)) => self.split = Some(i),
            None => {}
        }
    }

    fn central(&mut self, ui: &mut egui::Ui) {
        if self.tabs.is_empty() {
            if self.root.is_none() {
                ui.vertical_centered(|ui| {
                    ui.add_space(ui.available_height() / 3.0);
                    ui.heading("edittr");
                    ui.add_space(8.0);
                    if ui.button(format!("{} Open Folder…", ic::FOLDER_OPEN)).clicked() {
                        self.request_open_folder();
                    }
                });
            } else {
                ui.centered_and_justified(|ui| {
                    ui.weak("Open a file from the Explorer, or press Ctrl/Cmd+P");
                });
            }
            return;
        }
        self.tab_strip(ui);
        ui.separator();
        let active = self.active;
        let mut open_path = None;
        let mut goto_at = None;
        // Diagnostics of the active file, for squiggles (collected before borrowing the tab).
        let diags = self.squiggles_for(active, ui.visuals());
        self.split_pane(ui);
        let marks: Vec<std::ops::Range<usize>> = self.find_matches().map(<[_]>::to_vec).unwrap_or_default();
        // .editorconfig's indent wins over the one detected from the file.
        let editorconfig_indent = match self.tabs.get(active) {
            Some(Tab::File(doc)) => self.editorconfig_indent(&doc.path),
            _ => None,
        };
        match &mut self.tabs[active] {
            Tab::File(doc) => {
                if let Some(unit) = editorconfig_indent
                    && doc.indent != unit
                {
                    doc.indent = unit;
                }
                let dialog_open = self.quick_open.is_some()
                    || self.confirm.is_some()
                    || self.install_prompt.is_some()
                    || self.open_folder.is_some()
                    || self.search_replace.confirming();
                self.find.bar(ui, doc, dialog_open);
                let fresh = match self.vcs {
                    Some(Vcs::Svn) => doc.blame_version.is_some() && !doc.dirty(),
                    _ => doc.blame_version == Some(doc.version),
                };
                let inline = if self.settings.inline_blame && fresh {
                    let line = doc.line_col().0;
                    doc.blame.as_ref().and_then(|b| b.line(line)).map(|l| l.describe(blame::now()))
                } else {
                    None
                };
                if let Some(idx) = editor(ui, doc, self.settings, inline, &diags, &marks, 0) {
                    goto_at = Some(idx);
                }
                self.find.after_editor(ui, doc);
            }
            Tab::Diff(d) => {
                ui.horizontal(|ui| {
                    if ui.button(format!("{} Open file", ic::ARROW_SQUARE_OUT)).clicked() {
                        open_path = Some(d.path.clone());
                    }
                    ui.separator();
                    if ui.selectable_label(!d.inline, format!("{} Side by side", ic::COLUMNS)).clicked() {
                        d.inline = false;
                    }
                    if ui.selectable_label(d.inline, format!("{} Inline", ic::ROWS)).clicked() {
                        d.inline = true;
                    }
                    if !d.inline && !d.blocks.is_empty() {
                        ui.separator();
                        let n = d.blocks.len();
                        let current = d.blocks.iter().rposition(|&b| b <= d.top_row + 3).unwrap_or(0);
                        if icon_button(ui, ic::ARROW_UP).on_hover_text("Previous change").clicked() {
                            d.jump = d
                                .blocks
                                .iter()
                                .rev()
                                .find(|&&b| b + 3 < d.top_row + 3)
                                .copied()
                                .or(d.blocks.last().copied());
                        }
                        if icon_button(ui, ic::ARROW_DOWN).on_hover_text("Next change").clicked() {
                            d.jump = d.blocks.iter().find(|&&b| b > d.top_row + 3).copied().or(d.blocks.first().copied());
                        }
                        ui.weak(format!("change {} of {n}", current + 1));
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.colored_label(diff_colors(ui.visuals().dark_mode).1, format!("−{}", d.removed));
                        ui.colored_label(diff_colors(ui.visuals().dark_mode).0, format!("+{}", d.added));
                    });
                });
                if d.inline {
                    diff_view(ui, &d.lines, "diff");
                } else {
                    side_by_side_view(ui, d);
                }
            }
            Tab::Graph => {
                let Some(graph) = &self.graph else {
                    ui.label(if self.graph_rx.is_some() {
                        "Loading history…"
                    } else {
                        "No history to show."
                    });
                    return;
                };
                let mut clicked = None;
                egui::Panel::bottom("commit_detail")
                    .resizable(true)
                    .default_size(260.0)
                    .show(ui, |ui| {
                        if self.graph_detail_rx.is_some() {
                            ui.weak("Loading commit details…");
                        } else if self.graph_detail.is_empty() {
                            ui.weak("Select a commit to see its details.");
                        } else {
                            diff_view(ui, &self.graph_detail, "detail");
                        }
                    });
                egui::CentralPanel::default().show(ui, |ui| {
                    clicked = graph.ui(ui, self.graph_sel);
                });
                if let Some(r) = clicked {
                    self.load_graph_detail(r);
                }
            }
        }
        if let Some(p) = open_path {
            self.open_file(&p);
        }
        if let Some(idx) = goto_at {
            self.go_to_definition(idx, ui.ctx());
        }
    }

    fn background_activity(&self, server: Option<&lsp::Client>) -> Vec<(String, Option<String>)> {
        let mut activity = Vec::new();
        if let Some(job) = &self.install {
            activity.push((job.progress.describe(job.command), None));
        }
        if self.goto_rx.is_some() {
            activity.push(("Finding definition…".into(), None));
        }
        if self.search.rx.is_some() {
            let suffix = if self.search.total == 1 { "result" } else { "results" };
            activity.push((format!("Searching… {} {suffix}", self.search.total), None));
        }

        let mut clients: Vec<_> = self.lsp.clients().map(Arc::as_ref).collect();
        clients.sort_unstable_by_key(|client| (server.map(|s| s.name) != Some(client.name), client.name));
        for client in clients {
            for progress in client.active_progress() {
                let mut label = format!("{}: {}", client.name, progress.title);
                if let Some(percentage) = progress.percentage {
                    label.push_str(&format!(" {percentage}%"));
                }
                activity.push((label, progress.message));
            }
            if client.is_starting() {
                activity.push((format!("{}: Starting…", client.name), None));
            }
        }

        if let Some(job) = &self.bg_task {
            activity.push((format!("{}…", job.operation.label()), None));
        }
        if self.status_rx.is_some()
            && let Some(vcs) = self.vcs
        {
            activity.push((format!("Refreshing {}…", vcs.name()), None));
        }
        if self.diff_rx.is_some() {
            activity.push(("Loading diff…".into(), None));
        }
        activity
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        let file = match self.tabs.get(self.active) {
            Some(Tab::File(doc)) => Some((doc, doc.ext())),
            _ => None,
        };
        let server = file.as_ref().and_then(|(doc, ext)| {
            if self.path_is_ignored(&doc.path) {
                return None;
            }
            let specs = lsp::servers_for(ext);
            self.lsp
                .clients()
                .find(|client| specs.iter().any(|spec| spec.name == client.name))
                .map(Arc::as_ref)
        });
        let activity = self.background_activity(server);
        let mut open_problems = false;
        ui.horizontal(|ui| {
            if self.repo.is_some() {
                let branch = format!("{} {} {}", ic::GIT_BRANCH, self.status.branch, self.status.ahead_behind);
                ui.add(egui::Label::new(branch).truncate());
                ui.separator();
            }
            open_problems = self.problems.status_button(ui).clicked();
            ui.separator();
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if let Some((doc, ext)) = &file {
                    let (backend, hint) = match server {
                        Some(client) => (client.name, "Language server for go to definition (Ctrl/Cmd+click or F12)"),
                        None => (
                            "text search",
                            "Go to definition (Ctrl/Cmd+click or F12) uses text search for this file",
                        ),
                    };
                    let language = format!("{} · {backend}", language_name(ext));
                    ui.add(
                        egui::Label::new(RichText::new(language).weak())
                            .truncate()
                            .show_tooltip_when_elided(false),
                    )
                    .on_hover_text(hint);
                    let (line, col) = doc.line_col();
                    ui.add(egui::Label::new(RichText::new(format!("Ln {}, Col {}", line + 1, col + 1)).weak()).truncate());
                }
                if let Some((first, _)) = activity.first() {
                    ui.ctx().request_repaint_after(ACTIVITY_POLL);
                    if file.is_some() {
                        ui.separator();
                    }
                    let label = if activity.len() == 1 {
                        first.clone()
                    } else {
                        format!("{first} · +{}", activity.len() - 1)
                    };
                    let tooltip = activity
                        .iter()
                        .map(|(label, detail)| match detail {
                            Some(detail) if !detail.is_empty() => format!("{label}\n{detail}"),
                            _ => label.clone(),
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    status_spinner(ui).on_hover_text(&tooltip);
                    ui.add(egui::Label::new(label).truncate().show_tooltip_when_elided(false))
                        .on_hover_text(tooltip);
                }
                if !self.message.is_empty() {
                    ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                        ui.add(egui::Label::new(&self.message).truncate());
                    });
                }
            });
        });
        if open_problems {
            self.open_problems();
        }
    }

    fn dialogs(&mut self, ctx: &egui::Context) {
        // Quick open
        if let Some(q) = &mut self.quick_open {
            let mut chosen = None;
            // Esc is taken here, before the text box can swallow it.
            let mut close = ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape));
            let popup = egui::Window::new("Go to file")
                .title_bar(false)
                .anchor(egui::Align2::CENTER_TOP, [0.0, 60.0])
                .fixed_size([520.0, 360.0])
                .show(ctx, |ui| {
                    let r = ui.add(
                        egui::TextEdit::singleline(&mut q.query)
                            .hint_text("Type to search files…")
                            .desired_width(f32::INFINITY),
                    );
                    r.request_focus();
                    if r.changed() {
                        q.selected = 0;
                    }
                    // Rank only when the query or the file list changed, not every frame.
                    if q.matched_query.as_deref() != Some(q.query.as_str()) {
                        q.matches = fuzzy_filter(&q.lower, &q.query, 200);
                        q.matched_query = Some(q.query.clone());
                    }
                    let mut moved = false;
                    ui.input(|i| {
                        if i.key_pressed(Key::ArrowDown) {
                            q.selected = (q.selected + 1).min(q.matches.len().saturating_sub(1));
                            moved = true;
                        }
                        if i.key_pressed(Key::ArrowUp) {
                            q.selected = q.selected.saturating_sub(1);
                            moved = true;
                        }
                        if i.key_pressed(Key::Enter) {
                            chosen = q.matches.get(q.selected).map(|&i| q.files[i].clone());
                        }
                    });
                    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                        for (i, &m) in q.matches.iter().enumerate() {
                            let r = ui.selectable_label(i == q.selected, &q.files[m]);
                            if i == q.selected && moved {
                                r.scroll_to_me(None);
                            }
                            if r.clicked() {
                                chosen = Some(q.files[m].clone());
                            }
                        }
                    });
                });
            // A click anywhere outside the popup closes it.
            if let Some(popup) = popup {
                let outside =
                    ctx.input(|i| i.pointer.any_pressed() && i.pointer.interact_pos().is_some_and(|p| !popup.response.rect.contains(p)));
                close |= outside;
            }
            if let (Some(f), Some(root)) = (chosen, self.root.clone()) {
                self.open_file(&root.join(f));
                close = true;
            }
            if close {
                self.quick_open = None;
            }
        }

        // Open folder
        if let Some(path) = &mut self.open_folder {
            let mut done = None;
            egui::Modal::new(egui::Id::unique("open_folder")).show(ctx, |ui| {
                ui.set_width(480.0);
                ui.heading("Open folder");
                let r = ui.add(egui::TextEdit::singleline(path).desired_width(f32::INFINITY));
                r.request_focus();
                ui.horizontal(|ui| {
                    if ui.button("Open").clicked() || ui.input(|i| i.key_pressed(Key::Enter)) {
                        done = Some(true);
                    }
                    if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(Key::Escape)) {
                        done = Some(false);
                    }
                });
            });
            match done {
                Some(true) => {
                    let p = expand_home(path.trim());
                    if p.is_dir() {
                        let p = std::fs::canonicalize(&p).unwrap_or(p);
                        self.open_folder = None;
                        self.set_root(p);
                    } else {
                        self.message = format!("Not a directory: {}", p.display());
                    }
                }
                Some(false) => self.open_folder = None,
                None => {}
            }
        }

        // Language server install offer
        if let Some(command) = self.install_prompt {
            let mut answer = None;
            let source = installer::installable(command).map(|i| i.source).unwrap_or_default();
            egui::Modal::new(egui::Id::unique("install_prompt")).show(ctx, |ui| {
                ui.set_max_width(440.0);
                ui.heading(format!("Install {command}?"));
                ui.add_space(4.0);
                ui.label(format!(
                    "Go to definition is much more accurate with the {command} language server. \
                     edittr can install it for you from {source}."
                ));
                ui.weak(format!("It goes into {}", installer::servers_dir().display()));
                ui.weak("Until then, go to definition uses text search.");
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.button(format!("{} Install", ic::CLOUD_ARROW_DOWN)).clicked() {
                        answer = Some(true);
                    }
                    if ui.button("Not now").clicked() || ui.input(|i| i.key_pressed(Key::Escape)) {
                        answer = Some(false);
                    }
                });
            });
            match answer {
                Some(true) => {
                    self.install_prompt = None;
                    self.start_install(command, ctx);
                }
                Some(false) => {
                    self.install_prompt = None;
                    self.install_declined.insert(command);
                }
                None => {}
            }
        }

        // Confirmations
        if let Some(c) = &self.confirm {
            let text = match c {
                Confirm::CloseTabs(idx) => {
                    let dirty: Vec<String> = idx
                        .iter()
                        .filter_map(|&i| match self.tabs.get(i) {
                            Some(Tab::File(d)) if d.dirty() => Some(d.name()),
                            _ => None,
                        })
                        .collect();
                    match dirty.as_slice() {
                        [one] => format!("{one} has unsaved changes. Close it and discard them?"),
                        many => format!(
                            "{} files have unsaved changes ({}). Close them and discard the changes?",
                            many.len(),
                            many.join(", ")
                        ),
                    }
                }
                Confirm::SwitchWorkspace(_) => "Discard unsaved changes and open the folder?".into(),
                Confirm::Discard(f) => format!("Discard all changes to {}?\nThis cannot be undone.", f.path),
            };
            let mut answer = None;
            egui::Modal::new(egui::Id::unique("confirm")).show(ctx, |ui| {
                ui.label(text);
                ui.horizontal(|ui| {
                    if ui.button("Discard").clicked() {
                        answer = Some(true);
                    }
                    if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(Key::Escape)) {
                        answer = Some(false);
                    }
                });
            });
            if let Some(yes) = answer {
                let c = self.confirm.take().unwrap();
                if yes {
                    match c {
                        Confirm::CloseTabs(idx) => self.close_tabs(idx, true),
                        Confirm::SwitchWorkspace(root) => self.restore_workspace(Some(root), None),
                        Confirm::Discard(f) => {
                            if let Some(repo) = &self.repo {
                                let path = repo.join(&f.path);
                                self.spawn_vcs(VcsOperation::Discard(path), move |vcs, repo| vcs.discard(repo, &f));
                            }
                        }
                    }
                }
            }
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        if self.poll_session_restore() {
            egui::CentralPanel::default().show(ui, |ui| {
                ui.centered_and_justified(|ui| ui.weak("Restoring session…"));
            });
            return;
        }
        self.poll_background(&ctx);
        self.shortcuts(&ctx);
        self.picker_dialog(&ctx);

        let title = match &self.root {
            Some(r) => format!("{} — edittr", dir_name(r)),
            None => "edittr".to_string(),
        };
        if title != self.title {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(title.clone()));
            self.title = title;
        }

        egui::Panel::bottom("status").show(ui, |ui| self.status_bar(ui));
        egui::Panel::left("activity")
            .resizable(false)
            .exact_size(44.0)
            .show(ui, |ui| self.activity_bar(ui));
        self.show_session_sidebar(ui);
        egui::CentralPanel::default().show(ui, |ui| self.central(ui));
        self.open_requested_folder(frame);
        self.dialogs(&ctx);
        self.sync_active_document();
        self.refresh_outline();
        self.project_replace_modal(&ctx);
        self.record_session(false);
        #[cfg(feature = "tour")]
        if let Some(mut t) = self.tour.take() {
            t.tick(self, &ctx);
            self.tour = Some(t);
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.record_session(true);
    }

    #[cfg(feature = "tour")]
    fn raw_input_hook(&mut self, _ctx: &egui::Context, raw: &mut egui::RawInput) {
        if let Some(t) = &mut self.tour {
            t.raw_input_hook(raw);
        }
    }
}

fn language_name(ext: &str) -> &'static str {
    match ext {
        "rs" => "Rust",
        "c" | "h" => "C",
        "cc" | "cpp" | "cxx" | "hh" | "hpp" | "hxx" => "C++",
        "ts" | "mts" | "cts" => "TypeScript",
        "tsx" => "TypeScript React",
        "js" | "mjs" | "cjs" => "JavaScript",
        "jsx" => "JavaScript React",
        "py" | "pyi" => "Python",
        "go" => "Go",
        "md" | "markdown" => "Markdown",
        "json" | "jsonc" => "JSON",
        "toml" => "TOML",
        "yaml" | "yml" => "YAML",
        "sh" | "bash" | "zsh" | "fish" => "Shell",
        "htm" | "html" => "HTML",
        "css" => "CSS",
        "scss" => "SCSS",
        "xml" | "svg" => "XML",
        "java" => "Java",
        "rb" => "Ruby",
        "php" => "PHP",
        "lua" => "Lua",
        "zig" => "Zig",
        "cs" => "C#",
        "swift" => "Swift",
        "kt" | "kts" => "Kotlin",
        "sql" => "SQL",
        _ => "Plain Text",
    }
}

// Advance on the activity timer so the spinner doesn't request full-speed animation.
fn status_spinner(ui: &mut egui::Ui) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(egui::Vec2::splat(14.0), egui::Sense::hover());
    response.widget_info(|| egui::WidgetInfo::new(egui::Role::ProgressIndicator));
    if ui.is_rect_visible(rect) {
        let step = std::f32::consts::TAU / 12.0;
        let phase = ui.input(|i| (i.time / ACTIVITY_POLL.as_secs_f64()).floor() % 12.0) as f32 * step;
        let points = (0..9)
            .map(|i| rect.center() + egui::Vec2::angled(phase + i as f32 * step) * 5.0)
            .collect();
        ui.painter().line(points, egui::Stroke::new(1.5, ui.visuals().strong_text_color()));
    }
    response
}

#[cfg(target_os = "linux")]
mod folder_picker {
    use std::cell::Cell;
    use std::sync::Once;

    struct PickerLog;

    thread_local! {
        static FAILED: Cell<bool> = const { Cell::new(false) };
    }

    impl log::Log for PickerLog {
        fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
            // rfd 0.18.0 reports its final picker error here. Portal errors
            // are logged by submodules and can be followed by a Zenity dialog.
            metadata.level() == log::Level::Error && metadata.target() == "rfd::backend::xdg_desktop_portal"
        }

        fn log(&self, record: &log::Record<'_>) {
            if self.enabled(record.metadata()) {
                FAILED.set(true);
            }
        }

        fn flush(&self) {}
    }

    pub(super) fn pick_folder(picker: rfd::FileDialog) -> (Option<std::path::PathBuf>, bool) {
        static INIT: Once = Once::new();
        INIT.call_once(|| {
            if log::set_logger(&PickerLog).is_ok() {
                log::set_max_level(log::LevelFilter::Error);
            }
        });
        FAILED.set(false);
        let path = picker.pick_folder();
        (path, FAILED.get())
    }
}

// -------------------------------------------------------------- widgets

/// Draws the editor. Returns a char index when the user Ctrl/Cmd+clicked a symbol.
/// Draws the editor. Returns a char index when the user Ctrl/Cmd+clicked a symbol.
fn editor(
    ui: &mut egui::Ui,
    doc: &mut Doc,
    settings: Settings,
    inline: Option<String>,
    diags: &[(usize, usize, usize, usize, Color32)],
    find_marks: &[std::ops::Range<usize>],
    pane: u8,
) -> Option<usize> {
    let id = ui.make_persistent_id(("editor", doc.id, pane));
    // A pending selection from elsewhere (search hit, go to definition): char range.
    // The main pane (0) takes it, so a file open in both panes jumps once.
    if pane == 0
        && let Some((a, b)) = doc.goto.take()
    {
        let ix = doc.ed.index(&doc.text, doc.version);
        let (a, b) = (ix.char_to_byte(&doc.text, a), ix.char_to_byte(&doc.text, b));
        doc.ed.select(a..b);
        ui.memory_mut(|m| m.request_focus(id));
    }
    let opts = editor::Options {
        lang: &doc.lang,
        highlight: doc.text.len() <= HIGHLIGHT_LIMIT,
        whitespace: settings.show_whitespace,
        current_line: settings.highlight_line,
        indent: &doc.indent,
        comment: doc.comment,
        minimap: settings.minimap,
        bracket_colors: settings.bracket_colors,
    };
    // Diagnostics: (line, char col) pairs to byte ranges.
    let squiggles: Vec<(std::ops::Range<usize>, Color32)> = {
        let ix = doc.ed.index(&doc.text, doc.version);
        let text = &doc.text;
        let at = |line: usize, col: usize| {
            if line >= ix.lines() {
                return text.len();
            }
            let r = ix.line_range(text, line);
            r.start + text[r.clone()].char_indices().nth(col).map_or(r.len(), |(i, _)| i)
        };
        diags
            .iter()
            .map(|&(l0, c0, l1, c1, color)| (at(l0, c0)..at(l1, c1), color))
            .collect()
    };
    // Find matches (char ranges) to bytes.
    let marks: Vec<std::ops::Range<usize>> = {
        let ix = doc.ed.index(&doc.text, doc.version);
        find_marks
            .iter()
            .take(10_000)
            .map(|r| ix.char_to_byte(&doc.text, r.start)..ix.char_to_byte(&doc.text, r.end))
            .collect()
    };
    let dark = ui.visuals().dark_mode;
    let gutter_marks: Vec<(usize, Color32, bool)> = doc
        .markers
        .lines
        .iter()
        .map(|&(l, c)| (l, gutter::color(c, dark), c == gutter::Change::Deleted))
        .collect();
    let extras = editor::Extras {
        inline: inline.as_deref(),
        squiggles: &squiggles,
        marks: &marks,
        gutter: &gutter_marks,
    };
    let before = doc.version;
    let out = editor::show(ui, id, &mut doc.ed, &mut doc.text, &mut doc.version, &mut doc.hl, &opts, &extras);
    if doc.version != before {
        doc.edited_at = Some(Instant::now());
    }
    // Mirror the primary selection as char indices for the rest of the app.
    let p = doc.ed.primary();
    let ix = doc.ed.index(&doc.text, doc.version);
    doc.cursor = ix.byte_to_char(&doc.text, p.head);
    doc.anchor = ix.byte_to_char(&doc.text, p.anchor);
    out.goto.map(|b| ix.byte_to_char(&doc.text, b))
}

fn diff_view(ui: &mut egui::Ui, lines: &[String], id: &str) {
    let dark = ui.visuals().dark_mode;
    let (add, del, hunk) = if dark {
        (
            Color32::from_rgb(0x8b, 0xd5, 0x8b),
            Color32::from_rgb(0xf0, 0x80, 0x80),
            Color32::from_rgb(0x7f, 0xb4, 0xf0),
        )
    } else {
        (
            Color32::from_rgb(0x1a, 0x7f, 0x37),
            Color32::from_rgb(0xcf, 0x22, 0x2e),
            Color32::from_rgb(0x09, 0x69, 0xda),
        )
    };
    let row_h = ui.text_style_height(&egui::TextStyle::Monospace);
    egui::ScrollArea::both()
        .id_salt(id)
        .auto_shrink([false, false])
        .show_rows(ui, row_h, lines.len(), |ui, range| {
            for line in &lines[range] {
                let mut t = RichText::new(line).monospace();
                if line.starts_with("+++") || line.starts_with("---") || line.starts_with("diff ") || line.starts_with("commit ") {
                    t = t.strong();
                } else if line.starts_with('+') {
                    t = t.color(add);
                } else if line.starts_with('-') {
                    t = t.color(del);
                } else if line.starts_with("@@") {
                    t = t.color(hunk);
                }
                ui.add(egui::Label::new(t).extend());
            }
        });
}

fn activity_button(ui: &mut egui::Ui, icon: &str, on: bool) -> egui::Response {
    let color = if on {
        ui.visuals().strong_text_color()
    } else {
        ui.visuals().weak_text_color()
    };
    ui.add(
        egui::Button::new(RichText::new(icon).size(22.0).color(color))
            .frame(false)
            .min_size(egui::vec2(36.0, 36.0)),
    )
}

fn toggle(ui: &mut egui::Ui, on: &mut bool, label: &str, tip: &str) {
    let text = RichText::new(label).monospace().strong();
    if ui.selectable_label(*on, text).on_hover_text(tip).clicked() {
        *on = !*on;
    }
}

fn icon_button(ui: &mut egui::Ui, icon: &str) -> egui::Response {
    ui.add(egui::Button::new(RichText::new(icon).size(15.0)).frame(false))
}

fn badge(ui: &egui::Ui, rect: egui::Rect, n: usize) {
    let text = if n > 99 { "99+".to_string() } else { n.to_string() };
    let center = rect.right_bottom() - egui::vec2(8.0, 8.0);
    ui.painter().circle_filled(center, 8.0, ui.visuals().selection.bg_fill);
    ui.painter().text(
        center,
        egui::Align2::CENTER_CENTER,
        text,
        egui::FontId::proportional(10.0),
        Color32::WHITE,
    );
}

enum TabMenu {
    Close(usize),
    CloseOthers(usize),
    CloseRight(usize),
    CloseAll,
    Copy(String),
    Format(usize),
    Split(usize),
}

/// (added, removed) text colors.
fn diff_colors(dark: bool) -> (Color32, Color32) {
    if dark {
        (Color32::from_rgb(0x8b, 0xd5, 0x8b), Color32::from_rgb(0xf0, 0x80, 0x80))
    } else {
        (Color32::from_rgb(0x1a, 0x7f, 0x37), Color32::from_rgb(0xcf, 0x22, 0x2e))
    }
}

/// Two synchronized panes: old file on the left, new on the right.
fn side_by_side_view(ui: &mut egui::Ui, d: &mut DiffTab) {
    use crate::diff::Kind;
    use egui::text::CCursor;
    if let Some(note) = &d.sbs.note {
        ui.add_space(20.0);
        ui.vertical_centered(|ui| ui.weak(note));
        return;
    }
    let dark = ui.visuals().dark_mode;
    let a = if dark { 38 } else { 34 };
    let (add_bg, del_bg) = (
        Color32::from_rgba_unmultiplied(60, 180, 90, a),
        Color32::from_rgba_unmultiplied(220, 70, 70, a),
    );
    let (add_hl, del_hl) = (
        Color32::from_rgba_unmultiplied(60, 180, 90, a * 3),
        Color32::from_rgba_unmultiplied(220, 70, 70, a * 3),
    );
    let empty_bg = ui.visuals().faint_bg_color;
    let weak = ui.visuals().weak_text_color();
    let theme = CodeTheme::from_style(ui.style());
    let font = egui::FontId::monospace(13.0);
    let row_h = ui.fonts_mut(|f| f.row_height(&font)) + 2.0;
    let gutter = ui.fonts_mut(|f| f.glyph_width(&font, '0')) * 6.0;

    ui.spacing_mut().item_spacing.y = 0.0;
    let mut area = egui::ScrollArea::vertical().id_salt("sbs").auto_shrink([false, false]);
    if let Some(row) = d.jump.take() {
        area = area.vertical_scroll_offset(row.saturating_sub(3) as f32 * row_h);
    }
    let mut widest = 0.0f32;
    let mut half_w = 0.0f32;
    let out = area.show_rows(ui, row_h, d.sbs.rows.len(), |ui, range| {
        let full = ui.available_width();
        half_w = (full - 1.0) / 2.0;
        for row in &d.sbs.rows[range] {
            let (rect, _) = ui.allocate_exact_size(egui::vec2(full, row_h), egui::Sense::hover());
            if let Kind::Gap(n) = row.kind {
                ui.painter().rect_filled(rect, 0.0, empty_bg);
                ui.painter().text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    format!("⋯ {n} unchanged lines"),
                    egui::FontId::proportional(12.0),
                    weak,
                );
                continue;
            }
            for (side, is_left) in [(&row.left, true), (&row.right, false)] {
                let half = if is_left {
                    egui::Rect::from_min_size(rect.min, egui::vec2(half_w, row_h))
                } else {
                    egui::Rect::from_min_size(egui::pos2(rect.min.x + half_w + 1.0, rect.min.y), egui::vec2(half_w, row_h))
                };
                let painter = ui.painter_at(half);
                let Some(side) = side else {
                    painter.rect_filled(half, 0.0, empty_bg);
                    continue;
                };
                let (bg, hl) = match (row.kind, is_left) {
                    (Kind::Removed | Kind::Changed, true) => (Some(del_bg), del_hl),
                    (Kind::Added | Kind::Changed, false) => (Some(add_bg), add_hl),
                    _ => (None, add_hl),
                };
                if let Some(bg) = bg {
                    painter.rect_filled(half, 0.0, bg);
                }
                painter.text(
                    egui::pos2(half.left() + gutter - 8.0, half.center().y),
                    egui::Align2::RIGHT_CENTER,
                    side.line.to_string(),
                    font.clone(),
                    weak,
                );
                let mut job = syntax_highlighting::highlight(ui.ctx(), ui.style(), &theme, &side.text, &d.lang);
                job.wrap.max_width = f32::INFINITY;
                let galley = ui.fonts_mut(|f| f.layout_job(job));
                widest = widest.max(galley.size().x);
                let text_clip = egui::Rect::from_min_max(egui::pos2(half.left() + gutter, half.top()), half.max);
                let painter = ui.painter_at(text_clip);
                let origin = egui::pos2(half.left() + gutter - d.hscroll, half.center().y - galley.size().y / 2.0);
                if let Some(r) = side.hl.as_ref().filter(|r| !r.is_empty()) {
                    let x0 = galley.pos_from_cursor(CCursor::new(r.start)).min.x;
                    let x1 = galley.pos_from_cursor(CCursor::new(r.end)).min.x;
                    painter.rect_filled(egui::Rect::from_x_y_ranges(origin.x + x0..=origin.x + x1, half.y_range()), 2.0, hl);
                }
                painter.galley(origin, galley, ui.visuals().text_color());
            }
            ui.painter().vline(
                rect.min.x + half_w + 0.5,
                rect.y_range(),
                egui::Stroke::new(1.0, ui.visuals().widgets.noninteractive.bg_stroke.color),
            );
        }
    });
    d.top_row = (out.state.clamped_offset().y / row_h) as usize;
    // Horizontal scrolling moves both sides together.
    if ui.rect_contains_pointer(out.inner_rect) {
        let dx = ui.input(|i| i.smooth_scroll_delta.x);
        let max = (widest - (half_w - gutter) + 20.0).max(0.0);
        d.hscroll = (d.hscroll - dx).clamp(0.0, max);
    }
}

enum RowAction {
    None,
    Open,
    Button(usize),
}

enum ChangeAction {
    OpenDiff(FileChange, bool),
    OpenFile(String),
    Stage(String),
    Unstage(String),
    Discard(FileChange),
    ToggleFolder(String),
}

/// One section (staged or unstaged) of the Changes view, as a flat list or a folder tree.
fn change_section(
    ui: &mut egui::Ui,
    items: &[FileChange],
    staged: bool,
    show_stage: bool,
    as_tree: bool,
    collapsed: &HashSet<String>,
) -> Option<ChangeAction> {
    let mut action = None;
    let mut file_row = |ui: &mut egui::Ui, c: &FileChange, depth: usize, show_dir: bool| {
        const UNSTAGED_BUTTONS: &[(&str, &str)] = &[
            (ic::PLUS, "Stage"),
            (ic::ARROW_U_UP_LEFT, "Discard changes"),
            (ic::ARROW_SQUARE_OUT, "Open file"),
        ];
        let buttons = if staged {
            if show_stage { &[(ic::MINUS, "Unstage")][..] } else { &[] }
        } else {
            &UNSTAGED_BUTTONS[usize::from(!show_stage)..]
        };
        match change_row(ui, c, buttons, depth, show_dir) {
            RowAction::Open => action = Some(ChangeAction::OpenDiff(c.clone(), staged)),
            RowAction::Button(0) if staged => action = Some(ChangeAction::Unstage(c.path.clone())),
            RowAction::Button(0) if show_stage => action = Some(ChangeAction::Stage(c.path.clone())),
            RowAction::Button(i) if i == usize::from(show_stage) => action = Some(ChangeAction::Discard(c.clone())),
            RowAction::Button(_) => action = Some(ChangeAction::OpenFile(c.path.clone())),
            _ => {}
        }
    };
    if !as_tree {
        for c in items {
            file_row(ui, c, 0, true);
        }
        return action;
    }
    let key_prefix = if staged { "S:" } else { "U:" };
    let section_collapsed: HashSet<String> = collapsed
        .iter()
        .filter_map(|k| k.strip_prefix(key_prefix).map(str::to_string))
        .collect();
    let paths: Vec<&str> = items.iter().map(|c| c.path.as_str()).collect();
    let mut folder_action = None;
    for row in crate::tree::layout(&paths, &section_collapsed) {
        match row {
            crate::tree::Row::File { depth, index } => file_row(ui, &items[index], depth, false),
            crate::tree::Row::Folder {
                depth,
                path,
                label,
                files,
                collapsed,
            } => {
                ui.horizontal(|ui| {
                    ui.add_space(depth as f32 * 12.0);
                    let (caret, folder) = if collapsed {
                        (ic::CARET_RIGHT, ic::FOLDER)
                    } else {
                        (ic::CARET_DOWN, ic::FOLDER_OPEN)
                    };
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let (icon, tip) = if staged {
                            (ic::MINUS, "Unstage folder")
                        } else {
                            (ic::PLUS, "Stage folder")
                        };
                        if show_stage && icon_button(ui, icon).on_hover_text(tip).clicked() {
                            folder_action = Some(if staged {
                                ChangeAction::Unstage(path.clone())
                            } else {
                                ChangeAction::Stage(path.clone())
                            });
                        }
                        ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                            let text = format!("{caret} {folder} {label}");
                            if ui
                                .add(egui::Button::selectable(false, text).truncate())
                                .on_hover_text(&path)
                                .clicked()
                            {
                                folder_action = Some(ChangeAction::ToggleFolder(format!("{key_prefix}{path}")));
                            }
                            ui.weak(files.to_string());
                        });
                    });
                });
            }
        }
    }
    folder_action.or(action)
}

fn change_row(ui: &mut egui::Ui, c: &FileChange, buttons: &[(&str, &str)], depth: usize, show_dir: bool) -> RowAction {
    let mut action = RowAction::None;
    ui.horizontal(|ui| {
        ui.add_space(depth as f32 * 12.0);
        ui.label(RichText::new(c.code.to_string()).monospace().color(status_color(c.code)));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            for (i, (icon, tip)) in buttons.iter().enumerate().rev() {
                if icon_button(ui, icon).on_hover_text(*tip).clicked() {
                    action = RowAction::Button(i);
                }
            }
            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                let dir = Path::new(&c.path).parent().map(|p| p.display().to_string()).unwrap_or_default();
                let r = ui
                    .add(egui::Button::selectable(false, file_name(&c.path)).truncate())
                    .on_hover_text(&c.path);
                if r.clicked() {
                    action = RowAction::Open;
                }
                if show_dir && !dir.is_empty() {
                    ui.add(egui::Label::new(RichText::new(dir).weak().small()).truncate());
                }
            });
        });
    });
    action
}

// --------------------------------------------------------------- helpers

fn status_color(code: char) -> Color32 {
    match code {
        'M' => Color32::from_rgb(0xe2, 0xb3, 0x40),
        'A' | '?' => Color32::from_rgb(0x73, 0xc9, 0x91),
        'D' => Color32::from_rgb(0xe0, 0x6c, 0x75),
        'R' | 'C' => Color32::from_rgb(0x56, 0xb6, 0xc2),
        'U' => Color32::from_rgb(0xff, 0x6b, 0x6b),
        _ => Color32::GRAY,
    }
}

fn file_name(p: &str) -> String {
    Path::new(p)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.to_string())
}

fn dir_name(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.display().to_string())
}

fn expand_home(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~") {
        let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
        if let Some(h) = home {
            return PathBuf::from(h).join(rest.trim_start_matches(['/', '\\']));
        }
    }
    PathBuf::from(p)
}

fn recent_file() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("edittr").join("recent-folders.txt"))
}

fn load_recent() -> Vec<PathBuf> {
    recent_file()
        .and_then(|f| std::fs::read_to_string(f).ok())
        .map(|s| s.lines().filter(|l| !l.trim().is_empty()).map(PathBuf::from).collect())
        .unwrap_or_default()
}

fn save_recent(list: &[PathBuf]) {
    if cfg!(feature = "tour") && std::env::var_os("EDITTR_TOUR_OUT").is_some() {
        return; // the screenshot tour must not touch the user's recent list
    }
    let Some(f) = recent_file() else { return };
    let _ = std::fs::create_dir_all(f.parent().unwrap());
    let text: String = list.iter().map(|p| format!("{}\n", p.display())).collect();
    let _ = std::fs::write(f, text);
}

struct DiskText {
    text: String,
    crlf: bool,
}

fn read_doc_text(path: &Path) -> std::io::Result<Option<DiskText>> {
    let bytes = std::fs::read(path)?;
    Ok((!bytes.iter().take(8000).any(|&b| b == 0)).then(|| {
        let text = String::from_utf8_lossy(&bytes);
        let crlf = text.contains("\r\n");
        DiskText {
            text: if crlf { text.replace("\r\n", "\n") } else { text.into_owned() },
            crlf,
        }
    }))
}

fn read_dir_sorted(dir: &Path) -> DirEntries {
    let mut v: DirEntries = std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(Result::ok)
                .filter(|e| e.file_name() != ".git" && e.file_name() != ".svn")
                .map(|e| {
                    let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
                    (e.path(), is_dir)
                })
                .collect()
        })
        .unwrap_or_default();
    v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.file_name().cmp(&b.0.file_name())));
    v
}

fn background<T: Send + 'static>(ctx: &egui::Context, work: impl FnOnce() -> T + Send + 'static) -> Receiver<T> {
    let (tx, rx) = channel();
    let ctx = ctx.clone();
    std::thread::spawn(move || {
        let _ = tx.send(work());
        drop(tx);
        ctx.request_repaint();
    });
    rx
}

fn receive<T>(rx: &mut Option<Receiver<T>>) -> Option<T> {
    match rx.as_ref()?.try_recv() {
        Ok(value) => {
            *rx = None;
            Some(value)
        }
        Err(TryRecvError::Disconnected) => {
            *rx = None;
            None
        }
        Err(TryRecvError::Empty) => None,
    }
}

/// Files under `root` as root-relative paths: git's view (tracked + untracked, minus
/// ignored) inside a repo, otherwise a directory walk that skips common build dirs.
fn project_files(root: &Path, vcs: Option<Vcs>, ignored: &[String]) -> Vec<String> {
    if let Some(mut files) = vcs::list_files(root, vcs) {
        if !ignored.is_empty() {
            files.retain(|rel| !workspace::is_ignored(rel, ignored));
        }
        files
    } else {
        walk_files(root, 50_000, ignored)
    }
}

fn walk_files(root: &Path, limit: usize, ignored: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for (p, is_dir) in read_dir_sorted(&dir) {
            let Some(rel) = workspace::relative_path(root, &p) else { continue };
            if workspace::is_ignored(&rel, ignored) {
                continue;
            }
            if is_dir {
                let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                if !IGNORED_DIRS.contains(&name.as_str()) {
                    stack.push(p);
                }
            } else {
                out.push(rel);
                if out.len() >= limit {
                    return out;
                }
            }
        }
    }
    out
}

/// Subsequence fuzzy match, scored to favour matches in the file name and contiguous runs.
/// Indices of the best `limit` matches of `query` (subsequence, case-insensitive)
/// among `lower` (lowercased paths), best first; favours file-name and contiguous hits.
fn fuzzy_filter(lower: &[String], query: &str, limit: usize) -> Vec<usize> {
    let q: Vec<char> = query.to_lowercase().chars().filter(|c| !c.is_whitespace()).collect();
    if q.is_empty() {
        return (0..lower.len().min(limit)).collect();
    }
    let mut scored: Vec<(i32, usize)> = lower
        .iter()
        .enumerate()
        .filter_map(|(idx, f)| {
            let name_start = f.rfind('/').map(|i| i + 1).unwrap_or(0);
            let mut qi = 0;
            let mut score = 0;
            let mut prev = usize::MAX - 1;
            for (i, ch) in f.char_indices() {
                if ch == q[qi] {
                    score += if i == prev + 1 { 5 } else { 1 };
                    if i >= name_start {
                        score += 3;
                    }
                    prev = i;
                    qi += 1;
                    if qi == q.len() {
                        return Some((score * 100 - f.len() as i32, idx));
                    }
                }
            }
            None
        })
        .collect();
    let key = |&(score, idx): &(i32, usize)| (std::cmp::Reverse(score), idx);
    if scored.len() > limit {
        scored.select_nth_unstable_by_key(limit, key);
        scored.truncate(limit);
    }
    scored.sort_unstable_by_key(key);
    scored.into_iter().map(|(_, idx)| idx).collect()
}

#[cfg(test)]
mod tests {
    use super::language_name;

    #[test]
    fn fuzzy_filter_ranks_file_name_matches_first_and_is_fast() {
        use super::fuzzy_filter;
        let files: Vec<String> = ["src/app.rs", "docs/apples.md", "src/main.rs", "a/p/p.rs"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let m = fuzzy_filter(&files, "App", 10);
        assert_eq!(files[m[0]], "src/app.rs");
        assert_eq!(m.len(), 3);
        assert!(fuzzy_filter(&files, "zzz", 10).is_empty());
        assert_eq!(fuzzy_filter(&files, "", 2), vec![0, 1]);
        assert_eq!(fuzzy_filter(&files, "  ", 2), vec![0, 1]);

        let many: Vec<String> = (0..100_000).map(|i| format!("crates/module{}/src/file_{i}.rs", i % 97)).collect();
        let t = std::time::Instant::now();
        let m = fuzzy_filter(&many, "mod5file9", 200);
        assert_eq!(m.len(), 200);
        eprintln!("fuzzy_filter over 100k paths: {:?}", t.elapsed());
    }

    #[test]
    fn language_names_for_extensions() {
        for (ext, name) in [
            ("rs", "Rust"),
            ("h", "C"),
            ("hpp", "C++"),
            ("mts", "TypeScript"),
            ("tsx", "TypeScript React"),
            ("jsx", "JavaScript React"),
            ("yml", "YAML"),
            ("zsh", "Shell"),
            ("", "Plain Text"),
            ("unknown", "Plain Text"),
        ] {
            assert_eq!(language_name(ext), name, "extension {ext:?}");
        }
    }
}
