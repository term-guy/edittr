use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use eframe::egui;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub(crate) fn enabled() -> bool {
    !(cfg!(feature = "tour") && std::env::var_os("EDITTR_TOUR_OUT").is_some())
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Side {
    Explorer,
    Changes,
    Search,
    Problems,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct Sidebar {
    pub side: Side,
    pub open: bool,
    pub width: f32,
}

impl Default for Sidebar {
    fn default() -> Self {
        Self {
            side: Side::Explorer,
            open: true,
            width: 260.0,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct FileTab {
    pub path: OsString,
    pub cursor: usize,
    pub anchor: usize,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct Workspace {
    pub tabs: Vec<FileTab>,
    pub active: Option<OsString>,
    pub sidebar: Sidebar,
}

impl Workspace {
    pub(crate) fn load(root: Option<&Path>) -> Self {
        read(&workspace_filename(root)).unwrap_or_default()
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct Geometry {
    position: Option<[f32; 2]>,
    inner_size: Option<[f32; 2]>,
}

impl Geometry {
    pub(crate) fn capture(&mut self, ctx: &egui::Context) {
        ctx.input(|input| {
            let viewport = input.viewport();
            if viewport.minimized == Some(true) {
                return;
            }
            if let Some(rect) = viewport.outer_rect.filter(|rect| rect.is_finite()) {
                self.position = Some([rect.min.x, rect.min.y]);
            }
            let size = viewport.inner_rect.unwrap_or_else(|| input.viewport_rect()).size();
            if size.is_finite() && size.x > 0.0 && size.y > 0.0 {
                self.inner_size = Some([size.x, size.y]);
            }
        });
    }

    pub(crate) fn apply(self, mut viewport: egui::ViewportBuilder) -> egui::ViewportBuilder {
        if let Some(size) = self.inner_size.filter(|size| size.iter().all(|v| v.is_finite() && *v > 0.0)) {
            viewport = viewport.with_inner_size(size);
        }
        if let Some(pos) = self.position.filter(|pos| pos.iter().all(|v| v.is_finite())) {
            viewport = viewport.with_position(pos);
        }
        viewport
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct Window {
    pub root: Option<OsString>,
    pub geometry: Geometry,
}

impl Window {
    pub(crate) fn load() -> Self {
        if enabled() {
            read("window.json").unwrap_or_default()
        } else {
            Self::default()
        }
    }
}

pub(crate) struct Startup {
    pub root: Option<PathBuf>,
    pub file: Option<PathBuf>,
    pub restore: bool,
    pub window: Window,
}

impl Startup {
    pub(crate) fn new(arg: Option<PathBuf>, restore_previous: bool) -> Self {
        let window = Window::load();
        let (root, file, restore) = match arg {
            Some(p) if p.is_file() => (None, Some(crate::workspace::canonical(&p).unwrap_or(p)), true),
            Some(p) if p.is_dir() => (Some(crate::workspace::canonical(&p).unwrap_or(p)), None, true),
            Some(_) => (None, None, false),
            None if restore_previous && enabled() => (
                window
                    .root
                    .clone()
                    .map(|p| crate::workspace::plain_path(Path::new(&p)))
                    .filter(|path| path.is_dir()),
                None,
                true,
            ),
            None => (None, None, false),
        };
        Self {
            root,
            file,
            restore,
            window,
        }
    }
}

fn workspace_filename(root: Option<&Path>) -> String {
    match root {
        Some(root) => {
            let hash: String = Sha256::digest(root.as_os_str().as_encoded_bytes())
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect();
            format!("{hash}.json")
        }
        None => "no-folder.json".into(),
    }
}

fn directory() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("edittr/sessions"))
}

fn read<T: serde::de::DeserializeOwned>(filename: &str) -> Option<T> {
    let path = directory()?.join(filename);
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

pub(crate) enum Update {
    Workspace(Option<PathBuf>, Workspace),
    Window(Window),
    SearchFilters(PathBuf, crate::workspace::SearchFilters),
}

impl Update {
    fn write(self, dir: &Path) -> io::Result<()> {
        let (filename, json) = match self {
            Self::Workspace(root, data) => (workspace_filename(root.as_deref()), serde_json::to_string(&data)?),
            Self::Window(data) => ("window.json".into(), serde_json::to_string(&data)?),
            Self::SearchFilters(root, filters) => return crate::workspace::save_search_filters(&root, &filters),
        };
        crate::workspace::write_config(&dir.join(filename), &json)
    }
}

pub(crate) struct Writer {
    requests: Sender<Request>,
    pub errors: Receiver<io::Error>,
}

enum Request {
    Save(Update, egui::Context),
    Barrier(Sender<()>),
}

impl Writer {
    pub(crate) fn start() -> Option<(Self, JoinHandle<()>)> {
        let dir = directory().filter(|_| enabled())?;
        let (requests, queue) = channel::<Request>();
        let (report, errors) = channel();
        let worker = thread::spawn(move || {
            for request in queue {
                match request {
                    Request::Save(update, ctx) => {
                        if let Err(error) = update.write(&dir)
                            && report.send(error).is_ok()
                        {
                            ctx.request_repaint();
                        }
                    }
                    Request::Barrier(done) => {
                        let _ = done.send(());
                    }
                }
            }
        });
        Some((Self { requests, errors }, worker))
    }

    pub(crate) fn save(&self, update: Update, ctx: &egui::Context) {
        let _ = self.requests.send(Request::Save(update, ctx.clone()));
    }

    /// A restore may follow a write for the same workspace before it reaches disk.
    pub(crate) fn after_pending_writes(&self) -> Receiver<()> {
        let (done, ready) = channel();
        let _ = self.requests.send(Request::Barrier(done));
        ready
    }
}

const WRITE_INTERVAL: Duration = Duration::from_secs(2);

pub(crate) struct Debounce<T> {
    last_value: Option<T>,
    last_write: Option<Instant>,
}

impl<T: PartialEq> Debounce<T> {
    pub(crate) fn new(baseline: Option<T>) -> Self {
        Self {
            last_value: baseline,
            last_write: None,
        }
    }

    pub(crate) fn update(&mut self, value: T, now: Instant, flush: bool) -> Decision<'_, T> {
        if self.last_value.as_ref() == Some(&value) {
            return Decision::Unchanged;
        }
        if !flush
            && let Some(last) = self.last_write
            && let Some(remaining) = WRITE_INTERVAL.checked_sub(now.duration_since(last))
            && !remaining.is_zero()
        {
            return Decision::Wait(remaining);
        }
        self.last_value = Some(value);
        self.last_write = Some(now);
        Decision::Write(self.last_value.as_ref().unwrap())
    }
}

pub(crate) enum Decision<'a, T> {
    Unchanged,
    Wait(Duration),
    Write(&'a T),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_round_trip() {
        let first = FileTab {
            path: "project/main.rs".into(),
            cursor: 24,
            anchor: 17,
        };
        let second = FileTab {
            path: "project/lib.rs".into(),
            cursor: 6,
            anchor: 10,
        };
        let workspace = Workspace {
            active: Some(first.path.clone()),
            tabs: vec![first, second],
            sidebar: Sidebar {
                side: Side::Search,
                open: false,
                width: 324.0,
            },
        };
        let json = serde_json::to_string(&workspace).unwrap();
        assert_eq!(serde_json::from_str::<Workspace>(&json).unwrap(), workspace);
    }

    #[test]
    fn window_round_trip() {
        let window = Window {
            root: Some("project".into()),
            geometry: Geometry {
                position: Some([-90.0, 128.0]),
                inner_size: Some([1000.0, 700.0]),
            },
        };
        let json = serde_json::to_string(&window).unwrap();
        assert_eq!(serde_json::from_str::<Window>(&json).unwrap(), window);
    }

    #[test]
    fn saves_latest_change_at_interval_and_flushes_early_on_exit() {
        let start = Instant::now();
        let mut debounce = Debounce::new(None);
        assert!(matches!(debounce.update(1, start, false), Decision::Write(&1)));
        assert!(matches!(debounce.update(2, start, false), Decision::Wait(d) if d == WRITE_INTERVAL));
        assert!(matches!(debounce.update(3, start + WRITE_INTERVAL, false), Decision::Write(&3)));
        assert!(matches!(debounce.update(3, start + WRITE_INTERVAL, false), Decision::Unchanged));
        assert!(matches!(debounce.update(4, start + WRITE_INTERVAL, true), Decision::Write(&4)));
    }

    #[test]
    fn unchanged_baseline_and_reverted_changes_do_not_write() {
        let now = Instant::now();
        let mut debounce = Debounce::new(Some(0));
        assert!(matches!(debounce.update(0, now, true), Decision::Unchanged));
        assert!(matches!(debounce.update(1, now, false), Decision::Write(&1)));
        assert!(matches!(debounce.update(2, now, false), Decision::Wait(_)));
        assert!(matches!(debounce.update(1, now + WRITE_INTERVAL, false), Decision::Unchanged));
    }
}
