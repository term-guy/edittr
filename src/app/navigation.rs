use super::{App, Side, Tab, VcsOperation, background, fuzzy_filter, project_files, receive};
use crate::commands::{self, COMMANDS, Command};
use crate::{lsp, symbols, workspace};
use eframe::egui::{self, Key, Modifiers};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

const DEBOUNCE: Duration = Duration::from_millis(150);

#[derive(Clone, Copy)]
enum Mode {
    Commands,
    Document { id: u64, version: u64 },
    Workspace,
}

impl Mode {
    fn title(self) -> &'static str {
        match self {
            Self::Commands => commands::entry(Command::Palette).title,
            Self::Document { .. } => commands::entry(Command::DocumentSymbols).title,
            Self::Workspace => commands::entry(Command::WorkspaceSymbols).title,
        }
    }
}

enum Action {
    Command(Command),
    Symbol(symbols::Symbol),
}

struct Row {
    title: String,
    detail: String,
    action: Action,
}

impl Row {
    fn symbol(symbol: symbols::Symbol, root: Option<&Path>) -> Self {
        let title = match &symbol.container {
            Some(container) => format!("{} ({container})", symbol.name),
            None => symbol.name.clone(),
        };
        let line = symbol.line.saturating_add(1);
        let detail = match root.and_then(|root| workspace::relative_path(root, &symbol.path)) {
            Some(path) => format!("{}  {path}:{line}", symbol.kind),
            None => format!("{}  Line {line}", symbol.kind),
        };
        Self {
            title,
            detail,
            action: Action::Symbol(symbol),
        }
    }
}

pub(super) struct Picker {
    mode: Mode,
    previous_focus: Option<egui::Id>,
    query: String,
    rows: Vec<Row>,
    lower: Vec<String>,
    matches: Vec<usize>,
    selected: usize,
    reveal_selected: bool,
    deadline: Option<Instant>,
    rx: Option<Receiver<Vec<Row>>>,
    cancel: Arc<AtomicBool>,
}

#[cfg(feature = "tour")]
impl Picker {
    pub(super) fn set_query_for_tour(&mut self, query: &str) {
        self.query = query.into();
        self.rank();
    }
}

impl Drop for Picker {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

impl Picker {
    fn new(mode: Mode) -> Self {
        Self {
            mode,
            previous_focus: None,
            query: String::new(),
            rows: Vec::new(),
            lower: Vec::new(),
            matches: Vec::new(),
            selected: 0,
            reveal_selected: false,
            deadline: None,
            rx: None,
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }

    fn set_rows(&mut self, rows: Vec<Row>) {
        if !matches!(self.mode, Mode::Workspace) {
            self.lower = rows.iter().map(|row| row.title.to_lowercase()).collect();
        }
        self.rows = rows;
        self.rank();
    }

    fn rank(&mut self) {
        self.selected = 0;
        self.reveal_selected = true;
        self.matches = if matches!(self.mode, Mode::Workspace) {
            (0..self.rows.len()).collect()
        } else {
            fuzzy_filter(&self.lower, &self.query, symbols::LIMIT)
        };
    }

    fn schedule_workspace_search(&mut self, ctx: &egui::Context) {
        self.cancel.store(true, Ordering::Relaxed);
        self.cancel = Arc::new(AtomicBool::new(false));
        self.rx = None;
        self.set_rows(Vec::new());
        self.deadline = None;
        if !self.query.trim().is_empty() {
            self.deadline = Some(Instant::now() + DEBOUNCE);
            ctx.request_repaint_after(DEBOUNCE);
        }
    }

    fn show(&mut self, ctx: &egui::Context) -> (Option<usize>, bool) {
        let id = egui::Id::unique("navigation_picker");
        let area = egui::Modal::default_area(id).anchor(egui::Align2::CENTER_TOP, [0.0, 60.0]);
        let response = egui::Modal::new(id).area(area).show(ctx, |ui| {
            let width = (ctx.content_rect().width() - 32.0).clamp(160.0, 520.0);
            ui.set_width(width);
            let (up, down, enter) = ui.input_mut(|input| {
                (
                    input.consume_key(Modifiers::NONE, Key::ArrowUp),
                    input.consume_key(Modifiers::NONE, Key::ArrowDown),
                    input.consume_key(Modifiers::NONE, Key::Enter),
                )
            });
            let input = ui.add(
                egui::TextEdit::singleline(&mut self.query)
                    .id_salt(self.mode.title())
                    .hint_text(self.mode.title())
                    .desired_width(f32::INFINITY),
            );
            input.request_focus();
            if input.changed() {
                if matches!(self.mode, Mode::Workspace) {
                    self.schedule_workspace_search(ctx);
                } else {
                    self.rank();
                }
            }
            if self.matches.is_empty() {
                let status = if self.deadline.is_some() || self.rx.is_some() {
                    "Finding symbols…"
                } else if matches!(self.mode, Mode::Commands) {
                    "No matching commands"
                } else if matches!(self.mode, Mode::Workspace) && self.query.trim().is_empty() {
                    "Type a symbol name to search the workspace"
                } else {
                    "No matching symbols"
                };
                ui.weak(status);
                return None;
            }

            if up {
                self.selected = self.selected.saturating_sub(1);
            }
            if down {
                self.selected = (self.selected + 1).min(self.matches.len() - 1);
            }
            let mut chosen = enter.then(|| self.matches[self.selected]);
            let row_height = ui.spacing().interact_size.y;
            let step = row_height + ui.spacing().item_spacing.y;
            let reveal = std::mem::take(&mut self.reveal_selected) || up || down;
            egui::ScrollArea::vertical()
                .max_height(320.0)
                .auto_shrink([false, false])
                .show_rows(ui, row_height, self.matches.len(), |ui, range| {
                    if reveal {
                        let y = ui.max_rect().top() + (self.selected as f32 - range.start as f32) * step;
                        let rect = egui::Rect::from_min_size(egui::pos2(ui.min_rect().left(), y), egui::vec2(1.0, row_height));
                        ui.scroll_to_rect_animation(rect, None, egui::style::ScrollAnimation::none());
                    }
                    for i in range {
                        let index = self.matches[i];
                        let row = &self.rows[index];
                        let button = egui::Button::selectable(i == self.selected, &row.title)
                            .shortcut_text(&row.detail)
                            .wrap_mode(egui::TextWrapMode::Truncate);
                        if ui.add_sized([ui.available_width(), row_height], button).clicked() {
                            chosen = Some(index);
                        }
                    }
                });
            chosen
        });
        (response.inner, response.should_close())
    }
}

impl App {
    pub(super) fn shortcuts(&mut self, ctx: &egui::Context) {
        if self.search_replace.confirming() {
            return;
        }
        // The palette binding precedes Go to file because egui accepts extra Shift modifiers.
        for command in COMMANDS.iter().filter(|c| c.id != Command::Commit) {
            if ctx.input_mut(|input| command.consume(input)) {
                self.run_command(command.id);
            }
        }
    }

    pub(super) fn run_command(&mut self, cmd: Command) {
        match cmd {
            Command::Palette | Command::DocumentSymbols | Command::WorkspaceSymbols => self.start_picker(cmd),
            Command::OpenFolder => self.request_open_folder(),
            Command::QuickOpen => self.start_quick_open(),
            Command::Find => self.show_find(false),
            Command::Replace => self.show_find(true),
            Command::FindInFiles => {
                let selection = match self.tabs.get(self.active) {
                    Some(Tab::File(doc)) => doc.selected_text(),
                    _ => None,
                };
                self.show_search();
                if let Some(text) = selection {
                    self.search.opts.query = if self.search.opts.regex { regex::escape(&text) } else { text };
                    self.start_search(&self.ctx.clone());
                }
            }
            Command::GoToDefinition => {
                if let Some(Tab::File(doc)) = self.tabs.get(self.active) {
                    self.go_to_definition(doc.cursor, &self.ctx.clone());
                }
            }
            Command::GoBack => self.go_back(),
            Command::Explorer => {
                self.side = Side::Explorer;
                self.side_open = true;
            }
            Command::ToggleSidebar => self.side_open = !self.side_open,
            Command::SourceControl => {
                self.side = Side::Changes;
                self.side_open = true;
            }
            Command::History => {
                if self.repository().is_some() {
                    self.open_graph();
                } else {
                    self.message = "No repository history available".into();
                }
            }
            Command::Refresh => {
                self.invalidate_tree(|_| true);
                self.refresh_vcs();
                self.refresh_history();
            }
            Command::Commit => {
                if self.can_commit() {
                    let draft = self.commit_msg.clone();
                    let amend = self.vcs.is_some_and(|vcs| vcs.has_index()) && self.amend;
                    let message = draft.trim().to_string();
                    self.spawn_vcs(VcsOperation::Commit(draft, amend), move |vcs, repo| {
                        vcs.commit(repo, &message, amend)
                    });
                }
            }
            Command::Save => self.save_active(),
            Command::CloseTab => self.close_tab(self.active, false),
            Command::CloseAllTabs => self.close_tabs((0..self.tabs.len()).collect(), false),
            Command::ToggleTheme => self.ctx.set_theme(match self.ctx.theme() {
                egui::Theme::Dark => egui::Theme::Light,
                egui::Theme::Light => egui::Theme::Dark,
            }),
            Command::ToggleSetting(setting) => {
                let value = setting.value(&mut self.settings);
                *value = !*value;
                self.save_settings(&self.ctx.clone());
            }
        }
    }

    pub(super) fn can_commit(&self) -> bool {
        self.repo.is_some()
            && self.repo_rx.is_none()
            && self.bg_task.is_none()
            && !self.commit_msg.trim().is_empty()
            && self
                .vcs
                .is_some_and(|vcs| !vcs.has_index() || !self.status.staged.is_empty() || self.amend)
    }

    fn start_picker(&mut self, cmd: Command) {
        if self.confirm.is_some() || self.install_prompt.is_some() || self.open_folder.is_some() {
            return;
        }
        let mut picker = match cmd {
            Command::Palette => {
                let mut picker = Picker::new(Mode::Commands);
                picker.set_rows(
                    COMMANDS
                        .iter()
                        .map(|entry| Row {
                            title: entry.title.to_owned(),
                            detail: entry.shortcut_text(&self.ctx),
                            action: Action::Command(entry.id),
                        })
                        .collect(),
                );
                picker
            }
            Command::DocumentSymbols => {
                let Some(Tab::File(doc)) = self.tabs.get(self.active) else {
                    self.message = "Open a file to go to a symbol in it".into();
                    return;
                };
                let mut picker = Picker::new(Mode::Document {
                    id: doc.id,
                    version: doc.version,
                });
                let (path, text, ext, revision) = (doc.path.clone(), doc.text.clone(), doc.ext(), (doc.id, doc.version));
                let names: Vec<&str> = lsp::servers_for(&ext).iter().map(|spec| spec.name).collect();
                let client = (!self.path_is_ignored(&path))
                    .then(|| self.lsp.clients().find(|client| names.contains(&client.name)).cloned())
                    .flatten();
                picker.rx = Some(background(&self.ctx, move || {
                    symbols::document(&path, &text, revision, client)
                        .into_iter()
                        .map(|symbol| Row::symbol(symbol, None))
                        .collect()
                }));
                picker
            }
            Command::WorkspaceSymbols => {
                if self.root.is_none() {
                    self.message = "Open a folder to go to a workspace symbol".into();
                    return;
                }
                Picker::new(Mode::Workspace)
            }
            _ => return,
        };
        picker.previous_focus = match &self.picker {
            Some(previous) => previous.previous_focus,
            None => self.ctx.memory(|memory| memory.focused()),
        };
        self.quick_open = None;
        self.picker = Some(picker);
        self.ctx.request_repaint();
    }

    pub(super) fn poll_picker(&mut self) {
        let Some(picker) = &mut self.picker else { return };
        if let Some(rows) = receive(&mut picker.rx) {
            picker.set_rows(rows);
        }
        let Some(deadline) = picker.deadline else { return };
        if let Some(delay) = deadline.checked_duration_since(Instant::now()) {
            self.ctx.request_repaint_after(delay);
            return;
        }
        picker.deadline = None;
        let Some(root) = self.root.clone() else { return };
        let mut clients: Vec<_> = self.lsp.clients().cloned().collect();
        clients.sort_by_key(|client| client.name);
        let (query, cancel, vcs, ignored) = (
            picker.query.trim().to_owned(),
            picker.cancel.clone(),
            self.vcs,
            self.ignored.clone(),
        );
        picker.rx = Some(background(&self.ctx, move || {
            symbols::project(&root, &ignored, clients, &query, &cancel, || project_files(&root, vcs, &ignored))
                .into_iter()
                .map(|symbol| Row::symbol(symbol, Some(&root)))
                .collect()
        }));
    }

    pub(super) fn picker_dialog(&mut self, ctx: &egui::Context) {
        let Some(picker) = &mut self.picker else { return };
        let (chosen, close) = picker.show(ctx);
        if !close && chosen.is_none() {
            return;
        }
        let mut picker = self.picker.take().unwrap();
        if let Some(focus) = picker.previous_focus {
            ctx.memory_mut(|memory| memory.request_focus(focus));
        }
        if !close && let Some(index) = chosen {
            match picker.rows.swap_remove(index).action {
                Action::Command(cmd) => self.run_command(cmd),
                Action::Symbol(symbol) => self.reveal_symbol(picker.mode, symbol),
            }
        }
    }

    fn reveal_symbol(&mut self, mode: Mode, symbol: symbols::Symbol) {
        if let Mode::Document { id, version } = mode {
            if let Some(Tab::File(doc)) = self.tabs.get_mut(self.active)
                && doc.id == id
                && doc.version == version
            {
                self.nav_back.push((doc.path.clone(), doc.cursor));
                doc.goto = Some(symbols::selection(&doc.text, symbol.line, symbol.cols));
            } else {
                self.message = "File changed while finding symbols".into();
            }
        } else {
            let from = match self.tabs.get(self.active) {
                Some(Tab::File(doc)) => Some((doc.path.clone(), doc.cursor)),
                _ => None,
            };
            self.open_at(&symbol.path, symbol.line, symbol.cols);
            if let Some(from) = from
                && matches!(self.tabs.get(self.active), Some(Tab::File(doc)) if doc.path == symbol.path)
            {
                self.nav_back.push(from);
            }
        }
    }
}
