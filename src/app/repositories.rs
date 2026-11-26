//! Repositories of the project folder: the one containing it plus the ones nested inside
//! (submodules, independent clones), shown as a tree at the top of Source Control. One of
//! them is active (`App::repo`): the branch, commit box, stashes and changes below are its.
//! Blame and gutter markers follow each file's own repository.

use super::{App, icon_button};
use crate::icons as ic;
use crate::vcs::{Status, Vcs};
use crate::watch::Changes;
use eframe::egui::{self, RichText};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq)]
pub(super) struct Summary {
    pub(super) root: PathBuf,
    pub(super) vcs: Vcs,
    pub(super) branch: String,
    /// Staged plus unstaged changes.
    pub(super) changes: usize,
    pub(super) error: Option<String>,
}

impl Summary {
    fn new(root: PathBuf, vcs: Vcs, status: &Result<Status, String>) -> Self {
        let (branch, changes, error) = match status {
            Ok(status) => (status.branch.clone(), status.staged.len() + status.unstaged.len(), None),
            Err(error) => (String::new(), 0, Some(error.clone())),
        };
        Self {
            root,
            vcs,
            branch,
            changes,
            error,
        }
    }
}

#[derive(Default)]
pub(super) struct Repositories {
    /// By path, so a repository comes right before the ones nested in it.
    pub(super) list: Vec<Summary>,
    /// Bumped when the user picks a repository: a refresh started earlier must not switch back.
    pub(super) choice: u64,
    collapsed: HashSet<PathBuf>,
    hidden: bool,
}

impl Repositories {
    pub(super) fn known(&self) -> Vec<(PathBuf, Vcs)> {
        self.list.iter().map(|r| (r.root.clone(), r.vcs)).collect()
    }
}

/// The innermost repository in `list` that contains `path`.
pub(super) fn owner(list: &[Summary], path: &Path) -> Option<(PathBuf, Vcs)> {
    list.iter()
        .filter(|repo| path.starts_with(&repo.root))
        .max_by_key(|repo| repo.root.components().count())
        .map(|repo| (repo.root.clone(), repo.vcs))
}

/// Summaries for `repos` (run off the UI thread). The active repository's comes from its full
/// `status`; others keep their `previous` summary unless `changes` may have touched them, and
/// the rest run their status in parallel.
pub(super) fn summarize(
    repos: &[(PathBuf, Vcs)],
    active: Option<(&Path, &Result<Status, String>)>,
    previous: &[Summary],
    changes: &Changes,
) -> Vec<Summary> {
    let everything = changes.vcs || changes.rescan || changes.rediscover;
    let mut out: Vec<Option<Summary>> = repos
        .iter()
        .map(|(root, vcs)| {
            if let Some((active, status)) = active
                && active == root
            {
                return Some(Summary::new(root.clone(), *vcs, status));
            }
            let touched = everything || changes.paths.iter().any(|path| path.starts_with(root));
            previous
                .iter()
                .find(|old| old.root == *root && old.vcs == *vcs)
                .filter(|_| !touched)
                .cloned()
        })
        .collect();
    let pending: Vec<usize> = (0..repos.len()).filter(|&i| out[i].is_none()).collect();
    let workers = std::thread::available_parallelism().map_or(4, |n| n.get()).clamp(1, 8);
    let chunk = pending.len().div_ceil(workers).max(1);
    let computed: Vec<(usize, Summary)> = std::thread::scope(|scope| {
        let handles: Vec<_> = pending
            .chunks(chunk)
            .map(|indices| {
                scope.spawn(move || {
                    indices
                        .iter()
                        .map(|&i| {
                            let (root, vcs) = &repos[i];
                            (i, Summary::new(root.clone(), *vcs, &vcs.status(root)))
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        handles.into_iter().flat_map(|h| h.join().unwrap_or_default()).collect()
    });
    for (i, summary) in computed {
        out[i] = Some(summary);
    }
    out.into_iter().flatten().collect()
}

/// One visible row of the repository tree.
#[derive(Debug, PartialEq)]
pub(super) struct Row {
    pub(super) index: usize,
    pub(super) depth: usize,
    pub(super) label: String,
    pub(super) has_children: bool,
}

/// Rows of `list` (sorted by path) as a tree by containment, skipping collapsed subtrees.
/// Labels are relative to the parent repository, or to the project folder at the top.
pub(super) fn rows(list: &[Summary], project: Option<&Path>, collapsed: &HashSet<PathBuf>) -> Vec<Row> {
    let mut rows = Vec::new();
    // Ancestors of the current row: (index in list).
    let mut stack: Vec<usize> = Vec::new();
    let mut hidden_under: Option<&Path> = None;
    for (index, repo) in list.iter().enumerate() {
        while stack.last().is_some_and(|&top| !repo.root.starts_with(&list[top].root)) {
            stack.pop();
        }
        if hidden_under.is_some_and(|dir| repo.root.starts_with(dir)) {
            continue;
        }
        hidden_under = None;
        let parent = stack.last().map(|&i| list[i].root.as_path());
        let base = parent.or(project.filter(|p| repo.root.starts_with(p) && repo.root != *p));
        let label = base
            .and_then(|base| repo.root.strip_prefix(base).ok())
            .map(|rel| rel.to_string_lossy().replace('\\', "/"))
            .filter(|rel| !rel.is_empty())
            .unwrap_or_else(|| {
                repo.root
                    .file_name()
                    .map_or_else(|| repo.root.display().to_string(), |n| n.to_string_lossy().into_owned())
            });
        let has_children = list.get(index + 1).is_some_and(|next| next.root.starts_with(&repo.root));
        rows.push(Row {
            index,
            depth: stack.len(),
            label,
            has_children,
        });
        if has_children && collapsed.contains(&repo.root) {
            hidden_under = Some(&repo.root);
        }
        stack.push(index);
    }
    rows
}

impl App {
    /// The repository a file belongs to: the innermost known one, else the active one.
    pub(super) fn repo_of(&self, path: &Path) -> Option<(PathBuf, Vcs)> {
        owner(&self.repos.list, path).or_else(|| self.repository())
    }

    /// The version control of the project folder itself (lists its files), which may differ
    /// from the active repository's when that one is nested.
    pub(super) fn project_vcs(&self) -> Option<Vcs> {
        let root = self.root.as_deref()?;
        match owner(&self.repos.list, root) {
            Some((_, vcs)) => Some(vcs),
            None => self.repo.as_deref().filter(|repo| root.starts_with(repo)).and(self.vcs),
        }
    }

    pub(super) fn select_repository(&mut self, root: PathBuf, vcs: Vcs) {
        if self.bg_task.is_some() || self.repository().is_some_and(|(r, v)| r == root && v == vcs) {
            return;
        }
        self.repos.choice += 1;
        if self.update_repository(Some((root, vcs))) {
            self.refresh_history();
        }
        self.refresh_vcs();
    }

    /// The repository tree; drawn only when the project has more than one repository.
    pub(super) fn repositories_panel(&mut self, ui: &mut egui::Ui) {
        if self.repos.list.len() < 2 {
            return;
        }
        let (caret, tip) = if self.repos.hidden {
            (ic::CARET_RIGHT, "Show repositories")
        } else {
            (ic::CARET_DOWN, "Hide repositories")
        };
        let header = ui
            .horizontal(|ui| {
                let toggle = icon_button(ui, caret).on_hover_text(tip);
                ui.label(RichText::new(format!("REPOSITORIES ({})", self.repos.list.len())).small().strong());
                toggle
            })
            .inner;
        if header.clicked() {
            self.repos.hidden = !self.repos.hidden;
        }
        if self.repos.hidden {
            ui.separator();
            return;
        }
        let rows = rows(&self.repos.list, self.root.as_deref(), &self.repos.collapsed);
        let active = self.repo.clone();
        let busy = self.bg_task.is_some();
        let mut picked = None;
        let mut toggled = None;
        egui::ScrollArea::vertical()
            .id_salt("repositories")
            .max_height(180.0)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                for row in &rows {
                    let repo = &self.repos.list[row.index];
                    let is_active = active.as_ref() == Some(&repo.root);
                    ui.horizontal(|ui| {
                        ui.add_space(row.depth as f32 * 14.0);
                        if row.has_children {
                            let collapsed = self.repos.collapsed.contains(&repo.root);
                            let icon = if collapsed { ic::CARET_RIGHT } else { ic::CARET_DOWN };
                            if icon_button(ui, icon).clicked() {
                                toggled = Some(repo.root.clone());
                            }
                        } else {
                            ui.add_space(19.0);
                        }
                        let icon = if repo.vcs == Vcs::Svn { ic::FOLDER } else { ic::GIT_BRANCH };
                        let mut text = format!("{icon} {}", row.label);
                        if !repo.branch.is_empty() {
                            text.push_str(&format!("  · {}", repo.branch));
                        }
                        let tip = match &repo.error {
                            Some(error) => format!("{} ({})\n{error}", repo.root.display(), repo.vcs.name()),
                            None if is_active => format!("{} ({}) — active", repo.root.display(), repo.vcs.name()),
                            None => format!("{} ({})\nClick to show its changes", repo.root.display(), repo.vcs.name()),
                        };
                        let response = ui
                            .add_enabled_ui(!busy || is_active, |ui| ui.selectable_label(is_active, text))
                            .inner;
                        if response.on_hover_text(tip).clicked() && !is_active {
                            picked = Some((repo.root.clone(), repo.vcs));
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if repo.error.is_some() {
                                ui.weak(ic::WARNING);
                            } else if repo.changes > 0 {
                                ui.weak(repo.changes.to_string());
                            }
                        });
                    });
                }
            });
        ui.separator();
        if let Some(root) = toggled
            && !self.repos.collapsed.remove(&root)
        {
            self.repos.collapsed.insert(root);
        }
        if let Some((root, vcs)) = picked {
            self.select_repository(root, vcs);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(root: &str) -> Summary {
        Summary {
            root: PathBuf::from(root),
            vcs: Vcs::Git,
            branch: "main".into(),
            changes: 0,
            error: None,
        }
    }

    #[test]
    fn repositories_form_a_tree_by_containment() {
        let list: Vec<_> = ["/p", "/p/a", "/p/a/deep/x", "/p/b", "/q"].into_iter().map(summary).collect();
        let shape = |collapsed: &HashSet<PathBuf>| {
            rows(&list, Some(Path::new("/p")), collapsed)
                .into_iter()
                .map(|r| (r.label, r.depth, r.has_children))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            shape(&HashSet::new()),
            [
                ("p".to_string(), 0, true),
                ("a".to_string(), 1, true),
                ("deep/x".to_string(), 2, false),
                ("b".to_string(), 1, false),
                ("q".to_string(), 0, false),
            ]
        );
        let collapsed = HashSet::from([PathBuf::from("/p/a")]);
        assert_eq!(
            shape(&collapsed),
            [
                ("p".to_string(), 0, true),
                ("a".to_string(), 1, true),
                ("b".to_string(), 1, false),
                ("q".to_string(), 0, false),
            ]
        );
    }

    #[test]
    fn nested_repositories_without_a_parent_are_labelled_from_the_project() {
        let list: Vec<_> = ["/p/one", "/p/tools/two"].into_iter().map(summary).collect();
        let labels: Vec<_> = rows(&list, Some(Path::new("/p")), &HashSet::new())
            .into_iter()
            .map(|r| (r.label, r.depth))
            .collect();
        assert_eq!(labels, [("one".to_string(), 0), ("tools/two".to_string(), 0)]);
    }

    #[test]
    fn files_belong_to_their_innermost_repository() {
        let list: Vec<_> = ["/p", "/p/a", "/p/ab"].into_iter().map(summary).collect();
        assert_eq!(owner(&list, Path::new("/p/a/src/x.rs")).unwrap().0, PathBuf::from("/p/a"));
        assert_eq!(owner(&list, Path::new("/p/ab/x.rs")).unwrap().0, PathBuf::from("/p/ab"));
        assert_eq!(owner(&list, Path::new("/p/x.rs")).unwrap().0, PathBuf::from("/p"));
        assert!(owner(&list, Path::new("/elsewhere/x.rs")).is_none());
    }
}
