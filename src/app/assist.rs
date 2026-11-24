//! Language-server assistance in the editor: hover info, completion and references.
//! Every server request runs on a background thread; the UI only polls results.

use super::{App, EditorOut, Tab, background, receive};
use crate::editor::buffer::LineIndex;
use crate::lsp;
use eframe::egui::{self, Key, Modifiers, RichText};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::Receiver;
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(3);
const MAX_SHOWN: usize = 200;

#[derive(Default)]
pub(super) struct Assist {
    hover: Option<Hover>,
    completion: Option<Popup>,
    refs: Option<References>,
}

struct Hover {
    /// (doc id, version, byte) the hover is for.
    key: (u64, u64, usize),
    pos: egui::Pos2,
    diagnostic: Option<String>,
    info: Option<String>,
    rx: Option<Receiver<Option<String>>>,
}

struct Popup {
    doc: u64,
    /// Byte where the word being completed starts.
    start: usize,
    items: Vec<lsp::Completion>,
    /// Indices into `items` matching the typed prefix, best first.
    shown: Vec<usize>,
    selected: usize,
    rx: Option<Receiver<Vec<lsp::Completion>>>,
    caret: egui::Rect,
    prefix: String,
}

struct References {
    title: String,
    rx: Option<Receiver<Result<Vec<RefRow>, String>>>,
    rows: Vec<RefRow>,
    error: Option<String>,
}

struct RefRow {
    path: PathBuf,
    line: usize,
    cols: std::ops::Range<usize>,
    text: String,
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Start (byte) of the identifier ending at `b`.
fn word_start(text: &str, b: usize) -> usize {
    text[..b]
        .char_indices()
        .rev()
        .take_while(|&(_, c)| is_word(c))
        .last()
        .map_or(b, |(i, _)| i)
}

/// LSP position (line, UTF-16 column) of byte `b`.
fn position(text: &str, ix: &LineIndex, b: usize) -> (usize, usize) {
    let (l, _) = ix.line_col(b);
    (l, text[ix.line_start(l)..b].encode_utf16().count())
}

/// Rank completions for `prefix`: prefix matches first (case-sensitive, then not), then
/// subsequence matches, then by the server's sort text.
pub(super) fn rank(items: &[lsp::Completion], prefix: &str) -> Vec<usize> {
    let lower = prefix.to_lowercase();
    let mut scored: Vec<(u8, &str, usize)> = items
        .iter()
        .enumerate()
        .filter_map(|(i, it)| {
            let f = it.filter.as_str();
            let score = if f.starts_with(prefix) {
                0
            } else if f.to_lowercase().starts_with(&lower) {
                1
            } else {
                let mut chars = f.chars().flat_map(char::to_lowercase);
                if !lower.chars().all(|q| chars.any(|c| c == q)) {
                    return None;
                }
                2
            };
            Some((score, it.sort.as_str(), i))
        })
        .collect();
    scored.sort();
    scored.into_iter().take(MAX_SHOWN).map(|(_, _, i)| i).collect()
}

impl App {
    /// Keys for the completion popup, taken before the editor sees them.
    pub(super) fn completion_keys(&mut self, ctx: &egui::Context) {
        if ctx.input_mut(|i| i.consume_key(Modifiers::CTRL, Key::Space)) {
            self.request_completion(true);
            return;
        }
        let Some(p) = &mut self.assist.completion else { return };
        if p.shown.is_empty() {
            return;
        }
        let (down, up, accept, esc) = ctx.input_mut(|i| {
            (
                i.consume_key(Modifiers::NONE, Key::ArrowDown),
                i.consume_key(Modifiers::NONE, Key::ArrowUp),
                i.consume_key(Modifiers::NONE, Key::Enter) || i.consume_key(Modifiers::NONE, Key::Tab),
                i.consume_key(Modifiers::NONE, Key::Escape),
            )
        });
        if down {
            p.selected = (p.selected + 1) % p.shown.len();
        }
        if up {
            p.selected = (p.selected + p.shown.len() - 1) % p.shown.len();
        }
        if esc {
            self.assist.completion = None;
        } else if accept {
            self.accept_completion();
        }
    }

    fn accept_completion(&mut self) {
        let Some(p) = self.assist.completion.take() else { return };
        let Some(&i) = p.shown.get(p.selected) else { return };
        let item = &p.items[i];
        let Some(Tab::File(doc)) = self.tabs.get_mut(self.active) else {
            return;
        };
        if doc.id != p.doc {
            return;
        }
        let head = doc.ed.primary().head;
        if head < p.start || head > doc.text.len() {
            return;
        }
        doc.ed.replace(&mut doc.text, p.start..head, &item.insert);
        doc.version += 1;
        doc.edited_at = Some(std::time::Instant::now());
    }

    /// Ask the server for completions at the active caret (`manual`: Ctrl+Space).
    fn request_completion(&mut self, manual: bool) {
        let Some(Tab::File(doc)) = self.tabs.get(self.active) else { return };
        let (path, ext) = (doc.path.clone(), doc.ext());
        if self.path_is_ignored(&path) {
            return;
        }
        let Some(launch) = self.launch_server(&ext, false) else {
            if manual {
                self.message = "No language server for completions".into();
            }
            return;
        };
        let Some(Tab::File(doc)) = self.tabs.get_mut(self.active) else {
            return;
        };
        let head = doc.ed.primary().head;
        let start = word_start(&doc.text, head);
        let ix = doc.ed.index(&doc.text, doc.version).clone();
        let pos = position(&doc.text, &ix, head);
        let (text, revision) = (doc.text.clone(), (doc.id, doc.version));
        let rx = background(&self.ctx, move || {
            let Some(client) = launch.wait() else { return Vec::new() };
            client.completion(&path, &text, revision, pos, TIMEOUT).unwrap_or_default()
        });
        let caret = self.assist.completion.as_ref().map(|p| p.caret).unwrap_or(egui::Rect::NOTHING);
        self.assist.completion = Some(Popup {
            doc: doc.id,
            start,
            items: Vec::new(),
            shown: Vec::new(),
            selected: 0,
            rx: Some(rx),
            caret,
            prefix: String::new(),
        });
    }

    /// After the editor ran: trigger / filter / close completion, start hover requests.
    pub(super) fn after_editor(&mut self, out: &EditorOut) {
        self.update_completion(out);
        self.update_hover(out);
    }

    fn update_completion(&mut self, out: &EditorOut) {
        let Some(Tab::File(doc)) = self.tabs.get(self.active) else {
            self.assist.completion = None;
            return;
        };
        let head = doc.ed.primary().head.min(doc.text.len());
        let before = doc.text[..head].chars().next_back();
        if let Some(p) = &mut self.assist.completion {
            if let Some(caret) = out.caret {
                p.caret = caret;
            }
            if let Some(items) = receive(&mut p.rx) {
                p.items = items;
                p.prefix.clear(); // re-rank below
            }
            // The caret left the word (or another doc is active): close.
            if p.doc != doc.id || head < p.start || doc.ed.sels.len() > 1 || !doc.text[p.start..head].chars().all(is_word) {
                self.assist.completion = None;
                return;
            }
            let prefix = &doc.text[p.start..head];
            if prefix != p.prefix || p.shown.is_empty() && !p.items.is_empty() {
                p.prefix = prefix.to_string();
                p.shown = rank(&p.items, prefix);
                p.selected = 0;
            }
            if p.rx.is_none() && p.shown.is_empty() && out.changed {
                self.assist.completion = None;
            }
            return;
        }
        // Typing an identifier character or a member access opens it.
        if out.changed && out.focused {
            let trigger = before.is_some_and(is_word) || matches!(before, Some('.')) || {
                let t = &doc.text[..head];
                t.ends_with("::") || t.ends_with("->")
            };
            if trigger && !doc.text[..head].ends_with(|c: char| c.is_ascii_digit() && !is_word_before_digit(&doc.text[..head])) {
                if let Some(caret) = out.caret {
                    self.assist.completion = Some(Popup {
                        doc: doc.id,
                        start: head,
                        items: Vec::new(),
                        shown: Vec::new(),
                        selected: 0,
                        rx: None,
                        caret,
                        prefix: String::new(),
                    });
                }
                self.request_completion(false);
            }
        }
    }

    fn update_hover(&mut self, out: &EditorOut) {
        // Typing or an open completion list hides hover info.
        let Some((b, pos, diagnostic)) = out.hover.clone().filter(|_| !out.changed && self.assist.completion.is_none()) else {
            self.assist.hover = None;
            return;
        };
        let Some(Tab::File(doc)) = self.tabs.get(self.active) else { return };
        let key = (doc.id, doc.version, b);
        if let Some(h) = &mut self.assist.hover
            && h.key == key
        {
            if let Some(info) = receive(&mut h.rx) {
                h.info = info;
            }
            return;
        }
        // Only over a word: no request for whitespace or punctuation.
        let on_word = doc.text[b..].chars().next().is_some_and(is_word);
        let mut hover = Hover {
            key,
            pos,
            diagnostic,
            info: None,
            rx: None,
        };
        if on_word && !self.path_is_ignored(&doc.path) {
            let (path, ext) = (doc.path.clone(), doc.ext());
            if let Some(launch) = self.launch_server(&ext, false)
                && let Some(Tab::File(doc)) = self.tabs.get_mut(self.active)
            {
                let ix = doc.ed.index(&doc.text, doc.version).clone();
                let p = position(&doc.text, &ix, b);
                let (text, revision) = (doc.text.clone(), (doc.id, doc.version));
                hover.rx = Some(background(&self.ctx, move || {
                    let client = launch.wait()?;
                    client.hover(&path, &text, revision, p, TIMEOUT).ok().flatten()
                }));
            }
        }
        if hover.rx.is_some() || hover.diagnostic.is_some() {
            self.assist.hover = Some(hover);
        } else {
            self.assist.hover = None;
        }
    }

    /// Shift+F12: find all references to the symbol at the caret.
    pub(super) fn find_references(&mut self) {
        let Some(Tab::File(doc)) = self.tabs.get(self.active) else { return };
        let head = doc.ed.primary().head.min(doc.text.len());
        let start = word_start(&doc.text, head);
        let end = head
            + doc.text[head..]
                .chars()
                .take_while(|&c| is_word(c))
                .map(char::len_utf8)
                .sum::<usize>();
        let word = doc.text[start..end].to_string();
        if word.is_empty() {
            self.message = "Put the cursor on a symbol to find its references".into();
            return;
        }
        let (path, ext) = (doc.path.clone(), doc.ext());
        let launch = (!self.path_is_ignored(&path)).then(|| self.launch_server(&ext, false)).flatten();
        let Some(launch) = launch else {
            // No language server: a whole-word search over the project instead.
            self.show_search();
            self.search.opts.query = word;
            self.search.opts.whole_word = true;
            self.search.opts.regex = false;
            self.start_search(&self.ctx.clone());
            return;
        };
        let Some(Tab::File(doc)) = self.tabs.get_mut(self.active) else {
            return;
        };
        let ix = doc.ed.index(&doc.text, doc.version).clone();
        let p = position(&doc.text, &ix, start);
        let (text, revision) = (doc.text.clone(), (doc.id, doc.version));
        let ignored: Vec<String> = self.ignored.clone();
        let root = self.root.clone();
        let rx = background(&self.ctx, move || {
            let client = launch.wait().ok_or("The language server didn't start")?;
            let locs = client.references(&path, &text, revision, p, Duration::from_secs(10))?;
            let mut rows = Vec::new();
            let mut cache: std::collections::HashMap<PathBuf, Arc<Vec<String>>> = Default::default();
            for loc in locs {
                if let Some(root) = &root
                    && crate::workspace::relative_path(root, &loc.path).is_some_and(|r| crate::workspace::is_ignored(&r, &ignored))
                {
                    continue;
                }
                let lines = cache
                    .entry(loc.path.clone())
                    .or_insert_with(|| {
                        Arc::new(
                            std::fs::read_to_string(&loc.path)
                                .unwrap_or_default()
                                .lines()
                                .map(str::to_string)
                                .collect(),
                        )
                    })
                    .clone();
                let line_text = lines.get(loc.line).cloned().unwrap_or_default();
                // UTF-16 columns to chars.
                let to_char = |c16: usize| {
                    let mut n = 0;
                    line_text
                        .chars()
                        .take_while(|ch| {
                            n += ch.len_utf16();
                            n <= c16
                        })
                        .count()
                };
                let cols = to_char(loc.col16.start)..to_char(loc.col16.end);
                rows.push(RefRow {
                    path: loc.path,
                    line: loc.line,
                    cols,
                    text: line_text.trim().to_string(),
                });
            }
            rows.sort_by(|a, b| (&a.path, a.line).cmp(&(&b.path, b.line)));
            Ok(rows)
        });
        self.assist.refs = Some(References {
            title: format!("References to `{}`", doc.text[start..end].to_owned()),
            rx: Some(rx),
            rows: Vec::new(),
            error: None,
        });
    }

    /// Popups: completion list, hover tooltip, references window.
    pub(super) fn assist_ui(&mut self, ctx: &egui::Context) {
        // Completion.
        let mut clicked = None;
        if let Some(p) = &self.assist.completion
            && !p.shown.is_empty()
            && p.caret.is_finite()
        {
            egui::Area::new(egui::Id::unique("completion"))
                .order(egui::Order::Foreground)
                .fixed_pos(p.caret.left_bottom() + egui::vec2(0.0, 2.0))
                .show(ctx, |ui| {
                    egui::Frame::popup(ui.style()).show(ui, |ui| {
                        ui.set_width(420.0);
                        egui::ScrollArea::vertical().max_height(220.0).show(ui, |ui| {
                            for (n, &i) in p.shown.iter().enumerate() {
                                let it = &p.items[i];
                                let selected = n == p.selected;
                                let r = ui
                                    .horizontal(|ui| {
                                        let label = ui.selectable_label(selected, RichText::new(&it.label).monospace());
                                        ui.weak(it.kind);
                                        if !it.detail.is_empty() {
                                            ui.add(egui::Label::new(RichText::new(&it.detail).weak().small()).truncate());
                                        }
                                        label
                                    })
                                    .inner;
                                if selected {
                                    r.scroll_to_me(None);
                                }
                                if r.clicked() {
                                    clicked = Some(n);
                                }
                            }
                        });
                    });
                });
        }
        if let Some(n) = clicked {
            if let Some(p) = &mut self.assist.completion {
                p.selected = n;
            }
            self.accept_completion();
        }

        // Hover.
        if let Some(h) = &self.assist.hover
            && (h.info.is_some() || h.diagnostic.is_some())
        {
            egui::Area::new(egui::Id::unique("hover"))
                .order(egui::Order::Tooltip)
                .fixed_pos(h.pos + egui::vec2(12.0, 18.0))
                .interactable(false)
                .show(ctx, |ui| {
                    egui::Frame::popup(ui.style()).show(ui, |ui| {
                        ui.set_max_width(560.0);
                        if let Some(d) = &h.diagnostic {
                            ui.colored_label(ui.visuals().warn_fg_color, d);
                        }
                        if let Some(info) = &h.info {
                            if h.diagnostic.is_some() {
                                ui.separator();
                            }
                            let shown: String = info.lines().take(24).collect::<Vec<_>>().join("\n");
                            ui.label(RichText::new(shown).monospace());
                        }
                    });
                });
        }

        // References.
        let mut open = None;
        let mut close = false;
        if let Some(r) = &mut self.assist.refs {
            if let Some(res) = receive(&mut r.rx) {
                match res {
                    Ok(rows) => r.rows = rows,
                    Err(e) => r.error = Some(e),
                }
            }
            let mut window_open = true;
            egui::Window::new(format!("{} ({})", r.title, r.rows.len()))
                .id(egui::Id::unique("references"))
                .open(&mut window_open)
                .default_size([640.0, 360.0])
                .collapsible(false)
                .show(ctx, |ui| {
                    if r.rx.is_some() {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.weak("Finding references…");
                        });
                    }
                    if let Some(e) = &r.error {
                        ui.colored_label(ui.visuals().error_fg_color, e);
                    }
                    let root = self.root.clone();
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .show_rows(ui, 20.0, r.rows.len(), |ui, range| {
                            for row in &r.rows[range] {
                                let rel = root
                                    .as_deref()
                                    .and_then(|root| row.path.strip_prefix(root).ok())
                                    .unwrap_or(&row.path)
                                    .display()
                                    .to_string();
                                let label = format!("{rel}:{}  {}", row.line + 1, row.text);
                                if ui.selectable_label(false, RichText::new(label).monospace()).clicked() {
                                    open = Some((row.path.clone(), row.line, row.cols.clone()));
                                }
                            }
                        });
                });
            close = !window_open;
        }
        if close {
            self.assist.refs = None;
        }
        if let Some((path, line, cols)) = open {
            self.open_at(&path, line, cols);
        }
        // Keep polling while a request is pending (results also request a repaint).
        let pending =
            self.assist.hover.as_ref().is_some_and(|h| h.rx.is_some()) || self.assist.completion.as_ref().is_some_and(|p| p.rx.is_some());
        if pending {
            ctx.request_repaint_after(Duration::from_millis(50));
        }
    }
}

/// `x1` is part of a word, `12` is a number (no completion after digits of a number).
fn is_word_before_digit(text: &str) -> bool {
    text.chars()
        .rev()
        .find(|c| !c.is_ascii_digit())
        .is_some_and(|c| c.is_alphabetic() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(label: &str) -> lsp::Completion {
        lsp::Completion {
            label: label.into(),
            kind: "",
            detail: String::new(),
            insert: label.into(),
            filter: label.into(),
            sort: label.into(),
        }
    }

    #[test]
    fn ranks_prefix_then_case_insensitive_then_subsequence() {
        let items = vec![item("to_string"), item("Total"), item("tostr"), item("into_iter"), item("format")];
        let ranked: Vec<&str> = rank(&items, "to").iter().map(|&i| items[i].label.as_str()).collect();
        assert_eq!(ranked, ["to_string", "tostr", "Total", "into_iter"]);
        assert_eq!(word_start("let foo_bar", 11), 4);
        assert!(is_word_before_digit("x1"));
        assert!(!is_word_before_digit("= 12"));
    }
}
