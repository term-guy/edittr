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
    "folding",
    "lsp-hover",
    "lsp-completion",
    "find-replace",
    "command-palette",
    "source-control",
    "source-control-tree",
    "diff-side-by-side",
    "diff-inline",
    "git-graph",
    "tab-menu",
    "go-to-definition-install-offer",
    "go-to-definition",
    "split-editor",
    "light-theme",
    "column-select",
    "c-flags",
    "c-settings",
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
        if name == "diff-side-by-side" {
            // The right pane is the real editor: click into a line and type, once the
            // diff (loaded in the background) is showing.
            if !matches!(app.tabs.get(app.active), Some(Tab::Diff(_))) {
                self.frames = 1;
            }
            let at = egui::pos2(900.0, 209.0);
            match self.frames {
                20 => self.click(at, egui::PointerButton::Primary, true),
                21 => self.click(at, egui::PointerButton::Primary, false),
                23 => self.inject.push(egui::Event::Text(" // edited in the diff".into())),
                _ => {}
            }
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
        if name == "search-in-files" && self.frames == 5 {
            for pressed in [true, false] {
                self.inject.push(egui::Event::Key {
                    key: Key::F,
                    physical_key: None,
                    pressed,
                    repeat: false,
                    modifiers: Modifiers::COMMAND | Modifiers::SHIFT,
                });
            }
        }
        if name == "lsp-hover" && self.frames == 20 {
            if let Some(r) = app.last_caret {
                self.inject.push(egui::Event::PointerMoved(r.center() + egui::vec2(4.0, 0.0)));
            }
        }
        if name == "lsp-completion" {
            match self.frames {
                20 => {
                    // Start a new statement on its own line, then type a prefix.
                    self.key(Key::End);
                    self.key(Key::Enter);
                }
                24 => self.inject.push(egui::Event::Text("ad".into())),
                _ => {}
            }
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
        let min_frames = match name {
            "column-select" | "diff-side-by-side" => 40,
            // Give the language server time to answer.
            "lsp-hover" | "lsp-completion" | "c-flags" => 150,
            _ => 10,
        };
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
                app.clear_project();
                app.recent = vec![demo.clone()];
                app.side = Side::Explorer;
            }
            "editor" => {
                app.set_root(demo.clone());
                app.expanded.insert(demo.join("src"));
                app.expanded.insert(demo.join("src/ui"));
                // EDITTR_TOUR_FILE: open another file (relative to the demo repo) instead.
                let file = std::env::var("EDITTR_TOUR_FILE").unwrap_or_else(|_| "src/ui/graph.rs".into());
                app.open_file(&demo.join(file));
            }
            "go-to-file" => {
                app.start_quick_open();
                if let Some(q) = &mut app.quick_open {
                    q.query = "gra".into();
                }
            }
            "search-in-files" => {
                // Select a word in the editor; a real Ctrl/Cmd+Shift+F follows (see tick):
                // it must search all files for the selection, not open Find in file.
                // (No Escape here: the editor would clear the selection with it.)
                app.quick_open = None;
                app.search.opts.include = "*.rs".into();
                app.search.show_filters = true;
                app.ignored = vec!["target".into()];
                if let Some(Tab::File(d)) = app.tabs.get_mut(app.active)
                    && let Some(b) = d.text.find("lane_color").or_else(|| d.text.find("add"))
                {
                    let start = d.text[..b].chars().count();
                    let word = if d.text[b..].starts_with("lane_color") { 10 } else { 3 };
                    d.goto = Some((start, start + word));
                }
            }
            "folding" => {
                // Fold `pub struct Row {` and `const PALETTE` (0-based lines 5 and 18).
                if let Some(Tab::File(d)) = app.tabs.iter_mut().find(|t| matches!(t, Tab::File(_))) {
                    d.ed.folded.insert(5);
                    d.ed.folded.insert(18);
                }
            }
            "lsp-hover" | "lsp-completion" => {
                // Put the caret on a call of `add` (C demo) or `lane_color` (Rust demo).
                if let Some(Tab::File(d)) = app.tabs.get_mut(app.active) {
                    let at = d.text.find("add(1").or_else(|| d.text.find("lane_color(i"));
                    if let Some(b) = at {
                        let c = d.text[..b].chars().count() + 1;
                        d.goto = Some((c, c));
                    }
                }
            }
            "find-replace" => {
                if let Some(Tab::File(d)) = app.tabs.iter_mut().find(|t| matches!(t, Tab::File(_))) {
                    d.ed.folded.clear();
                }
                if let Some(i) = app.tabs.iter().position(|t| matches!(t, Tab::File(_))) {
                    app.active = i;
                }
                app.show_find(true);
                app.find.set_query_for_tour("lane");
            }
            "command-palette" => {
                app.find.close_for_tour();
                app.run_command(crate::commands::Command::Palette);
                if let Some(p) = &mut app.picker {
                    p.set_query_for_tour("tog");
                }
            }
            "source-control" => {
                app.picker = None;
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
            "c-flags" => {
                // Back to the opened C file, where the status bar shows where clangd's flags come from.
                ctx.set_theme(egui::Theme::Dark);
                app.split = None;
                if let Some(i) = app.tabs.iter().position(|t| matches!(t, Tab::File(d) if d.ext() == "c")) {
                    app.active = i;
                }
            }
            "c-settings" => app.open_c_settings(),
            "split-editor" => {
                app.side_open = false;
                let graph = app
                    .tabs
                    .iter()
                    .position(|t| matches!(t, Tab::File(d) if d.path.ends_with("graph.rs")));
                app.open_file(&self.demo.join("a.rs"));
                app.split = graph;
            }
            "light-theme" => {
                app.split = None;
                app.side_open = true;
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
