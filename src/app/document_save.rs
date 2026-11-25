//! Asynchronous formatting and saving, coordinated against the live buffer version.
use super::{App, Doc, Tab, Vcs, background, receive};
use crate::{editorconfig, formatter, watch::Changes};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;
use std::time::Instant;

#[derive(Default)]
pub(super) struct DocumentSave {
    config: editorconfig::Cache,
    jobs: HashMap<PathBuf, Job>,
}

#[derive(Clone, Copy)]
enum Action {
    Save { format: bool },
    Format,
}

#[derive(Clone, Copy)]
struct Request {
    id: u64,
    version: u64,
    action: Action,
}

struct Job {
    request: Request,
    pending: Option<Request>,
    phase: Phase,
}

enum Phase {
    Prepare(Option<Receiver<Prepared>>),
    Write {
        rx: Option<Receiver<std::io::Result<()>>>,
        text: String,
        warning: Option<String>,
    },
}

struct Prepared {
    text: Option<String>,
    warning: Option<String>,
    crlf: bool,
}

impl App {
    pub(super) fn document_action_pending(&self, matches: impl Fn(&Path) -> bool) -> bool {
        self.document_save.jobs.keys().any(|path| matches(path))
    }

    pub(super) fn save_active(&mut self) {
        // In an editable diff, save the file being edited there.
        let index = self.live_diff_doc(self.active).unwrap_or(self.active);
        self.save_doc(index);
    }

    /// Save tab `index`; an Untitled tab asks for a file name first.
    pub(super) fn save_doc(&mut self, index: usize) {
        if matches!(self.tabs.get(index), Some(Tab::File(doc)) if doc.untitled) {
            self.request_save_as(index);
            return;
        }
        self.request_document_action(
            index,
            Action::Save {
                format: self.settings.format_on_save,
            },
        );
    }

    pub(super) fn format_document(&mut self, index: usize) {
        self.request_document_action(index, Action::Format);
    }

    fn request_document_action(&mut self, index: usize, action: Action) {
        let Some(Tab::File(doc)) = self.tabs.get(index) else { return };
        let request = Request {
            id: doc.id,
            version: doc.version,
            action,
        };
        let path = doc.path.clone();
        if let Some(job) = self.document_save.jobs.get_mut(&path) {
            if matches!(action, Action::Save { .. }) || job.pending.is_none() {
                job.pending = Some(request);
            }
        } else {
            self.start_document_action(path, request);
        }
    }

    fn start_document_action(&mut self, path: PathBuf, request: Request) {
        let Some(doc) = self.document_mut(request.id).filter(|doc| doc.version == request.version) else {
            return;
        };
        let (text, original_crlf) = (doc.text.clone(), doc.crlf);
        let (config, file) = (self.document_save.config.clone(), path.clone());
        let rx = background(&self.ctx, move || prepare(&file, text, original_crlf, request.action, &config));
        self.message = match request.action {
            Action::Save { .. } => format!("Saving {}…", path.display()),
            Action::Format => format!("Formatting {}…", path.display()),
        };
        self.document_save.jobs.insert(
            path,
            Job {
                request,
                pending: None,
                phase: Phase::Prepare(Some(rx)),
            },
        );
    }

    pub(super) fn poll_document_saves(&mut self) {
        if self.document_save.jobs.is_empty() {
            return;
        }
        // Each path has at most one worker writing it, even if its tab is closed and reopened.
        let mut jobs = std::mem::take(&mut self.document_save.jobs);
        let mut queued = Vec::new();
        jobs.retain(|path, job| {
            if self.poll_document_job(path, job) {
                return true;
            }
            if let Some(request) = job.pending {
                queued.push((path.clone(), request));
            }
            false
        });
        self.document_save.jobs = jobs;
        for (path, request) in queued {
            self.start_document_action(path, request);
        }
    }

    fn poll_document_job(&mut self, path: &Path, job: &mut Job) -> bool {
        match &mut job.phase {
            Phase::Prepare(rx) => {
                let Some(prepared) = receive(rx) else {
                    if rx.is_none() {
                        self.message = format!("Worker stopped while preparing {}", path.display());
                    }
                    return rx.is_some();
                };
                let Some(doc) = self.document_mut(job.request.id) else {
                    return false;
                };
                if doc.version != job.request.version {
                    self.message = format!("{} changed while preparing the file. Run the command again.", path.display());
                    return false;
                }
                if let Some(text) = prepared.text {
                    replace_buffer(doc, text);
                }
                let saving = matches!(job.request.action, Action::Save { .. });
                if saving {
                    // Invalidate reads of the previous disk contents started before the write.
                    doc.version += 1;
                }
                if let Some(pending) = &mut job.pending
                    && pending.id == job.request.id
                    && pending.version == job.request.version
                {
                    pending.version = doc.version;
                }
                if saving {
                    let text = doc.text.clone();
                    job.request.version = doc.version;
                    let file = path.to_owned();
                    let snapshot = text.clone();
                    let rx = background(&self.ctx, move || write_text(&file, &snapshot, prepared.crlf));
                    job.phase = Phase::Write {
                        rx: Some(rx),
                        text,
                        warning: prepared.warning,
                    };
                    return true;
                }
                self.message = prepared.warning.unwrap_or_else(|| format!("Formatted {}", path.display()));
            }
            Phase::Write { rx, text, warning } => {
                let Some(result) = receive(rx) else {
                    if rx.is_none() {
                        self.message = format!("Save worker stopped for {}", path.display());
                    }
                    return rx.is_some();
                };
                self.message = match result {
                    Ok(()) => {
                        let svn = self.vcs == Some(Vcs::Svn);
                        if let Some(doc) = self.document_mut(job.request.id) {
                            let version = doc.version;
                            if version == job.request.version {
                                doc.mark_saved();
                            } else {
                                doc.mark_saved_snapshot(std::mem::take(text));
                            }
                            if svn {
                                doc.clear_blame();
                            }
                        }
                        let mut message = format!("Saved {}", path.display());
                        if let Some(warning) = warning {
                            message.push_str(" · ");
                            message.push_str(warning);
                        }
                        message
                    }
                    Err(err) => {
                        let mut message = format!("Save failed for {}: {err}", path.display());
                        if let Some(warning) = warning {
                            message.push_str(" · ");
                            message.push_str(warning);
                        }
                        message
                    }
                };
                // A watcher event can arrive before the write's worker returns.
                // Recheck this path after the job is removed, including a reopened tab.
                self.pending_changes.paths.insert(path.to_owned());
                self.refresh_pending = true;
            }
        }
        false
    }

    fn document_mut(&mut self, id: u64) -> Option<&mut Doc> {
        self.tabs.iter_mut().find_map(|tab| match tab {
            Tab::File(doc) if doc.id == id => Some(doc),
            _ => None,
        })
    }

    pub(super) fn writing_document(&self, path: &Path) -> bool {
        self.document_save
            .jobs
            .get(path)
            .is_some_and(|job| matches!(job.phase, Phase::Write { .. }))
    }

    pub(super) fn prime_editorconfig(&self, path: PathBuf) {
        self.load_editorconfigs(vec![path]);
    }

    pub(super) fn invalidate_editorconfigs(&self, changes: &Changes, watcher_rescan: bool) {
        if self.document_save.config.invalidate(&changes.paths, watcher_rescan) || changes.rescan {
            let paths = self.tabs.iter().filter_map(|tab| match tab {
                Tab::File(doc) => Some(doc.path.clone()),
                _ => None,
            });
            self.load_editorconfigs(paths.collect());
        }
    }

    fn load_editorconfigs(&self, paths: Vec<PathBuf>) {
        if !paths.is_empty() {
            let config = self.document_save.config.clone();
            // The worker populates the shared cache. There is no result to apply to a tab.
            drop(background(&self.ctx, move || {
                for path in paths {
                    config.load(&path);
                }
            }));
        }
    }

    /// Tab insertion is owned by the editor widget. Its rewrite can call this without file I/O.
    /// The .editorconfig indent for `path`, if it sets one.
    pub(crate) fn editorconfig_indent(&self, path: &Path) -> Option<String> {
        self.document_save.config.cached(path).explicit_indent()
    }
}

fn prepare(path: &Path, mut text: String, original_crlf: bool, action: Action, config: &editorconfig::Cache) -> Prepared {
    let mut warning = None;
    if matches!(action, Action::Format | Action::Save { format: true }) {
        match formatter::format(path, &text) {
            Ok(Some(formatted)) => text = formatted,
            Ok(None) if matches!(action, Action::Format) => warning = Some(format!("No formatter for {}", path.display())),
            Err(error) => warning = Some(error),
            Ok(None) => {}
        }
    }
    let crlf = if matches!(action, Action::Save { .. }) {
        let properties = config.load(path);
        text = properties.transform(text);
        properties.crlf(original_crlf)
    } else {
        false
    };
    Prepared {
        text: (matches!(action, Action::Save { .. }) || warning.is_none()).then_some(text),
        warning,
        crlf,
    }
}

fn write_text(path: &Path, text: &str, crlf: bool) -> std::io::Result<()> {
    if crlf {
        std::fs::write(path, text.replace('\n', "\r\n"))
    } else {
        std::fs::write(path, text)
    }
}

fn replace_buffer(doc: &mut Doc, text: String) {
    if doc.text == text {
        return;
    }
    let remap = |index| {
        let (mut line, mut column) = (0, 0);
        for ch in doc.text.chars().take(index) {
            if ch == '\n' {
                line += 1;
                column = 0;
            } else {
                column += 1;
            }
        }
        let mut start = 0;
        for (number, current) in text.split('\n').enumerate() {
            if number == line {
                return start + current.chars().take(column).count();
            }
            start += current.chars().count() + 1;
        }
        start - 1
    };
    (doc.cursor, doc.anchor) = (remap(doc.cursor), remap(doc.anchor));
    doc.goto = Some((doc.anchor, doc.cursor));
    doc.text = text;
    doc.version += 1;
    doc.edited_at = Some(Instant::now());
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::SystemTime;

    #[test]
    fn saves_transformed_utf8_with_configured_or_original_line_endings() {
        let unique = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_nanos();
        let dir = std::env::temp_dir().join(format!("edittr-save-{}-{unique}", std::process::id()));
        fs::create_dir(&dir).unwrap();
        let config_path = dir.join(".editorconfig");
        let file = dir.join("test.txt");
        let cache = editorconfig::Cache::default();
        let action = Action::Save { format: false };
        fs::write(
            &config_path,
            "root=true\n[*]\nend_of_line=crlf\ntrim_trailing_whitespace=true\ninsert_final_newline=true\ncharset=utf-8",
        )
        .unwrap();

        let prepared = prepare(&file, "\u{feff}café  \nsecond\t".into(), false, action, &cache);
        let text = prepared.text.unwrap();
        assert_eq!(text, "café\nsecond\n");
        write_text(&file, &text, prepared.crlf).unwrap();
        assert_eq!(fs::read(&file).unwrap(), "café\r\nsecond\r\n".as_bytes());

        fs::write(&config_path, "root=true\n[*]\nend_of_line=lf\ninsert_final_newline=false").unwrap();
        assert!(cache.invalidate([&config_path], false));
        for (crlf, expected) in [(false, "first\nsecond"), (true, "first\r\nsecond")] {
            let prepared = prepare(&file, "first\nsecond\n".into(), crlf, action, &cache);
            write_text(&file, &prepared.text.unwrap(), prepared.crlf).unwrap();
            assert_eq!(fs::read(&file).unwrap(), expected.as_bytes());
        }
        fs::remove_dir_all(dir).unwrap();
    }
}
