use super::{App, VcsOperation};
use crate::git::{Commit, CommitRef, HistoryAction as GitAction, ResetMode, Status};
use crate::vcs::{HistoryAction, Vcs};
use eframe::egui;
use std::collections::HashSet;

pub(super) struct GraphMenu<'a> {
    pub vcs: Vcs,
    pub status: &'a Status,
    pub branches: &'a [String],
    pub busy: bool,
}

pub(super) enum RowAction {
    Run(HistoryAction),
    Prompt(GraphDialog),
}

pub(super) enum GraphDialog {
    Confirm {
        action: HistoryAction,
        text: String,
    },
    NameRef {
        hash: String,
        short_hash: String,
        name: String,
        branch: bool,
        checkout: bool,
    },
}

impl RowAction {
    fn git(action: GitAction) -> Self {
        Self::Run(HistoryAction::Git(action))
    }

    fn confirm(action: GitAction, text: String) -> Self {
        Self::Prompt(GraphDialog::Confirm {
            action: HistoryAction::Git(action),
            text,
        })
    }

    fn name_ref(commit: &Commit, branch: bool) -> Self {
        Self::Prompt(GraphDialog::NameRef {
            hash: commit.hash.clone(),
            short_hash: commit.short_hash.clone(),
            name: String::new(),
            branch,
            checkout: false,
        })
    }
}

impl GraphMenu<'_> {
    pub fn show(&self, ui: &mut egui::Ui, commit: &Commit) -> Option<RowAction> {
        let revision = commit.hash.strip_prefix('r').filter(|_| self.vcs == Vcs::Svn);
        let action = ui
            .add_enabled_ui(!self.busy, |ui| match self.vcs {
                Vcs::Git => self.git_commands(ui, commit),
                Vcs::Svn => {
                    let revision = revision?.parse::<u64>().ok()?;
                    button(ui, true, format!("Update working copy to r{revision}")).then(|| {
                        RowAction::Prompt(GraphDialog::Confirm {
                            action: HistoryAction::UpdateRevision(revision),
                            text: format!("Update the working copy to Subversion revision r{revision}?"),
                        })
                    })
                }
            })
            .inner;
        ui.separator();
        match self.vcs {
            Vcs::Git => {
                copy(ui, "Copy commit hash", &commit.hash);
                copy(ui, "Copy short hash", &commit.short_hash);
                copy(ui, "Copy commit message", &commit.message);
                for reference in &commit.refs {
                    match reference {
                        CommitRef::Local { name, .. } | CommitRef::Remote(name) => copy(ui, format!("Copy branch name {name}"), name),
                        CommitRef::Tag(name) => copy(ui, format!("Copy tag name {name}"), name),
                        CommitRef::Head | CommitRef::Other(_) => {}
                    }
                }
            }
            Vcs::Svn => {
                if let Some(revision) = revision {
                    copy(ui, "Copy revision number", revision);
                }
                copy(ui, "Copy message", &commit.message);
            }
        }
        copy(ui, "Copy author", &commit.author);
        action
    }

    fn git_commands(&self, ui: &mut egui::Ui, commit: &Commit) -> Option<RowAction> {
        let current = self.branches.iter().find(|name| **name == self.status.branch).map(String::as_str);
        for reference in &commit.refs {
            match reference {
                CommitRef::Local { name, .. } => {
                    if button(ui, current != Some(name), format!("Checkout {name}")) {
                        return Some(RowAction::git(GitAction::Checkout(name.clone())));
                    }
                }
                CommitRef::Remote(remote) => {
                    let (_, name) = remote.split_once('/').expect("remote-tracking ref has a remote prefix");
                    if !self.branches.iter().any(|local| local == name)
                        && button(ui, true, format!("Checkout {name} (new local branch tracking {remote})"))
                    {
                        return Some(RowAction::git(GitAction::Track {
                            local: name.into(),
                            remote: format!("refs/remotes/{remote}"),
                        }));
                    }
                }
                CommitRef::Tag(_) | CommitRef::Head | CommitRef::Other(_) => {}
            }
        }
        let short = &commit.short_hash;
        if button(ui, true, "Checkout this commit (detached HEAD)") {
            return Some(RowAction::confirm(
                GitAction::Detach(commit.hash.clone()),
                format!("Check out {short} with a detached HEAD? New commits will be outside any branch until you create one."),
            ));
        }
        if button(ui, true, "Create branch here…") {
            return Some(RowAction::name_ref(commit, true));
        }

        let reset_label = current.map_or_else(
            || "Reset current branch to this commit".into(),
            |name| format!("Reset {name} to this commit"),
        );
        let reset = ui
            .add_enabled_ui(current.is_some(), |ui| {
                ui.menu_button(reset_label, |ui| {
                    let branch = current?;
                    for mode in [ResetMode::Soft, ResetMode::Mixed, ResetMode::Hard] {
                        if button(ui, true, mode.label()) {
                            let action = GitAction::Reset {
                                branch: branch.into(),
                                hash: commit.hash.clone(),
                                mode,
                            };
                            return Some(match mode {
                                ResetMode::Hard => {
                                    let count = self
                                        .status
                                        .staged
                                        .iter()
                                        .chain(&self.status.unstaged)
                                        .map(|change| &change.path)
                                        .collect::<HashSet<_>>()
                                        .len();
                                    let files = if count == 1 { "file" } else { "files" };
                                    RowAction::confirm(
                                        action,
                                        format!(
                                            "Hard reset {branch} to {short}? Uncommitted changes to tracked files will be lost. Git reports {count} changed {files}."
                                        ),
                                    )
                                }
                                _ => RowAction::git(action),
                            });
                        }
                    }
                    None
                })
                .inner
                .flatten()
            })
            .inner;
        if reset.is_some() {
            return reset;
        }

        let single_parent = commit.parents.len() <= 1;
        if ui
            .add_enabled(single_parent, egui::Button::new("Cherry-pick this commit"))
            .on_disabled_hover_text("Cherry-picking a merge commit requires choosing a mainline parent.")
            .clicked()
        {
            ui.close();
            return Some(RowAction::confirm(
                GitAction::CherryPick(commit.hash.clone()),
                format!("Apply the changes from {short} as a new commit?"),
            ));
        }
        if ui
            .add_enabled(single_parent, egui::Button::new("Revert this commit"))
            .on_disabled_hover_text("Reverting a merge commit requires choosing a mainline parent.")
            .clicked()
        {
            ui.close();
            return Some(RowAction::confirm(
                GitAction::Revert(commit.hash.clone()),
                format!("Create a new commit that reverses the changes from {short}?"),
            ));
        }

        if let Some(branch) = current {
            for reference in &commit.refs {
                let (name, prefix) = match reference {
                    CommitRef::Local { name, .. } if name != branch => (name, "refs/heads/"),
                    CommitRef::Remote(name) => (name, "refs/remotes/"),
                    _ => continue,
                };
                let label = format!("Merge {name} into {branch}");
                if button(ui, true, &label) {
                    return Some(RowAction::confirm(
                        GitAction::Merge {
                            branch: branch.into(),
                            source: format!("{prefix}{name}"),
                        },
                        format!("{label}?"),
                    ));
                }
            }
        }
        if button(ui, true, "Create tag here…") {
            return Some(RowAction::name_ref(commit, false));
        }
        for reference in &commit.refs {
            if let CommitRef::Local { name, .. } = reference
                && current != Some(name)
                && button(ui, true, format!("Delete branch {name}"))
            {
                return Some(RowAction::confirm(
                    GitAction::DeleteBranch {
                        name: name.clone(),
                        force: false,
                    },
                    format!("Delete the local branch {name}?"),
                ));
            }
        }
        None
    }
}

fn button(ui: &mut egui::Ui, enabled: bool, label: impl Into<egui::WidgetText>) -> bool {
    let clicked = ui.add_enabled(enabled, egui::Button::new(label)).clicked();
    if clicked {
        ui.close();
    }
    clicked
}

fn copy(ui: &mut egui::Ui, label: impl Into<egui::WidgetText>, text: &str) {
    if button(ui, true, label) {
        ui.ctx().copy_text(text.into());
    }
}

fn ref_name_id() -> egui::Id {
    egui::Id::unique("history_ref_name")
}

impl App {
    pub(super) fn handle_graph_action(&mut self, action: RowAction) {
        match action {
            RowAction::Run(action) => self.run_graph_action(action),
            RowAction::Prompt(dialog) => {
                if matches!(dialog, GraphDialog::NameRef { .. }) {
                    self.ctx.memory_mut(|memory| memory.request_focus(ref_name_id()));
                }
                self.graph_dialog = Some(dialog);
            }
        }
    }

    fn run_graph_action(&mut self, action: HistoryAction) {
        self.spawn_vcs(VcsOperation::Graph(action.clone()), move |vcs, root| action.run(vcs, root));
    }

    pub(super) fn offer_force_delete(&mut self, name: String) {
        self.handle_graph_action(RowAction::confirm(
            GitAction::DeleteBranch {
                name: name.clone(),
                force: true,
            },
            format!("Force-delete {name}? Commits unique to this branch may become unreachable."),
        ));
    }

    pub(super) fn graph_dialog(&mut self, ctx: &egui::Context) {
        let idle = self.bg_task.is_none() && self.repo.is_some() && self.vcs.is_some();
        let Some(dialog) = &mut self.graph_dialog else { return };
        let mut run = None;
        let modal = egui::Modal::new(egui::Id::unique("history_action")).show(ctx, |ui| {
            ui.set_width(360.0);
            match dialog {
                GraphDialog::Confirm { action, text } => {
                    ui.heading(action.label());
                    ui.label(text.as_str());
                    ui.horizontal(|ui| {
                        if button(ui, idle, "Confirm") {
                            run = Some(action.clone());
                        }
                        button(ui, true, "Cancel");
                    });
                }
                GraphDialog::NameRef {
                    hash,
                    short_hash,
                    name,
                    branch,
                    checkout,
                } => {
                    let kind = if *branch { "branch" } else { "tag" };
                    ui.heading(format!("Create {kind} at {short_hash}"));
                    ui.label(format!("{kind} name"));
                    ui.add(egui::TextEdit::singleline(name).id(ref_name_id()).desired_width(f32::INFINITY));
                    if *branch {
                        ui.checkbox(checkout, "Check out after creating");
                    }
                    ui.horizontal(|ui| {
                        let name = name.trim();
                        if button(ui, idle && !name.is_empty(), "Create") {
                            let action = if *branch {
                                GitAction::CreateBranch {
                                    name: name.into(),
                                    hash: hash.clone(),
                                    checkout: *checkout,
                                }
                            } else {
                                GitAction::CreateTag {
                                    name: name.into(),
                                    hash: hash.clone(),
                                }
                            };
                            run = Some(HistoryAction::Git(action));
                        }
                        button(ui, true, "Cancel");
                    });
                }
            }
        });
        if modal.should_close() || run.is_some() {
            self.graph_dialog = None;
        }
        if let Some(action) = run {
            self.run_graph_action(action);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::Graph;
    use egui::{Event, FullOutput, Modifiers, PointerButton, Pos2, Rect, Vec2};

    fn click(pos: Pos2, button: PointerButton) -> Vec<Event> {
        let mut events = vec![Event::PointerMoved(pos)];
        for pressed in [true, false] {
            events.push(Event::PointerButton {
                pos,
                button,
                pressed,
                modifiers: Modifiers::NONE,
            });
        }
        events
    }

    fn label(output: &FullOutput, text: &str) -> Option<Pos2> {
        output.shapes.iter().find_map(|shape| match &shape.shape {
            egui::Shape::Text(shape) if shape.galley.text().starts_with(text) => Some(shape.visual_bounding_rect().center()),
            _ => None,
        })
    }

    fn render(ctx: &egui::Context, graph: &Graph, menu: &GraphMenu<'_>, events: Vec<Event>) -> (FullOutput, Option<RowAction>) {
        let mut action = None;
        let mut output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1000.0, 800.0))),
                events,
                ..Default::default()
            },
            |ui| {
                assert_eq!(
                    graph.ui(ui, None, |ui, commit| action = menu.show(ui, commit)),
                    None,
                    "menu interactions should not select a history row"
                );
            },
        );
        output.textures_delta.clear();
        (output, action)
    }

    fn graph(vcs: Vcs) -> Graph {
        let (hash, short_hash, refs) = match vcs {
            Vcs::Git => (
                "123456789abcdef",
                "1234567",
                vec![
                    CommitRef::Local {
                        name: "topic/work".into(),
                        head: false,
                    },
                    CommitRef::Tag("v1".into()),
                ],
            ),
            Vcs::Svn => ("r123", "r123", Vec::new()),
        };
        Graph::build(vec![Commit {
            hash: hash.into(),
            short_hash: short_hash.into(),
            refs,
            subject: "History subject".into(),
            message: "History subject\n\nMessage body.".into(),
            author: "Author".into(),
            date: "2026-01-01".into(),
            parents: Vec::new(),
        }])
    }

    #[test]
    fn graph_row_menu_routes_reset_and_keeps_copy_commands_enabled_when_busy() {
        let ctx = egui::Context::default();
        let graph = graph(Vcs::Git);
        let file = crate::git::FileChange {
            path: "file.txt".into(),
            code: 'M',
        };
        let status = Status {
            branch: "main".into(),
            staged: vec![file.clone()],
            unstaged: vec![file],
            ..Default::default()
        };
        let mut menu = GraphMenu {
            vcs: Vcs::Git,
            status: &status,
            branches: &["main".into(), "topic/work".into()],
            busy: false,
        };

        let (output, _) = render(&ctx, &graph, &menu, vec![]);
        let row = label(&output, "History subject").unwrap();
        render(&ctx, &graph, &menu, click(row, PointerButton::Secondary));
        let (output, _) = render(&ctx, &graph, &menu, vec![]);
        assert!(label(&output, "Checkout topic/work").is_some());
        assert!(label(&output, "Copy tag name v1").is_some());
        let reset = label(&output, "Reset main to this commit").unwrap();
        render(&ctx, &graph, &menu, click(reset, PointerButton::Primary));
        let (output, _) = render(&ctx, &graph, &menu, vec![]);
        let hard = label(&output, "Hard").unwrap();
        let (_, action) = render(&ctx, &graph, &menu, click(hard, PointerButton::Primary));
        let Some(RowAction::Prompt(GraphDialog::Confirm {
            action: HistoryAction::Git(GitAction::Reset { mode: ResetMode::Hard, .. }),
            text,
        })) = action
        else {
            panic!("Hard reset should open a confirmation dialog");
        };
        assert!(text.contains("1 changed file."));

        menu.busy = true;
        render(&ctx, &graph, &menu, vec![]);
        render(&ctx, &graph, &menu, click(row, PointerButton::Secondary));
        let (output, _) = render(&ctx, &graph, &menu, vec![]);
        let checkout = label(&output, "Checkout topic/work").unwrap();
        let (_, action) = render(&ctx, &graph, &menu, click(checkout, PointerButton::Primary));
        assert!(action.is_none());

        let (mut output, _) = render(&ctx, &graph, &menu, vec![]);
        if label(&output, "Copy commit message").is_none() {
            render(&ctx, &graph, &menu, click(row, PointerButton::Secondary));
            (output, _) = render(&ctx, &graph, &menu, vec![]);
        }
        let copy_message = label(&output, "Copy commit message").unwrap();
        let (output, action) = render(&ctx, &graph, &menu, click(copy_message, PointerButton::Primary));
        assert!(action.is_none());
        assert!(
            output
                .platform_output
                .commands
                .contains(&egui::OutputCommand::CopyText(graph.commits[0].message.clone()))
        );
    }

    #[test]
    fn svn_row_menu_has_revision_actions() {
        let ctx = egui::Context::default();
        let graph = graph(Vcs::Svn);
        let menu = GraphMenu {
            vcs: Vcs::Svn,
            status: &Status::default(),
            branches: &[],
            busy: false,
        };
        let (output, _) = render(&ctx, &graph, &menu, vec![]);
        let row = label(&output, "History subject").unwrap();
        render(&ctx, &graph, &menu, click(row, PointerButton::Secondary));
        let (output, _) = render(&ctx, &graph, &menu, vec![]);
        assert!(label(&output, "Copy revision number").is_some());
        assert!(label(&output, "Copy message").is_some());
        assert!(label(&output, "Copy author").is_some());
        assert!(label(&output, "Create branch here").is_none());
        let update = label(&output, "Update working copy to r123").unwrap();
        let (_, action) = render(&ctx, &graph, &menu, click(update, PointerButton::Primary));
        assert!(matches!(
            action,
            Some(RowAction::Prompt(GraphDialog::Confirm {
                action: HistoryAction::UpdateRevision(123),
                ..
            }))
        ));
    }
}
