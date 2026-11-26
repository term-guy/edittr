use super::{App, Tab, background, project_files, receive};
use crate::{clangd, symbol_cache, vcs, workspace};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::time::Duration;

#[derive(Default)]
pub(super) struct CodeNavigation {
    counterpart: Option<Receiver<(u64, Option<PathBuf>)>>,
    symbols: Option<Arc<symbol_cache::Snapshot>>,
    symbols_rx: Option<Receiver<symbol_cache::Snapshot>>,
    cancel: Arc<AtomicBool>,
    changed: HashSet<PathBuf>,
    refresh: bool,
}

impl Drop for CodeNavigation {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

impl App {
    pub(super) fn can_switch_source_header(&self) -> bool {
        matches!(self.tabs.get(self.active), Some(Tab::File(doc)) if clangd::language(&doc.ext()).is_some())
    }

    pub(super) fn switch_source_header(&mut self) {
        let Some(Tab::File(doc)) = self.tabs.get(self.active) else { return };
        let ext = doc.ext();
        if clangd::language(&ext).is_none() {
            return;
        }
        let (revision, path, text) = ((doc.id, doc.version), doc.path.clone(), doc.text.clone());
        let root = self.root.clone();
        let vcs = self.project_vcs();
        let ignored = self.ignored.clone();
        let launch = (!self.path_is_ignored(&path)).then(|| self.launch_server(&ext, false)).flatten();
        self.code_navigation.counterpart = Some(background(&self.ctx, move || {
            let from_server = launch
                .as_ref()
                .and_then(|launch| launch.wait())
                .and_then(|client| client.switch_source_header(&path, &text, revision, Duration::from_secs(2)))
                .filter(|candidate| {
                    candidate != &path
                        && candidate.is_file()
                        && !root
                            .as_deref()
                            .and_then(|root| workspace::relative_path(root, candidate))
                            .is_some_and(|rel| workspace::is_ignored(&rel, &ignored))
                });
            let counterpart = from_server.or_else(|| {
                clangd::counterpart(&path, root.as_deref(), &ignored, || {
                    root.as_ref().map_or_else(Vec::new, |root| project_files(root, vcs, &ignored))
                })
            });
            (revision.0, counterpart)
        }));
    }

    pub(super) fn refresh_symbol_cache(&mut self) {
        self.code_navigation.refresh = self.root.is_some();
    }

    pub(super) fn symbol_cache_changed(&mut self, changes: &crate::watch::Changes, rescan: bool) {
        let Some(root) = &self.root else { return };
        let nav = &mut self.code_navigation;
        let paths = changes.paths.iter().filter(|path| path.starts_with(root));
        let old_len = nav.changed.len();
        nav.changed.extend(paths.cloned());
        nav.refresh |= rescan || changes.vcs || nav.changed.len() != old_len;
    }

    pub(super) fn code_navigation_ignored_changed(&mut self) {
        self.code_navigation.counterpart = None;
        self.refresh_symbol_cache();
        self.refresh_workspace_symbols();
    }

    pub(super) fn symbol_snapshot(&self) -> Option<Arc<symbol_cache::Snapshot>> {
        let nav = &self.code_navigation;
        (!nav.refresh && nav.symbols_rx.is_none()).then(|| nav.symbols.clone()).flatten()
    }

    pub(super) fn poll_code_navigation(&mut self) {
        let nav = &mut self.code_navigation;
        if let Some((id, counterpart)) = receive(&mut nav.counterpart)
            && matches!(self.tabs.get(self.active), Some(Tab::File(doc)) if doc.id == id)
        {
            if let Some(path) = counterpart {
                self.open_at(&path, 0, 0..0);
            } else {
                self.message = "No matching source or header file found".into();
            }
        }

        let project_vcs = self.project_vcs();
        let nav = &mut self.code_navigation;
        let mut updated = false;
        if let Some(snapshot) = receive(&mut nav.symbols_rx) {
            nav.symbols = Some(Arc::new(snapshot));
            updated = !nav.refresh;
        }
        if nav.symbols_rx.is_none()
            && nav.refresh
            && let Some(root) = self.root.clone()
        {
            nav.refresh = false;
            let changed = std::mem::take(&mut nav.changed);
            let previous = nav.symbols.clone();
            let cancel = nav.cancel.clone();
            let vcs = project_vcs;
            let ignored = self.ignored.clone();
            nav.symbols_rx = Some(background(&self.ctx, move || {
                let vcs = vcs.or_else(|| previous.is_none().then(|| vcs::detect(&root)).flatten().map(|(_, vcs)| vcs));
                let files = project_files(&root, vcs, &ignored);
                symbol_cache::Snapshot::refresh(
                    &root,
                    symbol_cache::location(&root).as_deref(),
                    previous.as_deref(),
                    files,
                    &changed,
                    &cancel,
                )
            }));
        }
        if updated {
            self.refresh_workspace_symbols();
        }
    }
}
