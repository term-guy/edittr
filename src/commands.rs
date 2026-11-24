use crate::settings::Settings;
use eframe::egui::{self, Key, KeyboardShortcut, ModifierNames, Modifiers};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Command {
    Palette,
    OpenFolder,
    QuickOpen,
    Find,
    Replace,
    FindInFiles,
    DocumentSymbols,
    WorkspaceSymbols,
    GoToDefinition,
    GoBack,
    Explorer,
    ToggleSidebar,
    SourceControl,
    History,
    Refresh,
    Commit,
    Save,
    CloseTab,
    CloseAllTabs,
    ToggleTheme,
    ToggleSetting(Setting),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Setting {
    HighlightLine,
    ShowWhitespace,
    InlineBlame,
    ChangesTree,
}

impl Setting {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::HighlightLine => "Highlight current line",
            Self::ShowWhitespace => "Show whitespace",
            Self::InlineBlame => "Inline blame (who changed the current line)",
            Self::ChangesTree => "Source Control: view as tree",
        }
    }

    pub(crate) fn value(self, settings: &mut Settings) -> &mut bool {
        match self {
            Self::HighlightLine => &mut settings.highlight_line,
            Self::ShowWhitespace => &mut settings.show_whitespace,
            Self::InlineBlame => &mut settings.inline_blame,
            Self::ChangesTree => &mut settings.changes_tree,
        }
    }
}

pub(crate) struct Entry {
    pub(crate) id: Command,
    pub(crate) title: &'static str,
    shortcuts: &'static [KeyboardShortcut],
}

impl Entry {
    const fn new(id: Command, title: &'static str, shortcuts: &'static [KeyboardShortcut]) -> Self {
        Self { id, title, shortcuts }
    }

    pub(crate) fn shortcut_text(&self, ctx: &egui::Context) -> String {
        self.shortcuts
            .iter()
            .map(|shortcut| shortcut.format(&ModifierNames::NAMES, ctx.os().is_mac()))
            .collect::<Vec<_>>()
            .join(" / ")
    }

    pub(crate) fn consume(&self, input: &mut egui::InputState) -> bool {
        self.shortcuts.iter().any(|shortcut| input.consume_shortcut(shortcut))
    }
}

const CMD: Modifiers = Modifiers::COMMAND;
const CMD_SHIFT: Modifiers = CMD.plus(Modifiers::SHIFT);
#[cfg(target_os = "macos")]
const GO_BACK: KeyboardShortcut = KeyboardShortcut::new(Modifiers::CTRL, Key::Minus);
#[cfg(not(target_os = "macos"))]
const GO_BACK: KeyboardShortcut = KeyboardShortcut::new(Modifiers::ALT, Key::ArrowLeft);

use Command::*;

pub(crate) const COMMANDS: &[Entry] = &[
    Entry::new(
        Palette,
        "Command palette",
        &[
            KeyboardShortcut::new(CMD_SHIFT, Key::P),
            KeyboardShortcut::new(Modifiers::NONE, Key::F1),
        ],
    ),
    Entry::new(OpenFolder, "Open folder", &[]),
    Entry::new(QuickOpen, "Go to file", &[KeyboardShortcut::new(CMD, Key::P)]),
    Entry::new(Find, "Find in file", &[KeyboardShortcut::new(CMD, Key::F)]),
    Entry::new(
        Replace,
        "Replace in file",
        &[
            KeyboardShortcut::new(CMD, Key::H),
            KeyboardShortcut::new(CMD.plus(Modifiers::ALT), Key::F),
        ],
    ),
    Entry::new(FindInFiles, "Search in files", &[KeyboardShortcut::new(CMD_SHIFT, Key::F)]),
    Entry::new(DocumentSymbols, "Go to symbol in file", &[KeyboardShortcut::new(CMD_SHIFT, Key::O)]),
    Entry::new(WorkspaceSymbols, "Go to symbol in workspace", &[KeyboardShortcut::new(CMD, Key::T)]),
    Entry::new(
        GoToDefinition,
        "Go to definition",
        &[KeyboardShortcut::new(Modifiers::NONE, Key::F12)],
    ),
    Entry::new(GoBack, "Go back", &[GO_BACK]),
    Entry::new(Explorer, "Show Explorer", &[]),
    Entry::new(ToggleSidebar, "Toggle sidebar", &[]),
    Entry::new(SourceControl, "Show source control", &[]),
    Entry::new(History, "Show Git graph / repository history", &[]),
    Entry::new(Refresh, "Refresh project and source control", &[]),
    Entry::new(Commit, "Commit changes", &[KeyboardShortcut::new(CMD, Key::Enter)]),
    Entry::new(Save, "Save", &[KeyboardShortcut::new(CMD, Key::S)]),
    Entry::new(CloseTab, "Close tab", &[KeyboardShortcut::new(CMD, Key::W)]),
    Entry::new(CloseAllTabs, "Close all tabs", &[]),
    Entry::new(ToggleTheme, "Toggle theme", &[]),
    Entry::new(ToggleSetting(Setting::HighlightLine), "Toggle current line highlight", &[]),
    Entry::new(ToggleSetting(Setting::ShowWhitespace), "Toggle whitespace", &[]),
    Entry::new(ToggleSetting(Setting::InlineBlame), "Toggle inline blame", &[]),
    Entry::new(ToggleSetting(Setting::ChangesTree), "Toggle source control tree view", &[]),
];

pub(crate) fn entry(id: Command) -> &'static Entry {
    COMMANDS.iter().find(|entry| entry.id == id).expect("command is registered")
}
