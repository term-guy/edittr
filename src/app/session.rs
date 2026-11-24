use super::{App, Confirm, DiskText, Tab, background, read_doc_text, receive, save_recent};
use crate::session::{Debounce, Decision, FileTab, Side, Sidebar, SidebarUi, Startup, Update, Window, Workspace, WorkspaceLayout, Writer};
use crate::workspace::{self, SearchFilters};
use eframe::egui;
use std::borrow::Borrow;
use std::collections::HashSet;
use std::hash::Hash;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

const SEARCH_FILTER_DEBOUNCE: Duration = Duration::from_millis(500);

/// Track folder edits so the per-frame session comparison can skip both sets.
#[derive(Default)]
pub(super) struct FolderSet<T> {
    items: HashSet<T>,
    revision: u64,
}

impl<T> Deref for FolderSet<T> {
    type Target = HashSet<T>;

    fn deref(&self) -> &Self::Target {
        &self.items
    }
}

impl<T: Eq + Hash> FolderSet<T> {
    pub(super) fn insert(&mut self, item: T) -> bool {
        let inserted = self.items.insert(item);
        self.revision += u64::from(inserted);
        inserted
    }

    pub(super) fn remove<Q: Eq + Hash + ?Sized>(&mut self, item: &Q) -> bool
    where
        T: Borrow<Q>,
    {
        let removed = self.items.remove(item);
        self.revision += u64::from(removed);
        removed
    }
}

#[derive(Default, PartialEq)]
struct WorkspaceSnapshot {
    layout: WorkspaceLayout,
    expanded_revision: u64,
    collapsed_revision: u64,
    search_filters_shown: bool,
    search_replace_shown: bool,
}

pub(super) struct Sessions {
    writer: Option<Writer>,
    loading: Option<Receiver<LoadedWorkspace>>,
    workspace: Debounce<WorkspaceSnapshot>,
    saved_filters: SearchFilters,
    /// The next restore is "close folder": show the empty start page.
    start_page: bool,
    pending_filters: Option<(SearchFilters, Instant)>,
    window: Window,
    saved_window: Debounce<Window>,
    width: f32,
    reset_panel_width: bool,
}

struct LoadedWorkspace {
    root: Option<PathBuf>,
    ignored: Vec<String>,
    search_filters: SearchFilters,
    sidebar: Sidebar,
    expanded: HashSet<PathBuf>,
    changes_collapsed: HashSet<String>,
    search_filters_shown: Option<bool>,
    search_replace_shown: bool,
    tabs: Vec<(FileTab, DiskText)>,
    active: Option<PathBuf>,
    error: Option<String>,
}

impl LoadedWorkspace {
    fn load(root: Option<PathBuf>, file: Option<PathBuf>) -> Self {
        let Workspace { layout, sidebar_ui } = Workspace::load(root.as_deref());
        let expanded = root.as_deref().map(|root| sidebar_ui.restore_expanded(root)).unwrap_or_default();
        let mut active = layout.active.map(|p| workspace::plain_path(Path::new(&p)));
        let mut tabs: Vec<_> = layout
            .tabs
            .into_iter()
            .filter_map(|mut tab| {
                let path = Path::new(&tab.path);
                if !path.is_file() {
                    return None;
                }
                let text = read_doc_text(path).ok().flatten()?;
                if tab.cursor != 0 || tab.anchor != 0 {
                    let len = text.text.chars().count();
                    tab.cursor = tab.cursor.min(len);
                    tab.anchor = tab.anchor.min(len);
                }
                Some((tab, text))
            })
            .collect();
        let mut error = None;
        if let Some(path) = file {
            if tabs.iter().any(|(tab, _)| tab.path == path.as_os_str()) {
                active = Some(path);
            } else {
                match read_doc_text(&path) {
                    Ok(Some(text)) => {
                        tabs.push((
                            FileTab {
                                path: path.as_os_str().to_owned(),
                                cursor: 0,
                                anchor: 0,
                            },
                            text,
                        ));
                        active = Some(path);
                    }
                    Ok(None) => error = Some(format!("{} looks binary; not opening", path.display())),
                    Err(err) => error = Some(format!("{}: {err}", path.display())),
                }
            }
        }
        let ignored = root.as_deref().map(workspace::load_ignored).unwrap_or_default();
        let search_filters = root.as_deref().map(workspace::load_search_filters).unwrap_or_default();
        Self {
            root,
            ignored,
            search_filters,
            sidebar: layout.sidebar,
            expanded,
            changes_collapsed: sidebar_ui.changes_collapsed,
            search_filters_shown: sidebar_ui.search_filters_shown,
            search_replace_shown: sidebar_ui.search_replace_shown,
            tabs,
            active,
            error,
        }
    }
}

impl Sessions {
    pub(super) fn new(writer: Option<Writer>, window: Window) -> Self {
        Self {
            writer,
            loading: None,
            workspace: Debounce::new(Some(WorkspaceSnapshot::default())),
            saved_filters: SearchFilters::default(),
            start_page: false,
            pending_filters: None,
            saved_window: Debounce::new(Some(window.clone())),
            window,
            width: Sidebar::default().width,
            reset_panel_width: false,
        }
    }
}

impl App {
    pub(super) fn reset_sidebar_ui(&mut self) {
        self.expanded = FolderSet::default();
        self.changes_collapsed = FolderSet::default();
        self.changes_rows = None;
        self.search_replace.shown = false;
    }

    pub(super) fn start_session(&mut self, startup: Startup) {
        if !crate::session::enabled() {
            if let Some(root) = startup.root {
                self.set_root(root);
            }
            if let Some(file) = startup.file {
                self.open_file(&file);
            }
        } else if startup.restore {
            self.restore_workspace(startup.root, startup.file);
        } else {
            self.sessions.window.root = None;
        }
    }

    pub(super) fn set_root(&mut self, root: PathBuf) {
        if self.root.as_ref() == Some(&root) {
            return;
        }
        if !crate::session::enabled() {
            let ignored = workspace::load_ignored(&root);
            let search_filters = workspace::load_search_filters(&root);
            self.activate_root(root, ignored);
            self.apply_search_filters(search_filters);
        } else if self.tabs.iter().any(|tab| matches!(tab, Tab::File(doc) if doc.dirty())) {
            self.confirm = Some(Confirm::SwitchWorkspace(root));
        } else {
            self.restore_workspace(Some(root), None);
        }
    }

    /// Close the folder (and its tabs, which its session keeps) and show the start page.
    pub(super) fn close_folder(&mut self, force: bool) {
        if self.root.is_none() {
            return;
        }
        if !force && self.tabs.iter().any(|tab| matches!(tab, Tab::File(doc) if doc.dirty())) {
            self.confirm = Some(Confirm::CloseFolder);
            return;
        }
        if crate::session::enabled() {
            self.sessions.start_page = true;
            self.restore_workspace(None, None);
        } else {
            self.tabs.clear();
            self.active = 0;
            self.clear_project();
        }
    }

    pub(super) fn restore_workspace(&mut self, root: Option<PathBuf>, file: Option<PathBuf>) {
        // Queue the outgoing snapshot before reading the incoming one. The barrier
        // also covers a quick return to a workspace whose last save is still queued.
        let now = Instant::now();
        self.save_workspace_session(now, true);
        self.save_search_filters(now, true);
        let pending = self.sessions.writer.as_ref().map(Writer::after_pending_writes);
        let recent = root.as_ref().map(|root| {
            self.recent.retain(|r| r != root);
            self.recent.insert(0, root.clone());
            self.recent.truncate(10);
            self.recent.clone()
        });
        self.sessions.loading = Some(background(&self.ctx, move || {
            if let Some(pending) = pending {
                let _ = pending.recv();
            }
            if let Some(recent) = recent {
                save_recent(&recent);
            }
            LoadedWorkspace::load(root, file)
        }));
    }

    pub(super) fn poll_session_restore(&mut self) -> bool {
        if let Some(mut loaded) = receive(&mut self.sessions.loading) {
            if std::mem::take(&mut self.sessions.start_page) {
                // Closing a folder lands on an empty start page, not the no-folder session's files.
                loaded.tabs.clear();
                loaded.active = None;
            }
            self.tabs.clear();
            self.active = 0;
            if let Some(root) = loaded.root {
                self.activate_root(root, loaded.ignored);
            } else {
                self.clear_project();
            }
            self.apply_search_filters(loaded.search_filters);
            // Install the tree state before the sidebar can draw for this workspace.
            self.expanded.items = loaded.expanded;
            self.changes_collapsed.items = loaded.changes_collapsed;
            if let Some(shown) = loaded.search_filters_shown {
                self.search.show_filters = shown;
            }
            self.search_replace.shown = loaded.search_replace_shown;
            self.side = loaded.sidebar.side;
            self.side_open = loaded.sidebar.open;
            self.sessions.width = loaded.sidebar.width.max(160.0);
            self.sessions.reset_panel_width = true;
            self.sessions.workspace = Debounce::new(None);
            self.sessions.window.root = self.root.as_ref().map(|path| path.as_os_str().to_owned());
            for (tab, text) in loaded.tabs {
                // Sessions saved before paths were normalized may hold both spellings.
                let path = workspace::plain_path(Path::new(&tab.path));
                if self.tabs.iter().any(|t| matches!(t, Tab::File(d) if d.path == path)) {
                    continue;
                }
                let index = self.tabs.len();
                self.add_file_tab(&path, text.text, text.crlf);
                if let Tab::File(doc) = &mut self.tabs[index] {
                    doc.cursor = tab.cursor;
                    doc.anchor = tab.anchor;
                    // TODO(editor rewrite): expose pub scroll_y: f32 on Doc and have
                    // the editor honor it so sessions can restore the exact offset.
                    // Doc::goto passes the anchor first to egui's CCursorRange::two.
                    doc.goto = Some((tab.anchor, tab.cursor));
                }
                if loaded.active.as_ref() == Some(&path) {
                    self.active = index;
                }
            }
            // The saved Explorer layout wins on reopen: the restored active file counts
            // as revealed, so its folders aren't expanded until the user switches files.
            self.explorer_revealed = match self.tabs.get(self.active) {
                Some(Tab::File(doc)) => Some(doc.path.clone()),
                _ => None,
            };
            if let Some(error) = loaded.error {
                self.message = error;
            }
        }
        self.sessions.loading.is_some()
    }

    pub(super) fn show_session_sidebar(&mut self, ui: &mut egui::Ui) {
        if !self.side_open {
            return;
        }
        let id = ui.make_persistent_id("side");
        if std::mem::take(&mut self.sessions.reset_panel_width) {
            ui.ctx().data_mut(|data| data.remove::<egui::PanelState>(id));
        }
        egui::Panel::left("side")
            .resizable(true)
            .default_size(self.sessions.width)
            .min_size(160.0)
            .show(ui, |ui| match self.side {
                Side::Explorer => self.explorer(ui),
                Side::Changes => self.changes(ui),
                Side::Search => self.search_panel(ui),
                Side::Problems => self.problems_panel(ui),
            });
        if let Some(panel) = egui::PanelState::load(ui.ctx(), id) {
            self.sessions.width = panel.size().x;
        }
    }

    pub(super) fn record_session(&mut self, flush: bool) {
        if self.sessions.writer.is_none() {
            return;
        }
        let now = Instant::now();
        self.sessions.window.geometry.capture(&self.ctx);
        self.save_workspace_session(now, flush);
        self.save_search_filters(now, flush);
        let writer = self.sessions.writer.as_ref().unwrap();
        match self.sessions.saved_window.update(self.sessions.window.clone(), now, flush) {
            Decision::Unchanged => {}
            Decision::Wait(delay) => self.ctx.request_repaint_after(delay),
            Decision::Write(window) => writer.save(Update::Window(window.clone()), &self.ctx),
        }
        if let Some(error) = writer.errors.try_iter().last() {
            self.message = format!("Could not save session: {error}");
        }
    }

    fn apply_search_filters(&mut self, filters: SearchFilters) {
        self.search.opts.include.clone_from(&filters.include);
        self.search.opts.exclude.clone_from(&filters.exclude);
        self.search.show_filters = !filters.include.is_empty() || !filters.exclude.is_empty();
        self.sessions.saved_filters = filters;
        self.sessions.pending_filters = None;
    }

    fn save_search_filters(&mut self, now: Instant, flush: bool) {
        let (Some(writer), Some(root)) = (&self.sessions.writer, &self.root) else {
            return;
        };
        let opts = &self.search.opts;
        let matches = |filters: &SearchFilters| filters.include == opts.include && filters.exclude == opts.exclude;
        if matches(&self.sessions.saved_filters) {
            self.sessions.pending_filters = None;
            return;
        }
        let pending = &mut self.sessions.pending_filters;
        if pending.as_ref().is_none_or(|(filters, _)| !matches(filters)) {
            *pending = Some((
                SearchFilters {
                    include: opts.include.clone(),
                    exclude: opts.exclude.clone(),
                },
                now,
            ));
        }
        let elapsed = now.duration_since(pending.as_ref().unwrap().1);
        if !flush && elapsed < SEARCH_FILTER_DEBOUNCE {
            self.ctx.request_repaint_after(SEARCH_FILTER_DEBOUNCE - elapsed);
            return;
        }
        let (filters, _) = pending.take().unwrap();
        writer.save(Update::SearchFilters(root.clone(), filters.clone()), &self.ctx);
        self.sessions.saved_filters = filters;
    }

    fn save_workspace_session(&mut self, now: Instant, flush: bool) {
        let Some(writer) = &self.sessions.writer else { return };
        if self.sessions.loading.is_some() {
            return;
        }
        let layout = WorkspaceLayout {
            tabs: self
                .tabs
                .iter()
                .filter_map(|tab| match tab {
                    Tab::File(doc) => {
                        let (anchor, cursor) = doc.goto.unwrap_or((doc.anchor, doc.cursor));
                        Some(FileTab {
                            path: doc.path.as_os_str().to_owned(),
                            cursor,
                            anchor,
                        })
                    }
                    _ => None,
                })
                .collect(),
            active: match self.tabs.get(self.active) {
                Some(Tab::File(doc)) => Some(doc.path.as_os_str().to_owned()),
                _ => None,
            },
            sidebar: Sidebar {
                side: self.side,
                open: self.side_open,
                width: self.sessions.width,
            },
        };
        let snapshot = WorkspaceSnapshot {
            layout,
            expanded_revision: self.expanded.revision,
            collapsed_revision: self.changes_collapsed.revision,
            search_filters_shown: self.search.show_filters,
            search_replace_shown: self.search_replace.shown,
        };
        match self.sessions.workspace.update(snapshot, now, flush) {
            Decision::Unchanged => {}
            Decision::Wait(delay) => self.ctx.request_repaint_after(delay),
            Decision::Write(snapshot) => {
                let sidebar_ui = SidebarUi {
                    explorer_expanded: self
                        .root
                        .as_deref()
                        .map(|root| SidebarUi::capture_expanded(root, &self.expanded))
                        .unwrap_or_default(),
                    changes_collapsed: self.changes_collapsed.items.clone(),
                    search_filters_shown: Some(snapshot.search_filters_shown),
                    search_replace_shown: snapshot.search_replace_shown,
                };
                let workspace = Workspace {
                    layout: snapshot.layout.clone(),
                    sidebar_ui,
                };
                writer.save(Update::Workspace(self.root.clone(), workspace), &self.ctx);
            }
        }
    }
}
