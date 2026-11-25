//! Rename symbol (F2) with a preview, and code actions / quick fixes (Ctrl/Cmd+.).
//! Server requests run on background threads; edits apply to open editors (one undo
//! step each) and are written directly to files that aren't open.

use super::{App, Tab, background, receive};
use crate::icons as ic;
use crate::lsp::{self, features::CodeAction, features::FileEdit, features::TextEdit};
use crate::workspace;
use eframe::egui::{self, Key, Modifiers, RichText};
use std::collections::HashMap;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::Receiver;
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(10);

/// Edits to apply, and maybe a server command to run after them.
type EditResult = Result<(Vec<FileEdit>, Option<FollowUp>), String>;

#[derive(Default)]
pub(super) struct LspEdit {
    rename: Option<Rename>,
    actions: Option<Actions>,
    /// Edits from a code action's resolve or command, still on their way.
    pending: Option<Receiver<EditResult>>,
    pending_origin: Option<Origin>,
    /// Screen rect of the primary caret (last frame), where the actions menu opens.
    pub(super) caret: Option<egui::Rect>,
}

/// What the document looked like when a request was sent.
#[derive(Clone)]
struct Origin {
    doc: u64,
    version: u64,
    path: PathBuf,
    text: String,
}

struct Rename {
    origin: Origin,
    /// UTF-16 position of the symbol, and its byte range in the document.
    at: (usize, usize),
    word: Range<usize>,
    name: String,
    focus: bool,
    launch: Option<Arc<lsp::Launch>>,
    prepare: Option<Receiver<Result<Option<String>, String>>>,
    plan: Option<Receiver<Result<Plan, String>>>,
    preview: Option<Plan>,
    error: Option<String>,
}

/// Edits with a line-by-line preview.
pub(super) struct Plan {
    files: Vec<FileEdit>,
    rows: Vec<PreviewRow>,
}

struct PreviewRow {
    path: PathBuf,
    line: usize,
    before: String,
    after: String,
}

struct Actions {
    origin: Origin,
    caret: egui::Rect,
    rx: Option<Receiver<Result<Vec<CodeAction>, String>>>,
    items: Vec<CodeAction>,
    selected: usize,
    error: Option<String>,
    launch: Arc<lsp::Launch>,
}

/// A server command to run after a code action's edit was applied.
struct FollowUp {
    launch: Arc<lsp::Launch>,
    command: serde_json::Value,
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Byte range of the identifier at (or just before) `b`.
fn word_range(text: &str, b: usize) -> Range<usize> {
    let b = b.min(text.len());
    let start = text[..b]
        .char_indices()
        .rev()
        .take_while(|&(_, c)| is_word(c))
        .last()
        .map_or(b, |(i, _)| i);
    let end = b + text[b..].chars().take_while(|&c| is_word(c)).map(char::len_utf8).sum::<usize>();
    start..end
}

/// Byte offsets where each line starts.
fn line_starts(text: &str) -> Vec<usize> {
    std::iter::once(0).chain(text.match_indices('\n').map(|(i, _)| i + 1)).collect()
}

/// Byte offset of an LSP position; columns past the end of a line clamp to it (before
/// a `\r`), lines past the end to the end of the text.
fn byte_at(text: &str, starts: &[usize], (line, col16): (usize, usize)) -> usize {
    let Some(&start) = starts.get(line) else { return text.len() };
    let end = starts.get(line + 1).map_or(text.len(), |&next| next - 1);
    let content = text[start..end].strip_suffix('\r').unwrap_or(&text[start..end]);
    let mut units = 0;
    for (i, c) in content.char_indices() {
        if units >= col16 {
            return start + i;
        }
        units += c.len_utf16();
    }
    start + content.len()
}

/// LSP position (line, UTF-16 column) of byte `b`.
fn position_of(text: &str, b: usize) -> (usize, usize) {
    let b = b.min(text.len());
    let start = text[..b].rfind('\n').map_or(0, |i| i + 1);
    (text[..start].matches('\n').count(), text[start..b].encode_utf16().count())
}

/// The edits as byte ranges of `text`. `crlf`: new lines in the inserted text become `\r\n`.
fn byte_edits(text: &str, edits: &[TextEdit], crlf: bool) -> Vec<(Range<usize>, String)> {
    let starts = line_starts(text);
    edits
        .iter()
        .map(|e| {
            let (a, b) = (byte_at(text, &starts, e.start), byte_at(text, &starts, e.end));
            let insert = if crlf {
                e.new_text.replace("\r\n", "\n").replace('\n', "\r\n")
            } else {
                e.new_text.clone()
            };
            (a.min(b)..a.max(b), insert)
        })
        .collect()
}

/// `text` with the edits applied (they must not overlap).
fn apply_to(text: &str, mut edits: Vec<(Range<usize>, String)>) -> String {
    edits.sort_by_key(|(r, _)| std::cmp::Reverse((r.start, r.end)));
    let mut out = text.to_string();
    for (r, s) in edits {
        out.replace_range(r, &s);
    }
    out
}

/// One row per changed line: the line before and after the edits on it.
fn preview_rows(path: &Path, text: &str, edits: &[TextEdit]) -> Vec<PreviewRow> {
    let starts = line_starts(text);
    let mut by_line: std::collections::BTreeMap<usize, Vec<&TextEdit>> = Default::default();
    for e in edits {
        by_line.entry(e.start.0).or_default().push(e);
    }
    by_line
        .into_iter()
        .map(|(line, edits)| {
            let start = starts.get(line).copied().unwrap_or(text.len());
            let end = starts.get(line + 1).map_or(text.len(), |&n| n - 1);
            let before = text[start..end].trim_end_matches('\r').to_string();
            // Edits on this line only (a multi-line edit shows its first line).
            let local: Vec<(Range<usize>, String)> = edits
                .iter()
                .map(|e| {
                    let a = byte_at(text, &starts, e.start) - start;
                    let b = (byte_at(text, &starts, e.end) - start).min(before.len());
                    (a.min(b)..b, e.new_text.lines().next().unwrap_or_default().to_string())
                })
                .collect();
            let after = apply_to(&before, local);
            PreviewRow {
                path: path.to_path_buf(),
                line,
                before: before.trim().to_string(),
                after: after.trim().to_string(),
            }
        })
        .collect()
}

/// Edits renaming every whole-word `word` in `text` (no language server: this file only).
fn rename_in_text(text: &str, word: &str, new_name: &str) -> Vec<TextEdit> {
    let mut out = Vec::new();
    for (i, _) in text.match_indices(word) {
        let before = text[..i].chars().next_back();
        let after = text[i + word.len()..].chars().next();
        if before.is_some_and(is_word) || after.is_some_and(is_word) {
            continue;
        }
        let (start, end) = (position_of(text, i), position_of(text, i + word.len()));
        out.push(TextEdit {
            start,
            end,
            new_text: new_name.to_string(),
        });
    }
    out
}

fn plan(files: Vec<FileEdit>, open: &HashMap<PathBuf, String>) -> Plan {
    let files: Vec<FileEdit> = files
        .into_iter()
        .map(|f| FileEdit {
            path: workspace::plain_path(&f.path),
            edits: f.edits,
        })
        .collect();
    let mut rows = Vec::new();
    for f in &files {
        let text = open.get(&f.path).cloned().or_else(|| std::fs::read_to_string(&f.path).ok());
        if let Some(text) = text {
            rows.extend(preview_rows(&f.path, &text, &f.edits));
        }
    }
    Plan { files, rows }
}

/// Write edits to a file that isn't open, keeping its line endings.
fn write_file_edits(path: &Path, edits: &[TextEdit]) -> std::io::Result<()> {
    let text = std::fs::read_to_string(path)?;
    let crlf = text.contains("\r\n");
    let new = apply_to(&text, byte_edits(&text, edits, crlf));
    if new == text {
        return Ok(());
    }
    let tmp = path.with_file_name(format!(
        ".{}.edittr-{}.tmp",
        path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default(),
        std::process::id()
    ));
    std::fs::write(&tmp, new.as_bytes())?;
    if let Ok(meta) = std::fs::metadata(path) {
        let _ = std::fs::set_permissions(&tmp, meta.permissions());
    }
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

impl App {
    fn origin(&mut self) -> Option<(Origin, usize, String)> {
        let Some(Tab::File(doc)) = self.tabs.get(self.active) else {
            return None;
        };
        let head = doc.ed.primary().head.min(doc.text.len());
        Some((
            Origin {
                doc: doc.id,
                version: doc.version,
                path: doc.path.clone(),
                text: doc.text.clone(),
            },
            head,
            doc.ext(),
        ))
    }

    fn server_for(&mut self, path: &Path, ext: &str) -> Option<Arc<lsp::Launch>> {
        (!self.path_is_ignored(path)).then(|| self.launch_server(ext, false)).flatten()
    }

    fn open_texts(&self) -> HashMap<PathBuf, String> {
        self.tabs
            .iter()
            .filter_map(|t| match t {
                Tab::File(d) => Some((d.path.clone(), d.text.clone())),
                _ => None,
            })
            .collect()
    }

    pub(super) fn rename_open(&self) -> bool {
        self.lsp_edit.rename.is_some()
    }

    /// F2: ask for the new name of the symbol at the caret.
    pub(super) fn start_rename(&mut self) {
        let Some((origin, head, ext)) = self.origin() else { return };
        let word = word_range(&origin.text, head);
        if word.is_empty() {
            self.message = "Put the cursor on a symbol to rename it".into();
            return;
        }
        let at = position_of(&origin.text, word.start);
        let launch = self.server_for(&origin.path, &ext);
        let prepare = launch.clone().map(|launch| {
            let o = origin.clone();
            background(&self.ctx, move || {
                let client = launch.wait().ok_or("The language server didn't start")?;
                Ok(client
                    .prepare_rename(&o.path, &o.text, (o.doc, o.version), at, TIMEOUT)?
                    .and_then(|(_, placeholder)| placeholder))
            })
        });
        self.lsp_edit.rename = Some(Rename {
            name: origin.text[word.clone()].to_string(),
            origin,
            at,
            word,
            focus: true,
            launch,
            prepare,
            plan: None,
            preview: None,
            error: None,
        });
    }

    fn submit_rename(&mut self) {
        let open = self.open_texts();
        let Some(r) = &mut self.lsp_edit.rename else { return };
        let name = r.name.trim().to_string();
        if name.is_empty() || name == r.origin.text[r.word.clone()] {
            self.lsp_edit.rename = None;
            return;
        }
        r.error = None;
        let (o, at) = (r.origin.clone(), r.at);
        let word = o.text[r.word.clone()].to_string();
        let launch = r.launch.clone();
        r.plan = Some(background(&self.ctx, move || {
            let files = match launch.as_ref().and_then(|l| l.wait()) {
                Some(client) => client.rename(&o.path, &o.text, (o.doc, o.version), at, &name, TIMEOUT)?,
                None => vec![FileEdit {
                    path: o.path.clone(),
                    edits: rename_in_text(&o.text, &word, &name),
                }],
            };
            if files.iter().all(|f| f.edits.is_empty()) {
                return Err("Nothing to rename here".into());
            }
            Ok(plan(files, &open))
        }));
    }

    /// Apply edits: open editors get them as one undo step each, other files are written.
    /// `origin`: the document the edits were computed for, which must not have changed.
    fn apply_workspace_edit(&mut self, files: Vec<FileEdit>, origin: Option<&Origin>) -> Result<String, String> {
        if let Some(o) = origin
            && self
                .tabs
                .iter()
                .any(|t| matches!(t, Tab::File(d) if d.id == o.doc && d.version != o.version))
        {
            return Err("The file changed meanwhile; try again".into());
        }
        let (mut open, mut written, mut edits, mut failed) = (0, 0, 0, Vec::new());
        for f in files {
            if f.edits.is_empty() {
                continue;
            }
            let path = workspace::plain_path(&f.path);
            edits += f.edits.len();
            if let Some(doc) = self.tabs.iter_mut().find_map(|t| match t {
                Tab::File(d) if d.path == path => Some(d),
                _ => None,
            }) {
                let byte = byte_edits(&doc.text, &f.edits, false);
                doc.ed.apply_edits(&mut doc.text, byte);
                doc.version += 1;
                doc.edited_at = Some(std::time::Instant::now());
                open += 1;
            } else if self.search_replace.is_writing(&path) {
                failed.push(format!("{}: busy", path.display()));
            } else {
                match write_file_edits(&path, &f.edits) {
                    Ok(()) => written += 1,
                    Err(e) => failed.push(format!("{}: {e}", path.display())),
                }
            }
        }
        if !failed.is_empty() {
            return Err(format!("Could not change {}", failed.join(", ")));
        }
        let files = open + written;
        let mut msg = format!("{edits} change{} in {files} file{}", plural(edits), plural(files));
        if open > 0 {
            msg.push_str(" (open files are modified, not saved)");
        }
        Ok(msg)
    }

    /// Ctrl/Cmd+.: code actions for the selection, or the caret's line diagnostics.
    pub(super) fn request_code_actions(&mut self) {
        let Some(Tab::File(doc)) = self.tabs.get(self.active) else { return };
        let (a, b) = {
            let p = doc.ed.primary();
            (p.anchor.min(p.head).min(doc.text.len()), p.anchor.max(p.head).min(doc.text.len()))
        };
        let Some((origin, _, ext)) = self.origin() else { return };
        let Some(launch) = self.server_for(&origin.path, &ext) else {
            self.message = "Code actions need a language server for this file".into();
            return;
        };
        let range = (position_of(&origin.text, a), position_of(&origin.text, b));
        let o = origin.clone();
        let l = launch.clone();
        let rx = background(&self.ctx, move || {
            let client = l.wait().ok_or("The language server didn't start")?;
            client.code_actions(&o.path, &o.text, (o.doc, o.version), range, TIMEOUT)
        });
        let caret = self.lsp_edit.caret.unwrap_or(egui::Rect::NOTHING);
        self.lsp_edit.actions = Some(Actions {
            origin,
            caret,
            rx: Some(rx),
            items: Vec::new(),
            selected: 0,
            error: None,
            launch,
        });
    }

    fn run_code_action(&mut self, action: CodeAction) {
        let Some(menu) = self.lsp_edit.actions.take() else { return };
        if let Some(reason) = &action.disabled {
            self.message = format!("{}: {reason}", action.title);
            return;
        }
        let launch = menu.launch.clone();
        let origin = menu.origin.clone();
        self.lsp_edit.pending = Some(background(&self.ctx, move || {
            let client = launch.wait().ok_or("The language server didn't start")?;
            let action = client.resolve_code_action(action, TIMEOUT);
            let edits = action.edit.clone().unwrap_or_default();
            let follow = action.command.clone().map(|c| {
                // A bare Command is the item itself; a CodeAction nests one.
                let command = if c["command"].is_string() { c } else { c["command"].clone() };
                FollowUp {
                    launch: launch.clone(),
                    command,
                }
            });
            Ok((edits, follow))
        }));
        self.lsp_edit.pending_origin = Some(origin);
    }

    /// Keys for the code action menu and the rename box, before the editor sees them.
    pub(super) fn lsp_edit_keys(&mut self, ctx: &egui::Context) {
        let Some(m) = &mut self.lsp_edit.actions else { return };
        if m.items.is_empty() {
            if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape)) {
                self.lsp_edit.actions = None;
            }
            return;
        }
        let (down, up, enter, esc) = ctx.input_mut(|i| {
            (
                i.consume_key(Modifiers::NONE, Key::ArrowDown),
                i.consume_key(Modifiers::NONE, Key::ArrowUp),
                i.consume_key(Modifiers::NONE, Key::Enter),
                i.consume_key(Modifiers::NONE, Key::Escape),
            )
        });
        let n = m.items.len();
        if down {
            m.selected = (m.selected + 1) % n;
        }
        if up {
            m.selected = (m.selected + n - 1) % n;
        }
        if esc {
            self.lsp_edit.actions = None;
        } else if enter {
            let action = m.items[m.selected].clone();
            self.run_code_action(action);
        }
    }

    /// The rename box, the code action menu, and applying finished edits.
    pub(super) fn lsp_edit_ui(&mut self, ctx: &egui::Context) {
        self.rename_ui(ctx);
        self.code_actions_ui(ctx);
        if let Some(res) = receive(&mut self.lsp_edit.pending) {
            let origin = self.lsp_edit.pending_origin.take();
            match res {
                Ok((edits, follow)) => {
                    let applied = if edits.is_empty() {
                        Ok(String::new())
                    } else {
                        self.apply_workspace_edit(edits, origin.as_ref())
                    };
                    match applied {
                        Ok(msg) => {
                            if !msg.is_empty() {
                                self.message = msg;
                            }
                            if let Some(f) = follow {
                                self.run_follow_up(f);
                            }
                        }
                        Err(e) => self.message = e,
                    }
                }
                Err(e) => self.message = e,
            }
        }
    }

    /// Run a code action's command after syncing the edited document to the server.
    fn run_follow_up(&mut self, f: FollowUp) {
        let synced = match self.tabs.get(self.active) {
            Some(Tab::File(d)) => Some((d.path.clone(), d.text.clone(), (d.id, d.version))),
            _ => None,
        };
        self.lsp_edit.pending = Some(background(&self.ctx, move || {
            let client = f.launch.wait().ok_or("The language server didn't start")?;
            if let Some((path, text, revision)) = synced {
                client.sync_ready(&path, &text, revision);
            }
            Ok((client.execute_command(&f.command, TIMEOUT)?, None))
        }));
    }

    fn rename_ui(&mut self, ctx: &egui::Context) {
        let Some(r) = &mut self.lsp_edit.rename else { return };
        if let Some(res) = receive(&mut r.prepare) {
            match res {
                Ok(Some(placeholder)) if r.name == r.origin.text[r.word.clone()] => r.name = placeholder,
                Ok(_) => {}
                Err(e) => r.error = Some(e),
            }
        }
        if let Some(res) = receive(&mut r.plan) {
            match res {
                Ok(plan) => r.preview = Some(plan),
                Err(e) => r.error = Some(e),
            }
        }
        let mut close = ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape));
        let (mut submit, mut apply) = (false, false);
        let root = self.root.clone();
        let old = r.origin.text[r.word.clone()].to_string();
        egui::Window::new("Rename symbol")
            .id(egui::Id::unique("rename_symbol"))
            .title_bar(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_TOP, [0.0, 60.0])
            .fixed_size([560.0, 0.0])
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format!("{} Rename", ic::PENCIL_SIMPLE)).strong());
                    ui.monospace(&old);
                    if r.launch.is_none() {
                        ui.weak("(no language server: whole words in this file)");
                    }
                });
                if let Some(plan) = &r.preview {
                    let files = plan.files.iter().filter(|f| !f.edits.is_empty()).count();
                    let edits: usize = plan.files.iter().map(|f| f.edits.len()).sum();
                    ui.label(format!(
                        "`{old}` → `{}`: {edits} change{} in {files} file{}",
                        r.name.trim(),
                        plural(edits),
                        plural(files)
                    ));
                    // One line per change: `file:line  new text` (the old text on hover).
                    let row_h = ui.spacing().interact_size.y;
                    egui::ScrollArea::vertical().max_height(320.0).auto_shrink([false, true]).show_rows(
                        ui,
                        row_h,
                        plan.rows.len(),
                        |ui, rows| {
                            for row in &plan.rows[rows] {
                                let rel = root
                                    .as_deref()
                                    .and_then(|root| row.path.strip_prefix(root).ok())
                                    .unwrap_or(&row.path)
                                    .display()
                                    .to_string();
                                ui.allocate_ui(egui::vec2(ui.available_width(), row_h), |ui| {
                                    ui.horizontal_centered(|ui| {
                                        ui.weak(format!("{rel}:{}", row.line + 1));
                                        ui.add(egui::Label::new(RichText::new(&row.after).monospace()).truncate())
                                            .on_hover_text(format!("Before: {}", row.before));
                                    });
                                });
                            }
                        },
                    );
                    ui.horizontal(|ui| {
                        apply |= ui.button(format!("{} Apply (Enter)", ic::CHECK)).clicked();
                        close |= ui.button("Cancel (Esc)").clicked();
                    });
                    apply |= ui.input(|i| i.key_pressed(Key::Enter));
                } else {
                    let edit = ui.add(egui::TextEdit::singleline(&mut r.name).desired_width(f32::INFINITY));
                    if std::mem::take(&mut r.focus) {
                        edit.request_focus();
                        // Select the whole name so typing replaces it.
                        if let Some(mut state) = egui::TextEdit::load_state(ui.ctx(), edit.id) {
                            let n = r.name.chars().count();
                            state.cursor.set_char_range(Some(egui::text::CCursorRange::two(
                                egui::text::CCursor::new(0),
                                egui::text::CCursor::new(n),
                            )));
                            state.store(ui.ctx(), edit.id);
                        }
                    }
                    if r.plan.is_some() {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.weak("Finding every use…");
                        });
                    } else {
                        ui.weak("Enter to preview the changes, Esc to cancel");
                    }
                    submit = edit.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                }
                if let Some(e) = &r.error {
                    ui.colored_label(ui.visuals().error_fg_color, e);
                }
            });
        if submit {
            self.submit_rename();
        } else if apply && let Some(r) = self.lsp_edit.rename.take() {
            let origin = r.origin.clone();
            if let Some(plan) = r.preview {
                match self.apply_workspace_edit(plan.files, Some(&origin)) {
                    Ok(msg) => self.message = format!("Renamed: {msg}"),
                    Err(e) => self.message = e,
                }
            }
        } else if close {
            self.lsp_edit.rename = None;
        }
    }

    fn code_actions_ui(&mut self, ctx: &egui::Context) {
        let Some(m) = &mut self.lsp_edit.actions else { return };
        if let Some(res) = receive(&mut m.rx) {
            match res {
                Ok(items) if items.is_empty() => {
                    self.lsp_edit.actions = None;
                    self.message = "No code actions here".into();
                    return;
                }
                Ok(items) => m.items = items,
                Err(e) => m.error = Some(e),
            }
        }
        let pos = if m.caret.is_finite() {
            m.caret.left_bottom() + egui::vec2(0.0, 2.0)
        } else {
            ctx.content_rect().center_top() + egui::vec2(-200.0, 80.0)
        };
        let mut chosen = None;
        let area = egui::Area::new(egui::Id::unique("code_actions"))
            .order(egui::Order::Foreground)
            .fixed_pos(pos)
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_min_width(320.0);
                    ui.set_max_width(560.0);
                    if m.rx.is_some() {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.weak("Looking for code actions…");
                        });
                    }
                    if let Some(e) = &m.error {
                        ui.colored_label(ui.visuals().error_fg_color, e);
                    }
                    egui::ScrollArea::vertical().max_height(280.0).show(ui, |ui| {
                        for (i, a) in m.items.iter().enumerate() {
                            let icon = if a.kind.starts_with("quickfix") || a.preferred {
                                ic::LIGHTBULB
                            } else if a.kind.starts_with("source") {
                                ic::GEAR
                            } else {
                                ic::WRENCH
                            };
                            let mut text = RichText::new(format!("{icon}  {}", a.title));
                            if a.disabled.is_some() {
                                text = text.weak();
                            }
                            let r = ui.selectable_label(i == m.selected, text);
                            let r = match &a.disabled {
                                Some(reason) => r.on_hover_text(reason),
                                None => r,
                            };
                            if i == m.selected {
                                r.scroll_to_me(None);
                            }
                            if r.clicked() {
                                chosen = Some(i);
                            }
                        }
                    });
                });
            });
        if let Some(i) = chosen {
            m.selected = i;
            let action = m.items[i].clone();
            self.run_code_action(action);
        } else if area.response.clicked_elsewhere() && m.rx.is_none() {
            self.lsp_edit.actions = None;
        }
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edit(start: (usize, usize), end: (usize, usize), t: &str) -> TextEdit {
        TextEdit {
            start,
            end,
            new_text: t.into(),
        }
    }

    #[test]
    fn converts_positions_to_bytes_with_utf16_and_crlf() {
        let text = "é𝄞x\r\nab\nlast";
        let starts = line_starts(text);
        assert_eq!(byte_at(text, &starts, (0, 0)), 0);
        assert_eq!(byte_at(text, &starts, (0, 1)), 2); // after é (2 bytes, 1 unit)
        assert_eq!(byte_at(text, &starts, (0, 3)), 6); // after 𝄞 (4 bytes, 2 units)
        assert_eq!(byte_at(text, &starts, (0, 99)), 7); // before \r
        assert_eq!(byte_at(text, &starts, (1, 1)), 10);
        assert_eq!(byte_at(text, &starts, (2, 4)), text.len());
        assert_eq!(byte_at(text, &starts, (9, 0)), text.len());
        assert_eq!(position_of(text, 10), (1, 1));
        assert_eq!(position_of(text, 6), (0, 3));
    }

    #[test]
    fn applies_edits_keeping_crlf() {
        let text = "int foo;\r\nfoo = 1;\r\n";
        let edits = [edit((0, 4), (0, 7), "bar"), edit((1, 0), (1, 3), "bar\nbaz")];
        assert_eq!(apply_to(text, byte_edits(text, &edits, true)), "int bar;\r\nbar\r\nbaz = 1;\r\n");
    }

    #[test]
    fn renames_whole_words_only_without_a_server() {
        let text = "foo foo_bar foo(x) afoo\nfoo";
        let edits = rename_in_text(text, "foo", "q");
        assert_eq!(apply_to(text, byte_edits(text, &edits, false)), "q foo_bar q(x) afoo\nq");
    }

    #[test]
    fn previews_each_changed_line() {
        let text = "  let foo = foo + 1;\nother\n";
        let rows = preview_rows(
            Path::new("a.rs"),
            text,
            &[edit((0, 6), (0, 9), "bar"), edit((0, 12), (0, 15), "bar")],
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].before, "let foo = foo + 1;");
        assert_eq!(rows[0].after, "let bar = bar + 1;");
        assert_eq!(word_range("a foo_1 b", 4), 2..7);
        assert_eq!(word_range("foo", 3), 0..3);
    }
}
