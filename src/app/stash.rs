use super::{App, Confirm, Side, Tab, VcsOperation, commit_view::CommitView};
use crate::commands::Command;
use crate::icons as ic;
use crate::theme::Palette;
use crate::vcs::{Stash, StashAction, Vcs};
use eframe::egui;
use std::path::PathBuf;

#[derive(Default)]
pub(super) struct State {
    pub(super) entries: Vec<Stash>,
    pub(super) prompt: Option<PushPrompt>,
    reveal: bool,
}

#[derive(Default)]
pub(super) struct PushPrompt {
    message: String,
    include_untracked: bool,
}

pub(super) struct StashView {
    pub(super) entry: Stash,
    pub(super) detail: CommitView,
    repo: PathBuf,
}

impl StashView {
    pub(super) fn show(&mut self, ui: &mut egui::Ui, palette: &Palette) {
        self.detail.show_stash(ui, &self.entry.commit, &self.repo, palette);
    }
}

impl App {
    pub(super) fn stash_dialog_open(&self) -> bool {
        self.stash.prompt.is_some() || matches!(self.confirm, Some(Confirm::DropStash(_)))
    }

    pub(super) fn reveal_stashes(&mut self) {
        if self.vcs == Some(Vcs::Git) {
            self.side = Side::Changes;
            self.side_open = true;
            self.stash.reveal = true;
        }
    }

    pub(super) fn prompt_stash_changes(&mut self) {
        if self.vcs == Some(Vcs::Git) && self.bg_task.is_none() && self.confirm.is_none() {
            self.stash.prompt = Some(PushPrompt::default());
        }
    }

    pub(super) fn stashes_panel(&mut self, ui: &mut egui::Ui) {
        let reveal = std::mem::take(&mut self.stash.reveal);
        if ui
            .add_enabled(
                self.bg_task.is_none(),
                egui::Button::new(format!("{} Stash Changes…", ic::ARROW_DOWN)),
            )
            .clicked()
        {
            self.prompt_stash_changes();
        }
        let busy = self.bg_task.is_some();
        let mut selected = None;
        egui::CollapsingHeader::new("Stashes")
            .open(reveal.then_some(true))
            .show_unindented(ui, |ui| {
                if self.stash.entries.is_empty() {
                    ui.weak("No stashes.");
                    return;
                }
                let row_height = ui.spacing().interact_size.y;
                egui::ScrollArea::vertical()
                    .id_salt("stashes")
                    .max_height((ui.available_height() / 3.0).clamp(row_height, 180.0))
                    .show_rows(ui, row_height, self.stash.entries.len(), |ui, rows| {
                        for i in rows {
                            let entry = &self.stash.entries[i];
                            ui.push_id((&entry.selector, &entry.commit.hash), |ui| {
                                if let Some(command) = stash_row(ui, entry, busy) {
                                    selected = Some((command, entry.clone()));
                                }
                            });
                        }
                    });
            });
        if let Some((command, entry)) = selected {
            self.select_stash(command, entry);
        }
    }

    pub(super) fn select_stash(&mut self, command: Command, entry: Stash) {
        let Some((repo, Vcs::Git)) = self.repository() else { return };
        match command {
            Command::ViewStash => self.show_tab(Tab::Stash(Box::new(StashView {
                entry,
                detail: CommitView::default(),
                repo,
            }))),
            Command::StashEntry(StashAction::Drop) if self.bg_task.is_none() && self.confirm.is_none() => {
                self.confirm = Some(Confirm::DropStash(Box::new(entry)));
            }
            Command::StashEntry(StashAction::Apply) => self.run_stash_action(entry, StashAction::Apply),
            Command::StashEntry(StashAction::Pop) => self.run_stash_action(entry, StashAction::Pop),
            _ => {}
        }
    }

    pub(super) fn run_stash_action(&mut self, entry: Stash, action: StashAction) {
        if self.vcs == Some(Vcs::Git) {
            self.spawn_vcs(VcsOperation::Stash(action.label()), move |vcs, repo| {
                vcs.stash_action(repo, &entry, action)
            });
        }
    }

    pub(super) fn stash_dialog(&mut self, ctx: &egui::Context) {
        let Some(prompt) = &mut self.stash.prompt else { return };
        let busy = self.bg_task.is_some();
        let dialog = egui::Modal::new(egui::Id::unique("stash_push")).show(ctx, |ui| {
            ui.heading("Stash Changes");
            ui.add(egui::TextEdit::singleline(&mut prompt.message).hint_text("Message (optional)"));
            ui.checkbox(&mut prompt.include_untracked, "Include untracked files");
            ui.horizontal(|ui| {
                let stash = ui.add_enabled(!busy, egui::Button::new("Stash")).clicked();
                let cancel = ui.button("Cancel").clicked();
                (stash, cancel)
            })
            .inner
        });
        let (stash, cancel) = dialog.inner;
        if stash {
            let prompt = self.stash.prompt.take().unwrap();
            self.spawn_vcs(VcsOperation::Stash("Stash changes"), move |vcs, repo| {
                vcs.stash_push(repo, prompt.message.trim(), prompt.include_untracked)
            });
        } else if cancel || dialog.should_close() {
            self.stash.prompt = None;
        }
    }
}

fn stash_row(ui: &mut egui::Ui, entry: &Stash, busy: bool) -> Option<Command> {
    let mut selected = None;
    ui.horizontal(|ui| {
        ui.menu_button(ic::LIST, |ui| {
            for (command, icon, label, enabled) in [
                (Command::ViewStash, ic::GIT_DIFF, "View Diff", true),
                (Command::StashEntry(StashAction::Apply), ic::ARROW_U_UP_LEFT, "Apply", !busy),
                (Command::StashEntry(StashAction::Pop), ic::ARROW_UP, "Pop", !busy),
                (Command::StashEntry(StashAction::Drop), ic::TRASH, "Drop…", !busy),
            ] {
                if ui.add_enabled(enabled, egui::Button::new(format!("{icon} {label}"))).clicked() {
                    selected = Some(command);
                    ui.close();
                }
            }
        })
        .response
        .on_hover_text("Stash actions");
        let title = entry.title();
        if ui
            .add(egui::Button::selectable(false, &title).truncate())
            .on_hover_text(title)
            .clicked()
        {
            selected = Some(Command::ViewStash);
        }
    });
    selected
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{session::Startup, settings::SettingsWriter, vcs::Commit};

    #[test]
    fn stash_section_is_git_only_and_drop_waits_for_confirmation() {
        let ctx = egui::Context::default();
        ctx.global_style_mut(|style| style.animation_time = 0.0);
        let cc = eframe::CreationContext::_new_kittest(ctx.clone());
        let (writer, worker) = SettingsWriter::start();
        let startup = Startup {
            root: None,
            file: None,
            restore: false,
            window: Default::default(),
        };
        let mut app = App::new(&cc, startup, Default::default(), writer, None);
        app.root = Some(PathBuf::from("."));
        app.repo = app.root.clone();
        app.stash.entries.push(Stash {
            selector: "stash@{0}".into(),
            commit: Commit {
                hash: "stash-test-hash".into(),
                short_hash: "stash-test".into(),
                parents: Vec::new(),
                author: "Test Author".into(),
                date: "2026-01-01".into(),
                refs: Vec::new(),
                subject: "Changes for the test".into(),
                message: "Changes for the test".into(),
            },
        });
        let frame = |app: &mut App, events| {
            let mut output = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(340.0, 650.0))),
                    events,
                    ..Default::default()
                },
                |ui| {
                    app.changes(ui);
                    app.dialogs(ui.ctx());
                },
            );
            output.textures_delta.clear();
            output.shapes
        };
        let has_text = |shapes: &[egui::epaint::ClippedShape], text: &str| {
            shapes
                .iter()
                .any(|shape| matches!(&shape.shape, egui::Shape::Text(label) if label.galley.text().contains(text)))
        };
        app.vcs = Some(Vcs::Svn);
        assert!(!has_text(&frame(&mut app, vec![]), "Stash"));
        app.run_command(Command::StashChanges);
        assert!(app.stash.prompt.is_none());

        app.vcs = Some(Vcs::Git);
        let collapsed = frame(&mut app, vec![]);
        assert!(has_text(&collapsed, "Stashes"));
        assert!(!has_text(&collapsed, "Changes for the test"));
        app.run_command(Command::ShowStashes);
        let expanded = frame(&mut app, vec![]);
        assert!(has_text(&expanded, "Changes for the test"));

        let entry = app.stash.entries[0].clone();
        app.select_stash(Command::StashEntry(StashAction::Drop), entry);
        assert!(matches!(app.confirm, Some(Confirm::DropStash(_))));
        assert!(app.bg_task.is_none());
        frame(&mut app, vec![]);
        let confirm = frame(&mut app, vec![]);
        assert!(has_text(&confirm, "Drop stash@{0}"));
        frame(
            &mut app,
            vec![egui::Event::Key {
                key: egui::Key::Escape,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }],
        );
        assert!(app.confirm.is_none());
        assert!(app.bg_task.is_none());
        assert_eq!(app.stash.entries.len(), 1);
        drop(app);
        worker.join().unwrap();
    }
}
