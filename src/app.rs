#[cfg(feature = "tour")]
mod tour;

use crate::git::{self, FileChange, Status};
use crate::graph::Graph;
use crate::icons as ic;
use crate::search::{self, FileHits};
use crate::{goto, installer, lsp};
use eframe::egui::{self, Color32, Key, KeyboardShortcut, Modifiers, RichText};
use egui_extras::syntax_highlighting::{self, CodeTheme};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

const SAVE: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::S);
const QUICK_OPEN: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::P);
const CLOSE_TAB: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::W);
const FIND_IN_FILES: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND.plus(Modifiers::SHIFT), Key::F);
const SEARCH_DEBOUNCE: Duration = Duration::from_millis(250);
const GOTO_DEF: KeyboardShortcut = KeyboardShortcut::new(Modifiers::NONE, Key::F12);
#[cfg(target_os = "macos")]
const GO_BACK: KeyboardShortcut = KeyboardShortcut::new(Modifiers::CTRL, Key::Minus);
#[cfg(not(target_os = "macos"))]
const GO_BACK: KeyboardShortcut = KeyboardShortcut::new(Modifiers::ALT, Key::ArrowLeft);
const COMMIT: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::Enter);
const STATUS_POLL: Duration = Duration::from_secs(3);
const HIGHLIGHT_LIMIT: usize = 1_000_000; // bytes; bigger files are shown as plain text
const IGNORED_DIRS: &[&str] = &[".git", "node_modules", "target", ".venv", "__pycache__", "dist", "build"];

struct Doc {
    path: PathBuf,
    text: String,
    saved: String,
    lang: String,
    /// Pending selection (char range) to reveal on the next frame, e.g. from a search hit.
    goto: Option<(usize, usize)>,
    /// Char index of the text cursor (last frame).
    cursor: usize,
    /// Other end of the selection (== cursor when nothing is selected).
    anchor: usize,
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

impl Doc {
    fn dirty(&self) -> bool {
        self.text != self.saved
    }
    fn name(&self) -> String {
        self.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
    }
}

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

#[derive(PartialEq, Clone, Copy)]
enum Side {
    Explorer,
    Changes,
    Search,
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
}

enum SearchRow {
    File(usize),
    Hit(usize, usize),
}

struct QuickOpen {
    query: String,
    files: Vec<String>,
    selected: usize,
}

enum Confirm {
    CloseTabs(Vec<usize>),
    Discard(FileChange),
}

pub struct App {
    /// Open project folder; None = empty project.
    root: Option<PathBuf>,
    /// Recently opened folders, most recent first.
    recent: Vec<PathBuf>,
    repo: Option<PathBuf>,
    side: Side,
    side_open: bool,
    tabs: Vec<Tab>,
    active: usize,

    tree: HashMap<PathBuf, Vec<(PathBuf, bool)>>,
    expanded: HashSet<PathBuf>,

    status: Status,
    status_rx: Option<Receiver<Result<Status, String>>>,
    last_poll: Instant,
    branches: Vec<String>,
    commit_msg: String,
    amend: bool,

    graph: Option<Graph>,
    graph_sel: Option<usize>,
    graph_detail: Vec<String>,

    bg_task: Option<(String, Receiver<Result<String, String>>)>,
    message: String,
    quick_open: Option<QuickOpen>,
    confirm: Option<Confirm>,
    open_folder_requested: bool,
    open_folder: Option<String>,
    search: SearchState,

    lsp: HashMap<&'static str, Arc<lsp::Client>>,
    lsp_failed: HashSet<&'static str>,
    goto_rx: Option<Receiver<goto::Outcome>>,
    /// Where we jumped from, for "go back".
    nav_back: Vec<(PathBuf, usize)>,
    install: Option<InstallJob>,
    install_prompt: Option<&'static str>,
    install_declined: HashSet<&'static str>,
    changes_tree: bool,
    /// Collapsed folders in the Changes tree, keyed "S:<path>" (staged) or "U:<path>".
    changes_collapsed: HashSet<String>,
    #[cfg(feature = "tour")]
    tour: Option<tour::Tour>,
}

impl App {
    pub fn new(cc: &eframe::CreationContext, root: Option<PathBuf>, file: Option<PathBuf>) -> Self {
        ic::install(&cc.egui_ctx);
        cc.egui_ctx.global_style_mut(|s| {
            s.spacing.item_spacing.y = 3.0;
        });
        let mut app = App {
            repo: None,
            root: None,
            recent: load_recent(),
            side: Side::Explorer,
            side_open: true,
            tabs: Vec::new(),
            active: 0,
            tree: HashMap::new(),
            expanded: HashSet::new(),
            status: Status::default(),
            status_rx: None,
            last_poll: Instant::now(),
            branches: Vec::new(),
            commit_msg: String::new(),
            amend: false,
            graph: None,
            graph_sel: None,
            graph_detail: Vec::new(),
            bg_task: None,
            message: String::new(),
            quick_open: None,
            confirm: None,
            open_folder_requested: false,
            open_folder: None,
            search: SearchState::default(),
            lsp: HashMap::new(),
            lsp_failed: HashSet::new(),
            goto_rx: None,
            nav_back: Vec::new(),
            install: None,
            install_prompt: None,
            install_declined: HashSet::new(),
            changes_tree: false,
            changes_collapsed: HashSet::new(),
            #[cfg(feature = "tour")]
            tour: tour::Tour::from_env(),
        };
        if let Some(root) = root {
            app.set_root(root);
        }
        if let Some(f) = file {
            app.open_file(&f);
        }
        app
    }

    fn set_root(&mut self, root: PathBuf) {
        self.repo = git::repo_root(&root);
        self.recent.retain(|r| r != &root);
        self.recent.insert(0, root.clone());
        self.recent.truncate(10);
        save_recent(&self.recent);
        self.root = Some(root);
        self.tree.clear();
        self.expanded.clear();
        self.graph = None;
        self.graph_sel = None;
        self.status = Status::default();
        self.lsp.clear(); // servers are per project; dropping kills them
        self.lsp_failed.clear();
        self.nav_back.clear();
        self.refresh_git();
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
            || self.confirm.is_some()
            || self.install_prompt.is_some()
            || self.open_folder.is_some()
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

    // ---------------------------------------------------------------- git

    fn refresh_git(&mut self) {
        let Some(repo) = self.repo.clone() else { return };
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let _ = tx.send(git::status(&repo));
        });
        self.status_rx = Some(rx);
        self.last_poll = Instant::now();
        self.branches = git::branches(self.repo.as_ref().unwrap());
        if self.graph.is_some() {
            self.load_graph();
        }
    }

    fn load_graph(&mut self) {
        let Some(repo) = &self.repo else { return };
        match git::log(repo, 2000) {
            Ok(commits) => self.graph = Some(Graph::build(commits)),
            Err(e) => self.message = e,
        }
    }

    fn git_op(&mut self, res: Result<String, String>) {
        match res {
            Ok(out) => self.message = out.lines().last().unwrap_or("").to_string(),
            Err(e) => self.message = e,
        }
        self.refresh_git();
    }

    fn spawn_remote(&mut self, label: &str, args: &'static [&'static str], ctx: &egui::Context) {
        let Some(repo) = self.repo.clone() else { return };
        if self.bg_task.is_some() {
            return;
        }
        let (tx, rx) = channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(git::run(&repo, args));
            ctx.request_repaint();
        });
        self.message = format!("{label}…");
        self.bg_task = Some((label.to_string(), rx));
    }

    fn poll_background(&mut self, ctx: &egui::Context) {
        self.poll_goto(ctx);
        self.poll_search(ctx);
        if let Some(rx) = &self.status_rx
            && let Ok(res) = rx.try_recv()
        {
            match res {
                Ok(s) => self.status = s,
                Err(e) => self.message = e,
            }
            self.status_rx = None;
        }
        if let Some((label, rx)) = &self.bg_task
            && let Ok(res) = rx.try_recv()
        {
            self.message = match res {
                Ok(_) => format!("{label} done"),
                Err(e) => format!("{label} failed: {e}"),
            };
            self.bg_task = None;
            self.refresh_git();
        }
        let focused = ctx.input(|i| i.viewport().focused.unwrap_or(true));
        if self.repo.is_some() && focused && self.last_poll.elapsed() > STATUS_POLL && self.status_rx.is_none() {
            self.refresh_git();
        }
        if self.repo.is_some() {
            ctx.request_repaint_after(STATUS_POLL);
        }
    }

    // --------------------------------------------------------------- tabs

    fn open_file(&mut self, path: &Path) {
        if let Some(i) = self.tabs.iter().position(|t| matches!(t, Tab::File(d) if d.path == path)) {
            self.active = i;
            return;
        }
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) => {
                self.message = format!("{}: {e}", path.display());
                return;
            }
        };
        if bytes.iter().take(8000).any(|&b| b == 0) {
            self.message = format!("{} looks binary; not opening", path.display());
            return;
        }
        let text = String::from_utf8_lossy(&bytes).replace("\r\n", "\n");
        let lang = path
            .extension()
            .or_else(|| path.file_name())
            .map(|e| e.to_string_lossy().into_owned())
            .unwrap_or_default();
        self.tabs.push(Tab::File(Doc {
            path: path.to_path_buf(),
            saved: text.clone(),
            text,
            lang,
            goto: None,
            cursor: 0,
            anchor: 0,
        }));
        // Warm up the language server so the first go-to-definition is fast.
        let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
        self.client_for(&ext);
        self.active = self.tabs.len() - 1;
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
        let Some(repo) = &self.repo else { return };
        let text = git::diff(repo, change, staged);
        let title = format!("{} ({})", file_name(&change.path), if staged { "staged" } else { "working tree" });
        let path = repo.join(&change.path);
        let inline = matches!(self.tabs.iter().find(|t| matches!(t, Tab::Diff(_))), Some(Tab::Diff(d)) if d.inline);
        let mut tab = DiffTab::new(title, path, &text);
        tab.inline = inline; // keep the user's chosen layout
        self.show_tab(Tab::Diff(Box::new(tab)));
    }

    fn open_graph(&mut self) {
        if self.graph.is_none() {
            self.load_graph();
        }
        self.show_tab(Tab::Graph);
    }

    fn save_active(&mut self) {
        if let Some(Tab::File(doc)) = self.tabs.get_mut(self.active) {
            match std::fs::write(&doc.path, &doc.text) {
                Ok(()) => {
                    doc.saved = doc.text.clone();
                    self.message = format!("Saved {}", doc.path.display());
                }
                Err(e) => self.message = format!("Save failed: {e}"),
            }
            self.refresh_git();
        }
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

    // ----------------------------------------------------------- shortcuts

    fn shortcuts(&mut self, ctx: &egui::Context) {
        if ctx.input_mut(|i| i.consume_shortcut(&SAVE)) {
            self.save_active();
        }
        if ctx.input_mut(|i| i.consume_shortcut(&QUICK_OPEN)) {
            self.start_quick_open();
        }
        if ctx.input_mut(|i| i.consume_shortcut(&CLOSE_TAB)) {
            self.close_tab(self.active, false);
        }
        if ctx.input_mut(|i| i.consume_shortcut(&GOTO_DEF))
            && let Some(Tab::File(d)) = self.tabs.get(self.active)
        {
            let idx = d.cursor;
            self.go_to_definition(idx, ctx);
        }
        if ctx.input_mut(|i| i.consume_shortcut(&GO_BACK)) {
            self.go_back();
        }
        if ctx.input_mut(|i| i.consume_shortcut(&FIND_IN_FILES)) {
            let selection = match self.tabs.get(self.active) {
                Some(Tab::File(d)) => d.selected_text(),
                _ => None,
            };
            self.show_search();
            if let Some(sel) = selection {
                // Search right away for exactly what's selected.
                self.search.opts.query = if self.search.opts.regex { regex::escape(&sel) } else { sel };
                self.start_search(ctx);
            }
        }
    }

    fn show_search(&mut self) {
        self.side = Side::Search;
        self.side_open = true;
        self.search.focus = true;
    }

    fn start_quick_open(&mut self) {
        let Some(root) = &self.root else {
            self.message = "Open a folder to use Go to file".into();
            return;
        };
        let files = project_files(root, self.repo.is_some());
        self.quick_open = Some(QuickOpen {
            query: String::new(),
            files,
            selected: 0,
        });
    }

    // ------------------------------------------------------------------ UI

    // ------------------------------------------------- code intelligence

    /// Running language server for files with extension `ext`, starting one if installed.
    fn client_for(&mut self, ext: &str) -> Option<Arc<lsp::Client>> {
        for spec in lsp::servers_for(ext) {
            if let Some(c) = self.lsp.get(spec.name) {
                if c.alive() {
                    return Some(c.clone());
                }
                self.lsp.remove(spec.name);
                self.lsp_failed.insert(spec.name); // crashed: don't restart in a loop
                self.message = format!("{} stopped; using text search", spec.name);
                continue;
            }
            if self.lsp_failed.contains(spec.name) || lsp::resolve(spec.command).is_none() {
                continue;
            }
            let root = self.work_dir();
            match lsp::Client::start(&spec, &root) {
                Ok(c) => {
                    self.lsp.insert(spec.name, c.clone());
                    return Some(c);
                }
                Err(e) => {
                    self.lsp_failed.insert(spec.name);
                    self.message = e;
                }
            }
        }
        None
    }

    fn go_to_definition(&mut self, char_idx: usize, ctx: &egui::Context) {
        let Some(Tab::File(doc)) = self.tabs.get(self.active) else { return };
        let (path, text, ext, from) = (doc.path.clone(), doc.text.clone(), doc.ext(), doc.cursor);
        let Some((_, word)) = goto::word_at(&text, char_idx) else { return };
        let client = self.client_for(&ext);
        if client.is_none() {
            self.offer_install(&ext);
        }
        // Without a project, the text-search fallback only looks at the current file.
        let project = self.root.clone();
        let in_repo = self.repo.is_some();
        let root = project.clone().unwrap_or_else(|| self.work_dir());
        let req = goto::Request {
            root,
            files: Box::new(move || project.map(|r| project_files(&r, in_repo)).unwrap_or_default()),
            path: path.clone(),
            text,
            char_idx,
            client,
        };
        let (tx, rx) = channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(goto::run(req));
            ctx.request_repaint();
        });
        self.goto_rx = Some(rx);
        self.nav_back.push((path, from));
        self.message = format!("Finding definition of `{word}`…");
    }

    fn poll_goto(&mut self, ctx: &egui::Context) {
        if let Some(job) = &self.install {
            ctx.request_repaint_after(Duration::from_millis(200));
            if let Ok(res) = job.rx.try_recv() {
                let command = job.command;
                self.install = None;
                match res {
                    Ok(path) => {
                        self.message = format!("Installed {command} at {}", path.display());
                        self.lsp_failed.clear();
                        if let Some(Tab::File(d)) = self.tabs.get(self.active) {
                            let ext = d.ext();
                            self.client_for(&ext);
                        }
                    }
                    Err(e) => self.message = format!("Could not install {command}: {e}"),
                }
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
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(installer::install(&inst, &p));
            ctx.request_repaint();
        });
        self.install = Some(InstallJob { command, progress, rx });
    }

    // -------------------------------------------------------------- search

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
        let in_repo = self.repo.is_some();
        let list_root = root.clone();
        st.rx = Some(search::spawn(
            root,
            move || project_files(&list_root, in_repo),
            st.opts.clone(),
            st.cancel.clone(),
            ctx.clone(),
        ));
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

    fn open_at(&mut self, path: &Path, line: usize, cols: std::ops::Range<usize>) {
        self.open_file(path);
        if let Some(Tab::File(doc)) = self.tabs.get_mut(self.active)
            && doc.path == path
        {
            let line_start: usize = doc.text.split('\n').take(line).map(|l| l.chars().count() + 1).sum();
            let max = doc.text.chars().count();
            doc.goto = Some(((line_start + cols.start).min(max), (line_start + cols.end).min(max)));
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
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            toggle(ui, &mut st.opts.match_case, "Aa", "Match case");
            toggle(ui, &mut st.opts.whole_word, "ab", "Match whole word");
            toggle(ui, &mut st.opts.regex, ".*", "Use regular expression");
            if st.rx.is_some() {
                ui.add_space(6.0);
                ui.spinner();
            }
        });
        if st.opts != st.searched && st.edited.is_none() {
            st.edited = Some(Instant::now());
        }
        if enter {
            self.start_search(ui.ctx());
            self.search.focus = true;
        }
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
        let mut open = None;
        let row_h = 20.0;
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show_rows(ui, row_h, rows.len(), |ui, range| {
                for row in &rows[range] {
                    let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), row_h), egui::Sense::click());
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
    }

    fn activity_bar(&mut self, ui: &mut egui::Ui) {
        ui.add_space(6.0);
        ui.vertical_centered(|ui| {
            ui.spacing_mut().item_spacing.y = 6.0;
            let n = self.status.staged.len() + self.status.unstaged.len();
            let changes_tip = if n > 0 {
                format!("Source control ({n} changes)")
            } else {
                "Source control".into()
            };
            for (side, icon, tip) in [
                (Side::Explorer, ic::FILES, "Explorer".to_string()),
                (Side::Search, ic::MAGNIFYING_GLASS, "Search in files (Ctrl/Cmd+Shift+F)".to_string()),
                (Side::Changes, ic::GIT_BRANCH, changes_tip),
            ] {
                let on = self.side_open && self.side == side;
                if side == Side::Search && !on {
                    self.search.focus = true;
                }
                let r = activity_button(ui, icon, on).on_hover_text(tip);
                if side == Side::Changes && n > 0 {
                    badge(ui, r.rect, n);
                }
                if r.clicked() {
                    // Clicking the active view collapses the sidebar.
                    self.side_open = !on;
                    self.side = side;
                }
            }
            if activity_button(ui, ic::FILE_SEARCH, false)
                .on_hover_text("Go to file (Ctrl/Cmd+P)")
                .clicked()
            {
                self.start_quick_open();
            }
            ui.with_layout(egui::Layout::bottom_up(egui::Align::Center), |ui| {
                ui.add_space(4.0);
                let dark = ui.visuals().dark_mode;
                if activity_button(ui, if dark { ic::SUN } else { ic::MOON }, false)
                    .on_hover_text("Toggle theme")
                    .clicked()
                {
                    ui.ctx().set_theme(if dark { egui::Theme::Light } else { egui::Theme::Dark });
                }
                if activity_button(ui, ic::FOLDER_OPEN, false).on_hover_text("Open folder").clicked() {
                    self.request_open_folder();
                }
            });
        });
    }

    fn explorer(&mut self, ui: &mut egui::Ui) {
        let Some(root) = self.root.clone() else {
            self.welcome(ui);
            return;
        };
        ui.horizontal(|ui| {
            ui.label(RichText::new(dir_name(&root).to_uppercase()).small().strong());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if icon_button(ui, ic::ARROWS_CLOCKWISE).on_hover_text("Refresh").clicked() {
                    self.tree.clear();
                    self.refresh_git();
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
        if let Some(p) = open {
            self.open_file(&p);
        }
    }

    /// Explorer content when no folder is open.
    fn welcome(&mut self, ui: &mut egui::Ui) {
        ui.label(RichText::new("NO FOLDER OPENED").small().strong());
        ui.separator();
        ui.add_space(6.0);
        ui.weak("Open a folder to browse its files, search it and use git.");
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
        let entries = self.tree.entry(dir.to_path_buf()).or_insert_with(|| read_dir_sorted(dir)).clone();
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
                    if ui.selectable_label(false, format!("{caret} {folder} {name}")).clicked() {
                        if expanded {
                            self.expanded.remove(&path);
                        } else {
                            self.expanded.insert(path.clone());
                        }
                    }
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

    fn changes(&mut self, ui: &mut egui::Ui) {
        let Some(root) = self.root.clone() else {
            ui.label(RichText::new("SOURCE CONTROL").small().strong());
            ui.separator();
            ui.weak("Open a folder to see its git changes.");
            ui.add_space(6.0);
            if ui.button(format!("{} Open Folder…", ic::FOLDER_OPEN)).clicked() {
                self.request_open_folder();
            }
            return;
        };
        let Some(repo) = self.repo.clone() else {
            ui.label("Not a git repository.");
            if ui.button("Initialize repository").clicked() {
                let r = git::run(&root, &["init"]);
                self.git_op(r);
                self.repo = git::repo_root(&root);
                self.refresh_git();
            }
            return;
        };

        ui.horizontal(|ui| {
            let current = self.status.branch.clone();
            egui::ComboBox::from_id_salt("branch")
                .selected_text(format!("{} {current}", ic::GIT_BRANCH))
                .width(140.0)
                .show_ui(ui, |ui| {
                    for b in self.branches.clone() {
                        if ui.selectable_label(b == current, &b).clicked() && b != current {
                            let r = git::switch(&repo, &b);
                            self.git_op(r);
                        }
                    }
                });
            ui.weak(&self.status.ahead_behind);
        });
        ui.horizontal(|ui| {
            let busy = self.bg_task.is_some();
            ui.add_enabled_ui(!busy, |ui| {
                if icon_button(ui, ic::CLOUD_ARROW_DOWN).on_hover_text("Fetch all").clicked() {
                    self.spawn_remote("Fetch", &["fetch", "--all", "--prune"], ui.ctx());
                }
                if icon_button(ui, ic::ARROW_DOWN).on_hover_text("Pull (fast-forward only)").clicked() {
                    self.spawn_remote("Pull", &["pull", "--ff-only"], ui.ctx());
                }
                if icon_button(ui, ic::ARROW_UP).on_hover_text("Push").clicked() {
                    self.spawn_remote("Push", &["push"], ui.ctx());
                }
            });
            if busy {
                ui.spinner();
            }
            if icon_button(ui, ic::ARROWS_CLOCKWISE).on_hover_text("Refresh").clicked() {
                self.refresh_git();
            }
            let (icon, tip) = if self.changes_tree {
                (ic::LIST, "View as list")
            } else {
                (ic::TREE_VIEW, "View as tree (grouped by folder)")
            };
            if icon_button(ui, icon).on_hover_text(tip).clicked() {
                self.changes_tree = !self.changes_tree;
            }
            if icon_button(ui, ic::TREE_STRUCTURE).on_hover_text("Git graph").clicked() {
                self.open_graph();
            }
        });
        ui.add_space(4.0);
        ui.add(
            egui::TextEdit::multiline(&mut self.commit_msg)
                .hint_text("Commit message (Ctrl/Cmd+Enter)")
                .desired_rows(3)
                .desired_width(f32::INFINITY),
        );
        let commit_key = ui.input_mut(|i| i.consume_shortcut(&COMMIT));
        ui.horizontal(|ui| {
            let can = !self.commit_msg.trim().is_empty() && (!self.status.staged.is_empty() || self.amend);
            let clicked = ui.add_enabled(can, egui::Button::new(format!("{} Commit", ic::CHECK))).clicked();
            ui.checkbox(&mut self.amend, "Amend");
            if (clicked || (commit_key && can)) && can {
                let r = git::commit(&repo, self.commit_msg.trim(), self.amend);
                if r.is_ok() {
                    self.commit_msg.clear();
                    self.amend = false;
                }
                self.git_op(r);
            }
        });
        ui.separator();

        let staged = self.status.staged.clone();
        let unstaged = self.status.unstaged.clone();
        let mut actions = Vec::new();
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.strong(format!("Staged ({})", staged.len()));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if !staged.is_empty() && icon_button(ui, ic::MINUS).on_hover_text("Unstage all").clicked() {
                        let r = git::unstage_all(&repo);
                        self.git_op(r);
                    }
                });
            });
            if let Some(a) = change_section(ui, &staged, true, self.changes_tree, &self.changes_collapsed) {
                actions.push(a);
            }
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.strong(format!("Changes ({})", unstaged.len()));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if !unstaged.is_empty() && icon_button(ui, ic::PLUS).on_hover_text("Stage all").clicked() {
                        let r = git::stage_all(&repo);
                        self.git_op(r);
                    }
                });
            });
            if let Some(a) = change_section(ui, &unstaged, false, self.changes_tree, &self.changes_collapsed) {
                actions.push(a);
            }
        });
        for action in actions {
            match action {
                ChangeAction::OpenDiff(c, staged) => self.open_diff(&c, staged),
                ChangeAction::OpenFile(path) => self.open_file(&repo.join(path)),
                ChangeAction::Stage(path) => {
                    let r = git::stage(&repo, &path);
                    self.git_op(r);
                }
                ChangeAction::Unstage(path) => {
                    let r = git::unstage(&repo, &path);
                    self.git_op(r);
                }
                ChangeAction::Discard(c) => self.confirm = Some(Confirm::Discard(c)),
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
                        Tab::Graph => format!("{} Git Graph", ic::TREE_STRUCTURE),
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
            Some(TabMenu::CloseAll) => self.close_tabs((0..self.tabs.len()).collect(), false),
            Some(TabMenu::Copy(text)) => ui.ctx().copy_text(text),
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
        match &mut self.tabs[active] {
            Tab::File(doc) => {
                if let Some(idx) = editor(ui, doc) {
                    goto_at = Some(idx);
                }
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
                    ui.label("No history to show.");
                    return;
                };
                let mut clicked = None;
                egui::Panel::bottom("commit_detail")
                    .resizable(true)
                    .default_size(260.0)
                    .show(ui, |ui| {
                        if self.graph_detail.is_empty() {
                            ui.weak("Select a commit to see its details.");
                        } else {
                            diff_view(ui, &self.graph_detail, "detail");
                        }
                    });
                egui::CentralPanel::default().show(ui, |ui| {
                    clicked = graph.ui(ui, self.graph_sel);
                });
                if let Some(r) = clicked {
                    self.graph_sel = Some(r);
                    let hash = graph.commits[r].hash.clone();
                    if let Some(repo) = &self.repo {
                        self.graph_detail = git::show_commit(repo, &hash).lines().map(str::to_string).collect();
                    }
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

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if self.repo.is_some() {
                ui.label(format!("{} {} {}", ic::GIT_BRANCH, self.status.branch, self.status.ahead_behind));
                ui.separator();
            }
            ui.label(&self.message);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if let Some(job) = &self.install {
                    ui.label(job.progress.describe());
                    ui.spinner();
                }
                if let Some(Tab::File(d)) = self.tabs.get(self.active) {
                    let server = lsp::servers_for(&d.ext()).iter().find_map(|s| self.lsp.get(s.name)).map(|c| c.name);
                    match server {
                        Some(name) => ui
                            .weak(name)
                            .on_hover_text("Language server running: Ctrl/Cmd+click or F12 to go to definition"),
                        None => ui
                            .weak("text search")
                            .on_hover_text("No language server for this file; go to definition uses text search"),
                    };
                    ui.weak(&d.lang);
                    let (line, col) = goto::line_col16(&d.text, d.cursor);
                    ui.weak(format!("Ln {}, Col {}", line + 1, col + 1));
                    if self.goto_rx.is_some() {
                        ui.spinner();
                    }
                }
            });
        });
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
                    let matches = fuzzy_filter(&q.files, &q.query, 200);
                    ui.input(|i| {
                        if i.key_pressed(Key::ArrowDown) {
                            q.selected = (q.selected + 1).min(matches.len().saturating_sub(1));
                        }
                        if i.key_pressed(Key::ArrowUp) {
                            q.selected = q.selected.saturating_sub(1);
                        }
                        if i.key_pressed(Key::Enter) {
                            chosen = matches.get(q.selected).cloned();
                        }
                    });
                    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                        for (i, m) in matches.iter().enumerate() {
                            let r = ui.selectable_label(i == q.selected, m);
                            if i == q.selected {
                                r.scroll_to_me(None);
                            }
                            if r.clicked() {
                                chosen = Some(m.clone());
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
                        Confirm::Discard(f) => {
                            if let Some(repo) = self.repo.clone() {
                                let r = git::discard(&repo, &f);
                                self.git_op(r);
                                self.reload_clean_docs();
                            }
                        }
                    }
                }
            }
        }
    }

    /// Reload open files that have no unsaved edits (e.g. after a discard).
    fn reload_clean_docs(&mut self) {
        for tab in &mut self.tabs {
            if let Tab::File(d) = tab
                && !d.dirty()
                && let Ok(t) = std::fs::read_to_string(&d.path)
            {
                let t = t.replace("\r\n", "\n");
                d.text = t.clone();
                d.saved = t;
            }
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.poll_background(&ctx);
        self.shortcuts(&ctx);

        let title = match &self.root {
            Some(r) => format!("{} — edittr", dir_name(r)),
            None => "edittr".to_string(),
        };
        ctx.send_viewport_cmd(egui::ViewportCommand::Title(title));

        egui::Panel::bottom("status").show(ui, |ui| self.status_bar(ui));
        egui::Panel::left("activity")
            .resizable(false)
            .exact_size(44.0)
            .show(ui, |ui| self.activity_bar(ui));
        if self.side_open {
            egui::Panel::left("side")
                .resizable(true)
                .default_size(260.0)
                .min_size(160.0)
                .show(ui, |ui| match self.side {
                    Side::Explorer => self.explorer(ui),
                    Side::Changes => self.changes(ui),
                    Side::Search => self.search_panel(ui),
                });
        }
        egui::CentralPanel::default().show(ui, |ui| self.central(ui));
        self.open_requested_folder(frame);
        self.dialogs(&ctx);
        #[cfg(feature = "tour")]
        if let Some(mut t) = self.tour.take() {
            t.tick(self, &ctx);
            self.tour = Some(t);
        }
    }

    #[cfg(feature = "tour")]
    fn raw_input_hook(&mut self, _ctx: &egui::Context, raw: &mut egui::RawInput) {
        if let Some(t) = &mut self.tour {
            t.raw_input_hook(raw);
        }
    }
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
fn editor(ui: &mut egui::Ui, doc: &mut Doc) -> Option<usize> {
    let mut goto_request = None;
    let theme = CodeTheme::from_style(ui.style());
    let highlight = doc.text.len() <= HIGHLIGHT_LIMIT;
    let lang = doc.lang.clone();
    let mut layouter = |ui: &egui::Ui, buf: &dyn egui::TextBuffer, _wrap: f32| {
        let mut job = if highlight {
            syntax_highlighting::highlight(ui.ctx(), ui.style(), &theme, buf.as_str(), &lang)
        } else {
            egui::text::LayoutJob::simple(
                buf.as_str().to_string(),
                egui::FontId::monospace(13.0),
                ui.visuals().text_color(),
                f32::INFINITY,
            )
        };
        job.wrap.max_width = f32::INFINITY;
        ui.fonts_mut(|f| f.layout_job(job))
    };
    let lines = doc.text.split('\n').count();
    egui::ScrollArea::both()
        .id_salt(&doc.path)
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.horizontal_top(|ui| {
                let gutter: String = (1..=lines).map(|n| format!("{n:>4}\n")).collect();
                ui.add(egui::Label::new(RichText::new(gutter).monospace().color(ui.visuals().weak_text_color())).selectable(false));
                let out = egui::TextEdit::multiline(&mut doc.text)
                    .code_editor()
                    .frame(egui::Frame::NONE)
                    .lock_focus(true)
                    .desired_width(f32::INFINITY)
                    .desired_rows(40)
                    .layouter(&mut layouter)
                    .show(ui);
                if let Some((a, b)) = doc.goto.take() {
                    use egui::text::{CCursor, CCursorRange};
                    let mut state = out.state;
                    state
                        .cursor
                        .set_char_range(Some(CCursorRange::two(CCursor::new(a), CCursor::new(b))));
                    state.store(ui.ctx(), out.response.id);
                    out.response.request_focus();
                    let rect = out.galley.pos_from_cursor(CCursor::new(a)).translate(out.galley_pos.to_vec2());
                    ui.scroll_to_rect(rect.expand(40.0), Some(egui::Align::Center));
                } else if let Some(r) = out.cursor_range {
                    doc.cursor = r.primary.index.0;
                    doc.anchor = r.secondary.index.0;
                }

                // Ctrl/Cmd + hover underlines the symbol; Ctrl/Cmd + click jumps to it.
                let mods = ui.input(|i| i.modifiers);
                if (mods.command || mods.ctrl)
                    && let Some(pos) = out.response.hover_pos()
                {
                    let idx = out.galley.cursor_from_pos(pos - out.galley_pos).index.0;
                    if let Some((range, _)) = goto::word_at(&doc.text, idx) {
                        use egui::text::CCursor;
                        let a = out
                            .galley
                            .pos_from_cursor(CCursor::new(range.start))
                            .translate(out.galley_pos.to_vec2());
                        let b = out
                            .galley
                            .pos_from_cursor(CCursor::new(range.end))
                            .translate(out.galley_pos.to_vec2());
                        let color = ui.visuals().hyperlink_color;
                        ui.painter()
                            .line_segment([a.left_bottom(), b.left_bottom()], egui::Stroke::new(1.0, color));
                        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                        if out.response.clicked() {
                            goto_request = Some(idx);
                        }
                    }
                }
            });
        });
    goto_request
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
    as_tree: bool,
    collapsed: &HashSet<String>,
) -> Option<ChangeAction> {
    let mut action = None;
    let mut file_row = |ui: &mut egui::Ui, c: &FileChange, depth: usize, show_dir: bool| {
        let buttons: &[(&str, &str)] = if staged {
            &[(ic::MINUS, "Unstage")]
        } else {
            &[
                (ic::PLUS, "Stage"),
                (ic::ARROW_U_UP_LEFT, "Discard changes"),
                (ic::ARROW_SQUARE_OUT, "Open file"),
            ]
        };
        match (change_row(ui, c, buttons, depth, show_dir), staged) {
            (RowAction::Open, _) => action = Some(ChangeAction::OpenDiff(c.clone(), staged)),
            (RowAction::Button(0), true) => action = Some(ChangeAction::Unstage(c.path.clone())),
            (RowAction::Button(0), false) => action = Some(ChangeAction::Stage(c.path.clone())),
            (RowAction::Button(1), false) => action = Some(ChangeAction::Discard(c.clone())),
            (RowAction::Button(2), false) => action = Some(ChangeAction::OpenFile(c.path.clone())),
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
                        if icon_button(ui, icon).on_hover_text(tip).clicked() {
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

fn read_dir_sorted(dir: &Path) -> Vec<(PathBuf, bool)> {
    let mut v: Vec<(PathBuf, bool)> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(Result::ok)
                .filter(|e| e.file_name() != ".git")
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

/// Files under `root` as root-relative paths: git's view (tracked + untracked, minus
/// ignored) inside a repo, otherwise a directory walk that skips common build dirs.
fn project_files(root: &Path, in_repo: bool) -> Vec<String> {
    in_repo
        .then(|| git::list_files(root))
        .flatten()
        .unwrap_or_else(|| walk_files(root, 50_000))
}

fn walk_files(root: &Path, limit: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for (p, is_dir) in read_dir_sorted(&dir) {
            if is_dir {
                let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                if !IGNORED_DIRS.contains(&name.as_str()) {
                    stack.push(p);
                }
            } else if let Ok(rel) = p.strip_prefix(root) {
                out.push(rel.to_string_lossy().replace('\\', "/"));
                if out.len() >= limit {
                    return out;
                }
            }
        }
    }
    out
}

/// Subsequence fuzzy match, scored to favour matches in the file name and contiguous runs.
fn fuzzy_filter(files: &[String], query: &str, limit: usize) -> Vec<String> {
    if query.is_empty() {
        return files.iter().take(limit).cloned().collect();
    }
    let q: Vec<char> = query.to_lowercase().chars().filter(|c| !c.is_whitespace()).collect();
    let mut scored: Vec<(i32, &String)> = files
        .iter()
        .filter_map(|f| {
            let lower = f.to_lowercase();
            let name_start = lower.rfind('/').map(|i| i + 1).unwrap_or(0);
            let mut qi = 0;
            let mut score = 0;
            let mut prev = usize::MAX - 1;
            for (i, ch) in lower.char_indices() {
                if qi < q.len() && ch == q[qi] {
                    score += if i == prev + 1 { 5 } else { 1 };
                    if i >= name_start {
                        score += 3;
                    }
                    prev = i;
                    qi += 1;
                }
            }
            (qi == q.len()).then(|| (score * 100 - f.len() as i32, f))
        })
        .collect();
    scored.sort_by_key(|&(score, _)| std::cmp::Reverse(score));
    scored.into_iter().take(limit).map(|(_, f)| f.clone()).collect()
}
