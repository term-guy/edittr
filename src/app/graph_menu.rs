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
    pub fn show(&self, ui: &mut egui::Ui, commit: &Commit, reference: Option<&CommitRef>) -> Option<RowAction> {
        if let Some(reference) = reference {
            return self.ref_menu(ui, commit, reference);
        }
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
                        CommitRef::Head | CommitRef::SvnBase | CommitRef::Other(_) => {}
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

    fn current_branch(&self) -> Option<&str> {
        self.branches.iter().find(|name| **name == self.status.branch).map(String::as_str)
    }

    fn checkout(&self, ui: &mut egui::Ui, commit: &Commit, reference: &CommitRef, badge: bool) -> Option<RowAction> {
        match reference {
            CommitRef::Local { name, head } => {
                let current = *head || self.status.branch == *name;
                let suffix = if current { " (current)" } else { "" };
                button(ui, !current, format!("Checkout {name}{suffix}")).then(|| RowAction::git(GitAction::Checkout(name.clone())))
            }
            CommitRef::Remote(remote) => {
                let (_, name) = remote.split_once('/')?;
                let has_local = self.branches.iter().any(|local| local == name)
                    || commit
                        .refs
                        .iter()
                        .any(|reference| matches!(reference, CommitRef::Local { name: local, .. } if local == name));
                if has_local {
                    return (badge && button(ui, true, format!("Checkout {remote} (detached HEAD)"))).then(|| {
                        RowAction::confirm(
                            GitAction::Detach(format!("refs/remotes/{remote}")),
                            format!("Check out {remote} with a detached HEAD?"),
                        )
                    });
                }
                button(ui, true, format!("Checkout {name} (new local branch tracking {remote})")).then(|| {
                    RowAction::git(GitAction::Track {
                        local: name.into(),
                        remote: format!("refs/remotes/{remote}"),
                    })
                })
            }
            _ => None,
        }
    }

    fn merge(&self, ui: &mut egui::Ui, reference: &CommitRef) -> Option<RowAction> {
        let branch = self.current_branch()?;
        let (name, prefix) = match reference {
            CommitRef::Local { name, head: false } if name != branch => (name, "refs/heads/"),
            CommitRef::Remote(name) => (name, "refs/remotes/"),
            _ => return None,
        };
        let label = format!("Merge {name} into {branch}");
        button(ui, true, &label).then(|| {
            RowAction::confirm(
                GitAction::Merge {
                    branch: branch.into(),
                    source: format!("{prefix}{name}"),
                },
                format!("{label}?"),
            )
        })
    }

    fn delete_branch(&self, ui: &mut egui::Ui, reference: &CommitRef) -> Option<RowAction> {
        let CommitRef::Local { name, head: false } = reference else {
            return None;
        };
        (self.status.branch != *name && button(ui, true, format!("Delete branch {name}"))).then(|| {
            RowAction::confirm(
                GitAction::DeleteBranch {
                    name: name.clone(),
                    force: false,
                },
                format!("Delete the local branch {name}?"),
            )
        })
    }

    fn ref_menu(&self, ui: &mut egui::Ui, commit: &Commit, reference: &CommitRef) -> Option<RowAction> {
        let action = ui
            .add_enabled_ui(!self.busy, |ui| match reference {
                CommitRef::Local { .. } | CommitRef::Remote(_) => self
                    .checkout(ui, commit, reference, true)
                    .or_else(|| self.merge(ui, reference))
                    .or_else(|| self.delete_branch(ui, reference)),
                CommitRef::Tag(name) => {
                    if button(ui, true, format!("Checkout tag {name} (detached HEAD)")) {
                        return Some(RowAction::confirm(
                            GitAction::Detach(format!("refs/tags/{name}")),
                            format!("Check out tag {name} with a detached HEAD?"),
                        ));
                    }
                    button(ui, true, format!("Delete tag {name}"))
                        .then(|| RowAction::confirm(GitAction::DeleteTag(name.clone()), format!("Delete the local tag {name}?")))
                }
                _ => None,
            })
            .inner;
        ui.separator();
        match reference {
            CommitRef::Local { name, .. } | CommitRef::Remote(name) => copy(ui, "Copy branch name", name),
            CommitRef::Tag(name) => copy(ui, "Copy tag name", name),
            _ => {}
        }
        action
    }

    fn git_commands(&self, ui: &mut egui::Ui, commit: &Commit) -> Option<RowAction> {
        let current = self.current_branch();
        for reference in &commit.refs {
            if let Some(action) = self.checkout(ui, commit, reference, false) {
                return Some(action);
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

        for reference in &commit.refs {
            if let Some(action) = self.merge(ui, reference) {
                return Some(action);
            }
        }
        if button(ui, true, "Create tag here…") {
            return Some(RowAction::name_ref(commit, false));
        }
        for reference in &commit.refs {
            if let Some(action) = self.delete_branch(ui, reference) {
                return Some(action);
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
                    graph.ui(ui, None, |ui, commit, reference| action = menu.show(ui, commit, reference)),
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
    fn badge_menus_only_show_actions_for_the_clicked_ref() {
        let graph = graph(Vcs::Git);
        let status = Status {
            branch: "main".into(),
            ..Default::default()
        };
        let menu = GraphMenu {
            vcs: Vcs::Git,
            status: &status,
            branches: &["main".into(), "topic/work".into()],
            busy: false,
        };

        for (badge, item, absent) in [
            ("topic/work", "Checkout topic/work", "Copy tag name"),
            ("v1", "Delete tag v1", "Checkout topic/work"),
        ] {
            let ctx = egui::Context::default();
            let (output, _) = render(&ctx, &graph, &menu, vec![]);
            let badge = label(&output, badge).unwrap();
            render(&ctx, &graph, &menu, click(badge, PointerButton::Secondary));
            let (output, _) = render(&ctx, &graph, &menu, vec![]);
            let item = label(&output, item).unwrap();
            assert!(label(&output, absent).is_none());
            assert!(label(&output, "Copy commit hash").is_none());
            let (_, action) = render(&ctx, &graph, &menu, click(item, PointerButton::Primary));
            match action {
                Some(RowAction::Run(HistoryAction::Git(GitAction::Checkout(name)))) => assert_eq!(name, "topic/work"),
                Some(RowAction::Prompt(GraphDialog::Confirm {
                    action: HistoryAction::Git(GitAction::DeleteTag(name)),
                    ..
                })) => {
                    assert_eq!(name, "v1");
                }
                _ => panic!("Unexpected ref action"),
            }
        }
    }

    #[test]
    fn row_offers_each_eligible_checkout_from_real_git_decorations() {
        use crate::app::commit_view::tests::TestDir;
        use crate::git;

        let dir = TestDir::new();
        let origin = dir.0.join("origin");
        let local = dir.0.join("local");
        std::fs::create_dir(&origin).unwrap();
        let upstream = |args: &[&str]| git::run(&origin, args).unwrap();
        upstream(&["init", "-b", "main"]);
        upstream(&["config", "user.name", "Test Author"]);
        upstream(&["config", "user.email", "author@example.invalid"]);
        upstream(&["config", "commit.gpgsign", "false"]);
        upstream(&["config", "core.hooksPath", ".git/test-hooks"]);
        upstream(&["commit", "--allow-empty", "-m", "History subject"]);
        upstream(&["branch", "remote/topic"]);
        upstream(&["branch", "local-one"]);
        upstream(&["tag", "--no-sign", "v1"]);
        upstream(&["tag", "--no-sign", "release/1,2"]);
        git::run(&dir.0, &["clone", "--local", "origin", "local"]).unwrap();
        let run = |args: &[&str]| git::run(&local, args).unwrap();
        run(&["branch", "local-one"]);
        run(&["branch", "local/two"]);

        let graph = Graph::build(git::log(&local, 10).unwrap());
        let commit = &graph.commits[0];
        for name in ["origin/main", "origin/local-one", "origin/remote/topic"] {
            assert!(commit.refs.contains(&CommitRef::Remote(name.into())));
        }
        for name in ["v1", "release/1,2"] {
            assert!(commit.refs.contains(&CommitRef::Tag(name.into())));
        }

        for (item, expected) in [
            ("Checkout main (current)", None),
            ("Checkout local-one", Some("local-one")),
            ("Checkout local/two", Some("local/two")),
            (
                "Checkout remote/topic (new local branch tracking origin/remote/topic)",
                Some("remote/topic"),
            ),
        ] {
            run(&["switch", "main"]);
            let status = git::status(&local).unwrap();
            let branches = git::branches(&local);
            let menu = GraphMenu {
                vcs: Vcs::Git,
                status: &status,
                branches: &branches,
                busy: false,
            };
            let ctx = egui::Context::default();
            let (output, _) = render(&ctx, &graph, &menu, vec![]);
            let row = label(&output, "History subject").unwrap();
            render(&ctx, &graph, &menu, click(row, PointerButton::Secondary));
            let (output, _) = render(&ctx, &graph, &menu, vec![]);
            assert!(label(&output, "Checkout local-one (new local branch tracking").is_none());
            let item = label(&output, item).unwrap();
            let (_, action) = render(&ctx, &graph, &menu, click(item, PointerButton::Primary));
            if let Some(expected) = expected {
                let Some(RowAction::Run(action)) = action else {
                    panic!("Expected a checkout action")
                };
                action.run(Vcs::Git, &local).unwrap();
                assert_eq!(run(&["branch", "--show-current"]).trim(), expected);
            } else {
                assert!(action.is_none());
            }
        }
        assert_eq!(
            run(&["rev-parse", "--symbolic-full-name", "@{upstream}"]).trim(),
            "refs/remotes/origin/remote/topic"
        );
        GitAction::Detach("refs/tags/v1".into()).run(&local).unwrap();
        assert!(run(&["branch", "--show-current"]).trim().is_empty());
        GitAction::DeleteTag("v1".into()).run(&local).unwrap();
        assert_eq!(run(&["tag", "--list"]).trim(), "release/1,2");
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
