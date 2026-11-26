//! Settings > Keyboard shortcuts: change, remove or reset the shortcut of any command.
//! A shortcut already in use is only moved after the user confirms.

use super::App;
use crate::commands::{self, COMMANDS};
use eframe::egui::{self, Event, Key, KeyboardShortcut, ModifierNames, Modifiers, RichText};

#[derive(Default)]
pub(super) struct Shortcuts {
    /// The command (index in `COMMANDS`) whose new shortcut is being typed.
    recording: Option<usize>,
    /// A change that takes shortcuts from other commands or editor keys, waiting for confirmation.
    pending: Option<Pending>,
}

struct Pending {
    entry: usize,
    /// The command's new shortcuts.
    shortcuts: Vec<KeyboardShortcut>,
    /// Other commands that lose one of them.
    taken_from: Vec<(usize, KeyboardShortcut)>,
    /// Editor keys (Undo, Copy, ...) a command would take over.
    editor_keys: Vec<(KeyboardShortcut, &'static str)>,
}

fn text(shortcut: &KeyboardShortcut, ctx: &egui::Context) -> String {
    shortcut.format(&ModifierNames::NAMES, ctx.os().is_mac())
}

/// Every word of `filter` starts a word of the title, or is part of a shortcut's text.
fn row_matches(filter: &str, title: &str, keys: &str) -> bool {
    let words: Vec<String> = title
        .split(|c: char| !c.is_alphanumeric())
        .chain(["keyboard", "shortcuts", "keys", "keybindings"])
        .map(str::to_lowercase)
        .collect();
    let keys = keys.to_lowercase();
    filter
        .split_whitespace()
        .map(str::to_lowercase)
        .all(|word| words.iter().any(|w| w.starts_with(&word)) || keys.contains(&word))
}

/// The pressed key as a shortcut: Cmd stands for Ctrl on Windows and Linux.
fn normalized(key: Key, m: Modifiers) -> KeyboardShortcut {
    let modifiers = Modifiers {
        alt: m.alt,
        shift: m.shift,
        command: m.command,
        ctrl: m.ctrl && !m.command,
        mac_cmd: false,
    };
    KeyboardShortcut::new(modifiers, key)
}

impl App {
    /// While a shortcut is being typed, take the next key press for it (before commands
    /// see it). Returns true when commands must not run this frame.
    pub(super) fn record_shortcut(&mut self, ctx: &egui::Context) -> bool {
        if self.shortcut_page.pending.is_some() {
            return true;
        }
        let Some(entry) = self.shortcut_page.recording else {
            return false;
        };
        let pressed = ctx.input_mut(|input| {
            let mut pressed = None;
            input.events.retain(|event| match event {
                Event::Key {
                    key,
                    pressed: down,
                    modifiers,
                    ..
                } => {
                    if *down && pressed.is_none() {
                        pressed = Some((*key, *modifiers));
                    }
                    false
                }
                // Option+letter types a character on macOS: not text for anyone now.
                Event::Text(_) => false,
                _ => true,
            });
            pressed
        });
        if let Some((key, m)) = pressed {
            self.shortcut_page.recording = None;
            if !(key == Key::Escape && m.is_none()) {
                self.change_shortcuts(entry, vec![normalized(key, m)]);
            }
        }
        true
    }

    /// Give `entry` these shortcuts, asking first when they are in use elsewhere.
    fn change_shortcuts(&mut self, entry: usize, shortcuts: Vec<KeyboardShortcut>) {
        let current = commands::all_shortcuts();
        if current[entry] == shortcuts {
            return;
        }
        let taken_from: Vec<(usize, KeyboardShortcut)> = current
            .iter()
            .enumerate()
            .filter(|&(i, _)| i != entry)
            .flat_map(|(i, list)| list.iter().filter(|s| shortcuts.contains(s)).map(move |s| (i, *s)))
            .collect();
        let editor_keys: Vec<_> = commands::editor_keys()
            .into_iter()
            .filter(|(key, _)| shortcuts.contains(key))
            .collect();
        let pending = Pending {
            entry,
            shortcuts,
            taken_from,
            editor_keys,
        };
        if pending.taken_from.is_empty() && pending.editor_keys.is_empty() {
            self.apply_shortcuts(pending);
        } else {
            self.shortcut_page.pending = Some(pending);
        }
    }

    fn apply_shortcuts(&mut self, change: Pending) {
        let mut current = commands::all_shortcuts();
        for (i, shortcut) in &change.taken_from {
            current[*i].retain(|s| s != shortcut);
        }
        current[change.entry] = change.shortcuts;
        self.save_shortcuts(&current);
    }

    fn save_shortcuts(&mut self, current: &[Vec<KeyboardShortcut>]) {
        self.settings.shortcuts = commands::overrides_for(current);
        commands::set_overrides(&self.settings.shortcuts);
        self.save_settings(&self.ctx.clone());
    }

    /// The "Keyboard shortcuts" part of the Settings page; returns the rows shown.
    pub(super) fn shortcuts_section(&mut self, ui: &mut egui::Ui, filter: &str, section_size: f32) -> usize {
        let ctx = ui.ctx().clone();
        let current = commands::all_shortcuts();
        let rows: Vec<(usize, String)> = COMMANDS
            .iter()
            .enumerate()
            .map(|(i, _)| {
                let keys: Vec<String> = current[i].iter().map(|s| text(s, &ctx)).collect();
                (i, keys.join(" / "))
            })
            .filter(|(i, keys)| filter.trim().is_empty() || row_matches(filter, COMMANDS[*i].title, keys))
            .collect();
        if rows.is_empty() {
            return 0;
        }
        ui.add_space(18.0);
        let searching = !filter.trim().is_empty() || self.shortcut_page.recording.is_some();
        let mut action: Option<(usize, RowAction)> = None;
        egui::CollapsingHeader::new(RichText::new("Keyboard shortcuts").strong().size(section_size))
            .id_salt("keyboard-shortcuts")
            .open(searching.then_some(true))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.weak("Click a shortcut to change it, then press the new keys (Esc cancels).");
                    if !self.settings.shortcuts.is_empty() && ui.small_button("Reset all").clicked() {
                        action = Some((usize::MAX, RowAction::ResetAll));
                    }
                });
                ui.add_space(4.0);
                for (i, keys) in &rows {
                    let entry = &COMMANDS[*i];
                    let changed = current[*i] != entry.default_shortcuts();
                    ui.separator();
                    ui.horizontal(|ui| {
                        let title = RichText::new(entry.title);
                        ui.add(egui::Label::new(if changed { title.strong() } else { title }).truncate());
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if changed
                                && ui
                                    .small_button(crate::icons::ARROW_U_UP_LEFT)
                                    .on_hover_text(format!(
                                        "Reset to {}",
                                        if entry.default_shortcuts().is_empty() {
                                            "no shortcut".to_string()
                                        } else {
                                            entry
                                                .default_shortcuts()
                                                .iter()
                                                .map(|s| text(s, &ctx))
                                                .collect::<Vec<_>>()
                                                .join(" / ")
                                        }
                                    ))
                                    .clicked()
                            {
                                action = Some((*i, RowAction::Reset));
                            }
                            if !current[*i].is_empty() && ui.small_button(crate::icons::X).on_hover_text("Remove the shortcut").clicked() {
                                action = Some((*i, RowAction::Remove));
                            }
                            let recording = self.shortcut_page.recording == Some(*i);
                            let label = if recording {
                                RichText::new("Press keys…").color(ui.visuals().hyperlink_color)
                            } else if keys.is_empty() {
                                RichText::new("Add shortcut").weak()
                            } else {
                                RichText::new(keys.as_str()).monospace()
                            };
                            let button = ui
                                .add(egui::Button::selectable(recording, label))
                                .on_hover_text(if entry.dispatches() {
                                    "Change the shortcut"
                                } else {
                                    "Change the shortcut (works while the editor has the focus)"
                                });
                            if button.clicked() {
                                action = Some((*i, if recording { RowAction::Cancel } else { RowAction::Record }));
                            }
                        });
                    });
                }
            });
        match action {
            Some((i, RowAction::Record)) => self.shortcut_page.recording = Some(i),
            Some((_, RowAction::Cancel)) => self.shortcut_page.recording = None,
            Some((i, RowAction::Remove)) => self.change_shortcuts(i, Vec::new()),
            Some((i, RowAction::Reset)) => self.change_shortcuts(i, COMMANDS[i].default_shortcuts().to_vec()),
            Some((_, RowAction::ResetAll)) => {
                self.save_shortcuts(&COMMANDS.iter().map(|e| e.default_shortcuts().to_vec()).collect::<Vec<_>>())
            }
            None => {}
        }
        if self.shortcut_page.recording.is_some() {
            ui.ctx().request_repaint();
        }
        self.shortcut_conflict_dialog(&ctx);
        rows.len()
    }

    fn shortcut_conflict_dialog(&mut self, ctx: &egui::Context) {
        let Some(pending) = &self.shortcut_page.pending else { return };
        let mut answer = None;
        egui::Modal::new(egui::Id::unique("shortcut_conflict")).show(ctx, |ui| {
            ui.set_width(460.0);
            ui.heading("Shortcut already in use");
            ui.add_space(4.0);
            let title = COMMANDS[pending.entry].title;
            for (i, shortcut) in &pending.taken_from {
                ui.label(format!(
                    "{} is used by \"{}\". It will be removed from there and assigned to \"{title}\".",
                    text(shortcut, ctx),
                    COMMANDS[*i].title
                ));
            }
            for (shortcut, what) in &pending.editor_keys {
                ui.label(format!(
                    "{} is the editor's {what}. \"{title}\" will take it over while it's assigned.",
                    text(shortcut, ctx)
                ));
            }
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button("Replace").clicked() || ui.input(|i| i.key_pressed(Key::Enter)) {
                    answer = Some(true);
                }
                if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(Key::Escape)) {
                    answer = Some(false);
                }
            });
        });
        if let Some(replace) = answer {
            let pending = self.shortcut_page.pending.take().unwrap();
            if replace {
                self.apply_shortcuts(pending);
            }
        }
    }
}

#[derive(Clone, Copy)]
enum RowAction {
    Record,
    Cancel,
    Remove,
    Reset,
    ResetAll,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::Command;
    use crate::{session::Startup, settings::SettingsWriter};

    fn key(key: Key, modifiers: Modifiers) -> Event {
        Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        }
    }

    #[test]
    fn recording_a_used_shortcut_asks_before_moving_it() {
        let ctx = egui::Context::default();
        let cc = eframe::CreationContext::_new_kittest(ctx.clone());
        let (writer, worker) = SettingsWriter::start();
        let startup = Startup {
            root: None,
            file: None,
            restore: false,
            window: Default::default(),
        };
        let mut app = App::new(&cc, startup, Default::default(), writer, None);
        let index = |id| commands::entry(id).index();
        let join = index(Command::Editor(crate::editor::Action::JoinLines));
        let duplicate = index(Command::Editor(crate::editor::Action::Duplicate));
        let frame = |app: &mut App, events: Vec<Event>| {
            let mut out = ctx.run_ui(
                egui::RawInput {
                    events,
                    ..Default::default()
                },
                |ui| {
                    app.record_shortcut(ui.ctx());
                    app.shortcut_conflict_dialog(ui.ctx());
                },
            );
            out.textures_delta.clear();
        };
        // Ctrl/Cmd+D belongs to "Duplicate line": Join lines only gets it after Replace.
        app.shortcut_page.recording = Some(join);
        frame(&mut app, vec![key(Key::D, Modifiers::COMMAND)]);
        assert!(app.shortcut_page.pending.is_some());
        assert_eq!(
            commands::all_shortcuts()[join],
            commands::entry(Command::Editor(crate::editor::Action::JoinLines)).default_shortcuts()
        );
        frame(&mut app, vec![key(Key::Escape, Modifiers::NONE)]);
        assert!(app.shortcut_page.pending.is_none());
        app.shortcut_page.recording = Some(join);
        frame(&mut app, vec![key(Key::D, Modifiers::COMMAND)]);
        frame(&mut app, vec![key(Key::Enter, Modifiers::NONE)]);
        let cmd_d = KeyboardShortcut::new(Modifiers::COMMAND, Key::D);
        assert_eq!(commands::all_shortcuts()[join], [cmd_d]);
        assert!(commands::all_shortcuts()[duplicate].is_empty());
        assert_eq!(
            commands::editor_action(Key::D, Modifiers::COMMAND),
            Some(crate::editor::Action::JoinLines)
        );
        assert_eq!(app.settings.shortcuts.len(), 2);
        // An unused shortcut is taken at once; Esc while recording cancels.
        app.shortcut_page.recording = Some(duplicate);
        frame(&mut app, vec![key(Key::F7, Modifiers::ALT)]);
        assert_eq!(
            commands::all_shortcuts()[duplicate],
            [KeyboardShortcut::new(Modifiers::ALT, Key::F7)]
        );
        app.shortcut_page.recording = Some(duplicate);
        frame(&mut app, vec![key(Key::Escape, Modifiers::NONE)]);
        assert!(app.shortcut_page.recording.is_none());
        assert_eq!(
            commands::all_shortcuts()[duplicate],
            [KeyboardShortcut::new(Modifiers::ALT, Key::F7)]
        );
        // Back to the defaults (the keymap is global: leave it clean for other tests).
        app.save_shortcuts(&COMMANDS.iter().map(|e| e.default_shortcuts().to_vec()).collect::<Vec<_>>());
        assert!(app.settings.shortcuts.is_empty());
        drop(app);
        worker.join().unwrap();
    }

    #[test]
    fn searching_the_settings_page_shows_matching_shortcuts() {
        let ctx = egui::Context::default();
        let cc = eframe::CreationContext::_new_kittest(ctx.clone());
        let (writer, worker) = SettingsWriter::start();
        let startup = Startup {
            root: None,
            file: None,
            restore: false,
            window: Default::default(),
        };
        let mut app = App::new(&cc, startup, Default::default(), writer, None);
        app.settings_page.filter = "duplicate".into();
        let mut texts = Vec::new();
        for _ in 0..3 {
            let mut out = ctx.run_ui(Default::default(), |ui| app.settings_tab(ui));
            out.textures_delta.clear();
            texts = out
                .shapes
                .iter()
                .filter_map(|shape| match &shape.shape {
                    egui::Shape::Text(text) => Some(text.galley.text().to_string()),
                    _ => None,
                })
                .collect();
        }
        assert!(texts.iter().any(|t| t == "Keyboard shortcuts"), "{texts:?}");
        assert!(texts.iter().any(|t| t == "Duplicate line or selection"), "{texts:?}");
        assert!(!texts.iter().any(|t| t == "Save"));
        drop(app);
        worker.join().unwrap();
    }

    #[test]
    fn filter_matches_titles_and_shortcut_text() {
        assert!(row_matches("dup", "Duplicate line or selection", "Ctrl+D"));
        assert!(row_matches("ctrl+d", "Duplicate line or selection", "Ctrl+D"));
        assert!(row_matches("shortcuts", "Save", "Ctrl+S"));
        assert!(!row_matches("dup", "Save", "Ctrl+S"));
    }
}
