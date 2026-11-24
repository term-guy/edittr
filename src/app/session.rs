use super::{App, Confirm, Tab, background, read_doc_text, receive, save_recent};
use crate::session::{Debounce, Decision, FileTab, Side, Sidebar, Startup, Update, Window, Workspace, Writer};
use crate::workspace;
use eframe::egui;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;
use std::time::Instant;

pub(super) struct Sessions {
    writer: Option<Writer>,
    loading: Option<Receiver<LoadedWorkspace>>,
    workspace: Debounce<Workspace>,
    window: Window,
    saved_window: Debounce<Window>,
    width: f32,
    reset_panel_width: bool,
}

struct LoadedWorkspace {
    root: Option<PathBuf>,
    ignored: Vec<String>,
    sidebar: Sidebar,
    tabs: Vec<(FileTab, String)>,
    active: Option<PathBuf>,
    error: Option<String>,
}

impl LoadedWorkspace {
    fn load(root: Option<PathBuf>, file: Option<PathBuf>) -> Self {
        let snapshot = Workspace::load(root.as_deref());
        let mut active = snapshot.active.map(PathBuf::from);
        let mut tabs: Vec<_> = snapshot
            .tabs
            .into_iter()
            .filter_map(|mut tab| {
                let path = Path::new(&tab.path);
                if !path.is_file() {
                    return None;
                }
                let text = read_doc_text(path).ok().flatten()?;
                if tab.cursor != 0 || tab.anchor != 0 {
                    let len = text.chars().count();
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
        Self {
            root,
            ignored,
            sidebar: snapshot.sidebar,
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
            workspace: Debounce::new(Some(Workspace::default())),
            saved_window: Debounce::new(Some(window.clone())),
            window,
            width: Sidebar::default().width,
            reset_panel_width: false,
        }
    }
}

impl App {
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
            self.activate_root(root, ignored);
        } else if self.tabs.iter().any(|tab| matches!(tab, Tab::File(doc) if doc.dirty())) {
            self.confirm = Some(Confirm::SwitchWorkspace(root));
        } else {
            self.restore_workspace(Some(root), None);
        }
    }

    pub(super) fn restore_workspace(&mut self, root: Option<PathBuf>, file: Option<PathBuf>) {
        // Queue the outgoing snapshot before reading the incoming one. The barrier
        // also covers a quick return to a workspace whose last save is still queued.
        self.save_workspace_session(Instant::now(), true);
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
        if let Some(loaded) = receive(&mut self.sessions.loading) {
            self.tabs.clear();
            self.active = 0;
            if let Some(root) = loaded.root {
                self.activate_root(root, loaded.ignored);
            }
            self.side = loaded.sidebar.side;
            self.side_open = loaded.sidebar.open;
            self.sessions.width = loaded.sidebar.width.max(160.0);
            self.sessions.reset_panel_width = true;
            self.sessions.workspace = Debounce::new(None);
            self.sessions.window.root = self.root.as_ref().map(|path| path.as_os_str().to_owned());
            for (tab, text) in loaded.tabs {
                let path = PathBuf::from(tab.path);
                let index = self.tabs.len();
                self.add_file_tab(&path, text);
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

    fn save_workspace_session(&mut self, now: Instant, flush: bool) {
        let Some(writer) = &self.sessions.writer else { return };
        if self.sessions.loading.is_some() {
            return;
        }
        let snapshot = Workspace {
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
        match self.sessions.workspace.update(snapshot, now, flush) {
            Decision::Unchanged => {}
            Decision::Wait(delay) => self.ctx.request_repaint_after(delay),
            Decision::Write(snapshot) => writer.save(Update::Workspace(self.root.clone(), snapshot.clone()), &self.ctx),
        }
    }
}
