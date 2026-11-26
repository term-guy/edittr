use super::{App, Side, Tab, background, receive};
use crate::diagnostics::{self, ByPath};
use crate::lsp::Severity;
use crate::{clangd, icons as ic, lsp, watch::Changes, workspace};
use eframe::egui::{self, RichText};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Weak};
use std::time::Duration;

const SYNC_DEBOUNCE: Duration = Duration::from_millis(400);

struct Started {
    database: Option<clangd::CompilationDatabase>,
    resolved: bool,
    failed: Vec<&'static str>,
    error: Option<String>,
}

#[derive(Default)]
pub(super) struct LanguageServers {
    pub(super) database: clangd::CompilationDatabase,
    launches: HashMap<&'static str, Arc<lsp::Launch>>,
    starting: HashMap<&'static str, Option<Receiver<Started>>>,
    failed: HashSet<&'static str>,
    sync_rx: Option<Receiver<()>>,
    synced: Option<lsp::Revision>,
    pub(super) install_check: Option<(String, Arc<lsp::Launch>)>,
}

impl LanguageServers {
    pub(super) fn clients(&self) -> impl Iterator<Item = &Arc<lsp::Client>> {
        self.launches
            .values()
            .filter_map(|launch| launch.get()?.as_ref())
            .filter(|client| client.is_available())
    }

    pub(super) fn retry_missing(&mut self) {
        self.failed.clear();
        self.launches.retain(|_, launch| !matches!(launch.get(), Some(None)));
        self.synced = None;
    }
}

impl App {
    pub(super) fn reset_lsp(&mut self) {
        let old = std::mem::take(&mut self.lsp);
        for launch in old.launches.values() {
            launch.cancel();
        }
        if !old.launches.is_empty() {
            // Dropping the last client reference can wait for the server process to exit.
            std::thread::spawn(move || {
                for launch in old.launches.values() {
                    launch.retire_if_cancelled();
                }
            });
        }
        if let Some(watcher) = &self.watcher {
            watcher.watch_compilation_database(Vec::new());
        }
        self.problems = Problems::default();
    }

    pub(super) fn reconfigure_ignored_servers(&mut self) {
        let had_clangd = self.lsp.launches.contains_key("clangd");
        self.reset_lsp();
        if had_clangd {
            self.launch_server("cpp", false);
        }
    }

    pub(super) fn compilation_database_changed(&mut self, changes: &Changes, rescan: bool) {
        if !self.lsp.launches.contains_key("clangd") {
            return;
        }
        let root = self.root.as_deref();
        let events: Vec<&PathBuf> = root
            .map(|root| changes.paths.iter().filter(|path| clangd::database_event(root, path)).collect())
            .unwrap_or_default();
        // Makefile edits only matter when the flags were inferred from them; a
        // database (generated or not) stays valid until it is rebuilt.
        let (build_files, others): (Vec<&PathBuf>, Vec<&PathBuf>) = events.into_iter().partition(|path| clangd::is_build_file(path));
        let from_database = matches!(
            self.lsp.database.flags,
            clangd::FlagSource::Database(_) | clangd::FlagSource::Generated { .. }
        );
        let changed = rescan
            || self.lsp.database.paths().any(|path| changes.affects(path))
            || !others.is_empty()
            || !build_files.is_empty() && !from_database;
        if !changed {
            if !build_files.is_empty()
                && let clangd::FlagSource::Generated { stale } = &mut self.lsp.database.flags
            {
                *stale = true;
            }
            return;
        }
        // Keep the previous watch until the next launch publishes its database.
        self.restart_clangd();
    }

    pub(super) fn lsp_has_launch(&self, key: &str) -> bool {
        self.lsp.launches.contains_key(key)
    }

    /// Forget a server's launch so the next request starts a new one.
    pub(super) fn lsp_remove_launch(&mut self, key: &'static str) -> Option<Arc<lsp::Launch>> {
        let launch = self.lsp.launches.remove(key)?;
        if self
            .lsp
            .install_check
            .as_ref()
            .is_some_and(|(_, pending)| Arc::ptr_eq(pending, &launch))
        {
            self.lsp.install_check = None;
        }
        self.lsp.starting.remove(key);
        self.lsp.failed.remove(key);
        self.lsp.synced = None;
        Some(launch)
    }

    pub(super) fn launch_server(&mut self, ext: &str, retry_missing: bool) -> Option<Arc<lsp::Launch>> {
        let specs = lsp::servers_for(ext);
        let key = specs.first()?.name;
        if let Some(launch) = self.lsp.launches.get(key) {
            match launch.get() {
                Some(Some(client)) if !client.is_available() => {
                    self.lsp.failed.insert(client.name);
                    self.message = format!("{} stopped; using text search", client.name);
                    let launch = self.lsp.launches.remove(key).unwrap();
                    // Drop the process handle off the UI thread.
                    std::thread::spawn(move || drop(launch));
                }
                Some(None) if retry_missing => {}
                _ => return Some(launch.clone()),
            }
        }
        let mut root = if key == "clangd" {
            self.root.clone().unwrap_or_else(|| self.work_dir())
        } else {
            self.work_dir()
        };
        let mut ignored = self.ignored.clone();
        if !ignored.is_empty()
            && let Some(project) = &self.root
        {
            // A server may start at the Git root even when a subfolder is open.
            match workspace::relative_path(&root, project) {
                Some(prefix) if !prefix.is_empty() => {
                    for dir in &mut ignored {
                        *dir = format!("{prefix}/{dir}");
                    }
                }
                None => root = project.clone(),
                _ => {}
            }
        }
        let failed = self.lsp.failed.clone();
        let vcs = self.project_vcs();
        let launch = Arc::new(lsp::Launch::default());
        let shared = launch.clone();
        let ctx = self.ctx.clone();
        let rx = background(&self.ctx, move || {
            let mut started = Started {
                database: None,
                resolved: false,
                failed: Vec::new(),
                error: None,
            };
            let repaint: Arc<dyn Fn() + Send + Sync> = Arc::new(move || ctx.request_repaint());
            let client = specs.into_iter().filter(|spec| !failed.contains(spec.name)).find_map(|spec| {
                if shared.is_cancelled() {
                    return None;
                }
                let mut command = lsp::resolve(spec.command)?;
                started.resolved = true;
                if spec.name == "clangd" {
                    let mut database = clangd::CompilationDatabase::find(&root);
                    let settings = workspace::load_c_settings(&root);
                    let files = || super::project_files(&root, vcs, &ignored);
                    let prepared = database.prepare(&root, &ignored, &settings, files, || shared.is_cancelled());
                    started.database = Some(database);
                    match prepared {
                        Ok(prepared) => {
                            if let Some(dir) = prepared.dir {
                                command.args.push(format!("--compile-commands-dir={}", dir.display()));
                            }
                            if let Some(driver) = prepared.query_driver {
                                command.args.push(format!("--query-driver={driver}"));
                            }
                        }
                        Err(error) => {
                            started.error = Some(format!("Could not prepare clangd compile flags: {error}"));
                            return None;
                        }
                    }
                }
                if shared.is_cancelled() {
                    return None;
                }
                match lsp::Client::start(&spec, command, &root, &ignored, Some(repaint.clone())) {
                    Ok(client) => Some(client),
                    Err(error) => {
                        started.failed.push(spec.name);
                        started.error = Some(error);
                        None
                    }
                }
            });
            if client.is_some() {
                started.error = None;
            }
            shared.finish(client);
            started
        });
        self.lsp.starting.insert(key, Some(rx));
        self.lsp.launches.insert(key, launch.clone());
        Some(launch)
    }

    pub(super) fn poll_problems(&mut self) {
        self.lsp.starting.retain(|key, rx| {
            if let Some(Started {
                database,
                resolved,
                failed,
                error,
            }) = receive(rx)
            {
                if resolved
                    && let Some((_, pending)) = &self.lsp.install_check
                    && self.lsp.launches.get(key).is_some_and(|launch| Arc::ptr_eq(pending, launch))
                {
                    self.lsp.install_check = None;
                }
                if let Some(database) = database {
                    if let Some(watcher) = &self.watcher {
                        watcher.watch_compilation_database(database.paths().cloned().collect());
                    }
                    self.lsp.database = database;
                }
                self.lsp.failed.extend(failed);
                if let Some(error) = error {
                    self.message = error;
                }
            }
            rx.is_some()
        });
        if self.lsp.install_check.as_ref().is_some_and(|(_, pending)| {
            pending.get().is_some()
                && !self
                    .lsp
                    .starting
                    .keys()
                    .any(|key| self.lsp.launches.get(key).is_some_and(|launch| Arc::ptr_eq(pending, launch)))
        }) {
            let (ext, launch) = self.lsp.install_check.take().unwrap();
            if matches!(launch.get(), Some(None)) {
                self.offer_install(&ext);
            }
        }
        self.problems.refresh(&self.ctx, &self.lsp);
    }

    pub(super) fn sync_active_document(&mut self) {
        receive(&mut self.lsp.sync_rx);
        let Some(Tab::File(doc)) = self.tabs.get(self.active) else { return };
        let revision = (doc.id, doc.version);
        if self.lsp.synced == Some(revision) || self.lsp.sync_rx.is_some() {
            return;
        }
        let ext = doc.ext();
        let launch = (!self.path_is_ignored(&doc.path))
            .then(|| self.launch_server(&ext, false))
            .flatten();
        let Some(launch) = launch else {
            self.lsp.synced = Some(revision);
            return;
        };
        let client = match launch.get() {
            None => return,                                       // The launch worker will request a frame.
            Some(Some(client)) if client.is_starting() => return, // So will the initialize worker.
            Some(Some(client)) if client.is_available() => client.clone(),
            _ => {
                self.lsp.synced = Some(revision);
                return;
            }
        };
        let Some(Tab::File(doc)) = self.tabs.get(self.active) else { return };
        if let Some(edited) = doc.edited_at
            && let Some(delay) = SYNC_DEBOUNCE.checked_sub(edited.elapsed())
            && !delay.is_zero()
        {
            self.ctx.request_repaint_after(delay);
            return;
        }
        let (path, text) = (doc.path.clone(), doc.text.clone());
        self.lsp.synced = Some(revision);
        self.lsp.sync_rx = Some(background(&self.ctx, move || client.sync_ready(&path, &text, revision)));
    }

    pub(super) fn diagnostics_for(&self, path: &Path) -> &[lsp::Diagnostic] {
        self.problems.snapshot.by_path.get(path).map_or(&[], Vec::as_slice)
    }

    pub(super) fn open_problems(&mut self) {
        self.side = Side::Problems;
        self.side_open = true;
    }
}

#[derive(Default)]
pub(super) struct Problems {
    snapshot: Snapshot,
    requested: HashMap<&'static str, (Weak<lsp::Client>, u64)>,
    rx: Option<Receiver<Snapshot>>,
}

#[derive(Default)]
struct Snapshot {
    by_path: ByPath,
    files: Vec<PathBuf>,
    // (file index, diagnostic index). A missing diagnostic index marks the file heading.
    rows: Vec<(usize, Option<usize>)>,
    errors: usize,
    warnings: usize,
}

impl Snapshot {
    fn collect(clients: Vec<Arc<lsp::Client>>) -> Self {
        let mut snapshot = Self {
            by_path: diagnostics::merge(clients.iter().map(|client| client.diagnostics_snapshot())),
            ..Self::default()
        };
        snapshot.files = snapshot.by_path.keys().cloned().collect();
        snapshot.files.sort_unstable();
        for (file, path) in snapshot.files.iter().enumerate() {
            snapshot.rows.push((file, None));
            for (i, diagnostic) in snapshot.by_path[path].iter().enumerate() {
                snapshot.rows.push((file, Some(i)));
                match diagnostic.severity {
                    Severity::Error => snapshot.errors += 1,
                    Severity::Warning => snapshot.warnings += 1,
                    _ => {}
                }
            }
        }
        snapshot
    }
}

impl Problems {
    fn refresh(&mut self, ctx: &egui::Context, servers: &LanguageServers) {
        if let Some(snapshot) = receive(&mut self.rx) {
            self.snapshot = snapshot;
        }
        if self.rx.is_some() {
            return;
        }
        // A client without publications cannot affect the merged cache.
        let sources = || servers.clients().filter(|client| client.diagnostics_generation() != 0);
        let mut count = 0;
        let unchanged = sources().all(|client| {
            count += 1;
            self.requested.get(client.name).is_some_and(|(previous, generation)| {
                previous.as_ptr() == Arc::as_ptr(client) && *generation == client.diagnostics_generation()
            })
        });
        if unchanged && count == self.requested.len() {
            return;
        }
        self.requested.clear();
        let clients = sources()
            .map(|client| {
                self.requested
                    .insert(client.name, (Arc::downgrade(client), client.diagnostics_generation()));
                client.clone()
            })
            .collect();
        self.rx = Some(background(ctx, move || Snapshot::collect(clients)));
    }

    pub(super) fn badge_count(&self) -> usize {
        self.snapshot.errors + self.snapshot.warnings
    }

    pub(super) fn status_button(&self, ui: &mut egui::Ui) -> egui::Response {
        ui.add(
            egui::Button::new(RichText::new(format!(
                "{} {}  {} {}",
                ic::X_CIRCLE,
                self.snapshot.errors,
                ic::WARNING,
                self.snapshot.warnings
            )))
            .frame(false),
        )
        .on_hover_text(format!(
            "Open Problems ({} errors, {} warnings)",
            self.snapshot.errors, self.snapshot.warnings
        ))
    }
}

impl App {
    pub(super) fn problems_panel(&mut self, ui: &mut egui::Ui) {
        let snapshot = &self.problems.snapshot;
        ui.label(RichText::new("PROBLEMS").small().strong());
        ui.separator();
        if snapshot.rows.is_empty() {
            ui.weak("No problems reported");
            return;
        }
        let mut open = None;
        let row_height = ui.spacing().interact_size.y.max(ui.text_style_height(&egui::TextStyle::Body));
        egui::ScrollArea::vertical()
            .id_salt("problems")
            .auto_shrink([false, false])
            .show_rows(ui, row_height, snapshot.rows.len(), |ui, range| {
                for &(file, diagnostic) in &snapshot.rows[range] {
                    let path = &snapshot.files[file];
                    let Some(i) = diagnostic else {
                        ui.add_sized(
                            [ui.available_width(), row_height],
                            egui::Label::new(RichText::new(format!("{} {}", ic::FILE, self.display_path(path))).strong())
                                .halign(egui::Align::Min)
                                .truncate(),
                        )
                        .on_hover_text(path.display().to_string());
                        continue;
                    };
                    let d = &self.diagnostics_for(path)[i];
                    let (icon, color, severity) = match d.severity {
                        Severity::Error => (ic::X_CIRCLE, ui.visuals().error_fg_color, "Error"),
                        Severity::Warning => (ic::WARNING, ui.visuals().warn_fg_color, "Warning"),
                        Severity::Info => (ic::INFO, ui.visuals().text_color(), "Information"),
                        Severity::Hint => (ic::LIGHTBULB, ui.visuals().weak_text_color(), "Hint"),
                    };
                    let message = d.message.lines().next().unwrap_or_default();
                    let mut label = egui::text::LayoutJob::default();
                    let font_id = egui::TextStyle::Body.resolve(ui.style());
                    label.append(&format!("  {icon}  "), 0.0, egui::TextFormat::simple(font_id.clone(), color));
                    label.append(
                        &format!("{}:{}  {message}", d.line.saturating_add(1), d.col_start.saturating_add(1)),
                        0.0,
                        egui::TextFormat::simple(font_id, ui.visuals().text_color()),
                    );
                    let response = ui
                        .add_sized(
                            [ui.available_width(), row_height],
                            egui::Button::new((label, egui::Atom::grow())).frame_when_inactive(false).truncate(),
                        )
                        .on_hover_text(format!(
                            "{}{}\n{}",
                            severity,
                            d.source.as_ref().map_or_else(String::new, |source| format!(" ({source})")),
                            d.message
                        ));
                    if response.clicked() {
                        let end = if d.end_line == d.line {
                            d.col_end.max(d.col_start)
                        } else {
                            d.col_start
                        };
                        open = Some((path.clone(), d.line, d.col_start..end));
                    }
                }
            });
        if let Some((path, line, cols)) = open {
            self.open_at(&path, line, cols);
        }
    }
}
