//! Screenshot tour (only built with `--features tour`).
//!
//! `EDITTR_TOUR_OUT=<dir> EDITTR_TOUR_DEMO=<git repo> cargo run --release --features tour`
//! steps through the main features, saves `<dir>/NN-<name>.png` for each, then quits.
//! The app captures its own frames, so this works even when the window isn't
//! visible on the current desktop.

use super::*;
use std::sync::atomic::AtomicBool;

pub struct Tour {
    out: PathBuf,
    demo: PathBuf,
    step: usize,
    frames: u32,
    /// When the current step was set up: fades and scroll animations are time-based.
    started: std::time::Instant,
    shot: Option<Arc<AtomicBool>>,
    /// Screen rects of the tabs (recorded by `tab_strip`), used to right-click one.
    pub tab_rects: Vec<egui::Rect>,
    /// Raw input events to feed egui on the next frame.
    inject: Vec<egui::Event>,
}

const STEPS: &[&str] = &[
    "welcome",
    "editor",
    "go-to-file",
    "search-in-files",
    "source-control",
    "source-control-tree",
    "diff-side-by-side",
    "diff-inline",
    "git-graph",
    "tab-menu",
    "go-to-definition-install-offer",
    "go-to-definition",
    "light-theme",
    "column-select",
];

impl Tour {
    pub fn from_env() -> Option<Tour> {
        let out = PathBuf::from(std::env::var_os("EDITTR_TOUR_OUT")?);
        let demo = std::fs::canonicalize(std::env::var_os("EDITTR_TOUR_DEMO")?).ok()?;
        std::fs::create_dir_all(&out).ok()?;
        Some(Tour {
            out,
            demo,
            step: 0,
            frames: 0,
            started: std::time::Instant::now(),
            shot: None,
            tab_rects: Vec::new(),
            inject: Vec::new(),
        })
    }

    pub fn raw_input_hook(&mut self, raw: &mut egui::RawInput) {
        raw.events.append(&mut self.inject);
    }

    fn click(&mut self, pos: egui::Pos2, button: egui::PointerButton, pressed: bool) {
        self.inject.push(egui::Event::PointerMoved(pos));
        self.inject.push(egui::Event::PointerButton {
            pos,
            button,
            pressed,
            modifiers: Modifiers::NONE,
        });
    }

    fn key(&mut self, key: Key) {
        for pressed in [true, false] {
            self.inject.push(egui::Event::Key {
                key,
                physical_key: None,
                pressed,
                repeat: false,
                modifiers: Modifiers::NONE,
            });
        }
    }

    /// Called at the end of every frame.
    pub fn tick(&mut self, app: &mut App, ctx: &egui::Context) {
        ctx.request_repaint();
        if let Some(done) = &self.shot {
            if done.load(Ordering::Relaxed) {
                self.shot = None;
                self.step += 1;
                self.frames = 0;
            }
            return;
        }
        let Some(&name) = STEPS.get(self.step) else {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        };
        if self.frames == 0 {
            self.started = std::time::Instant::now();
            self.setup(app, ctx, name);
        }
        self.frames += 1;
        // Start the jump only after the Esc that closed the tab menu has been
        // handled; otherwise that Esc would dismiss the install offer at once.
        if name == "go-to-definition-install-offer"
            && self.frames == 5
            && let Some(Tab::File(d)) = app.tabs.first()
            && let Some(b) = d.text.find("free_slot(&mut lanes)")
        {
            // A call site of `free_slot`, so the jump lands on its definition.
            let idx = d.text[..b].chars().count() + 2;
            app.go_to_definition(idx, ctx);
        }
        if name == "tab-menu" {
            // Right-click the first tab: move, press, release on separate frames.
            if let Some(&r) = self.tab_rects.first() {
                match self.frames {
                    3 => self.click(r.center(), egui::PointerButton::Secondary, true),
                    4 => self.click(r.center(), egui::PointerButton::Secondary, false),
                    _ => {}
                }
            }
        }
        if name == "column-select" {
            // Middle-drag a 4-line box in the editor, then type into every line.
            let (x0, y0, cw, row) = (350.0 + 4.0 * 7.8, 106.0 + 15.0, 7.8, 15.0);
            let p = |c: f32, l: f32| egui::pos2(x0 + c * cw, y0 + l * row);
            // Start after the scroll to the top (set up in `setup`) has settled.
            if self.started.elapsed() < std::time::Duration::from_millis(1500) {
                self.frames = 1;
            }
            match self.frames {
                30 => self.click(p(0.0, 0.0), egui::PointerButton::Middle, true),
                31..=33 => self.inject.push(egui::Event::PointerMoved(p(
                    (self.frames - 30) as f32 * 2.0,
                    (self.frames - 30) as f32,
                ))),
                34 => self.click(p(6.0, 3.0), egui::PointerButton::Middle, false),
                36 => self.inject.push(egui::Event::Text("X".into())),
                37 => self.inject.push(egui::Event::Text("Y".into())),
                _ => {}
            }
        }
        if name == "git-graph" && app.graph_sel.is_none() {
            app.load_graph_detail(1);
        }
        let ready = app.search.rx.is_none()
            && app.goto_rx.is_none()
            && app.repo_rx.is_none()
            && app.status_rx.is_none()
            && app.diff_rx.is_none()
            && app.graph_rx.is_none()
            && app.graph_detail_rx.is_none()
            && app.quick_open.as_ref().is_none_or(|q| q.files_rx.is_none());
        let elapsed = self.started.elapsed();
        let min_frames = if name == "column-select" { 40 } else { 10 };
        if self.frames < min_frames || elapsed < std::time::Duration::from_millis(800) || (!ready && elapsed.as_secs() < 20) {
            return;
        }
        let done = Arc::new(AtomicBool::new(false));
        let flag = done.clone();
        let path = self.out.join(format!("{:02}-{name}.png", self.step + 1));
        ctx.request_screenshot(move |img| {
            let [w, h] = img.size;
            let bytes: Vec<u8> = img.pixels.iter().flat_map(|c| c.to_array()).collect();
            if let Err(e) = image::save_buffer(&path, &bytes, w as u32, h as u32, image::ColorType::Rgba8) {
                eprintln!("tour: could not save {}: {e}", path.display());
            }
            flag.store(true, Ordering::Relaxed);
        });
        self.shot = Some(done);
    }

    fn setup(&mut self, app: &mut App, ctx: &egui::Context, name: &str) {
        let demo = self.demo.clone();
        match name {
            "welcome" => {
                app.root = None;
                app.ignored.clear();
                app.recent = vec![demo.clone()];
                app.side = Side::Explorer;
            }
            "editor" => {
                app.set_root(demo.clone());
                app.expanded.insert(demo.join("src"));
                app.expanded.insert(demo.join("src/ui"));
                app.open_file(&demo.join("src/ui/graph.rs"));
            }
            "go-to-file" => {
                app.start_quick_open();
                if let Some(q) = &mut app.quick_open {
                    q.query = "gra".into();
                }
            }
            "search-in-files" => {
                self.key(Key::Escape);
                app.quick_open = None;
                app.show_search();
                app.search.opts.query = "lane".into();
                app.search.opts.include = "*.rs".into();
                app.search.show_filters = true;
                app.ignored = vec!["target".into()];
                app.start_search(ctx);
            }
            "source-control" => {
                app.side = Side::Changes;
                app.settings.changes_tree = false;
                app.refresh_vcs();
            }
            "source-control-tree" => app.settings.changes_tree = true,
            "diff-side-by-side" => {
                if let Some(c) = app.status.unstaged.iter().find(|c| c.path == "a.rs").cloned() {
                    app.open_diff(&c, false);
                }
            }
            "diff-inline" => {
                if let Some(Tab::Diff(d)) = app.tabs.get_mut(app.active) {
                    d.inline = true;
                }
            }
            "git-graph" => app.open_graph(),
            "tab-menu" => {
                app.side = Side::Explorer;
                app.active = 0;
            }
            "go-to-definition-install-offer" => {
                self.key(Key::Escape); // close the tab menu; the jump starts in `tick`
                app.active = 0;
            }
            "go-to-definition" => {
                if let Some(cmd) = app.install_prompt.take() {
                    app.install_declined.insert(cmd);
                }
            }
            "column-select" => {
                ctx.set_theme(egui::Theme::Dark);
                app.settings.show_whitespace = true;
                app.side = Side::Explorer;
                if let Some(i) = app.tabs.iter().position(|t| matches!(t, Tab::File(_))) {
                    app.active = i;
                    if let Some(Tab::File(d)) = app.tabs.get_mut(i) {
                        d.goto = Some((0, 0)); // scroll to the top
                    }
                }
            }
            "light-theme" => {
                ctx.set_theme(egui::Theme::Light);
                if let Some(i) = app.tabs.iter().position(|t| matches!(t, Tab::Diff(_))) {
                    app.active = i;
                    if let Some(Tab::Diff(d)) = app.tabs.get_mut(i) {
                        d.inline = false;
                    }
                }
                app.side = Side::Changes;
            }
            _ => {}
        }
    }
}
