use super::*;

#[derive(Default)]
pub(super) enum Target {
    #[default]
    None,
    Caret(usize),
    Position(usize, symbols::Columns),
    Line(goto_line::LineTarget),
}

impl Target {
    fn select(self, tab: &mut Tab) {
        if let Tab::File(doc) = tab {
            match self {
                Self::None => {}
                Self::Caret(index) => doc.goto = Some((index, index)),
                Self::Position(line, cols) => doc.goto = Some(symbols::selection(&doc.text, line, cols)),
                Self::Line(target) => goto_line::select_target(doc, target),
            }
        }
    }
}

enum Opened {
    Text(DiskText),
    Large(u64),
    Hex(crate::hex::Source),
}

pub(super) struct OpenJob {
    path: PathBuf,
    target: Target,
    focus: bool,
    rx: Option<Receiver<Result<Opened, String>>>,
}

impl App {
    pub(super) fn open_file(&mut self, path: &Path) {
        self.open_with_target(path, Target::None);
    }

    pub(super) fn unfocus_pending_opens(&mut self) {
        for job in &mut self.open_jobs {
            job.focus = false;
        }
    }

    pub(super) fn open_with_target(&mut self, path: &Path, target: Target) {
        let path = workspace::plain_path(path);
        self.unfocus_pending_opens();
        if let Some(i) = self.tabs.iter().position(|tab| match tab {
            Tab::File(d) => d.path == path,
            Tab::Large(v) => v.path == path,
            Tab::Hex(v) => v.is_file(&path),
            _ => false,
        }) {
            self.active = i;
            target.select(&mut self.tabs[i]);
            return;
        }
        if self.search_replace.is_writing(&path) {
            self.message = "Wait for replacements to finish before opening this file".into();
            return;
        }
        if let Some(job) = self.open_jobs.iter_mut().find(|job| job.path == path) {
            job.focus = true;
            job.target = target;
            return;
        }
        let requested = path.clone();
        let rx = background(&self.ctx, move || {
            let opened = || -> std::io::Result<Opened> {
                let source = crate::hex::Source::open(&requested)?;
                let size = source.size;
                if source.is_binary()? {
                    Ok(Opened::Hex(source))
                } else if size >= crate::large::THRESHOLD {
                    Ok(Opened::Large(size))
                } else {
                    match read_doc_text(&requested)? {
                        Some(text) => Ok(Opened::Text(text)),
                        None => Ok(Opened::Hex(source)),
                    }
                }
            };
            opened().map_err(|err| format!("{}: {err}", requested.display()))
        });
        self.open_jobs.push(OpenJob {
            path,
            target,
            focus: true,
            rx: Some(rx),
        });
    }

    pub(super) fn poll_open_files(&mut self) {
        let mut jobs = std::mem::take(&mut self.open_jobs);
        for mut job in jobs.drain(..) {
            let result = receive(&mut job.rx);
            if job.rx.is_some() {
                self.open_jobs.push(job);
                continue;
            }
            let index = self.tabs.len();
            let path = job.path;
            match result {
                Some(Ok(Opened::Text(text))) => {
                    if self.tabs.iter().any(|t| matches!(t, Tab::File(doc) if doc.path == path)) {
                        continue;
                    }
                    self.add_file_tab(&path, text.text, text.crlf);
                }
                Some(Ok(Opened::Large(size))) => {
                    let viewer = crate::large::Viewer::open(&path, size, &self.ctx);
                    self.tabs.push(Tab::Large(Box::new(viewer)));
                }
                Some(Ok(Opened::Hex(source))) => {
                    self.tabs.push(Tab::Hex(Box::new(super::hex_view::Viewer::file(path, source))));
                }
                Some(Err(err)) => {
                    self.message = err;
                    continue;
                }
                None => continue,
            }
            job.target.select(&mut self.tabs[index]);
            if job.focus {
                self.active = index;
            }
        }
    }
}
