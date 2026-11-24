use super::{App, Doc, Side, Tab, background, receive};
use crate::{icons as ic, lsp, outline::Tree, symbols};
use eframe::egui::{self, RichText};
use std::sync::{Arc, Weak, mpsc::Receiver};
use std::time::Duration;

const DEBOUNCE: Duration = Duration::from_millis(500);

#[derive(Default)]
pub(super) struct Outline {
    last_active: Option<(lsp::Revision, usize)>,
    snapshot: Option<Snapshot>,
    rx: Option<Receiver<Snapshot>>,
}

struct Snapshot {
    revision: lsp::Revision,
    client: Option<Weak<lsp::Client>>,
    tree: Tree,
}

impl Snapshot {
    fn is_current(&self, revision: lsp::Revision, client: Option<&Arc<lsp::Client>>) -> bool {
        self.revision == revision && self.client.as_ref().map(Weak::as_ptr) == client.map(Arc::as_ptr)
    }
}

impl App {
    pub(super) fn document_symbol_client(&self, doc: &Doc) -> Option<&Arc<lsp::Client>> {
        if self.path_is_ignored(&doc.path) {
            return None;
        }
        let mut clients = self.lsp.clients().peekable();
        clients.peek()?;
        let servers = lsp::servers_for(&doc.ext());
        clients.find(|client| servers.iter().any(|spec| spec.name == client.name))
    }

    pub(super) fn poll_outline(&mut self) {
        let Some(mut result) = receive(&mut self.outline.rx) else { return };
        if let Some(Tab::File(doc)) = self.tabs.get(self.active)
            && result.revision.0 == doc.id
        {
            if let Some(previous) = &mut self.outline.snapshot
                && previous.revision.0 == doc.id
            {
                result.tree.restore_collapsed(&mut previous.tree);
            }
            self.outline.snapshot = Some(result);
        }
    }

    // Called after the editor and tabs have handled this frame's input.
    pub(super) fn refresh_outline(&mut self) {
        if !self.settings.show_outline || !self.side_open || self.side != Side::Explorer {
            return;
        }
        let doc = match self.tabs.get(self.active) {
            Some(Tab::File(doc)) => Some(doc),
            _ => None,
        };
        let active = doc.map(|doc| {
            if self.settings.outline_expanded {
                ((doc.id, doc.version), doc.cursor)
            } else {
                ((doc.id, 0), 0)
            }
        });
        // Explorer is drawn before the editor, so changes made there need one more frame.
        if self.outline.last_active != active {
            self.outline.last_active = active;
            self.ctx.request_repaint();
        }
        let Some(doc) = doc.filter(|_| self.settings.outline_expanded && self.outline.rx.is_none()) else {
            return;
        };
        let revision = (doc.id, doc.version);
        let client = self.document_symbol_client(doc).filter(|client| !client.is_starting());
        if self
            .outline
            .snapshot
            .as_ref()
            .is_some_and(|current| current.is_current(revision, client))
        {
            return;
        }
        if let Some(delay) = doc.edited_at.and_then(|edit| DEBOUNCE.checked_sub(edit.elapsed()))
            && !delay.is_zero()
        {
            self.ctx.request_repaint_after(delay);
            return;
        }
        let (path, text, client) = (doc.path.clone(), doc.text.clone(), client.cloned());
        let (root, ignored) = (self.root.clone(), self.ignored.clone());
        self.outline.rx = Some(background(&self.ctx, move || {
            let source = client.as_ref().map(Arc::downgrade);
            let project = root.as_deref().map(|root| (root, ignored.as_slice()));
            let tree = Tree::new(symbols::document(&path, &text, revision, client, project, None));
            Snapshot {
                revision,
                client: source,
                tree,
            }
        }));
    }

    pub(super) fn show_outline(&mut self, ui: &mut egui::Ui) {
        if !self.settings.show_outline || !matches!(self.tabs.get(self.active), Some(Tab::File(_))) {
            return;
        }
        let expanded = self.settings.outline_expanded;
        let header_height = ui.spacing().interact_size.y + 6.0;
        let available = ui.available_height();
        let height = if expanded {
            (available * 0.4).max(header_height)
        } else {
            header_height
        };
        egui::Panel::bottom("outline")
            .resizable(false)
            .exact_size(height.min(available))
            .frame(egui::Frame::NONE.inner_margin(egui::Margin::symmetric(0, 3)))
            .show(ui, |ui| {
                let caret = if expanded { ic::CARET_DOWN } else { ic::CARET_RIGHT };
                let label = RichText::new(format!("{caret} OUTLINE")).small().strong();
                if ui.add(egui::Button::new(label).frame(false)).clicked() {
                    self.settings.outline_expanded = !expanded;
                    self.save_settings(ui.ctx());
                    ui.ctx().request_repaint();
                }
                if expanded {
                    self.show_outline_rows(ui);
                }
            });
    }

    fn show_outline_rows(&mut self, ui: &mut egui::Ui) {
        let Some(Tab::File(doc)) = self.tabs.get_mut(self.active) else {
            return;
        };
        let Some(snapshot) = &mut self.outline.snapshot else {
            ui.weak("Finding symbols…");
            return;
        };
        let active = snapshot.revision.0 == doc.id;
        if !active {
            ui.weak("Finding symbols…");
        }
        let tree = &mut snapshot.tree;
        if tree.rows.is_empty() {
            if active {
                ui.weak("No symbols found");
            }
            return;
        }

        let selected = active.then(|| tree.selected(doc.line_col().0)).flatten();
        let row_height = ui.spacing().interact_size.y;
        let mut toggle = None;
        let mut reveal = None;
        egui::ScrollArea::vertical()
            .id_salt(snapshot.revision.0)
            .auto_shrink([false, false])
            .show_rows(ui, row_height, tree.visible.len(), |ui, range| {
                if !active {
                    ui.disable();
                }
                for &index in &tree.visible[range] {
                    let row = &tree.rows[index];
                    let symbol = &row.symbol;
                    ui.horizontal(|ui| {
                        let indent = (row.depth as f32 * 12.0).min(ui.available_width() * 0.25);
                        ui.add_space(indent);
                        if let Some(expanded) = tree.expanded(index) {
                            let caret = if expanded { ic::CARET_DOWN } else { ic::CARET_RIGHT };
                            let button = egui::Button::new(caret).frame(false);
                            if ui
                                .add_sized([row_height, row_height], button)
                                .on_hover_text(if expanded { "Collapse symbol" } else { "Expand symbol" })
                                .clicked()
                            {
                                toggle = Some(index);
                            }
                        } else {
                            ui.allocate_space(egui::vec2(row_height, row_height));
                        }
                        let details = format!("{}  {}", symbol.kind, symbol.line.saturating_add(1));
                        let button = egui::Button::selectable(selected == Some(index), &symbol.name)
                            .shortcut_text(RichText::new(&details).small())
                            .truncate();
                        if ui
                            .add_sized([ui.available_width(), row_height], button)
                            .on_hover_text(format!("{}  ({details})", symbol.name))
                            .clicked()
                        {
                            reveal = Some(index);
                        }
                    });
                }
            });
        if let Some(index) = toggle {
            tree.toggle(index);
            ui.ctx().request_repaint();
        }
        if let Some(index) = reveal {
            let symbol = &tree.rows[index].symbol;
            doc.goto = Some(symbols::selection(&doc.text, symbol.line, symbol.cols.clone()));
        }
    }
}
