use super::{App, Tab, background, icon_button, receive};
use crate::{
    icons as ic, lsp,
    lsp::call_hierarchy::{Direction, Item},
    symbols::Columns,
};
use eframe::egui::{self, RichText};
use std::{
    path::Path,
    sync::{Arc, Weak, mpsc::Receiver},
    time::Duration,
};

const TIMEOUT: Duration = Duration::from_secs(10);
type Items = Result<Vec<Item>, String>;

pub(super) struct CallHierarchy {
    launch: Weak<lsp::Launch>,
    prepare: Option<Receiver<Items>>,
    message: Option<String>,
    direction: Direction,
    incoming: Tree,
    outgoing: Tree,
}

#[derive(Default)]
struct Tree {
    nodes: Vec<Node>,
    roots: Vec<usize>,
    visible: Vec<(usize, usize)>,
    pending: Vec<(usize, Option<Receiver<Items>>)>,
}

struct Node {
    item: Arc<Item>,
    expanded: bool,
    children: Children,
}

enum Children {
    Unrequested,
    Pending,
    Ready(Vec<usize>),
    Failed(String),
}

impl Tree {
    fn add(&mut self, items: impl IntoIterator<Item = Arc<Item>>) -> Vec<usize> {
        let first = self.nodes.len();
        self.nodes.extend(items.into_iter().map(|item| Node {
            item,
            expanded: false,
            children: Children::Unrequested,
        }));
        (first..self.nodes.len()).collect()
    }

    fn update_visible(&mut self) {
        self.visible.clear();
        let mut stack = vec![(0, self.roots.iter())];
        while let Some((depth, siblings)) = stack.last_mut() {
            let Some(&index) = siblings.next() else {
                stack.pop();
                continue;
            };
            let depth = *depth;
            self.visible.push((index, depth));
            let node = &self.nodes[index];
            if node.expanded
                && let Children::Ready(children) = &node.children
                && !children.is_empty()
            {
                stack.push((depth + 1, children.iter()));
            }
        }
    }

    fn poll(&mut self) {
        let mut pending = std::mem::take(&mut self.pending);
        let mut changed = false;
        pending.retain_mut(|(index, rx)| {
            let result = receive(rx).or_else(|| rx.is_none().then(|| Err("Call hierarchy request ended unexpectedly".into())));
            if let Some(result) = result {
                self.nodes[*index].children = match result {
                    Ok(items) => Children::Ready(self.add(items.into_iter().map(Arc::new))),
                    Err(err) => Children::Failed(err),
                };
                changed = true;
            }
            rx.is_some()
        });
        self.pending = pending;
        if changed {
            self.update_visible();
        }
    }

    fn toggle(&mut self, index: usize, direction: Direction, launch: &Weak<lsp::Launch>, ctx: &egui::Context) {
        let node = &mut self.nodes[index];
        node.expanded = !node.expanded;
        if node.expanded && matches!(node.children, Children::Unrequested | Children::Failed(_)) {
            if let Some(launch) = launch.upgrade() {
                let item = node.item.clone();
                self.pending.push((
                    index,
                    Some(background(ctx, move || {
                        let client = launch.wait().ok_or("The language server stopped")?;
                        client.hierarchy_calls(item.wire.clone(), direction, TIMEOUT)
                    })),
                ));
                node.children = Children::Pending;
            } else {
                node.children = Children::Failed("The language server stopped".into());
            }
        }
        self.update_visible();
    }
}

impl CallHierarchy {
    fn tree_mut(&mut self) -> &mut Tree {
        match self.direction {
            Direction::Incoming => &mut self.incoming,
            Direction::Outgoing => &mut self.outgoing,
        }
    }

    fn toggle(&mut self, index: usize, ctx: &egui::Context) {
        let direction = self.direction;
        let launch = self.launch.clone();
        self.tree_mut().toggle(index, direction, &launch, ctx);
        ctx.request_repaint();
    }

    fn expand_first_root(&mut self, ctx: &egui::Context) {
        let tree = self.tree_mut();
        if let Some(&first) = tree.roots.first()
            && matches!(tree.nodes[first].children, Children::Unrequested)
        {
            self.toggle(first, ctx);
        }
    }

    #[cfg(feature = "tour")]
    pub(super) fn loading(&self) -> bool {
        self.prepare.is_some() || !self.incoming.pending.is_empty() || !self.outgoing.pending.is_empty()
    }

    fn ui(&mut self, ui: &mut egui::Ui, project: Option<&Path>) -> (bool, Option<Arc<Item>>) {
        let mut close = false;
        let mut direction = self.direction;
        ui.horizontal(|ui| {
            ui.strong("Call Hierarchy");
            ui.separator();
            ui.selectable_value(&mut direction, Direction::Incoming, "Incoming calls");
            ui.selectable_value(&mut direction, Direction::Outgoing, "Outgoing calls");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                close = icon_button(ui, ic::X).on_hover_text("Close Call Hierarchy").clicked();
            });
        });
        ui.separator();
        if direction != self.direction {
            self.direction = direction;
            self.expand_first_root(ui.ctx());
        }
        if self.prepare.is_some() {
            ui.weak("Preparing call hierarchy…");
        }
        if let Some(message) = &self.message {
            ui.label(message);
        }

        let tree = self.tree_mut();
        let mut toggle = None;
        let mut open = None;
        let row_h = ui.spacing().interact_size.y;
        egui::ScrollArea::vertical()
            .id_salt(direction == Direction::Outgoing)
            .auto_shrink([false, false])
            .show_rows(ui, row_h, tree.visible.len(), |ui, range| {
                for &(index, depth) in &tree.visible[range] {
                    let node = &tree.nodes[index];
                    let location = &node.item.location;
                    let path = project
                        .and_then(|root| location.path.strip_prefix(root).ok())
                        .unwrap_or(&location.path);
                    ui.horizontal(|ui| {
                        ui.set_min_height(row_h);
                        ui.add_space((depth as f32 * 14.0).min(ui.available_width() * 0.4));
                        let leaf = matches!(&node.children, Children::Ready(children) if children.is_empty());
                        if leaf {
                            ui.add_space(ui.spacing().interact_size.x.min(18.0));
                        } else {
                            let caret = if node.expanded { ic::CARET_DOWN } else { ic::CARET_RIGHT };
                            let tip = if node.expanded { "Collapse calls" } else { "Expand calls" };
                            if icon_button(ui, caret).on_hover_text(tip).clicked() {
                                toggle = Some(index);
                            }
                        }
                        let label = format!("{}  {}:{}", node.item.name, path.display(), location.line.saturating_add(1));
                        let status = match &node.children {
                            Children::Pending => "Loading calls…",
                            Children::Ready(children) if children.is_empty() => "No calls",
                            Children::Failed(err) if node.expanded => err,
                            Children::Failed(_) => "Expand to retry",
                            _ => "",
                        };
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if !status.is_empty() {
                                let color = if matches!(node.children, Children::Failed(_)) {
                                    ui.visuals().error_fg_color
                                } else {
                                    ui.visuals().weak_text_color()
                                };
                                let status_label = egui::Label::new(RichText::new(status).color(color)).truncate();
                                ui.add_sized([ui.available_width() * 0.4, row_h], status_label)
                                    .on_hover_text(status);
                            }
                            let button = egui::Button::selectable(false, RichText::new(&label).monospace()).truncate();
                            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                                if ui.add(button).on_hover_text(label).clicked() {
                                    open = Some(node.item.clone());
                                }
                            });
                        });
                    });
                }
            });
        if let Some(index) = toggle {
            self.toggle(index, ui.ctx());
        }
        (close, open)
    }
}

impl App {
    pub(super) fn show_call_hierarchy(&mut self) {
        let Some(Tab::File(doc)) = self.tabs.get_mut(self.active) else {
            self.message = "Open a source file to show its call hierarchy".into();
            return;
        };
        let (path, ext, text, revision) = (doc.path.clone(), doc.ext(), doc.text.clone(), (doc.id, doc.version));
        let head = doc.ed.primary().head.min(text.len());
        let ix = doc.ed.index(&text, revision.1);
        let line = ix.line_of_byte(head);
        let pos = (line, text[ix.line_start(line)..head].encode_utf16().count());
        let launch = (!self.path_is_ignored(&path)).then(|| self.launch_server(&ext, false)).flatten();
        let mut panel = CallHierarchy {
            launch: launch.as_ref().map_or_else(Weak::new, Arc::downgrade),
            prepare: None,
            message: None,
            direction: Direction::Incoming,
            incoming: Tree::default(),
            outgoing: Tree::default(),
        };
        if let Some(launch) = launch {
            panel.prepare = Some(background(&self.ctx, move || {
                let client = launch.wait().ok_or("No language server is available for this file")?;
                client.prepare_call_hierarchy(&path, &text, revision, pos, TIMEOUT)
            }));
        } else {
            panel.message = Some("No language server is available for this file".into());
        }
        self.call_hierarchy = Some(panel);
        self.ctx.request_repaint();
    }

    pub(super) fn poll_call_hierarchy(&mut self) {
        let Some(panel) = &mut self.call_hierarchy else { return };
        let preparing = panel.prepare.is_some();
        match receive(&mut panel.prepare) {
            Some(Ok(items)) if items.is_empty() => panel.message = Some("No call hierarchy at the cursor".into()),
            Some(Ok(items)) => {
                let items: Vec<_> = items.into_iter().map(Arc::new).collect();
                for tree in [&mut panel.incoming, &mut panel.outgoing] {
                    tree.roots = tree.add(items.iter().cloned());
                    tree.update_visible();
                }
                panel.expand_first_root(&self.ctx);
            }
            Some(Err(err)) => panel.message = Some(err),
            None if preparing && panel.prepare.is_none() => panel.message = Some("Call hierarchy request ended unexpectedly".into()),
            None => {}
        }
        panel.incoming.poll();
        panel.outgoing.poll();
    }

    pub(super) fn call_hierarchy_panel(&mut self, ui: &mut egui::Ui) {
        let Some(panel) = self.call_hierarchy.as_mut() else { return };
        let mut action = (false, None);
        egui::Panel::bottom("call_hierarchy")
            .resizable(true)
            .default_size(220.0)
            .show(ui, |ui| {
                // Keep the panel's size while its content is short ("Loading…"): egui persists it from the content.
                ui.set_min_height(ui.available_height());
                action = panel.ui(ui, self.root.as_deref())
            });
        let (close, open) = action;
        if close {
            self.call_hierarchy = None;
            ui.ctx().request_repaint();
        }
        if let Some(item) = open {
            let location = &item.location;
            self.open_at(&location.path, location.line, Columns::Utf16(location.col16.clone()));
        }
    }

    #[cfg(feature = "tour")]
    pub(super) fn call_hierarchy_tour_start(&mut self) {
        let Some(Tab::File(doc)) = self.tabs.get_mut(self.active) else {
            return;
        };
        let at = doc
            .text
            .find("free_slot(&mut lanes)")
            .or_else(|| doc.text.find("lane_color(i"))
            .or_else(|| doc.text.find("add(1"))
            .or_else(|| {
                let definition = crate::goto::definitions(&doc.text).find(|d| d.kind == "Function")?;
                let ix = doc.ed.index(&doc.text, doc.version);
                Some(ix.line_start(definition.line) + doc.text[ix.line_range(&doc.text, definition.line)].find(definition.name)?)
            });
        if let Some(byte) = at {
            doc.ed.select(byte..byte);
            self.show_call_hierarchy();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visible_tree_skips_collapsed_descendants_but_keeps_siblings() {
        let item = Arc::new(Item {
            name: "f".to_owned(),
            location: lsp::Location {
                path: std::path::PathBuf::from("f.rs"),
                line: 0,
                col16: 0..1,
            },
            wire: serde_json::Value::Null,
        });
        let mut tree = Tree::default();
        tree.roots = tree.add([item.clone(), item.clone()]);
        tree.nodes[0].children = Children::Ready(tree.add([item.clone()]));
        tree.nodes[2].children = Children::Ready(tree.add([item]));
        tree.nodes[0].expanded = true;
        tree.nodes[2].expanded = true;
        tree.update_visible();
        assert_eq!(tree.visible, vec![(0, 0), (2, 1), (3, 2), (1, 0)]);
        tree.nodes[0].expanded = false;
        tree.update_visible();
        assert_eq!(tree.visible, vec![(0, 0), (1, 0)]);
    }
}
