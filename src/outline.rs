use crate::symbols::Symbol;
use eframe::egui::Id;
use std::collections::{HashMap, HashSet};
use std::ops::Range;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Group {
    Macros,
    Constants,
    Variables,
    Types,
    Functions,
    Modules,
    Other,
}

impl Group {
    pub(crate) const ALL: [Self; 7] = [
        Self::Macros,
        Self::Constants,
        Self::Variables,
        Self::Types,
        Self::Functions,
        Self::Modules,
        Self::Other,
    ];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Macros => "Macros",
            Self::Constants => "Constants",
            Self::Variables => "Variables",
            Self::Types => "Types",
            Self::Functions => "Functions",
            Self::Modules => "Modules/Namespaces",
            Self::Other => "Other",
        }
    }

    fn of(symbol: &Symbol) -> Self {
        match symbol.kind.as_str() {
            "Macro" => Self::Macros,
            "Constant" | "Enum member" => Self::Constants,
            "Variable" | "Property" | "Field" => Self::Variables,
            "Class" | "Struct" | "Enum" | "Interface" | "Type" | "Type parameter" => Self::Types,
            "Function" | "Method" | "Constructor" | "Operator" => Self::Functions,
            "Module" | "Namespace" | "Package" => Self::Modules,
            _ => Self::Other,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct CollapsedGroups(u8);

impl CollapsedGroups {
    pub(crate) fn contains(self, group: Group) -> bool {
        self.0 & (1 << group as u8) != 0
    }

    pub(crate) fn toggle(&mut self, group: Group) {
        self.0 ^= 1 << group as u8;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum VisibleRow {
    Group(Group),
    Symbol(usize),
}

impl VisibleRow {
    #[cfg(test)]
    pub(crate) fn symbol_index(self) -> usize {
        match self {
            Self::Symbol(index) => index,
            Self::Group(group) => panic!("expected a symbol, got {group:?}"),
        }
    }
}

pub(crate) struct Row {
    pub(crate) symbol: Symbol,
    pub(crate) depth: usize,
    group: Group,
    key: Id,
    parent: Option<usize>,
    subtree_end: usize,
}

pub(crate) struct Tree {
    pub(crate) rows: Vec<Row>,
    pub(crate) visible: Vec<VisibleRow>,
    groups: [Vec<Range<usize>>; Group::ALL.len()],
    layout: (bool, CollapsedGroups),
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
                let symbol_group = parent.map_or_else(|| Group::of(&symbol), |p| rows[p].group);
                rows.push(Row {
                    symbol,
                    depth,
                    group: symbol_group,
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
        let mut groups = std::array::from_fn(|_| Vec::new());
        let mut index = 0;
        while let Some(row) = rows.get(index) {
            groups[row.group as usize].push(index..row.subtree_end);
            index = row.subtree_end;
        }
        let mut tree = Self {
            rows,
            visible: Vec::new(),
            groups,
            layout: (true, CollapsedGroups::default()),
            collapsed: HashSet::new(),
            cursor: None,
        };
        tree.update_visible();
        tree
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

    pub(crate) fn group_count(&self, group: Group) -> usize {
        self.groups[group as usize].len()
    }

    pub(crate) fn set_layout(&mut self, grouped: bool, collapsed: CollapsedGroups) {
        let layout = (grouped, collapsed);
        if self.layout != layout {
            self.layout = layout;
            self.update_visible();
        }
    }

    fn update_visible(&mut self) {
        fn append(visible: &mut Vec<VisibleRow>, rows: &[Row], collapsed: &HashSet<Id>, range: Range<usize>) {
            let mut index = range.start;
            while index < range.end {
                let row = &rows[index];
                visible.push(VisibleRow::Symbol(index));
                index = if collapsed.contains(&row.key) { row.subtree_end } else { index + 1 };
            }
        }

        self.visible.clear();
        let (grouped, collapsed_groups) = self.layout;
        if !grouped {
            append(&mut self.visible, &self.rows, &self.collapsed, 0..self.rows.len());
            return;
        }
        for group in Group::ALL {
            let roots = &self.groups[group as usize];
            if roots.is_empty() {
                continue;
            }
            self.visible.push(VisibleRow::Group(group));
            if !collapsed_groups.contains(group) {
                for range in roots {
                    append(&mut self.visible, &self.rows, &self.collapsed, range.clone());
                }
            }
        }
    }

    pub(crate) fn selected(&mut self, line: usize) -> Option<VisibleRow> {
        if let Some((cached, index)) = self.cursor
            && cached == line
        {
            return index.map(|i| self.visible_ancestor(i));
        }
        let index = self
            .rows
            .iter()
            .enumerate()
            .filter(|(_, row)| row.symbol.lines.contains(&line))
            .max_by_key(|(_, row)| (row.depth, *row.symbol.lines.start()))
            .map(|(i, _)| i);
        self.cursor = Some((line, index));
        index.map(|i| self.visible_ancestor(i))
    }

    fn visible_ancestor(&self, index: usize) -> VisibleRow {
        let group = self.rows[index].group;
        let (grouped, collapsed_groups) = self.layout;
        if grouped && collapsed_groups.contains(group) {
            return VisibleRow::Group(group);
        }
        let mut visible = index;
        let mut parent = self.rows[index].parent;
        while let Some(i) = parent {
            let row = &self.rows[i];
            if self.collapsed.contains(&row.key) {
                visible = i;
            }
            parent = row.parent;
        }
        VisibleRow::Symbol(visible)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::symbols;
    use std::path::Path;

    #[test]
    fn cursor_selection_falls_back_to_collapsed_parents_and_groups() {
        use VisibleRow::{Group as Header, Symbol};

        let path = Path::new("model.py");
        let text = "class Model:\n    def read(self):\n        return 1\n\nTOP = 0\n";
        let mut tree = Tree::new(symbols::document(path, text, (0, 0), None, None, None));
        assert_eq!(tree.rows[1].depth, 1);
        assert_eq!(tree.selected(2), Some(Symbol(1)));
        assert_eq!(tree.selected(3), None);
        tree.toggle(0);
        assert!(!tree.visible.contains(&Symbol(1)));
        assert_eq!(tree.selected(2), Some(Symbol(0)));

        let mut collapsed = CollapsedGroups::default();
        collapsed.toggle(Group::Types);
        tree.set_layout(true, collapsed);
        assert_eq!(tree.visible, [Header(Group::Constants), Symbol(2), Header(Group::Types)]);
        assert_eq!(tree.selected(2), Some(Header(Group::Types)));
        tree.set_layout(false, collapsed);
        assert_eq!(tree.visible, [Symbol(0), Symbol(2)]);
        assert_eq!(tree.selected(2), Some(Symbol(0)));

        let shifted = format!("\n{text}");
        let mut refreshed = Tree::new(symbols::document(path, &shifted, (0, 1), None, None, None));
        refreshed.restore_collapsed(&mut tree);
        refreshed.set_layout(true, collapsed);
        assert_eq!(refreshed.selected(3), Some(Header(Group::Types)));
        collapsed.toggle(Group::Types);
        refreshed.set_layout(true, collapsed);
        assert_eq!(refreshed.selected(3), Some(Symbol(0)));
        refreshed.toggle(0);
        assert_eq!(refreshed.selected(3), Some(Symbol(1)));
    }

    #[test]
    fn groups_present_roots_in_kind_order_and_keep_nested_source_order() {
        use VisibleRow::{Group as Header, Symbol as Item};

        let path = Path::new("lib.rs");
        let text = "fn first() {}\nstatic mut STATE: i32 = 0;\nconst LIMIT: i32 = 1;\nmacro_rules! update {}\nfn second() {}\nstruct Model {\n    fn nested() {}\n}\nstatic DEFAULT: i32 = 1;\nmod helpers {}\n";
        let mut symbols = symbols::document(path, text, (0, 0), None, None, None);
        // These kinds can come from an LSP server rather than the fallback.
        symbols[2].kind = "Enum member".to_owned();
        symbols.push(Symbol {
            name: "event".to_owned(),
            kind: "Event".to_owned(),
            detail: None,
            container: None,
            path: path.to_owned(),
            line: 10,
            cols: (0..5).into(),
            lines: 10..=10,
            children: Vec::new(),
        });
        let tree = Tree::new(symbols);
        assert_eq!(Group::ALL.map(|group| tree.group_count(group)), [1, 2, 1, 1, 2, 1, 1]);
        assert_eq!(
            tree.visible,
            [
                Header(Group::Macros),
                Item(3),
                Header(Group::Constants),
                Item(2),
                Item(7),
                Header(Group::Variables),
                Item(1),
                Header(Group::Types),
                Item(5),
                Item(6),
                Header(Group::Functions),
                Item(0),
                Item(4),
                Header(Group::Modules),
                Item(8),
                Header(Group::Other),
                Item(9),
            ]
        );
        assert_eq!(tree.rows[6].symbol.name, "nested");
        assert_eq!(tree.rows[6].depth, 1);
    }
}
