use crate::symbols::Symbol;
use eframe::egui::Id;
use std::collections::{HashMap, HashSet};

pub(crate) struct Row {
    pub(crate) symbol: Symbol,
    pub(crate) depth: usize,
    key: Id,
    parent: Option<usize>,
    subtree_end: usize,
}

pub(crate) struct Tree {
    pub(crate) rows: Vec<Row>,
    pub(crate) visible: Vec<usize>,
    collapsed: HashSet<Id>,
    cursor: Option<(usize, Option<usize>)>,
}

impl Tree {
    pub(crate) fn new(symbols: Vec<Symbol>) -> Self {
        fn append(rows: &mut Vec<Row>, symbols: Vec<Symbol>, depth: usize, parent: Option<usize>, group: Id) {
            let mut counts = HashMap::new();
            let parent_kind = parent.map(|p| rows[p].symbol.kind.clone());
            for mut symbol in symbols {
                // Locals inside functions are noise in an outline (language servers don't list them).
                if symbol.kind == "Variable" && matches!(parent_kind.as_deref(), Some("Function" | "Method" | "Constructor")) {
                    continue;
                }
                let name = group.with((&symbol.name, &symbol.kind));
                let count = counts.entry(name).or_insert(0usize);
                let key = name.with(*count);
                *count += 1;
                let children = std::mem::take(&mut symbol.children);
                let i = rows.len();
                rows.push(Row {
                    symbol,
                    depth,
                    key,
                    parent,
                    subtree_end: 0,
                });
                append(rows, children, depth + 1, Some(i), key);
                rows[i].subtree_end = rows.len();
            }
        }

        let mut rows = Vec::new();
        append(&mut rows, symbols, 0, None, Id::unique("outline_symbols"));
        let visible = (0..rows.len()).collect();
        Self {
            rows,
            visible,
            collapsed: HashSet::new(),
            cursor: None,
        }
    }

    pub(crate) fn restore_collapsed(&mut self, previous: &mut Self) {
        if !previous.collapsed.is_empty() {
            self.collapsed = std::mem::take(&mut previous.collapsed);
            self.update_visible();
        }
    }

    pub(crate) fn expanded(&self, index: usize) -> Option<bool> {
        let row = &self.rows[index];
        (row.subtree_end > index + 1).then(|| !self.collapsed.contains(&row.key))
    }

    pub(crate) fn toggle(&mut self, index: usize) {
        let key = self.rows[index].key;
        if !self.collapsed.remove(&key) {
            self.collapsed.insert(key);
        }
        self.update_visible();
    }

    fn update_visible(&mut self) {
        self.visible.clear();
        let mut index = 0;
        while let Some(row) = self.rows.get(index) {
            self.visible.push(index);
            index = if self.collapsed.contains(&row.key) {
                row.subtree_end
            } else {
                index + 1
            };
        }
    }

    pub(crate) fn selected(&mut self, line: usize) -> Option<usize> {
        if let Some((cached, index)) = self.cursor
            && cached == line
        {
            return self.visible_ancestor(index);
        }
        let index = self
            .rows
            .iter()
            .enumerate()
            .filter(|(_, row)| row.symbol.lines.contains(&line))
            .max_by_key(|(_, row)| (row.depth, *row.symbol.lines.start()))
            .map(|(i, _)| i);
        self.cursor = Some((line, index));
        self.visible_ancestor(index)
    }

    fn visible_ancestor(&self, mut index: Option<usize>) -> Option<usize> {
        while let Some(i) = index {
            if self.visible.binary_search(&i).is_ok() {
                return Some(i);
            }
            index = self.rows[i].parent;
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::symbols;
    use std::path::Path;

    #[test]
    fn fallback_scope_selection_and_collapse_survive_shifted_lines() {
        let path = Path::new("model.py");
        let text = "class Model:\n    def read(self):\n        return 1\n\nTOP = 0\n";
        let mut tree = Tree::new(symbols::document(path, text, (0, 0), None, None, None));
        assert_eq!(tree.rows[1].depth, 1);
        assert_eq!(tree.selected(2), Some(1));
        assert_eq!(tree.selected(3), None);
        tree.toggle(0);
        assert!(!tree.visible.contains(&1));
        assert_eq!(tree.selected(2), Some(0));

        let shifted = format!("\n{text}");
        let mut refreshed = Tree::new(symbols::document(path, &shifted, (0, 1), None, None, None));
        refreshed.restore_collapsed(&mut tree);
        assert_eq!(refreshed.selected(3), Some(0));
        refreshed.toggle(0);
        assert_eq!(refreshed.selected(3), Some(1));
    }
}
