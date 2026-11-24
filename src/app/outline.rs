use super::{App, Doc, Side, Tab, background, icon_button, receive};
use crate::{
    icons as ic, lsp,
    outline::{Tree, VisibleRow},
    symbols,
};
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
                ui.horizontal(|ui| {
                    let caret = if expanded { ic::CARET_DOWN } else { ic::CARET_RIGHT };
                    let label = RichText::new(format!("{caret} OUTLINE")).small().strong();
                    if ui.add(egui::Button::new(label).frame(false)).clicked() {
                        self.settings.outline_expanded = !expanded;
                        self.save_settings(ui.ctx());
                        ui.ctx().request_repaint();
                    }
                    if expanded {
                        let grouped = self.settings.outline_grouped;
                        let (icon, tip) = if grouped {
                            (ic::LIST, "Show symbols in source order")
                        } else {
                            (ic::TREE_VIEW, "Group symbols by kind")
                        };
                        if icon_button(ui, icon).on_hover_text(tip).clicked() {
                            self.settings.outline_grouped = !grouped;
                            self.save_settings(ui.ctx());
                        }
                    }
                });
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

        let collapsed_groups = self.settings.outline_collapsed_groups;
        let grouped = self.settings.outline_grouped;
        tree.set_layout(grouped, collapsed_groups);
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
                for &item in &tree.visible[range] {
                    let index = match item {
                        VisibleRow::Group(group) => {
                            let caret = if collapsed_groups.contains(group) {
                                ic::CARET_RIGHT
                            } else {
                                ic::CARET_DOWN
                            };
                            let title = format!("{caret}  {}", group.label());
                            let button = egui::Button::selectable(selected == Some(item), RichText::new(title).strong())
                                .shortcut_text(RichText::new(tree.group_count(group).to_string()).small().weak())
                                .truncate();
                            if ui.add_sized([ui.available_width(), row_height], button).clicked() {
                                toggle = Some(item);
                            }
                            continue;
                        }
                        VisibleRow::Symbol(index) => index,
                    };
                    let row = &tree.rows[index];
                    let symbol = &row.symbol;
                    ui.horizontal(|ui| {
                        let depth = row.depth + usize::from(grouped);
                        let indent = (depth as f32 * 12.0).min(ui.available_width() * 0.25);
                        ui.add_space(indent);
                        if let Some(expanded) = tree.expanded(index) {
                            let caret = if expanded { ic::CARET_DOWN } else { ic::CARET_RIGHT };
                            let button = egui::Button::new(caret).frame(false);
                            if ui
                                .add_sized([row_height, row_height], button)
                                .on_hover_text(if expanded { "Collapse symbol" } else { "Expand symbol" })
                                .clicked()
                            {
                                toggle = Some(item);
                            }
                        } else {
                            ui.allocate_space(egui::vec2(row_height, row_height));
                        }
                        let details = format!("{}  {}", symbol.kind, symbol.line.saturating_add(1));
                        let selected = selected == Some(item);
                        let (button, tooltip) = if let Some(detail) = &symbol.detail {
                            // The value goes on the right, like the kind/line of other rows,
                            // so names stay left-aligned.
                            (
                                egui::Button::selectable(selected, &symbol.name).shortcut_text(RichText::new(detail).small().weak()),
                                format!("{}  {detail}\n{details}", symbol.name),
                            )
                        } else {
                            (
                                egui::Button::selectable(selected, &symbol.name).shortcut_text(RichText::new(&details).small()),
                                format!("{}  ({details})", symbol.name),
                            )
                        };
                        if ui
                            .add_sized([ui.available_width(), row_height], button.truncate())
                            .on_hover_text(tooltip)
                            .clicked()
                        {
                            reveal = Some(index);
                        }
                    });
                }
            });
        if let Some(index) = reveal {
            let symbol = &tree.rows[index].symbol;
            doc.goto = Some(symbols::selection(&doc.text, symbol.line, symbol.cols.clone()));
        }
        if let Some(item) = toggle {
            match item {
                VisibleRow::Group(group) => {
                    self.settings.outline_collapsed_groups.toggle(group);
                    self.save_settings(ui.ctx());
                }
                VisibleRow::Symbol(index) => tree.toggle(index),
            }
            ui.ctx().request_repaint();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{outline::Group, session::Startup, settings::SettingsWriter};
    use std::path::Path;

    #[test]
    fn grouped_outline_paints_headers_and_navigates_visible_symbols() {
        let ctx = egui::Context::default();
        let cc = eframe::CreationContext::_new_kittest(ctx.clone());
        let (writer, worker) = SettingsWriter::start();
        let startup = Startup {
            root: None,
            file: None,
            restore: false,
            window: Default::default(),
        };
        let mut app = App::new(&cc, startup, Default::default(), writer, None);
        app.settings.outline_collapsed_groups.toggle(Group::Macros);
        let path = Path::new("outline-test.h");
        let source = "#define ENABLED 1\nvoid run(void);\n";
        app.add_file_tab(path, source.to_owned(), false);
        let revision = (0, 0);
        app.outline.snapshot = Some(Snapshot {
            revision,
            client: None,
            tree: Tree::new(symbols::document(path, source, revision, None, None, None)),
        });
        let mut render = |events| {
            let mut output = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(290.0, 400.0))),
                    events,
                    ..Default::default()
                },
                |ui| app.show_outline(ui),
            );
            output.textures_delta.clear();
            output.shapes
        };
        let shapes = render(Vec::new());
        let painted: Vec<_> = shapes
            .iter()
            .filter_map(|shape| match &shape.shape {
                egui::Shape::Text(text) => Some((text.galley.text(), text.pos)),
                _ => None,
            })
            .collect();
        assert!(painted.iter().any(|(label, _)| label.ends_with("Macros")));
        assert!(painted.iter().any(|(label, _)| label.ends_with("Functions")));
        assert_eq!(painted.iter().filter(|(label, _)| *label == "1").count(), 2);
        assert!(!painted.iter().any(|(label, _)| *label == "ENABLED"));

        let (_, position) = painted.iter().find(|(label, _)| *label == "run").unwrap();
        let pos = *position + egui::vec2(3.0, 3.0);
        render(vec![egui::Event::PointerMoved(pos)]);
        for pressed in [true, false] {
            render(vec![egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::NONE,
            }]);
        }
        let Some(Tab::File(doc)) = app.tabs.first() else {
            panic!("expected an open file")
        };
        assert_eq!(doc.goto, Some((23, 26)));
        drop(app);
        worker.join().unwrap();
    }
}
