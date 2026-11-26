//! Asynchronous formatting and saving, coordinated against the live buffer version.
use super::{App, Doc, Encoding, Tab, Vcs, background, receive};
use crate::{editor::buffer::Sel, editorconfig, formatter, watch::Changes};
use std::collections::HashMap;
use std::ops::Range;
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
    Save(SaveOptions),
    Format,
}

#[derive(Clone, Copy)]
struct SaveOptions {
    format: bool,
    trim: bool,
    final_newline: bool,
    crlf_override: Option<bool>,
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
        crlf: bool,
        encoding: Encoding,
        warning: Option<String>,
    },
}

struct Prepared {
    formatted: Option<String>,
    /// `formatted` as replacements of the changed line blocks (keeps bookmarks and folds in place).
    format_edits: Vec<(Range<usize>, String)>,
    edits: Vec<(Range<usize>, String)>,
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
            Action::Save(SaveOptions {
                format: self.settings.format_on_save,
                trim: self.settings.trim_trailing_whitespace_on_save,
                final_newline: self.settings.insert_final_newline_on_save,
                crlf_override: match self.tabs.get(index) {
                    Some(Tab::File(doc)) if doc.line_endings_explicit => Some(doc.crlf),
                    _ => None,
                },
            }),
        );
    }

    pub(super) fn save_with_encoding(&mut self, index: usize, encoding: Encoding) {
        let Some(Tab::File(doc)) = self.tabs.get_mut(index) else { return };
        if doc.encoding != encoding {
            doc.encoding = encoding;
            doc.version += 1;
            doc.edited_at = Some(Instant::now());
        }
        self.save_doc(index);
    }

    pub(super) fn change_line_endings(&mut self, index: usize, crlf: bool) {
        let Some(Tab::File(doc)) = self.tabs.get_mut(index) else { return };
        if doc.crlf != crlf || !doc.line_endings_explicit {
            doc.crlf = crlf;
            doc.line_endings_explicit = true;
            doc.version += 1;
            doc.edited_at = Some(Instant::now());
        }
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
            if matches!(action, Action::Save(_)) || job.pending.is_none() {
                job.pending = Some(request);
            }
        } else {
            self.start_document_action(path, request);
        }
    }

    fn start_document_action(&mut self, path: PathBuf, request: Request) {
        let Some(doc) = self
            .document_mut(request.id)
            .filter(|doc| doc.version == request.version && doc.path == path)
        else {
            return;
        };
        let (text, original_crlf) = (doc.text.clone(), doc.crlf);
        let (config, file) = (self.document_save.config.clone(), path.clone());
        let rx = background(&self.ctx, move || prepare(&file, text, original_crlf, request.action, &config));
        self.message = match request.action {
            Action::Save(_) => format!("Saving {}…", path.display()),
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
                if doc.version != job.request.version || doc.path != path {
                    self.message = format!("{} changed while preparing the file. Run the command again.", path.display());
                    return false;
                }
                if let Some(text) = prepared.formatted {
                    replace_buffer(doc, text, prepared.format_edits);
                }
                apply_whitespace_edits(doc, prepared.edits);
                let saving = matches!(job.request.action, Action::Save(_));
                if saving {
                    doc.crlf = prepared.crlf;
                    // Invalidate reads of the previous disk contents started before the write.
                    doc.version += 1;
                    doc.disk_version = doc.version;
                }
                if let Some(pending) = &mut job.pending
                    && pending.id == job.request.id
                    && pending.version == job.request.version
                {
                    pending.version = doc.version;
                }
                if saving {
                    let text = doc.text.clone();
                    let file = path.to_owned();
                    let snapshot = text.clone();
                    let (crlf, encoding) = (doc.crlf, doc.encoding);
                    let rx = background(&self.ctx, move || write_text(&file, &snapshot, crlf, encoding));
                    job.phase = Phase::Write {
                        rx: Some(rx),
                        text,
                        crlf,
                        encoding,
                        warning: prepared.warning,
                    };
                    return true;
                }
                self.message = prepared.warning.unwrap_or_else(|| format!("Formatted {}", path.display()));
            }
            Phase::Write {
                rx,
                text,
                crlf,
                encoding,
                warning,
            } => {
                let Some(result) = receive(rx) else {
                    if rx.is_none() {
                        self.message = format!("Save worker stopped for {}", path.display());
                    }
                    return rx.is_some();
                };
                self.message = match result {
                    Ok(()) => {
                        let svn = self.vcs == Some(Vcs::Svn);
                        if let Some(doc) = self.document_mut(job.request.id).filter(|doc| doc.path == path) {
                            doc.mark_saved(std::mem::take(text), *crlf, *encoding);
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

    pub(super) fn document_mut(&mut self, id: u64) -> Option<&mut Doc> {
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

fn prepare(path: &Path, text: String, original_crlf: bool, action: Action, config: &editorconfig::Cache) -> Prepared {
    let mut warning = None;
    let mut formatted = None;
    if matches!(action, Action::Format | Action::Save(SaveOptions { format: true, .. })) {
        match formatter::format(path, &text) {
            Ok(Some(output)) => {
                let output = if output.contains("\r\n") {
                    output.replace("\r\n", "\n")
                } else {
                    output
                };
                if output != text {
                    formatted = Some(output);
                }
            }
            Ok(None) if matches!(action, Action::Format) => warning = Some(format!("No formatter for {}", path.display())),
            Err(error) => warning = Some(error),
            Ok(None) => {}
        }
    }
    let (edits, crlf) = if let Action::Save(options) = action {
        if formatted.is_none() && text.contains("\r\n") {
            formatted = Some(text.replace("\r\n", "\n"));
        }
        let properties = config.load(path);
        let edits = properties.whitespace_edits(path, formatted.as_deref().unwrap_or(&text), options.trim, options.final_newline);
        (edits, options.crlf_override.unwrap_or_else(|| properties.crlf(original_crlf)))
    } else {
        (Vec::new(), original_crlf)
    };
    Prepared {
        format_edits: formatted.as_deref().map_or_else(Vec::new, |new| line_edits(&text, new)),
        formatted,
        edits,
        warning,
        crlf,
    }
}

fn write_text(path: &Path, text: &str, crlf: bool, encoding: Encoding) -> std::io::Result<()> {
    let with_endings = crlf.then(|| text.replace('\n', "\r\n"));
    let text = with_endings.as_deref().unwrap_or(text);
    if encoding == Encoding::Utf8 {
        return std::fs::write(path, text);
    }
    // Validate the entire conversion before opening the destination for writing.
    std::fs::write(path, encoding.encode(text)?)
}

fn apply_whitespace_edits(doc: &mut Doc, edits: Vec<(Range<usize>, String)>) {
    if edits.is_empty() {
        return;
    }
    if let Some((anchor, cursor)) = doc.goto {
        let index = doc.ed.index(&doc.text, doc.version);
        doc.ed.sels = vec![Sel {
            anchor: index.char_to_byte(&doc.text, anchor),
            head: index.char_to_byte(&doc.text, cursor),
        }];
    }
    doc.ed.apply_edits(&mut doc.text, edits);
    doc.version += 1;
    doc.edited_at = Some(Instant::now());
    let selection = doc.ed.primary();
    doc.cursor = doc.text[..selection.head].chars().count();
    doc.anchor = doc.text[..selection.anchor].chars().count();
    if doc.goto.is_some() {
        doc.goto = Some((doc.anchor, doc.cursor));
    }
}

/// Replacements turning `old` into `new`, one per changed block of lines.
fn line_edits(old: &str, new: &str) -> Vec<(Range<usize>, String)> {
    let starts = |text: &str| {
        let mut starts: Vec<usize> = std::iter::once(0).chain(text.match_indices('\n').map(|(i, _)| i + 1)).collect();
        if starts.last() != Some(&text.len()) {
            starts.push(text.len());
        }
        starts
    };
    let (old_starts, new_starts) = (starts(old), starts(new));
    let byte = |starts: &[usize], line: usize| starts[line.min(starts.len() - 1)];
    let edits: Vec<(Range<usize>, String)> = crate::diff::changed_blocks(old, new)
        .into_iter()
        .map(|hunk| {
            let range = byte(&old_starts, hunk.old.start)..byte(&old_starts, hunk.old.end);
            let with = new[byte(&new_starts, hunk.new.start)..byte(&new_starts, hunk.new.end)].to_string();
            (range, with)
        })
        .collect();
    // Check (off the UI thread) that they rebuild `new` exactly; otherwise replace everything.
    let mut rebuilt = String::with_capacity(new.len());
    let mut at = 0;
    for (range, with) in &edits {
        rebuilt.push_str(&old[at..range.start]);
        rebuilt.push_str(with);
        at = range.end;
    }
    rebuilt.push_str(&old[at..]);
    if rebuilt == new { edits } else { Vec::new() }
}

fn replace_buffer(doc: &mut Doc, text: String, edits: Vec<(Range<usize>, String)>) {
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
    if !edits.is_empty() {
        doc.ed.apply_edits(&mut doc.text, edits);
    } else {
        // The line diff gave up (timeout) or didn't match: replace everything.
        let len = doc.text.len();
        doc.ed.replace(&mut doc.text, 0..len, &text);
    }
    doc.version += 1;
    doc.edited_at = Some(Instant::now());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formatting_edits_only_the_changed_lines() {
        let old = "a\nb  \nc\nd\n";
        let new = "a\nb\nc\nd\ne";
        let edits = line_edits(old, new);
        assert_eq!(edits, [(2..6, "b\n".to_string()), (10..10, "e".to_string())]);
        assert!(line_edits("x", "x").is_empty());
    }
    use crate::commands::Command;
    use eframe::egui;
    use std::fs;
    use std::time::{Duration, SystemTime};

    fn fixture() -> (App, egui::Context, PathBuf, std::thread::JoinHandle<()>) {
        let unique = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_nanos();
        let dir = std::env::temp_dir().join(format!("edittr-save-{}-{unique}", std::process::id()));
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join(".editorconfig"), "root=true\n").unwrap();
        let ctx = egui::Context::default();
        let cc = eframe::CreationContext::_new_kittest(ctx.clone());
        let (writer, worker) = crate::settings::SettingsWriter::start();
        let startup = crate::session::Startup {
            root: None,
            file: None,
            restore: false,
            window: Default::default(),
        };
        (App::new(&cc, startup, Default::default(), writer, None), ctx, dir, worker)
    }

    fn wait(mut poll: impl FnMut() -> bool) {
        let until = Instant::now() + Duration::from_secs(5);
        while !poll() {
            assert!(Instant::now() < until, "background file operation timed out");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn finish_save(app: &mut App) {
        wait(|| {
            app.poll_document_saves();
            app.document_save.jobs.is_empty()
        });
    }

    fn doc(app: &mut App) -> &mut Doc {
        match &mut app.tabs[0] {
            Tab::File(doc) => doc,
            _ => panic!("expected an editable tab"),
        }
    }

    #[test]
    fn opens_and_saves_bom_and_legacy_files_with_selected_line_endings() {
        let (mut app, _, dir, worker) = fixture();
        fs::write(dir.join(".editorconfig"), "root=true\n[*]\nend_of_line=crlf\n").unwrap();
        for (encoding, bytes, content) in [
            (Encoding::Utf16Le, &b"\xff\xfe\xe9\0\r\0\n\0"[..], "é"),
            (Encoding::Utf16Be, &b"\xfe\xff\0\xe9\0\r\0\n"[..], "é"),
            (Encoding::Utf8Bom, &b"\xef\xbb\xbf\xc3\xa9\r\n"[..], "é"),
            (Encoding::Windows1252, &b"\x93\xe9\x94\r\n"[..], "“é”"),
        ] {
            let path = dir.join("text.txt");
            fs::write(&path, bytes).unwrap();
            app.open_file(&path);
            wait(|| {
                app.poll_open_files();
                app.open_jobs.is_empty()
            });
            let initial = doc(&mut app);
            assert_eq!(initial.encoding, encoding);
            assert_eq!(initial.text, format!("{content}\n"));
            assert!(initial.crlf);
            assert!(!initial.dirty());

            let old_version = initial.version;
            app.run_command(Command::ChangeLineEndings(false));
            assert!(doc(&mut app).version > old_version);
            assert!(doc(&mut app).dirty());
            app.run_command(Command::ChangeLineEndings(true));
            assert!(!doc(&mut app).dirty());
            app.run_command(Command::ChangeLineEndings(false));
            app.save_active();
            finish_save(&mut app);
            assert_eq!(fs::read(&path).unwrap(), encoding.encode(&format!("{content}\n")).unwrap());
            assert!(!doc(&mut app).dirty());

            app.run_command(Command::SaveWithEncoding(Encoding::Utf8));
            finish_save(&mut app);
            assert_eq!(fs::read(&path).unwrap(), format!("{content}\n").as_bytes());
            assert_eq!(doc(&mut app).encoding, Encoding::Utf8);
            assert!(!doc(&mut app).dirty());
            app.close_tab(0, false);
        }
        drop(app);
        worker.join().unwrap();
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn reopens_with_explicit_decoder_and_rejects_lossy_saves() {
        let (mut app, _, dir, worker) = fixture();
        let path = dir.join("legacy.txt");
        let bytes = b"\x80\xe9";
        fs::write(&path, bytes).unwrap();
        app.open_file(&path);
        wait(|| {
            app.poll_open_files();
            app.open_jobs.is_empty()
        });
        assert_eq!(doc(&mut app).text, "€é");
        app.run_command(Command::ReopenWithEncoding(Encoding::Latin1));
        wait(|| {
            app.poll_open_files();
            app.reopen_jobs.is_empty()
        });
        assert_eq!(doc(&mut app).text, "\u{80}é");
        assert_eq!(doc(&mut app).encoding, Encoding::Latin1);
        assert!(!doc(&mut app).dirty());
        app.save_active();
        finish_save(&mut app);
        assert_eq!(fs::read(&path).unwrap(), bytes);

        let doc = doc(&mut app);
        doc.text.push('😀');
        doc.version += 1;
        app.run_command(Command::ReopenWithEncoding(Encoding::Windows1252));
        assert!(matches!(app.confirm.take(), Some(super::super::Confirm::ReopenEncoding(..))));
        app.save_active();
        finish_save(&mut app);
        assert_eq!(fs::read(&path).unwrap(), bytes);
        assert!(app.message.contains("cannot represent"));
        drop(app);
        worker.join().unwrap();
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn save_trimming_is_a_single_editor_undo_with_the_original_caret() {
        let (mut app, ctx, dir, worker) = fixture();
        let path = dir.join("note.md");
        fs::write(
            dir.join(".editorconfig"),
            "root=true\n[*]\ntrim_trailing_whitespace=true\ninsert_final_newline=true\n",
        )
        .unwrap();
        let original = format!("café  \nsecond \t\n{}last", "line \t\n".repeat(40));
        app.add_file_tab(&path, original.clone(), false, Encoding::Utf8);
        doc(&mut app).goto = Some((14, 14));
        let render = |app: &mut App, events| {
            let mut output = ctx.run_ui(
                egui::RawInput {
                    events,
                    ..Default::default()
                },
                |ui| {
                    let Tab::File(doc) = &mut app.tabs[0] else { unreachable!() };
                    super::super::editor(ui, doc, &app.settings, None, &[], &[], 0, None, &[], &[]);
                },
            );
            output.textures_delta.clear();
        };
        render(&mut app, vec![]);
        app.save_doc(0);
        finish_save(&mut app);
        let saved = format!("café  \nsecond\n{}last\n", "line\n".repeat(40));
        assert_eq!(doc(&mut app).text, saved);
        assert_eq!(doc(&mut app).cursor, 13);
        assert!(!doc(&mut app).dirty());
        assert_eq!(fs::read_to_string(&path).unwrap(), saved);

        render(
            &mut app,
            vec![egui::Event::Key {
                key: egui::Key::Z,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::COMMAND,
            }],
        );
        assert_eq!(doc(&mut app).text, original);
        assert_eq!(doc(&mut app).cursor, 14);
        assert!(doc(&mut app).dirty());
        drop(app);
        worker.join().unwrap();
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn status_menus_offer_line_endings_and_encoding_actions() {
        let (mut app, ctx, dir, worker) = fixture();
        let path = dir.join("test.txt");
        fs::write(&path, b"line\n").unwrap();
        app.add_file_tab(&path, "line\n".into(), false, Encoding::Utf8);
        let mut time = 0.0;
        let mut render = |app: &mut App, events| {
            time += 0.1;
            let mut output = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(800.0, 400.0))),
                    time: Some(time),
                    events,
                    ..Default::default()
                },
                |ui| {
                    ui.horizontal(|ui| {
                        if let Some(command) = super::super::file_open::format_status(ui, doc(app)) {
                            app.run_command(command);
                        }
                    });
                },
            );
            output.textures_delta.clear();
            output.shapes
        };
        let position = |shapes: &[egui::epaint::ClippedShape], label: &str| {
            shapes
                .iter()
                .find_map(|shape| match &shape.shape {
                    egui::Shape::Text(text) if text.galley.text() == label => Some(text.pos + egui::vec2(3.0, 3.0)),
                    _ => None,
                })
                .unwrap_or_else(|| {
                    let labels: Vec<_> = shapes
                        .iter()
                        .filter_map(|shape| match &shape.shape {
                            egui::Shape::Text(text) => Some(text.galley.text()),
                            _ => None,
                        })
                        .collect();
                    panic!("missing status label {label}. Visible: {labels:?}");
                })
        };
        let press = |pos, pressed| egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        let shapes = render(&mut app, vec![]);
        let mut click = |app: &mut App, pos| {
            render(app, vec![egui::Event::PointerMoved(pos)]);
            render(app, vec![press(pos, true)]);
            render(app, vec![press(pos, false)]);
            render(app, vec![])
        };

        let shapes = click(&mut app, position(&shapes, "LF"));
        let shapes = click(&mut app, position(&shapes, "CRLF"));
        assert!(doc(&mut app).crlf);
        assert!(doc(&mut app).dirty());
        let shapes = click(&mut app, position(&shapes, "UTF-8"));
        position(&shapes, "Reopen with encoding");
        let shapes = click(&mut app, position(&shapes, "Save with encoding"));
        click(&mut app, position(&shapes, "UTF-8 with BOM"));
        finish_save(&mut app);
        assert_eq!(fs::read(&path).unwrap(), b"\xef\xbb\xbfline\r\n");
        drop(app);
        worker.join().unwrap();
        fs::remove_dir_all(dir).unwrap();
    }
}
