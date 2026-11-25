//! C/C++ compile flags: the per-workspace settings dialog and "Generate compile
//! database" (confirm the exact command, then stream its output with Cancel).

use super::App;
use crate::{compdb, icons as ic, workspace};
use eframe::egui::{self, Key, RichText};
use std::collections::VecDeque;

const LOG_LINES: usize = 400;

/// What edittr found by itself, shown at the top of the settings dialog.
struct Detected {
    /// Where clangd's flags come from, once clangd has started for this folder.
    flags: Option<String>,
    compiler: Option<String>,
    /// The "Generate compile database" command without a custom build command, or why there is none.
    build: Result<String, String>,
}

#[derive(Default)]
pub(super) struct CFlags {
    /// The settings dialog, with the values being edited.
    editing: Option<workspace::CSettings>,
    detected: Option<Detected>,
    /// A planned generation waiting for the user's go-ahead (or the reason it can't run).
    confirm: Option<Result<compdb::Plan, String>>,
    job: Option<compdb::Job>,
    log: VecDeque<String>,
    /// Keep the output window open after the job ends, until dismissed.
    finished: Option<Result<String, String>>,
}

impl CFlags {
    pub(super) fn is_open(&self) -> bool {
        self.editing.is_some() || self.confirm.is_some() || self.job.is_some() || self.finished.is_some()
    }

    pub(super) fn running(&self) -> bool {
        self.job.is_some()
    }
}

impl App {
    pub(super) fn open_c_settings(&mut self) {
        let Some(root) = &self.root else {
            self.message = "Open a folder first".into();
            return;
        };
        let clangd = self.lsp_has_launch("clangd");
        let database = &self.lsp.database;
        self.c_flags.detected = Some(Detected {
            flags: clangd.then(|| database.label(root)),
            compiler: database.detected_compiler.clone().filter(|_| clangd),
            build: compdb::plan(root, "").map(|plan| plan.command_line()),
        });
        self.c_flags.editing = Some(workspace::load_c_settings(root));
    }

    pub(super) fn plan_compile_database(&mut self) {
        let Some(root) = self.root.clone() else {
            self.message = "Open a folder first".into();
            return;
        };
        if self.c_flags.job.is_some() {
            return;
        }
        let settings = workspace::load_c_settings(&root);
        self.c_flags.confirm = Some(compdb::plan(&root, &settings.build));
    }

    pub(super) fn restart_clangd(&mut self) {
        if let Some(launch) = self.lsp_remove_launch("clangd") {
            launch.cancel();
            std::thread::spawn(move || launch.retire_if_cancelled());
        }
        self.launch_server("cpp", false);
    }

    pub(super) fn poll_c_flags(&mut self) {
        let Some(job) = &self.c_flags.job else { return };
        let mut done = None;
        while let Ok(event) = job.rx.try_recv() {
            match event {
                compdb::Event::Line(line) => {
                    if self.c_flags.log.len() == LOG_LINES {
                        self.c_flags.log.pop_front();
                    }
                    self.c_flags.log.push_back(line);
                }
                compdb::Event::Done(result) => done = Some(result),
            }
        }
        let Some(result) = done else { return };
        let method = job.plan.method.label();
        self.c_flags.job = None;
        self.c_flags.finished = Some(match result {
            Ok((path, count)) => {
                self.message = format!("Compile database generated ({count} files, {method})");
                self.restart_clangd();
                Ok(format!("{count} files, written to {}", path.display()))
            }
            Err(e) => Err(e),
        });
    }

    pub(super) fn c_flags_dialogs(&mut self, ctx: &egui::Context) {
        let escape = || ctx.input(|i| i.key_pressed(Key::Escape));

        if let Some(settings) = &mut self.c_flags.editing {
            let mut action = None;
            egui::Modal::new(egui::Id::unique("c_settings")).show(ctx, |ui| {
                ui.set_width(520.0);
                ui.heading("C/C++ compile flags");
                ui.weak("Saved for this folder. Used when clangd starts.");
                ui.add_space(6.0);
                let detected = self.c_flags.detected.as_ref();
                if let Some(detected) = detected {
                    detected_summary(ui, detected, settings);
                    ui.add_space(6.0);
                }
                ui.label("Extra flags for every file");
                ui.add(
                    egui::TextEdit::singleline(&mut settings.flags)
                        .hint_text("-DDEBUG -Iinclude -std=c11")
                        .desired_width(f32::INFINITY),
                );
                ui.add_space(4.0);
                ui.label("Compiler (lets clangd find a cross compiler's system headers)");
                ui.add(
                    egui::TextEdit::singleline(&mut settings.compiler)
                        .hint_text(match detected.and_then(|d| d.compiler.as_deref()) {
                            Some(compiler) => format!("detected: {compiler}"),
                            None => "detected from the build, e.g. arm-none-eabi-gcc".into(),
                        })
                        .desired_width(f32::INFINITY),
                );
                ui.add_space(4.0);
                ui.label("Build command for \"Generate compile database\"");
                ui.add(
                    egui::TextEdit::singleline(&mut settings.build)
                        .hint_text(match detected.map(|d| &d.build) {
                            Some(Ok(command)) => format!("detected: {command}"),
                            _ => "detected: CMake, Meson or make, e.g. make -C firmware all".into(),
                        })
                        .desired_width(f32::INFINITY),
                );
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("Save").clicked() {
                        action = Some(1);
                    }
                    if ui.button(format!("{} Save and generate compile database…", ic::GEAR)).clicked() {
                        action = Some(2);
                    }
                    if ui.button("Cancel").clicked() || escape() {
                        action = Some(0);
                    }
                });
            });
            if let Some(action) = action {
                let settings = self.c_flags.editing.take().unwrap();
                if action > 0
                    && let Some(root) = &self.root
                {
                    let changed = workspace::load_c_settings(root) != settings;
                    workspace::save_c_settings(root, &settings);
                    if action == 2 {
                        self.plan_compile_database();
                    } else if changed && self.lsp_has_launch("clangd") {
                        self.restart_clangd();
                    }
                }
            }
        }

        if let Some(plan) = &self.c_flags.confirm {
            let mut run = None;
            egui::Modal::new(egui::Id::unique("c_generate")).show(ctx, |ui| {
                ui.set_width(560.0);
                ui.heading("Generate compile database");
                ui.add_space(4.0);
                match plan {
                    Ok(plan) => {
                        let what = match plan.method {
                            compdb::Method::Cmake => "Configures the CMake project into edittr's cache folder (no build).",
                            compdb::Method::Meson => "Sets up the Meson project into edittr's cache folder (no build).",
                            compdb::Method::Bear => "Runs your build under bear, which records every compiler call. This is a real build.",
                            compdb::Method::MakeDryRun => {
                                "Runs make as a dry run and reads the compiler calls it prints. Make still runs $(shell …), \
                                 '+' lines and recursive $(MAKE) calls. Install bear for an exact capture."
                            }
                        };
                        ui.label(what);
                        ui.add_space(4.0);
                        ui.label(RichText::new(plan.command_line()).monospace());
                        ui.weak(format!("in {}", plan.cwd.display()));
                        ui.add_space(8.0);
                        ui.horizontal(|ui| {
                            if ui.button("Run").clicked() {
                                run = Some(true);
                            }
                            if ui.button("Cancel").clicked() || escape() {
                                run = Some(false);
                            }
                        });
                    }
                    Err(e) => {
                        ui.label(e);
                        ui.add_space(8.0);
                        ui.horizontal(|ui| {
                            if ui.button("Settings…").clicked() {
                                run = Some(false);
                                self.c_flags.editing = self.root.as_deref().map(workspace::load_c_settings);
                            }
                            if ui.button("Close").clicked() || escape() {
                                run = Some(false);
                            }
                        });
                    }
                }
            });
            if let Some(run) = run {
                if let (true, Some(Ok(plan))) = (run, self.c_flags.confirm.take()) {
                    let ctx = ctx.clone();
                    self.c_flags.log.clear();
                    self.c_flags.finished = None;
                    self.c_flags.job = Some(compdb::start(plan, move || ctx.request_repaint()));
                } else {
                    self.c_flags.confirm = None;
                }
            }
        }

        if self.c_flags.job.is_some() || self.c_flags.finished.is_some() {
            let mut close = false;
            let mut cancel = false;
            egui::Window::new("Generating compile database")
                .id(egui::Id::unique("c_generate_log"))
                .collapsible(false)
                .default_width(640.0)
                .anchor(egui::Align2::CENTER_BOTTOM, [0.0, -40.0])
                .show(ctx, |ui| {
                    if let Some(job) = &self.c_flags.job {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(RichText::new(job.plan.command_line()).monospace());
                        });
                    }
                    egui::ScrollArea::vertical().max_height(220.0).stick_to_bottom(true).show(ui, |ui| {
                        for line in &self.c_flags.log {
                            ui.label(RichText::new(line).monospace().small());
                        }
                    });
                    match &self.c_flags.finished {
                        Some(Ok(done)) => {
                            ui.label(format!("{} Done: {done}. clangd restarted.", ic::CHECK));
                        }
                        Some(Err(e)) => {
                            ui.colored_label(ui.visuals().error_fg_color, e);
                        }
                        None => {}
                    }
                    ui.horizontal(|ui| {
                        if self.c_flags.job.is_some() {
                            cancel = ui.button("Cancel").clicked();
                        } else {
                            close = ui.button("Close").clicked() || escape();
                        }
                    });
                });
            if cancel && let Some(job) = &self.c_flags.job {
                job.cancel();
            }
            if close {
                self.c_flags.finished = None;
            }
        }
    }
}

/// The "Detected" box of the settings dialog: what clangd uses when the fields below are empty.
fn detected_summary(ui: &mut egui::Ui, detected: &Detected, settings: &workspace::CSettings) {
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.label(RichText::new(format!("{} Auto-detected", ic::MAGNIFYING_GLASS)).strong());
        egui::Grid::new("c_detected").num_columns(2).spacing([12.0, 4.0]).show(ui, |ui| {
            let overridden = |ui: &mut egui::Ui, field: &str| {
                if !field.trim().is_empty() {
                    ui.weak("(overridden below)");
                }
            };
            ui.weak("Compile flags");
            match &detected.flags {
                Some(flags) => ui.label(flags.strip_prefix("flags: ").unwrap_or(flags)),
                None => ui.weak("clangd hasn't started yet: open a C/C++ file"),
            };
            ui.end_row();
            ui.weak("Compiler");
            ui.horizontal(|ui| {
                match (&detected.compiler, &detected.flags) {
                    (Some(compiler), _) => ui.label(RichText::new(compiler).monospace()),
                    (None, Some(_)) => ui.weak("none detected"),
                    (None, None) => ui.weak("—"),
                };
                overridden(ui, &settings.compiler);
            });
            ui.end_row();
            ui.weak("Build command");
            ui.horizontal_wrapped(|ui| {
                match &detected.build {
                    Ok(command) => ui.label(RichText::new(command).monospace()),
                    Err(reason) => ui.weak(reason),
                };
                overridden(ui, &settings.build);
            });
            ui.end_row();
        });
    });
}
