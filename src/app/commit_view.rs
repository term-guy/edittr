use super::{DiffSource, DiffTab, background, diff_colors, diff_view, receive, side_by_side_view};
use crate::git::{self, Commit};
use crate::icons as ic;
use crate::tree::{self, Row};
use crate::vcs::Vcs;
use eframe::egui::{self, RichText};
use serde::Deserialize;
use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;

#[derive(Clone)]
struct Change {
    path: String,
    old_path: Option<String>,
    code: char,
}

struct Files {
    changes: Vec<Change>,
    svn_url: Option<url::Url>,
    rows: Vec<Row>,
    visible: Vec<usize>,
    selected: Option<usize>,
}

impl Files {
    fn new(changes: Vec<Change>, svn_url: Option<url::Url>) -> Self {
        let paths: Vec<_> = changes.iter().map(|change| change.path.as_str()).collect();
        let rows = tree::layout(&paths, &HashSet::new());
        let selected = rows.iter().find_map(|row| match row {
            Row::File { index, .. } => Some(*index),
            Row::Folder { .. } => None,
        });
        let visible = (0..rows.len()).collect();
        Self {
            changes,
            svn_url,
            rows,
            visible,
            selected,
        }
    }

    fn toggle_folder(&mut self, index: usize) {
        let Row::Folder { collapsed, .. } = &mut self.rows[index] else {
            return;
        };
        *collapsed = !*collapsed;

        self.visible.clear();
        let mut hidden_below = None;
        for (index, row) in self.rows.iter().enumerate() {
            let depth = match row {
                Row::Folder { depth, .. } | Row::File { depth, .. } => *depth,
            };
            if hidden_below.is_some_and(|parent| depth > parent) {
                continue;
            }
            self.visible.push(index);
            hidden_below = match row {
                Row::Folder { collapsed: true, .. } => Some(depth),
                _ => None,
            };
        }
    }

    fn tree(&mut self, ui: &mut egui::Ui, hash: &str) {
        let row_height = ui.text_style_height(&egui::TextStyle::Body) + 2.0 * ui.spacing().button_padding.y;
        let (added, removed) = diff_colors(ui.visuals().dark_mode);
        let mut folder = None;
        egui::ScrollArea::vertical().id_salt(hash).auto_shrink([false, false]).show_rows(
            ui,
            row_height,
            self.visible.len(),
            |ui, visible| {
                for &row_index in &self.visible[visible] {
                    let row = &self.rows[row_index];
                    let (depth, selected, label, change) = match row {
                        Row::Folder {
                            depth, label, collapsed, ..
                        } => {
                            let (caret, icon) = if *collapsed {
                                (ic::CARET_RIGHT, ic::FOLDER)
                            } else {
                                (ic::CARET_DOWN, ic::FOLDER_OPEN)
                            };
                            (*depth, false, format!("{caret} {icon} {label}"), None)
                        }
                        Row::File { depth, index } => {
                            let change = &self.changes[*index];
                            let name = change.path.rsplit('/').next().unwrap_or(&change.path).replace(['\n', '\r'], " ");
                            let color = match change.code {
                                'A' => added,
                                'D' => removed,
                                'R' | 'C' => ui.visuals().hyperlink_color,
                                _ => ui.visuals().warn_fg_color,
                            };
                            (*depth, self.selected == Some(*index), name, Some((change, color)))
                        }
                    };
                    ui.horizontal(|ui| {
                        ui.add_space(depth as f32 * 12.0);
                        if let Some((change, color)) = change {
                            ui.label(RichText::new(change.code.to_string()).monospace().color(color));
                        }
                        let response = ui.add_sized(
                            [ui.available_width(), row_height],
                            egui::Button::selectable(selected, label).truncate(),
                        );
                        let response = if let Some((change, _)) = change {
                            response.on_hover_text(match &change.old_path {
                                Some(old) => format!("{old} → {}", change.path),
                                None => change.path.clone(),
                            })
                        } else {
                            response
                        };
                        if response.clicked() {
                            match row {
                                Row::Folder { .. } => folder = Some(row_index),
                                Row::File { index, .. } => self.selected = Some(*index),
                            }
                        }
                    });
                }
            },
        );
        if let Some(index) = folder {
            self.toggle_folder(index);
        }
    }
}

type LoadedFiles = Result<Files, String>;
type FileKey = (String, String);
type LoadedDiff = Result<DiffTab, String>;

#[derive(Default)]
pub(super) struct CommitView {
    hash: String,
    files: Option<LoadedFiles>,
    list_rx: Option<Receiver<(String, LoadedFiles)>>,
    diff_rx: Option<Receiver<(FileKey, LoadedDiff)>>,
    // Most recent selection first. Bound the retained parsed patches.
    diffs: VecDeque<(FileKey, LoadedDiff)>,
    inline: bool,
}

impl CommitView {
    /// The file list or a file's diff is still loading.
    #[cfg(feature = "tour")]
    pub(super) fn loading(&self) -> bool {
        self.list_rx.is_some() || self.diff_rx.is_some()
    }

    pub(super) fn clear_selection(&mut self) {
        self.hash.clear();
        self.files = None;
    }

    pub(super) fn poll(&mut self) {
        if let Some((hash, files)) = receive(&mut self.list_rx)
            && hash == self.hash
        {
            self.files = Some(files);
        }
        if let Some(diff) = receive(&mut self.diff_rx) {
            self.diffs.push_front(diff);
            self.diffs.truncate(24);
        }
    }

    pub(super) fn show(&mut self, ui: &mut egui::Ui, commit: &Commit, root: &Path, vcs: Vcs) {
        if self.hash != commit.hash {
            self.clear_selection();
            self.hash.clone_from(&commit.hash);
        }
        egui::ScrollArea::vertical()
            .id_salt(("commit header", &self.hash))
            .max_height(76.0)
            .show(ui, |ui| {
                ui.add(
                    egui::Label::new(format!(
                        "{}  •  {}  •  {}\n{}",
                        commit.hash, commit.author, commit.date, commit.message
                    ))
                    .selectable(true),
                );
            });
        ui.separator();

        let Some(files) = &mut self.files else {
            ui.weak("Loading changed files…");
            if self.list_rx.is_none() {
                let (root, hash, parent) = (root.to_owned(), commit.hash.clone(), commit.parents.first().cloned());
                self.list_rx = Some(background(ui.ctx(), move || {
                    let files = load_files(&root, vcs, &hash, parent.as_deref());
                    (hash, files)
                }));
            }
            return;
        };
        let files = match files {
            Ok(files) => files,
            Err(error) => {
                ui.label(error.as_str());
                if ui.button("Retry").clicked() {
                    self.files = None;
                }
                return;
            }
        };
        if files.changes.is_empty() {
            ui.weak("No changed files.");
            return;
        }

        egui::Panel::left("commit_files")
            .resizable(true)
            .default_size(220.0)
            .size_range(100.0..=(ui.available_width() * 0.55).max(100.0))
            .show(ui, |ui| files.tree(ui, &self.hash));

        egui::CentralPanel::no_frame().show(ui, |ui| self.show_diff(ui, commit, root));
    }

    fn show_diff(&mut self, ui: &mut egui::Ui, commit: &Commit, root: &Path) {
        let Some(Ok(files)) = &self.files else { return };
        let Some(index) = files.selected else { return };
        let change = &files.changes[index];
        let key = (self.hash.as_str(), change.path.as_str());
        let cached = self.diffs.iter().position(|(k, _)| (k.0.as_str(), k.1.as_str()) == key);
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.inline, false, format!("{} Side by side", ic::COLUMNS));
            ui.selectable_value(&mut self.inline, true, format!("{} Inline", ic::ROWS));
            ui.add(egui::Label::new(change.path.replace(['\n', '\r'], " ")).truncate())
                .on_hover_text(&change.path);
        });
        if let Some(cached) = cached {
            if cached != 0 {
                let entry = self.diffs.remove(cached).unwrap();
                self.diffs.push_front(entry);
            }
            let (_, diff) = self.diffs.front_mut().unwrap();
            match diff {
                Ok(diff) => {
                    ui.push_id(key, |ui| {
                        if self.inline {
                            diff_view(ui, &diff.lines, "commit_patch");
                        } else {
                            side_by_side_view(ui, diff);
                        }
                    });
                }
                Err(error) => {
                    ui.label(error.as_str());
                    if ui.button("Retry").clicked() {
                        self.diffs.pop_front();
                    }
                }
            }
        } else {
            ui.weak("Loading file changes…");
            if self.diff_rx.is_none() {
                let (root, hash, parent) = (root.to_owned(), commit.hash.clone(), commit.parents.first().cloned());
                let (change, svn_url) = (change.clone(), files.svn_url.clone());
                self.diff_rx = Some(background(ui.ctx(), move || {
                    let diff = load_diff(&root, svn_url, &hash, parent.as_deref(), &change).map(|patch| {
                        DiffTab::new(
                            change.path.clone(),
                            PathBuf::from(&change.path),
                            &patch,
                            DiffSource::Commit,
                            false,
                            false,
                        )
                    });
                    ((hash, change.path), diff)
                }));
            }
        }
    }
}

fn git_patch(root: &Path, hash: &str, parent: Option<&str>, options: &[&str], paths: &[&str]) -> Result<String, String> {
    let mut args = vec![
        "--literal-pathspecs",
        "diff-tree",
        "-r",
        "--root",
        "--no-commit-id",
        "--no-color",
        "--no-ext-diff",
        "-M",
    ];
    args.extend_from_slice(options);
    args.extend(parent);
    args.push(hash);
    args.push("--");
    args.extend_from_slice(paths);
    git::run(root, &args)
}

fn git_changes(output: &str) -> Result<Vec<Change>, String> {
    let mut fields = output.split_terminator('\0');
    let mut changes = Vec::new();
    while let Some(status) = fields.next() {
        let mut field = || fields.next().filter(|s| !s.is_empty()).ok_or("Invalid Git name-status output");
        let code = status.chars().next().ok_or("Invalid Git name-status output")?;
        let first_path = field()?.to_owned();
        let (old_path, path) = if matches!(code, 'R' | 'C') {
            (Some(first_path), field()?.to_owned())
        } else {
            (None, first_path)
        };
        changes.push(Change { path, old_path, code });
    }
    Ok(changes)
}

#[derive(Deserialize)]
struct SvnLog {
    #[serde(rename = "logentry")]
    entry: SvnEntry,
}

#[derive(Deserialize)]
struct SvnEntry {
    #[serde(default)]
    paths: SvnPaths,
}

#[derive(Default, Deserialize)]
struct SvnPaths {
    #[serde(rename = "path", default)]
    changes: Vec<SvnChange>,
}

#[derive(Deserialize)]
struct SvnChange {
    #[serde(rename = "@action")]
    code: char,
    #[serde(rename = "$text")]
    path: String,
}

fn load_files(root: &Path, vcs: Vcs, hash: &str, parent: Option<&str>) -> LoadedFiles {
    match vcs {
        Vcs::Git => {
            let output = git_patch(root, hash, parent, &["--name-status", "-z"], &[])?;
            Ok(Files::new(git_changes(&output)?, None))
        }
        Vcs::Svn => {
            let revision = hash.trim_start_matches('r');
            let repo_url = crate::svn::run(root, &["info", "--show-item", "repos-root-url", "--", "."])?;
            let repo_url = repo_url.trim_end();
            let log = crate::svn::run(root, &["log", "-v", "-r", revision, "--xml", "--", &format!("{repo_url}@")])?;
            let log: SvnLog = quick_xml::de::from_str(&log).map_err(|err| format!("Invalid Subversion log: {err}"))?;
            let changes = log.entry.paths.changes.into_iter().map(|change| Change {
                path: change.path.trim_start_matches('/').to_owned(),
                old_path: None,
                code: change.code,
            });
            let url = url::Url::parse(repo_url).map_err(|err| format!("Invalid Subversion repository URL: {err}"))?;
            Ok(Files::new(changes.collect(), Some(url)))
        }
    }
}

fn load_diff(root: &Path, svn_url: Option<url::Url>, hash: &str, parent: Option<&str>, change: &Change) -> Result<String, String> {
    if let Some(mut url) = svn_url {
        let revision = hash.trim_start_matches('r');
        url.path_segments_mut()
            .map_err(|_| "Invalid Subversion repository URL")?
            .pop_if_empty()
            .extend(change.path.split('/').filter(|part| !part.is_empty()));
        crate::svn::run(
            root,
            &["diff", "--internal-diff", "-c", revision, "--", &format!("{url}@{revision}")],
        )
    } else {
        let paths: Vec<_> = change.old_path.as_deref().into_iter().chain([change.path.as_str()]).collect();
        git_patch(root, hash, parent, &["-p", "--no-textconv", "--submodule=short"], &paths)
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};

    pub(in crate::app) struct TestDir(pub PathBuf);

    impl TestDir {
        pub(in crate::app) fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "edittr-commit-view-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn detail_layout_virtualizes_files_and_renders_both_diff_modes() {
        let changes = (0..4000)
            .rev()
            .map(|i| Change {
                path: format!("folder/file{i:04}.txt"),
                old_path: None,
                code: 'A',
            })
            .collect();
        let mut files = Files::new(changes, None);
        let selected = files.selected.unwrap();
        let path = files.changes[selected].path.clone();
        assert_eq!(path, "folder/file0000.txt");
        files.toggle_folder(0);
        assert_eq!(files.visible, [0]);
        files.toggle_folder(0);
        assert_eq!(files.visible.len(), 4001);

        let commit = Commit {
            hash: "test hash".into(),
            short_hash: "test".into(),
            parents: Vec::new(),
            author: "Test Author".into(),
            date: "2026-01-01".into(),
            refs: Vec::new(),
            subject: "Commit subject".into(),
            message: "Commit subject\n\nCommit body".into(),
        };
        let patch = "@@ -0,0 +1 @@\n+unique patched line\n";
        let key = (commit.hash.clone(), path.clone());
        let mut detail = CommitView {
            hash: commit.hash.clone(),
            files: Some(Ok(files)),
            diffs: [(
                key,
                Ok(DiffTab::new(
                    path.clone(),
                    PathBuf::from(path),
                    patch,
                    DiffSource::Commit,
                    false,
                    false,
                )),
            )]
            .into(),
            ..Default::default()
        };
        let ctx = egui::Context::default();
        for inline in [false, true] {
            detail.inline = inline;
            let mut frame = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1000.0, 260.0))),
                    ..Default::default()
                },
                |ui| detail.show(ui, &commit, Path::new("."), Vcs::Git),
            );
            frame.textures_delta.clear();
            let labels: Vec<_> = frame
                .shapes
                .iter()
                .filter_map(|shape| match &shape.shape {
                    egui::Shape::Text(text) => Some((text.galley.text(), text.pos.x)),
                    _ => None,
                })
                .collect();
            let first = labels.iter().find(|(text, _)| *text == "file0000.txt").unwrap().1;
            let expected = if inline { "+unique patched line" } else { "unique patched line" };
            let diff = labels.iter().find(|(text, _)| *text == expected).unwrap().1;
            assert!(diff > first);
            assert!(!labels.iter().any(|(text, _)| *text == "file3999.txt"));
            assert!(labels.iter().any(|(text, _)| text.contains("Commit body")));
        }
    }

    #[test]
    fn git_details_handle_root_literal_paths_renames_and_first_parent() {
        let dir = TestDir::new();
        let git = |args: &[&str]| git::run(&dir.0, args).unwrap();
        git(&["init", "-b", "main"]);
        git(&["config", "user.name", "Test Author"]);
        git(&["config", "user.email", "author@example.invalid"]);
        git(&["config", "commit.gpgsign", "false"]);
        git(&["config", "core.hooksPath", ".git/test-hooks"]);
        let literal = "a[1].txt";
        fs::write(dir.0.join(literal), "one\ntwo\nthree\nfour\n").unwrap();
        fs::write(dir.0.join("a1.txt"), "unrelated\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-m", "Root"]);
        let root = git(&["rev-parse", "HEAD"]).trim().to_owned();
        let files = load_files(&dir.0, Vcs::Git, &root, None).unwrap();
        assert_eq!(files.changes.len(), 2);
        let added = files.changes.iter().find(|change| change.path == literal).unwrap();
        assert_eq!(added.code, 'A');
        let patch = load_diff(&dir.0, None, &root, None, added).unwrap();
        assert!(patch.contains("+one") && !patch.contains("+unrelated"), "{patch}");

        git(&["switch", "-c", "topic"]);
        git(&["mv", literal, "renamed file.txt"]);
        fs::write(dir.0.join("renamed file.txt"), "one\ntwo\nthree\nchanged\n").unwrap();
        git(&["commit", "-am", "Rename"]);
        git(&["switch", "main"]);
        fs::write(dir.0.join("a1.txt"), "main branch change\n").unwrap();
        git(&["commit", "-am", "Edit unrelated file"]);
        git(&["merge", "--no-ff", "--no-edit", "topic"]);
        let history = git::log(&dir.0, 1).unwrap();
        let merge = &history[0];
        assert_eq!(merge.parents.len(), 2);
        let parent = Some(merge.parents[0].as_str());
        let files = load_files(&dir.0, Vcs::Git, &merge.hash, parent).unwrap();
        let [rename] = files.changes.as_slice() else {
            panic!("Expected the side branch change only")
        };
        assert_eq!(
            (rename.code, rename.old_path.as_deref(), rename.path.as_str()),
            ('R', Some(literal), "renamed file.txt")
        );
        let patch = load_diff(&dir.0, None, &merge.hash, parent, rename).unwrap();
        assert!(
            patch.contains("rename from a[1].txt") && patch.contains("-four") && patch.contains("+changed"),
            "{patch}"
        );
    }

    #[test]
    #[ignore = "requires svn"]
    fn svn_details_use_historical_urls_for_added_and_deleted_files() {
        let dir = TestDir::new();
        let output = crate::svn::command(&dir.0, "svnadmin").args(["create", "repo"]).output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let url = url::Url::from_directory_path(dir.0.join("repo").canonicalize().unwrap()).unwrap();
        let svn = |root, args: &[&str]| crate::svn::run(root, args).unwrap();
        svn(&dir.0, &["checkout", url.as_str(), "wc"]);
        let wc = dir.0.join("wc");
        let path = "file @% & test.txt";
        let target = format!("{path}@");
        fs::write(wc.join(path), "a line\n").unwrap();
        svn(wc.as_path(), &["add", "--", &target]);
        svn(wc.as_path(), &["commit", "-m", "Add file"]);
        svn(wc.as_path(), &["delete", "--", &target]);
        svn(wc.as_path(), &["commit", "-m", "Delete file"]);
        for (revision, code, line) in [("r1", 'A', "+a line"), ("r2", 'D', "-a line")] {
            let files = load_files(&wc, Vcs::Svn, revision, None).unwrap();
            let [change] = files.changes.as_slice() else {
                panic!("Expected one file")
            };
            assert_eq!((change.code, change.path.as_str()), (code, path));
            let patch = load_diff(&wc, files.svn_url, revision, None, change).unwrap();
            assert!(patch.contains(line), "{patch}");
        }
    }
}
