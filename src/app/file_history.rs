use super::*;
use crate::file_history::{Entry, selected_lines};
use std::ops::RangeInclusive;

struct PathLog {
    repo: PathBuf,
    vcs: Vcs,
    relative: String,
    directory: bool,
    entries: Vec<Arc<Entry>>,
}

pub(super) struct History {
    pub path: PathBuf,
    pub title: String,
    log: Option<Result<PathLog, String>>,
    rx: Option<Receiver<Result<PathLog, String>>>,
    selected: Option<usize>,
    diff_rx: Option<Receiver<Result<Tab, String>>>,
}

impl App {
    pub(super) fn active_path(&self) -> Option<&Path> {
        match self.tabs.get(self.active)? {
            Tab::File(doc) => Some(&doc.path),
            Tab::Large(view) => Some(&view.path),
            Tab::Diff(diff) => Some(&diff.path),
            Tab::FileHistory(history) => Some(&history.path),
            Tab::Hex(view) => Some(&view.path),
            Tab::Graph | Tab::Settings => None,
        }
    }

    pub(super) fn file_history_menu(&mut self, ui: &mut egui::Ui, path: &Path, file: bool) {
        if ui.button("File History").clicked() {
            self.open_file_history(path, None);
            ui.close();
        }
        if file {
            if ui.button("Select for Comparison").clicked() {
                self.select_for_compare(path.to_owned());
                ui.close();
            }
            if ui
                .add_enabled(self.compare_path.is_some(), egui::Button::new("Compare with Selected"))
                .clicked()
            {
                self.compare_with_selected(path.to_owned());
                ui.close();
            }
        }
    }

    pub(super) fn active_history(&mut self, selection: bool) {
        let Some(path) = self.active_path().map(Path::to_owned) else {
            self.message = "Open a file to see its history".into();
            return;
        };
        let lines = if selection {
            match self.tabs.get_mut(self.active) {
                Some(Tab::File(doc)) => {
                    let len = doc.text.len();
                    let range = doc.ed.primary().range();
                    let index = doc.ed.index(&doc.text, doc.version);
                    selected_lines(range.start.min(len)..range.end.min(len), |byte| index.line_of_byte(byte))
                }
                _ => None,
            }
        } else {
            None
        };
        if selection && lines.is_none() {
            self.message = "Select a range of lines to see its history".into();
            return;
        }
        self.open_file_history(&path, lines);
    }

    pub(super) fn open_file_history(&mut self, path: &Path, lines: Option<RangeInclusive<usize>>) {
        let path = workspace::plain_path(path);
        let title = if let Some(range) = &lines {
            format!("{} history (lines {}–{})", dir_name(&path), range.start(), range.end())
        } else {
            format!("{} history", dir_name(&path))
        };
        let requested = path.clone();
        let rx = background(&self.ctx, move || {
            let directory = requested.is_dir();
            let mut parent = if directory {
                requested.as_path()
            } else {
                requested.parent().unwrap_or(&requested)
            };
            // An open file may have been deleted along with its containing directory.
            while !parent.is_dir() {
                parent = parent.parent().ok_or("No repository found for this path")?;
            }
            let (repo, vcs) = vcs::detect(parent).ok_or("No repository found for this path")?;
            let repo = workspace::plain_path(&repo);
            let mut relative = workspace::relative_path(&repo, &requested)
                .or_else(|| {
                    workspace::canonical(&requested)
                        .ok()
                        .and_then(|path| workspace::relative_path(&repo, &path))
                })
                .ok_or("The path is outside its repository")?;
            if relative.is_empty() {
                relative.push('.');
            }
            let entries = vcs
                .path_history(&repo, &relative, directory, lines)?
                .into_iter()
                .map(Arc::new)
                .collect();
            Ok(PathLog {
                repo,
                vcs,
                relative,
                directory,
                entries,
            })
        });
        self.show_tab(Tab::FileHistory(Box::new(History {
            path,
            title,
            log: None,
            rx: Some(rx),
            selected: None,
            diff_rx: None,
        })));
    }

    pub(super) fn poll_file_history(&mut self) {
        let Some(tab) = self.tabs.iter_mut().find_map(|tab| match tab {
            Tab::FileHistory(history) => Some(history),
            _ => None,
        }) else {
            return;
        };
        if let Some(log) = receive(&mut tab.rx) {
            tab.log = Some(log);
        }
        match receive(&mut tab.diff_rx) {
            Some(Ok(tab)) => self.show_tab(tab),
            Some(Err(err)) => self.message = err,
            None => {}
        }
    }
}

impl History {
    #[cfg(feature = "tour")]
    pub(super) fn loading(&self) -> bool {
        self.rx.is_some() || self.diff_rx.is_some()
    }

    pub(super) fn show(&mut self, ui: &mut egui::Ui, ignore_eol: bool) {
        ui.heading(&self.title);
        let Some(log) = &self.log else {
            ui.colored_label(secondary_text_color(ui.visuals()), "Loading file history…");
            return;
        };
        let log = match log {
            Ok(log) => log,
            Err(err) => {
                ui.colored_label(ui.visuals().error_fg_color, err);
                return;
            }
        };
        if log.entries.is_empty() {
            ui.colored_label(secondary_text_color(ui.visuals()), "No commits for this path.");
            return;
        }
        let row_height = ui.spacing().interact_size.y;
        let mut clicked = None;
        egui::ScrollArea::vertical()
            .id_salt(("file-history", &self.path))
            .show_rows(ui, row_height, log.entries.len(), |ui, rows| {
                for i in rows {
                    let entry = &log.entries[i];
                    let caption = format!("{}   {}   {}   {}", entry.short_hash, entry.date, entry.author, entry.subject);
                    if ui
                        .add(
                            egui::Button::selectable(self.selected == Some(i), &caption)
                                .truncate()
                                .min_size(egui::vec2(ui.available_width(), row_height)),
                        )
                        .on_hover_text(caption)
                        .clicked()
                    {
                        clicked = Some(i);
                    }
                }
            });
        if let Some(i) = clicked {
            self.select(i, ui.ctx(), ignore_eol);
        }
    }

    pub(super) fn select(&mut self, i: usize, ctx: &egui::Context, ignore_eol: bool) {
        let Some(Ok(log)) = &self.log else { return };
        let Some(entry) = log.entries.get(i).cloned() else { return };
        self.selected = Some(i);
        let (repo, vcs, relative, path) = (log.repo.clone(), log.vcs, log.relative.clone(), self.path.clone());
        let directory = log.directory;
        let worker_ctx = ctx.clone();
        self.diff_rx = Some(background(ctx, move || {
            let patch = vcs.path_history_patch(&repo, &relative, &entry)?;
            let title = format!("{} ({})", dir_name(&path), entry.short_hash);
            if !directory && hex_view::binary_patch(&patch) {
                let pair = vcs.path_history_binary_sides(&repo, &relative, &entry)?;
                let viewer = hex_view::Viewer::comparison(path, title, pair, ("Before".into(), entry.short_hash.clone()), &worker_ctx);
                return Ok(Tab::Hex(Box::new(viewer)));
            }
            let mut diff = DiffTab::new(title, path, &patch, DiffSource::Commit, ignore_eol, false);
            if !diff.sbs.rows.iter().any(|row| row.left.is_some() || row.right.is_some()) {
                diff.inline = true; // A rename-only change still has useful patch headers.
            }
            Ok(Tab::Diff(Box::new(diff)))
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::super::commit_view::tests::TestDir;
    use super::*;

    #[test]
    fn git_path_history_follows_renames_and_scopes_folder_and_line_diffs() {
        let repo = TestDir::new();
        let run = |args: &[&str]| crate::git::run(&repo.0, args).unwrap();
        run(&["init"]);
        run(&["config", "user.email", "edittr@example.com"]);
        run(&["config", "user.name", "Edittr"]);
        std::fs::create_dir(repo.0.join("dir")).unwrap();
        std::fs::write(repo.0.join("dir/original.txt"), "first\nold\nlast\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "-m", "Original"]);
        run(&["mv", "dir/original.txt", "dir/new name.txt"]);
        run(&["commit", "-m", "Rename"]);
        std::fs::write(repo.0.join("dir/new name.txt"), "first\nnew\nlast\n").unwrap();
        std::fs::write(repo.0.join("outside.txt"), "excluded\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "-m", "Edit"]);

        let log = Vcs::Git.path_history(&repo.0, "dir/new name.txt", false, None).unwrap();
        assert_eq!(
            log.iter().map(|r| r.subject.as_str()).collect::<Vec<_>>(),
            ["Edit", "Rename", "Original"]
        );
        let patch = Vcs::Git.path_history_patch(&repo.0, "dir/new name.txt", &log[2]).unwrap();
        assert!(patch.contains("dir/original.txt") && patch.contains("+old"));
        let rename = Vcs::Git.path_history_patch(&repo.0, "dir/new name.txt", &log[1]).unwrap();
        assert!(rename.contains("rename from dir/original.txt") && rename.contains("rename to dir/new name.txt"));
        let folder = Vcs::Git.path_history(&repo.0, "dir", true, None).unwrap();
        let patch = Vcs::Git.path_history_patch(&repo.0, "dir", &folder[0]).unwrap();
        assert!(patch.contains("+new"));
        assert!(!patch.contains("outside.txt"));
        let lines = Vcs::Git.path_history(&repo.0, "dir/new name.txt", false, Some(2..=2)).unwrap();
        let patch = Vcs::Git.path_history_patch(&repo.0, "dir/new name.txt", &lines[0]).unwrap();
        assert!(patch.contains("-old\n+new"));
        assert!(!patch.contains("outside.txt"));

        std::fs::write(repo.0.join("dir/binary"), [0, 255]).unwrap();
        run(&["add", "."]);
        run(&["commit", "-m", "Add binary"]);
        let log = Vcs::Git.path_history(&repo.0, "dir/binary", false, None).unwrap();
        let patch = Vcs::Git.path_history_patch(&repo.0, "dir/binary", &log[0]).unwrap();
        assert!(hex_view::binary_patch(&patch));
        let sides = Vcs::Git.path_history_binary_sides(&repo.0, "dir/binary", &log[0]).unwrap();
        assert_eq!(sides.left.size, 0);
        assert_eq!(sides.right.read(0..2).unwrap(), [0, 255]);
    }
}
