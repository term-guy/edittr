//! Explorer selection and confirmed filesystem deletion.

use super::*;
use std::fs;
use std::io;
use std::path::Component;

pub(super) struct ExplorerSelection {
    pub(super) path: PathBuf,
    folder: bool,
    row: Option<egui::Id>,
    last_clicked: bool,
}

pub(super) struct Deletion {
    root: PathBuf,
    target: Target,
    phase: Phase,
    previous_focus: Option<egui::Id>,
}

#[derive(Clone)]
struct Target {
    path: PathBuf,
    folder: bool,
}

impl Target {
    fn contains(&self, path: &Path) -> bool {
        path == self.path || (self.folder && path.starts_with(&self.path))
    }

    fn tabs(&self, tabs: &[Tab]) -> Vec<usize> {
        tabs.iter()
            .enumerate()
            .filter_map(|(i, tab)| tab.path().is_some_and(|p| self.contains(p)).then_some(i))
            .collect()
    }
}

enum Phase {
    ConfirmTrash,
    Running(Option<Receiver<Outcome>>),
    ConfirmPermanent(String),
}

enum Mode {
    Trash,
    Permanent,
}

enum Outcome {
    Removed,
    TrashFailed(String),
    Failed(io::Error),
}

fn relative_target<'a>(root: &Path, path: &'a Path) -> Option<&'a Path> {
    let relative = path.strip_prefix(root).ok()?;
    (!relative.as_os_str().is_empty() && relative.components().all(|part| matches!(part, Component::Normal(_)))).then_some(relative)
}

fn checked_entry(root: &Path, target: &Target) -> io::Result<fs::FileType> {
    let outside = || io::Error::new(io::ErrorKind::PermissionDenied, "Choose an entry inside the open project folder");
    relative_target(root, &target.path).ok_or_else(outside)?;
    let parent = target.path.parent().ok_or_else(outside)?;
    // Check the parent, so a symlink itself can be removed without following its destination.
    // Reject redirected ancestors, including a project root replaced by a symlink.
    if workspace::canonical(parent)? != parent {
        return Err(outside());
    }
    let metadata = fs::symlink_metadata(&target.path)?;
    let kind = metadata.file_type();
    #[cfg(windows)]
    if kind.is_dir() {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(outside());
        }
    }
    if kind.is_dir() != target.folder {
        return Err(io::Error::other("The entry changed type. Select it again to delete it."));
    }
    Ok(kind)
}

fn remove_entry(root: &Path, target: &Target, mode: Mode, trash: impl FnOnce(&Path) -> Result<(), String>) -> Outcome {
    let kind = match checked_entry(root, target) {
        Ok(kind) => kind,
        Err(err) => return Outcome::Failed(err),
    };
    match mode {
        Mode::Trash => match trash(&target.path) {
            Ok(()) => Outcome::Removed,
            Err(err) => match checked_entry(root, target) {
                Ok(_) => Outcome::TrashFailed(err),
                Err(check_err) => Outcome::Failed(check_err),
            },
        },
        Mode::Permanent => {
            let remove_as_dir = kind.is_dir();
            #[cfg(windows)]
            let remove_as_dir = {
                use std::os::windows::fs::FileTypeExt;
                remove_as_dir || kind.is_symlink_dir()
            };
            let result = if kind.is_dir() {
                fs::remove_dir_all(&target.path)
            } else if remove_as_dir {
                fs::remove_dir(&target.path)
            } else {
                fs::remove_file(&target.path)
            };
            match result {
                Ok(()) => Outcome::Removed,
                Err(err) => Outcome::Failed(err),
            }
        }
    }
}

impl App {
    pub(super) fn begin_explorer_input(&mut self, ctx: &egui::Context) {
        if let Some(selection) = &mut self.explorer_selection {
            selection.row = None;
            if ctx.input(|i| i.pointer.any_pressed()) {
                selection.last_clicked = false;
            }
        }
    }

    pub(super) fn explorer_row(&mut self, row: &egui::Response, path: &Path, folder: bool) {
        if self.explorer_scroll_to.as_deref() == Some(path) {
            row.scroll_to_me(Some(egui::Align::Center));
            self.explorer_scroll_to = None;
        }
        let clicked = row.clicked_by(egui::PointerButton::Primary) || row.secondary_clicked();
        if clicked || row.gained_focus() {
            if clicked {
                row.request_focus();
            }
            self.explorer_marked = None;
            self.explorer_selection = Some(ExplorerSelection {
                path: path.to_owned(),
                folder,
                row: Some(row.id),
                last_clicked: clicked,
            });
        } else if let Some(selection) = &mut self.explorer_selection
            && selection.path == path
        {
            selection.row = Some(row.id);
        }
    }

    pub(super) fn explorer_delete_shortcut(&mut self, ctx: &egui::Context) {
        let Some(selection) = &self.explorer_selection else { return };
        let Some(row) = selection.row else { return };
        if self.deletion.is_some()
            || self.confirm.is_some()
            || self.quick_open.is_some()
            || self.open_folder.is_some()
            || egui::Popup::is_any_open(ctx)
            || ctx.memory(|m| m.top_modal_layer().is_some() || !m.focused().map_or(selection.last_clicked, |focused| focused == row))
        {
            return;
        }
        let mac = ctx.os().is_mac();
        let pressed = ctx.input_mut(|input| {
            let mut pressed = false;
            if input.focused && !input.pointer.any_down() {
                input.events.retain(|event| {
                    let matches = matches!(
                        event,
                        egui::Event::Key { key, pressed: true, repeat: false, modifiers, .. }
                            if (*key == Key::Delete && *modifiers == Modifiers::NONE)
                                || (mac && *key == Key::Backspace && *modifiers == (Modifiers::MAC_CMD | Modifiers::COMMAND))
                    );
                    pressed |= matches;
                    !matches
                });
            }
            pressed
        });
        if pressed {
            self.request_delete(selection.path.clone(), selection.folder);
        }
    }

    pub(super) fn explorer_delete_menu(&mut self, ui: &mut egui::Ui, path: &Path, folder: bool) {
        ui.separator();
        if ui.button(format!("{} Delete", ic::TRASH)).clicked() {
            self.request_delete(path.to_owned(), folder);
            ui.close();
        }
    }

    pub(super) fn delete_active_file(&mut self) {
        let path = match self.tabs.get(self.active) {
            Some(Tab::File(doc)) => &doc.path,
            Some(Tab::Large(view)) => &view.path,
            Some(Tab::Hex(view)) if !view.comparing() => &view.path,
            _ => {
                self.message = "Open a file to delete it".into();
                return;
            }
        };
        self.request_delete(path.clone(), false);
    }

    fn deletion_blocked(&self, target: &Target) -> bool {
        self.document_action_pending(|p| target.contains(p)) || self.search_replace.busy() || self.bg_task.is_some()
    }

    fn request_delete(&mut self, path: PathBuf, folder: bool) {
        if self.deletion.is_some() || self.confirm.is_some() {
            return;
        }
        let Some(root) = self.root.as_ref().filter(|root| relative_target(root, &path).is_some()) else {
            self.message = "Choose an entry inside the open project folder".into();
            return;
        };
        let target = Target { path, folder };
        if self.deletion_blocked(&target) {
            self.message = "Wait for pending file operations to finish before deleting".into();
            return;
        }
        let previous_focus = self.ctx.memory_mut(|m| {
            let focus = m.focused();
            if let Some(id) = focus {
                m.surrender_focus(id);
            }
            focus
        });
        self.deletion = Some(Deletion {
            root: root.clone(),
            target,
            phase: Phase::ConfirmTrash,
            previous_focus,
        });
        self.ctx.request_repaint();
    }

    pub(super) fn delete_dialog(&mut self, ctx: &egui::Context) {
        let Some(deletion) = &self.deletion else { return };
        let name = dir_name(&deletion.target.path);
        let dirty = self
            .tabs
            .iter()
            .filter(|tab| matches!(tab, Tab::File(doc) if deletion.target.contains(&doc.path) && doc.dirty()))
            .count();
        let blocked = self.deletion_blocked(&deletion.target);
        let mut answer = None;
        let mut cancelled = false;
        egui::Modal::new(egui::Id::unique("delete_entry")).show(ctx, |ui| {
            ui.set_max_width(440.0);
            let (button, mode) = match &deletion.phase {
                Phase::ConfirmTrash => {
                    ui.heading(format!("Delete {name}?"));
                    ("Move to Trash", Mode::Trash)
                }
                Phase::ConfirmPermanent(err) => {
                    ui.heading(format!("Permanently delete {name}?"));
                    ui.label(format!("Could not move it to Trash: {err}"));
                    ui.label("Permanent deletion cannot be undone.");
                    ("Delete Permanently", Mode::Permanent)
                }
                Phase::Running(_) => {
                    ui.heading(format!("Deleting {name}…"));
                    ui.spinner();
                    return;
                }
            };
            if deletion.target.folder {
                ui.label("This includes everything inside the folder.");
            }
            match dirty {
                0 => {}
                1 => {
                    ui.label("One open file has unsaved changes that will be discarded.");
                }
                n => {
                    ui.label(format!("{n} open files have unsaved changes that will be discarded."));
                }
            }
            if blocked {
                ui.label("Waiting for pending file operations…");
            }
            let enter = ui.input_mut(|input| {
                input.events.retain(|event| {
                    !matches!(
                        event,
                        egui::Event::Key {
                            key: Key::Enter | Key::Space,
                            pressed: true,
                            repeat: true,
                            ..
                        }
                    )
                });
                matches!(mode, Mode::Trash) && input.consume_key(Modifiers::NONE, Key::Enter)
            });
            ui.horizontal(|ui| {
                if ui.add_enabled(!blocked, egui::Button::new(button)).clicked() || (!blocked && enter) {
                    answer = Some(mode);
                }
                if ui.button("Cancel").clicked() || ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape)) {
                    cancelled = true;
                }
            });
        });
        if cancelled {
            if let Some(id) = deletion.previous_focus {
                ctx.memory_mut(|m| m.request_focus(id));
            }
            self.deletion = None;
        } else if let Some(mode) = answer {
            self.start_deletion(mode);
        }
    }

    fn start_deletion(&mut self, mode: Mode) {
        let deletion = self.deletion.as_ref().unwrap();
        let (root, target) = (deletion.root.clone(), deletion.target.clone());
        let rx = background(&self.ctx, move || {
            remove_entry(&root, &target, mode, |path| trash::delete(path).map_err(|err| err.to_string()))
        });
        self.deletion.as_mut().unwrap().phase = Phase::Running(Some(rx));
    }

    pub(super) fn poll_deletion(&mut self) {
        let Some(deletion) = &mut self.deletion else { return };
        let Phase::Running(rx) = &mut deletion.phase else { return };
        let outcome = match receive(rx) {
            Some(outcome) => outcome,
            None if rx.is_some() => return,
            None => Outcome::Failed(io::Error::other("The delete worker stopped")),
        };
        if let Outcome::TrashFailed(err) = outcome {
            deletion.phase = Phase::ConfirmPermanent(err);
            return;
        }
        let deletion = self.deletion.take().unwrap();
        let target = deletion.target;
        let name = dir_name(&target.path);
        let parent = target.path.parent().unwrap();
        self.invalidate_tree(|p| p == parent || target.contains(p));
        self.refresh_vcs();
        match outcome {
            Outcome::Removed => {
                self.close_tabs(target.tabs(&self.tabs), true);
                self.open_jobs.retain(|job| !target.contains(&job.path));
                self.tree.retain(|p, _| p != parent && !target.contains(p));
                self.tree_requested.retain(|p| !target.contains(p));
                let expanded: Vec<_> = self.expanded.iter().filter(|p| target.contains(p)).cloned().collect();
                for path in expanded {
                    self.expanded.remove(&path);
                }
                self.explorer_selection = None;
                self.explorer_marked = None;
                self.explorer_revealed = None;
                self.explorer_scroll_to = None;
                self.message = format!("Deleted {name}");
            }
            Outcome::Failed(err) => self.message = format!("Could not delete {name}: {err}"),
            Outcome::TrashFailed(_) => unreachable!(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("edittr-delete-{name}-{}", std::process::id()));
        fs::create_dir(&dir).unwrap();
        workspace::canonical(&dir).unwrap()
    }

    fn file(path: PathBuf) -> Target {
        Target { path, folder: false }
    }

    #[test]
    fn rejects_root_outside_paths_and_parent_traversal() {
        let dir = fixture("paths");
        let root = dir.join("project");
        fs::create_dir(&root).unwrap();
        let outside = dir.join("outside");
        fs::write(&outside, "keep").unwrap();
        let inside = root.join("inside");
        fs::write(&inside, "delete").unwrap();
        assert!(checked_entry(&root, &file(inside)).is_ok());
        for path in [root.clone(), outside.clone(), root.join("../outside")] {
            let result = remove_entry(&root, &file(path), Mode::Trash, |_| {
                panic!("unsafe entry reached the trash backend")
            });
            assert!(matches!(result, Outcome::Failed(err) if err.kind() == io::ErrorKind::PermissionDenied));
        }
        assert_eq!(fs::read_to_string(outside).unwrap(), "keep");
        fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_ancestors_and_removes_only_a_selected_symlink() {
        let dir = fixture("links");
        let root = dir.join("project");
        let outside = dir.join("outside");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("keep"), "keep").unwrap();
        let link = root.join("link");
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        let escaped = file(link.join("keep"));
        assert_eq!(checked_entry(&root, &escaped).unwrap_err().kind(), io::ErrorKind::PermissionDenied);
        assert!(matches!(
            remove_entry(&root, &file(link.clone()), Mode::Permanent, |_| unreachable!()),
            Outcome::Removed
        ));
        assert!(fs::symlink_metadata(link).is_err());
        assert_eq!(fs::read_to_string(outside.join("keep")).unwrap(), "keep");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn trash_failure_leaves_the_entry_for_a_separate_permanent_confirmation() {
        let root = fixture("fallback");
        let target = file(root.join("file"));
        fs::write(&target.path, "keep until confirmed").unwrap();
        let outcome = remove_entry(&root, &target, Mode::Trash, |_| Err("Trash unavailable".into()));
        assert!(matches!(outcome, Outcome::TrashFailed(err) if err == "Trash unavailable"));
        assert_eq!(fs::read_to_string(&target.path).unwrap(), "keep until confirmed");
        assert!(matches!(
            remove_entry(&root, &target, Mode::Permanent, |_| panic!(
                "permanent delete called the trash backend"
            )),
            Outcome::Removed
        ));
        assert!(!target.path.exists());
        fs::remove_dir_all(root).unwrap();
    }

    fn test_app(ctx: &egui::Context) -> (App, std::thread::JoinHandle<()>) {
        let cc = eframe::CreationContext::_new_kittest(ctx.clone());
        let (writer, worker) = SettingsWriter::start();
        let startup = Startup {
            root: None,
            file: None,
            restore: false,
            window: Default::default(),
        };
        let app = App::new(&cc, startup, Default::default(), writer, None);
        (app, worker)
    }

    #[test]
    fn deleting_folder_closes_its_tabs_and_keeps_siblings() {
        let ctx = egui::Context::default();
        let (mut app, worker) = test_app(&ctx);
        let root = fixture("tabs");
        let target = Target {
            path: root.join("src"),
            folder: true,
        };
        let direct = target.path.join("one.rs");
        let nested = target.path.join("nested/two.rs");
        let sibling = root.join("src-other/keep.rs");
        for path in [&direct, &nested, &sibling] {
            app.add_file_tab(path, String::new(), false);
        }
        assert_eq!(file(direct).tabs(&app.tabs), [0]);
        let Tab::File(doc) = &mut app.tabs[1] else { unreachable!() };
        doc.text = "unsaved".into();
        doc.version += 1;
        assert!(doc.dirty());
        app.active = 1;
        app.split = Some(2);
        app.root = Some(root.clone());
        let (tx, rx) = channel();
        tx.send(Outcome::Removed).unwrap();
        app.deletion = Some(Deletion {
            root: root.clone(),
            target,
            phase: Phase::Running(Some(rx)),
            previous_focus: None,
        });

        app.poll_deletion();

        assert!(app.deletion.is_none());
        assert_eq!(app.tabs.len(), 1);
        assert_eq!(app.active_path(), Some(sibling.as_path()));
        assert_eq!(app.split, Some(0));
        drop(app);
        worker.join().unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn delete_shortcut_respects_text_focus_and_accepts_mac_cmd_backspace() {
        let ctx = egui::Context::default();
        ctx.set_os(egui::os::OperatingSystem::Mac);
        let (mut app, worker) = test_app(&ctx);
        let root = fixture("focus");
        app.root = Some(root.clone());
        let path = root.join("selected");
        let mut text = String::from("editable");
        for (key, modifiers, text_focused, asks_to_delete) in [
            (Key::Delete, Modifiers::NONE, true, false),
            (Key::Backspace, Modifiers::MAC_CMD | Modifiers::COMMAND, true, false),
            (Key::Delete, Modifiers::NONE, false, true),
            (Key::Backspace, Modifiers::MAC_CMD | Modifiers::COMMAND, false, true),
        ] {
            let mut out = ctx.run_ui(
                egui::RawInput {
                    events: [true, false]
                        .map(|pressed| egui::Event::Key {
                            key,
                            physical_key: None,
                            pressed,
                            repeat: false,
                            modifiers,
                        })
                        .to_vec(),
                    ..Default::default()
                },
                |ui| {
                    let row = ui.selectable_label(true, "selected");
                    let field = ui.text_edit_singleline(&mut text);
                    if text_focused {
                        field.request_focus();
                    } else {
                        row.request_focus();
                    }
                    app.explorer_selection = Some(ExplorerSelection {
                        path: path.clone(),
                        folder: false,
                        row: Some(row.id),
                        last_clicked: true,
                    });
                    app.explorer_delete_shortcut(ui.ctx());
                },
            );
            out.textures_delta.clear();
            assert_eq!(app.deletion.take().is_some(), asks_to_delete, "{key:?}, text focus: {text_focused}");
        }
        drop(app);
        worker.join().unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn explorer_click_opens_and_selects_the_file_and_escape_cancels_delete() {
        let ctx = egui::Context::default();
        let (mut app, worker) = test_app(&ctx);
        let root = fixture("click");
        let path = root.join("click-target");
        app.root = Some(root.clone());
        app.tree.insert(root.clone(), vec![(path.clone(), false)]);
        fs::write(&path, "keep").unwrap();

        let mut frame = |events| {
            let mut open = None;
            let mut out = ctx.run_ui(
                egui::RawInput {
                    events,
                    ..Default::default()
                },
                |ui| {
                    app.begin_explorer_input(ui.ctx());
                    app.delete_dialog(ui.ctx());
                    app.tree_dir(ui, &root, 0, &HashMap::new(), &mut open);
                    app.explorer_delete_shortcut(ui.ctx());
                },
            );
            out.textures_delta.clear();
            (open, app.deletion.is_some(), out.shapes)
        };
        let pos = frame(Vec::new())
            .2
            .iter()
            .find_map(|shape| match &shape.shape {
                egui::Shape::Text(text) if text.galley.text().contains("click-target") => Some(text.pos + egui::vec2(3.0, 3.0)),
                _ => None,
            })
            .unwrap();
        frame(vec![egui::Event::PointerMoved(pos)]);
        for pressed in [true, false] {
            let (opened, _, _) = frame(vec![egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: Modifiers::NONE,
            }]);
            assert_eq!(opened.as_deref(), (!pressed).then_some(path.as_path()));
        }
        let key = |key| {
            vec![egui::Event::Key {
                key,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: Modifiers::NONE,
            }]
        };
        assert!(frame(key(Key::Delete)).1);
        assert!(frame(Vec::new()).1);
        assert!(!frame(key(Key::Escape)).1);
        assert_eq!(fs::read_to_string(path).unwrap(), "keep");
        drop(app);
        worker.join().unwrap();
        fs::remove_dir_all(root).unwrap();
    }
}
