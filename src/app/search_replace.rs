use super::{App, Tab, Vcs, background, find::apply_text, project_files, receive};
use crate::{replace, search, workspace};
use eframe::egui;
use regex::Regex;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Receiver;

#[derive(Default)]
pub(super) struct SearchReplace {
    pub(super) shown: bool,
    pub(super) text: String,
    preparing: Option<Receiver<Result<Plan, String>>>,
    confirmation: Option<Plan>,
    writing: Option<Receiver<Outcome>>,
    writing_paths: HashSet<PathBuf>,
}

struct Request {
    root: PathBuf,
    opts: search::Options,
    ignored: Vec<String>,
    replacement: String,
    file: Option<String>,
}

struct BufferEdit {
    path: PathBuf,
    id: u64,
    version: u64,
    edit: replace::Edit,
}

struct DiskEdit {
    path: PathBuf,
    digest: [u8; 32],
    count: usize,
}

struct Plan {
    request: Request,
    re: Regex,
    buffers: Vec<BufferEdit>,
    disk: Vec<DiskEdit>,
    occurrences: usize,
}

#[derive(Default)]
struct Outcome {
    occurrences: usize,
    files: usize,
    failed: usize,
    error: Option<String>,
}

impl SearchReplace {
    pub(super) fn busy(&self) -> bool {
        self.preparing.is_some() || self.confirmation.is_some() || self.writing.is_some()
    }

    pub(super) fn confirming(&self) -> bool {
        self.confirmation.is_some()
    }

    pub(super) fn is_writing(&self, path: &Path) -> bool {
        self.writing_paths.contains(path)
    }

    pub(super) fn status(&self, ui: &mut egui::Ui) {
        if self.preparing.is_some() {
            ui.weak("Counting replacements…");
        } else if self.writing.is_some() {
            ui.weak("Replacing in files…");
        }
    }
}

impl App {
    pub(super) fn can_replace_in_files(&self) -> bool {
        !self.search_replace.busy()
            && !self.search.opts.query.is_empty()
            && self.search.opts == self.search.searched
            && self.search.rx.is_none()
            && self.search.error.is_none()
            && self.search.total > 0
    }

    pub(super) fn start_project_replace(&mut self, file: Option<String>) {
        if !self.can_replace_in_files() {
            return;
        }
        let Some(root) = self.root.clone() else { return };
        let request = Request {
            root,
            opts: self.search.opts.clone(),
            ignored: self.ignored.clone(),
            replacement: self.search_replace.text.clone(),
            file,
        };
        let vcs = self.vcs;
        let open = self
            .tabs
            .iter()
            .filter_map(|tab| {
                let Tab::File(doc) = tab else { return None };
                let rel = workspace::relative_path(&request.root, &doc.path)?;
                request
                    .covers_file(&rel)
                    .then(|| (rel, (doc.path.clone(), doc.id, doc.version, doc.text.clone())))
            })
            .collect();
        self.search_replace.preparing = Some(background(&self.ctx, move || prepare(request, vcs, open)));
    }

    pub(super) fn poll_project_replace(&mut self, ctx: &egui::Context) {
        self.finish_disk_replacements(ctx);
        match receive(&mut self.search_replace.preparing) {
            Some(Ok(plan)) if self.replace_request_current(&plan.request) => {
                if plan.occurrences == 0 {
                    self.message = "No replaceable matches found".into();
                    self.start_search(ctx);
                } else if plan.request.file.is_none() {
                    self.search_replace.confirmation = Some(plan);
                } else {
                    self.apply_project_replace(ctx, plan);
                }
            }
            Some(Ok(_)) => self.message = "Search changed while counting replacements".into(),
            Some(Err(err)) => self.message = err,
            None => {}
        }
    }

    pub(super) fn project_replace_modal(&mut self, ctx: &egui::Context) {
        let Some(plan) = &self.search_replace.confirmation else { return };
        let modal = egui::Modal::new(egui::Id::unique("replace-in-files")).show(ctx, |ui| {
            ui.heading("Replace in files");
            ui.label(format!(
                "Replace {} occurrences in {} files?",
                plan.occurrences,
                plan.buffers.len() + plan.disk.len()
            ));
            ui.horizontal(|ui| {
                if ui.button("Cancel").clicked() {
                    ui.close();
                }
                ui.button("Replace all").clicked()
            })
            .inner
        });
        if modal.inner {
            let plan = self.search_replace.confirmation.take().unwrap();
            self.apply_project_replace(ctx, plan);
        } else if modal.should_close() {
            self.search_replace.confirmation = None;
        }
    }

    fn replace_request_current(&self, request: &Request) -> bool {
        self.root.as_ref() == Some(&request.root)
            && self.search.opts == request.opts
            && self.ignored == request.ignored
            && self.search_replace.text == request.replacement
    }

    fn apply_project_replace(&mut self, ctx: &egui::Context, plan: Plan) {
        let request_current = self.replace_request_current(&plan.request);
        let mut open: HashMap<_, _> = self
            .tabs
            .iter_mut()
            .filter_map(|tab| match tab {
                Tab::File(doc) => Some((doc.path.clone(), doc)),
                _ => None,
            })
            .collect();
        if !request_current
            || plan.buffers.iter().any(|edit| {
                open.get(&edit.path)
                    .is_none_or(|doc| doc.id != edit.id || doc.version != edit.version)
            })
            || plan.disk.iter().any(|edit| open.contains_key(&edit.path))
        {
            self.message = "Search or open files changed. Run replace again.".into();
            self.start_search(ctx);
            return;
        }
        let mut outcome = Outcome::default();
        for edit in plan.buffers {
            apply_text(open.remove(&edit.path).unwrap(), edit.edit.text, edit.edit.char_len);
            outcome.occurrences += edit.edit.count;
            outcome.files += 1;
        }
        if plan.disk.is_empty() {
            self.report_project_replace(ctx, outcome);
        } else {
            self.search_replace.writing_paths = plan.disk.iter().map(|edit| edit.path.clone()).collect();
            self.search_replace.writing = Some(background(&self.ctx, move || {
                write_replacements(&plan.request, &plan.re, plan.disk, outcome)
            }));
        }
    }

    fn finish_disk_replacements(&mut self, ctx: &egui::Context) {
        if let Some(outcome) = receive(&mut self.search_replace.writing) {
            self.report_project_replace(ctx, outcome);
        }
        if self.search_replace.writing.is_none() {
            self.search_replace.writing_paths.clear();
        }
    }

    fn report_project_replace(&mut self, ctx: &egui::Context, outcome: Outcome) {
        self.message = format!("Replaced {} occurrences in {} files", outcome.occurrences, outcome.files);
        if let Some(err) = outcome.error {
            self.message.push_str(&format!(" ({} failed: {err})", outcome.failed));
        }
        self.start_search(ctx);
    }
}

impl Request {
    fn covers_file(&self, rel: &str) -> bool {
        self.file.as_deref().is_none_or(|file| file == rel) && !workspace::is_ignored(rel, &self.ignored)
    }
}

fn prepare(request: Request, vcs: Option<Vcs>, mut open: HashMap<String, (PathBuf, u64, u64, String)>) -> Result<Plan, String> {
    let re = search::build_regex(&request.opts)?;
    let filter = search::PathFilter::new(&request.opts)?;
    let mut files = match &request.file {
        Some(rel) => vec![rel.clone()],
        None => project_files(&request.root, vcs, &request.ignored),
    };
    files.extend(open.keys().cloned());
    files.sort_unstable();
    files.dedup();

    let mut buffers = Vec::new();
    let mut disk = Vec::new();
    let mut occurrences = 0;
    for rel in files {
        if !request.covers_file(&rel) || !filter.allows(&rel) {
            continue;
        }
        let path = request.root.join(&rel);
        let build_edit = |text| replace::project_edit(text, &re, &request.opts, &request.replacement);
        if let Some((path, id, version, text)) = open.remove(&rel) {
            if let Some(edit) = build_edit(&text) {
                occurrences += edit.count;
                buffers.push(BufferEdit { path, id, version, edit });
            }
        } else if validate_disk_file(&request.root, &path).is_ok()
            && let Some(bytes) = search::read_searchable(&path)
            && let Ok(text) = std::str::from_utf8(&bytes)
        {
            let count = replace::project_count(text, &re, &request.opts);
            if count != 0 {
                occurrences += count;
                disk.push(DiskEdit {
                    path,
                    digest: Sha256::digest(&bytes).into(),
                    count,
                });
            }
        }
    }
    Ok(Plan {
        request,
        re,
        buffers,
        disk,
        occurrences,
    })
}

fn validate_disk_file(root: &Path, path: &Path) -> io::Result<()> {
    if path.starts_with(root) && workspace::canonical(path)? == path && fs::symlink_metadata(path)?.is_file() {
        Ok(())
    } else {
        Err(io::Error::other("Expected a regular file inside the project"))
    }
}

fn write_replacements(request: &Request, re: &Regex, disk: Vec<DiskEdit>, mut outcome: Outcome) -> Outcome {
    for item in disk {
        let result = (|| {
            let original = read_unchanged(&request.root, &item)?;
            let text = std::str::from_utf8(&original).map_err(io::Error::other)?;
            if let Some(edit) = replace::project_edit(text, re, &request.opts, &request.replacement)
                && edit.text != text
            {
                write_atomic(&request.root, &item, &edit.text)?;
            }
            Ok::<_, io::Error>(())
        })();
        match result {
            Ok(()) => {
                outcome.occurrences += item.count;
                outcome.files += 1;
            }
            Err(err) => {
                outcome.failed += 1;
                outcome.error.get_or_insert_with(|| format!("{}: {err}", item.path.display()));
            }
        }
    }
    outcome
}

fn read_unchanged(root: &Path, item: &DiskEdit) -> io::Result<Vec<u8>> {
    validate_disk_file(root, &item.path)?;
    let bytes = search::read_searchable(&item.path).ok_or_else(|| io::Error::other("File is no longer readable as text"))?;
    if <[u8; 32]>::from(Sha256::digest(&bytes)) != item.digest {
        return Err(io::Error::other("File changed after matches were counted"));
    }
    Ok(bytes)
}

fn write_atomic(root: &Path, item: &DiskEdit, text: &str) -> io::Result<()> {
    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

    let permissions = fs::metadata(&item.path)?.permissions();
    if permissions.readonly() {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, "File is read-only"));
    }
    let suffix = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
    let temp = item
        .path
        .with_file_name(format!(".edittr-replace-{}-{suffix}.tmp", std::process::id()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temp)?;
    let written = file.write_all(text.as_bytes()).and_then(|()| file.set_permissions(permissions));
    drop(file);
    let result = written.and_then(|()| {
        // The file can change while expanding the replacement or staging the write.
        read_unchanged(root, item)?;
        fs::rename(&temp, &item.path)
    });
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plans_unlimited_replacements_with_the_search_filters_and_open_buffers() {
        let root = std::env::temp_dir().join(format!("edittr-replace-test-{}", std::process::id()));
        fs::create_dir_all(root.join("ignored")).unwrap();
        let root = root.canonicalize().unwrap();
        let open_path = root.join("open.txt");
        let closed = root.join("closed.txt");
        let changed = root.join("changed.txt");
        let many = "word\r\n".repeat(1001);
        fs::write(&closed, &many).unwrap();
        fs::write(&changed, "word").unwrap();
        fs::write(&open_path, "disk word").unwrap();
        fs::write(root.join("ignored/untouched.txt"), "word").unwrap();
        fs::write(root.join("excluded.txt"), "word").unwrap();
        fs::write(root.join("other.rs"), "word").unwrap();

        let plan = prepare(
            Request {
                root: root.clone(),
                opts: search::Options {
                    query: r"^(word)$".into(),
                    regex: true,
                    include: "*.txt".into(),
                    exclude: "excluded.txt".into(),
                    ..Default::default()
                },
                ignored: vec!["ignored".into()],
                replacement: "<$1>".into(),
                file: None,
            },
            None,
            HashMap::from([("open.txt".into(), (open_path.clone(), 1, 2, "unsaved\nword".into()))]),
        )
        .unwrap();
        assert_eq!(plan.occurrences, 1003);
        assert_eq!(plan.disk.len(), 2);
        assert_eq!(plan.buffers.len(), 1);
        assert_eq!(plan.buffers[0].edit.text, "unsaved\n<word>");
        assert_eq!(fs::read_to_string(&closed).unwrap(), many);

        // A disk edit made after confirmation was prepared must survive.
        fs::write(&changed, "external edit").unwrap();
        let outcome = write_replacements(&plan.request, &plan.re, plan.disk, Outcome::default());
        assert_eq!((outcome.occurrences, outcome.files, outcome.failed), (1001, 1, 1));
        assert_eq!(fs::read_to_string(&closed).unwrap(), "<word>\r\n".repeat(1001));
        assert_eq!(fs::read_to_string(&changed).unwrap(), "external edit");
        assert_eq!(fs::read_to_string(&open_path).unwrap(), "disk word");
        fs::remove_dir_all(root).unwrap();
    }
}
