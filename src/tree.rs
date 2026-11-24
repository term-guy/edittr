//! Folder tree layout for the git Changes view.

use std::collections::{BTreeMap, HashSet};

#[derive(Debug, PartialEq)]
pub enum Row {
    /// `path` is the folder's full repo-relative path; `label` may span several
    /// folders when single-child folders are merged ("src/ui").
    Folder {
        depth: usize,
        path: String,
        label: String,
        files: usize,
        collapsed: bool,
    },
    /// Index into the input slice.
    File { depth: usize, index: usize },
}

#[derive(Default)]
struct Node {
    dirs: BTreeMap<String, Node>,
    files: Vec<(String, usize)>,
}

impl Node {
    fn count(&self) -> usize {
        self.files.len() + self.dirs.values().map(Node::count).sum::<usize>()
    }
}

/// Lay out `paths` as a folder tree. Folders whose path is in `collapsed` are shown
/// closed. Folders come before files; both sorted case-insensitively.
pub fn layout(paths: &[&str], collapsed: &HashSet<String>) -> Vec<Row> {
    let mut root = Node::default();
    for (i, p) in paths.iter().enumerate() {
        let mut parts: Vec<&str> = p.split('/').filter(|s| !s.is_empty()).collect();
        let Some(name) = parts.pop() else { continue };
        let mut node = &mut root;
        for dir in parts {
            node = node.dirs.entry(dir.to_string()).or_default();
        }
        node.files.push((name.to_string(), i));
    }
    let mut rows = Vec::new();
    emit(&root, "", 0, collapsed, &mut rows);
    rows
}

fn emit(node: &Node, prefix: &str, depth: usize, collapsed: &HashSet<String>, rows: &mut Vec<Row>) {
    let mut dirs: Vec<(&String, &Node)> = node.dirs.iter().collect();
    dirs.sort_by_key(|(name, _)| name.to_lowercase());
    for (name, mut child) in dirs {
        let mut label = name.clone();
        // Merge chains of folders that contain only one folder.
        while child.files.is_empty() && child.dirs.len() == 1 {
            let (next_name, next) = child.dirs.iter().next().unwrap();
            label = format!("{label}/{next_name}");
            child = next;
        }
        let path = if prefix.is_empty() {
            label.clone()
        } else {
            format!("{prefix}/{label}")
        };
        let is_collapsed = collapsed.contains(&path);
        rows.push(Row::Folder {
            depth,
            path: path.clone(),
            label,
            files: child.count(),
            collapsed: is_collapsed,
        });
        if !is_collapsed {
            emit(child, &path, depth + 1, collapsed, rows);
        }
    }
    let mut files: Vec<&(String, usize)> = node.files.iter().collect();
    files.sort_by_key(|(name, _)| name.to_lowercase());
    rows.extend(files.into_iter().map(|&(_, index)| Row::File { depth, index }));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(rows: &[Row], paths: &[&str]) -> Vec<String> {
        rows.iter()
            .map(|r| match r {
                Row::Folder {
                    depth,
                    label,
                    files,
                    collapsed,
                    ..
                } => {
                    format!("{}{}{label}/ ({files})", "  ".repeat(*depth), if *collapsed { "+" } else { "-" })
                }
                Row::File { depth, index } => format!("{}{}", "  ".repeat(*depth), paths[*index].rsplit('/').next().unwrap()),
            })
            .collect()
    }

    #[test]
    fn builds_compressed_sorted_tree() {
        let paths = [
            "README.md",
            "src/ui/widgets/button.rs",
            "src/ui/widgets/Label.rs",
            "src/main.rs",
            "docs/a/b/c.md",
            "Cargo.toml",
        ];
        let rows = layout(&paths, &HashSet::new());
        assert_eq!(
            render(&rows, &paths),
            [
                "-docs/a/b/ (1)", // single-child chain merged
                "  c.md",
                "-src/ (3)",
                "  -ui/widgets/ (2)",
                "    button.rs",
                "    Label.rs",
                "  main.rs",
                "Cargo.toml",
                "README.md",
            ]
        );
        // Folder paths are full repo paths, usable for `git add -- <path>`.
        assert!(
            rows.iter()
                .any(|r| matches!(r, Row::Folder { path, .. } if path == "src/ui/widgets"))
        );
    }

    #[test]
    fn collapsed_folders_hide_children() {
        let paths = ["src/a.rs", "src/b/c.rs", "x.txt"];
        let collapsed: HashSet<String> = ["src".to_string()].into();
        assert_eq!(render(&layout(&paths, &collapsed), &paths), ["+src/ (2)", "x.txt"]);
    }
}
