//! Creating files and folders from the Explorer, the command palette and `Ctrl/Cmd+N`.

use super::*;

pub(super) struct NewEntry {
    /// Folder the name is relative to.
    dir: PathBuf,
    name: String,
    folder: bool,
    error: Option<String>,
}

/// Checks a name typed in the dialog: a relative path inside `dir` (subfolders allowed).
fn target(dir: &Path, name: &str) -> Result<PathBuf, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("Enter a name".into());
    }
    let relative = Path::new(name);
    if relative.is_absolute() || relative.components().any(|c| !matches!(c, std::path::Component::Normal(_))) {
        return Err("Use a name inside this folder (no \"..\" or absolute paths)".into());
    }
    Ok(dir.join(relative))
}

fn create(path: &Path, folder: bool) -> std::io::Result<()> {
    if folder {
        if path.exists() {
            return Err(std::io::Error::new(std::io::ErrorKind::AlreadyExists, "already exists"));
        }
        return std::fs::create_dir_all(path);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::OpenOptions::new().write(true).create_new(true).open(path).map(drop)
}

impl App {
    /// The folder a new file goes into by default: the active file's folder inside the project, else its root.
    fn new_file_dir(&self) -> Option<PathBuf> {
        let root = self.root.as_ref()?;
        let dir = self
            .active_path()
            .and_then(Path::parent)
            .filter(|dir| dir.starts_with(root) && dir.is_dir())
            .map(Path::to_owned);
        Some(dir.unwrap_or_else(|| root.clone()))
    }

    pub(super) fn request_new_entry(&mut self, dir: Option<PathBuf>, folder: bool) {
        let Some(dir) = dir.or_else(|| self.new_file_dir()) else {
            if folder {
                self.message = "Open a folder first".into();
            } else {
                self.new_file_save_dialog = true;
            }
            return;
        };
        self.new_entry = Some(NewEntry {
            dir,
            name: String::new(),
            folder,
            error: None,
        });
    }

    /// Without a project there is no folder to put the file in: ask where to save it.
    pub(super) fn new_file_without_folder(&mut self, frame: &eframe::Frame) {
        if !std::mem::take(&mut self.new_file_save_dialog) {
            return;
        }
        let Some(path) = rfd::FileDialog::new().set_title("New file").set_parent(frame).save_file() else {
            return;
        };
        // The dialog already asked before replacing an existing file; keep its contents.
        let created = if path.exists() { Ok(()) } else { std::fs::write(&path, "") };
        match created {
            Ok(()) => self.open_file(&workspace::plain_path(&path)),
            Err(err) => self.message = format!("Could not create {}: {err}", path.display()),
        }
    }

    pub(super) fn new_entry_dialog(&mut self, ctx: &egui::Context) {
        let Some(entry) = &mut self.new_entry else { return };
        let mut done = None;
        let root = self.root.clone();
        egui::Modal::new(egui::Id::unique("new_entry")).show(ctx, |ui| {
            ui.set_width(480.0);
            ui.heading(if entry.folder { "New folder" } else { "New file" });
            let shown = root
                .as_deref()
                .and_then(|root| workspace::relative_path(root, &entry.dir))
                .filter(|rel| !rel.is_empty())
                .map_or_else(
                    || dir_name(&entry.dir),
                    |rel| format!("{}/{rel}", dir_name(root.as_deref().unwrap_or(&entry.dir))),
                );
            ui.weak(format!("In {shown}  (use / for subfolders)"));
            let hint = if entry.folder {
                "folder name"
            } else {
                "file name, e.g. src/main.rs"
            };
            let r = ui.add(
                egui::TextEdit::singleline(&mut entry.name)
                    .hint_text(hint)
                    .desired_width(f32::INFINITY),
            );
            r.request_focus();
            if r.changed() {
                entry.error = None;
            }
            if let Some(err) = &entry.error {
                ui.colored_label(ui.visuals().error_fg_color, err);
            }
            ui.horizontal(|ui| {
                if ui.button("Create").clicked() || ui.input(|i| i.key_pressed(Key::Enter)) {
                    done = Some(true);
                }
                if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(Key::Escape)) {
                    done = Some(false);
                }
            });
        });
        match done {
            Some(true) => {
                let result = target(&entry.dir, &entry.name).and_then(|path| {
                    create(&path, entry.folder).map_err(|err| match err.kind() {
                        std::io::ErrorKind::AlreadyExists => format!("{} already exists", entry.name.trim()),
                        _ => format!("Could not create it: {err}"),
                    })?;
                    Ok(path)
                });
                match result {
                    Ok(path) => {
                        let folder = entry.folder;
                        self.new_entry = None;
                        self.created(&path, folder);
                    }
                    Err(err) => entry.error = Some(err),
                }
            }
            Some(false) => self.new_entry = None,
            None => {}
        }
    }

    /// Show what was just created: refresh the tree, expand to it, open a new file.
    fn created(&mut self, path: &Path, folder: bool) {
        let path = workspace::plain_path(path);
        if let Some(root) = self.root.clone() {
            let parents: Vec<PathBuf> = path
                .ancestors()
                .skip(1)
                .take_while(|dir| dir.starts_with(&root))
                .map(Path::to_owned)
                .collect();
            self.invalidate_tree(|dir| parents.iter().any(|p| p == dir));
            for dir in parents.into_iter().filter(|dir| *dir != root) {
                self.expanded.insert(dir);
            }
        }
        if folder {
            self.expanded.insert(path.clone());
            self.explorer_scroll_to = Some(path);
        } else {
            self.open_file(&path);
        }
        self.refresh_vcs();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_stay_inside_the_folder_and_existing_files_are_kept() {
        let dir = std::env::temp_dir().join(format!("edittr-new-file-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(target(&dir, "  ").is_err());
        assert!(target(&dir, "../outside.txt").is_err());
        assert!(target(&dir, "/etc/passwd").is_err());
        let nested = target(&dir, "src/new.rs").unwrap();
        assert_eq!(nested, dir.join("src/new.rs"));

        create(&nested, false).unwrap();
        assert_eq!(std::fs::read_to_string(&nested).unwrap(), "");
        std::fs::write(&nested, "keep").unwrap();
        assert_eq!(create(&nested, false).unwrap_err().kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read_to_string(&nested).unwrap(), "keep");

        let folder = target(&dir, "a/b").unwrap();
        create(&folder, true).unwrap();
        assert!(folder.is_dir());
        assert!(create(&folder, true).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
