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
    pub(super) path: PathBuf,
    target: Target,
    focus: bool,
    rx: Option<Receiver<Result<Opened, String>>>,
}

pub(super) struct ReopenJob {
    id: u64,
    version: u64,
    path: PathBuf,
    rx: Option<Receiver<std::io::Result<DiskText>>>,
}

fn is_text_sample(sample: &[u8], previous: Option<Encoding>) -> bool {
    Encoding::detect_bom(sample).is_some_and(Encoding::is_utf16)
        || previous.is_some_and(Encoding::is_unmarked_utf16)
        || !crate::hex::has_binary_controls(sample)
}

fn normalize(text: String, encoding: Encoding) -> DiskText {
    let crlf = text.contains("\r\n");
    DiskText {
        raw_crlf: crlf.then(|| Arc::from(text.as_str())),
        text: if crlf { text.replace("\r\n", "\n") } else { text },
        crlf,
        encoding,
    }
}

pub(super) fn read_disk_text(path: &Path, previous: Option<Encoding>) -> std::io::Result<Option<DiskText>> {
    let bytes = std::fs::read(path)?;
    if !is_text_sample(&bytes[..bytes.len().min(8000)], previous) {
        return Ok(None);
    }
    let (text, encoding) = Encoding::decode_auto(bytes, previous)?;
    Ok(Some(normalize(text, encoding)))
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
            self.activate_tab(i);
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
                let sample = source.read(0..8000)?;
                if size >= crate::large::THRESHOLD {
                    // The large-file viewer indexes UTF-8 by byte. Keep its existing binary check.
                    if crate::hex::is_binary(&sample) || Encoding::detect_bom(&sample).is_some_and(Encoding::is_utf16) {
                        Ok(Opened::Hex(source))
                    } else {
                        Ok(Opened::Large(size))
                    }
                } else if !is_text_sample(&sample, None) {
                    Ok(Opened::Hex(source))
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

    pub(super) fn reopen_with_encoding(&mut self, index: usize, encoding: Encoding) {
        let Some(Tab::File(doc)) = self.tabs.get(index) else {
            return;
        };
        if doc.untitled {
            return;
        }
        if doc.dirty() {
            self.confirm = Some(Confirm::ReopenEncoding(doc.id, encoding, doc.name()));
        } else {
            self.start_reopen_with_encoding(doc.id, encoding);
        }
    }

    pub(super) fn start_reopen_with_encoding(&mut self, id: u64, encoding: Encoding) {
        let Some(doc) = self.tabs.iter().find_map(|tab| match tab {
            Tab::File(doc) if doc.id == id => Some(doc),
            _ => None,
        }) else {
            return;
        };
        if self.document_action_pending(|path| path == doc.path) || self.search_replace.is_writing(&doc.path) {
            self.message = "Wait for the file operation to finish before reopening".into();
            return;
        }
        let (version, path) = (doc.version, doc.path.clone());
        let source = path.clone();
        let rx = background(&self.ctx, move || {
            let bytes = std::fs::read(source)?;
            let encoding = encoding.for_reopen(&bytes);
            Ok(normalize(encoding.decode(&bytes)?, encoding))
        });
        self.reopen_jobs.retain(|job| job.id != id);
        self.reopen_jobs.push(ReopenJob {
            id,
            version,
            path,
            rx: Some(rx),
        });
    }

    fn poll_reopen_files(&mut self) {
        let jobs = std::mem::take(&mut self.reopen_jobs);
        for mut job in jobs {
            let result = receive(&mut job.rx);
            if job.rx.is_some() {
                self.reopen_jobs.push(job);
                continue;
            }
            let busy = self.document_action_pending(|path| path == job.path) || self.search_replace.is_writing(&job.path);
            let Some(doc) = self.document_mut(job.id).filter(|doc| doc.path == job.path) else {
                continue;
            };
            self.message = match result {
                Some(Ok(disk)) if !busy && doc.version == job.version => {
                    doc.reload(disk);
                    format!("Reopened {} with {}", job.path.display(), doc.encoding.label())
                }
                Some(Ok(_)) => format!("{} changed while reopening. Run the command again.", job.path.display()),
                Some(Err(err)) => format!("Reopen failed for {}: {err}", job.path.display()),
                None => format!("Reopen worker stopped for {}", job.path.display()),
            };
        }
    }

    pub(super) fn poll_open_files(&mut self) {
        self.poll_reopen_files();
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
                    self.add_file_tab(&path, text.text, text.crlf, text.encoding);
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
                self.activate_tab(index);
            } else {
                self.record_tab_recent(index);
            }
        }
    }
}

pub(super) fn format_status(ui: &mut egui::Ui, doc: &Doc) -> Option<Command> {
    let mut command = None;
    ui.menu_button(RichText::new(if doc.crlf { "CRLF" } else { "LF" }).weak(), |ui| {
        for (crlf, label) in [(false, "LF"), (true, "CRLF")] {
            if ui.radio(doc.crlf == crlf, label).clicked() {
                command = Some(Command::ChangeLineEndings(crlf));
                ui.close();
            }
        }
    });
    ui.menu_button(RichText::new(doc.encoding.label()).weak(), |ui| {
        ui.add_enabled_ui(!doc.untitled, |ui| {
            ui.menu_button("Reopen with encoding", |ui| {
                for encoding in Encoding::ALL {
                    if ui.button(encoding.label()).clicked() {
                        command = Some(Command::ReopenWithEncoding(encoding));
                    }
                }
            });
        });
        ui.menu_button("Save with encoding", |ui| {
            for encoding in Encoding::ALL {
                if ui.button(encoding.label()).clicked() {
                    command = Some(Command::SaveWithEncoding(encoding));
                }
            }
        });
        if command.is_some() {
            ui.close();
        }
    });
    command
}
