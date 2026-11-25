//! Renaming files and folders from the Explorer (F2 on the selected row, or its context menu).

use super::*;

pub(super) struct Renaming {
    path: PathBuf,
    folder: bool,
    name: String,
    error: Option<String>,
    /// The dialog selected the name (without its extension) on its first frame.
    selected: bool,
}

/// The new path for `name` next to `path`, if the name is usable.
fn renamed(path: &Path, name: &str) -> Result<PathBuf, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("Enter a name".into());
    }
    if name == "." || name == ".." || name.contains(['/', '\\']) {
        return Err("Use a plain name (no / or \\)".into());
    }
    Ok(path.with_file_name(name))
}

/// Characters to select when the dialog opens: the name without its extension.
fn stem_chars(name: &str, folder: bool) -> usize {
    let stem = match name.rfind('.') {
        Some(dot) if dot > 0 && !folder => &name[..dot],
        _ => name,
    };
    stem.chars().count()
}

fn rename_on_disk(from: &Path, to: &Path) -> std::io::Result<()> {
    // A case-only rename on a case-insensitive disk sees the target as "existing": it is the same entry.
    let same_entry = from.to_string_lossy().to_lowercase() == to.to_string_lossy().to_lowercase();
    if to.symlink_metadata().is_ok() && !same_entry {
        return Err(std::io::Error::new(std::io::ErrorKind::AlreadyExists, "already exists"));
    }
    std::fs::rename(from, to)
}

/// Where `path` ends up when `from` is renamed to `to` (None: not affected).
fn moved(path: &Path, from: &Path, to: &Path) -> Option<PathBuf> {
    let rest = path.strip_prefix(from).ok()?;
    Some(if rest.as_os_str().is_empty() {
        to.to_owned()
    } else {
        to.join(rest)
    })
}

impl App {
    /// F2 on the selected Explorer row renames it; checked before the global shortcuts, where F2 renames a symbol.
    pub(super) fn explorer_rename_shortcut(&mut self, ctx: &egui::Context) {
        let Some((path, folder)) = self.explorer_key_target(ctx) else {
            return;
        };
        if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::F2)) {
            self.request_rename(path, folder);
        }
    }

    pub(super) fn rename_active_file(&mut self) {
        match self.active_path().map(Path::to_owned) {
            Some(path) if !path.is_dir() => self.request_rename(path, false),
            _ => self.message = "Open a file to rename it".into(),
        }
    }

    pub(super) fn request_rename(&mut self, path: PathBuf, folder: bool) {
        if self.root.as_ref().is_none_or(|root| delete::relative_target(root, &path).is_none()) {
            self.message = "Choose an entry inside the open project folder".into();
            return;
        }
        if self.document_action_pending(|p| moved(p, &path, &path).is_some()) {
            self.message = "Wait for the file to finish saving before renaming it".into();
            return;
        }
        let name = dir_name(&path);
        self.renaming = Some(Renaming {
            path,
            folder,
            name,
            error: None,
            selected: false,
        });
    }

    pub(super) fn rename_dialog(&mut self, ctx: &egui::Context) {
        let Some(renaming) = &mut self.renaming else { return };
        let mut done = None;
        egui::Modal::new(egui::Id::unique("rename_entry")).show(ctx, |ui| {
            ui.set_width(420.0);
            ui.heading(if renaming.folder { "Rename folder" } else { "Rename file" });
            let edit = egui::TextEdit::singleline(&mut renaming.name).desired_width(f32::INFINITY);
            let mut output = edit.show(ui);
            if !renaming.selected {
                renaming.selected = true;
                let end = stem_chars(&renaming.name, renaming.folder);
                output.state.cursor.set_char_range(Some(egui::text::CCursorRange::two(
                    egui::text::CCursor::new(0),
                    egui::text::CCursor::new(end),
                )));
                output.state.store(ui.ctx(), output.response.id);
            }
            output.response.request_focus();
            if output.response.changed() {
                renaming.error = None;
            }
            if let Some(err) = &renaming.error {
                ui.colored_label(ui.visuals().error_fg_color, err);
            }
            ui.horizontal(|ui| {
                if ui.button("Rename").clicked() || ui.input(|i| i.key_pressed(Key::Enter)) {
                    done = Some(true);
                }
                if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(Key::Escape)) {
                    done = Some(false);
                }
            });
        });
        match done {
            Some(true) => {
                let (from, name) = (renaming.path.clone(), renaming.name.clone());
                let result = renamed(&from, &name).and_then(|to| {
                    if to == from {
                        return Ok(to);
                    }
                    rename_on_disk(&from, &to).map_err(|err| match err.kind() {
                        std::io::ErrorKind::AlreadyExists => format!("{} already exists", name.trim()),
                        _ => format!("Could not rename: {err}"),
                    })?;
                    Ok(to)
                });
                match result {
                    Ok(to) => {
                        self.renaming = None;
                        if to != from {
                            self.renamed(&from, &to);
                        }
                    }
                    Err(err) => renaming.error = Some(err),
                }
            }
            Some(false) => self.renaming = None,
            None => {}
        }
    }

    /// Follow a rename everywhere: open tabs (unsaved edits kept), expanded folders, the selection, the tree.
    fn renamed(&mut self, from: &Path, to: &Path) {
        for i in 0..self.tabs.len() {
            if let Tab::File(doc) = &self.tabs[i]
                && !doc.untitled
                && let Some(path) = moved(&doc.path, from, to)
            {
                self.retarget(i, path);
            }
        }
        let expanded: Vec<(PathBuf, PathBuf)> = self
            .expanded
            .iter()
            .filter_map(|p| Some((p.clone(), moved(p, from, to)?)))
            .collect();
        for (old, new) in expanded {
            self.expanded.remove(&old);
            self.expanded.insert(new);
        }
        if let Some(selection) = &mut self.explorer_selection
            && let Some(path) = moved(&selection.path, from, to)
        {
            selection.path = path;
        }
        self.explorer_revealed = None;
        if let Some(parent) = from.parent() {
            self.invalidate_tree(|dir| dir == parent || moved(dir, from, to).is_some());
        }
        self.refresh_vcs();
        self.message = format!("Renamed {} to {}", dir_name(from), dir_name(to));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_selection_and_moved_paths() {
        let file = Path::new("/p/src/main.rs");
        assert_eq!(renamed(file, " lib.rs ").unwrap(), Path::new("/p/src/lib.rs"));
        for bad in ["", "  ", "a/b", "a\\b", "..", "."] {
            assert!(renamed(file, bad).is_err(), "{bad:?}");
        }
        assert_eq!(stem_chars("main.rs", false), 4);
        assert_eq!(stem_chars(".gitignore", false), 10);
        assert_eq!(stem_chars("v1.2", true), 4);
        let (from, to) = (Path::new("/p/src"), Path::new("/p/code"));
        assert_eq!(moved(Path::new("/p/src/a/b.rs"), from, to).unwrap(), Path::new("/p/code/a/b.rs"));
        assert_eq!(moved(from, from, to).unwrap(), to);
        assert!(moved(Path::new("/p/srcs/x"), from, to).is_none());
    }

    #[test]
    fn renames_refuse_to_overwrite() {
        let dir = std::env::temp_dir().join(format!("edittr-rename-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (a, b) = (dir.join("a.txt"), dir.join("b.txt"));
        std::fs::write(&a, "a").unwrap();
        std::fs::write(&b, "b").unwrap();
        assert_eq!(rename_on_disk(&a, &b).unwrap_err().kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read_to_string(&b).unwrap(), "b");
        rename_on_disk(&a, &dir.join("A.txt")).unwrap();
        assert!(dir.join("A.txt").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
