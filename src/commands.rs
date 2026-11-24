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
    FindReferences,
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
    SplitEditor,
    FormatDocument,
    ToggleSetting(Setting),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Setting {
    HighlightLine,
    ShowWhitespace,
    InlineBlame,
    ChangesTree,
    RestoreSession,
    FormatOnSave,
    Minimap,
    BracketColors,
    ShowOutline,
}

impl Setting {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::HighlightLine => "Highlight current line",
            Self::ShowWhitespace => "Show whitespace",
            Self::InlineBlame => "Inline blame (who changed the current line)",
            Self::ChangesTree => "Source Control: view as tree",
            Self::RestoreSession => "Restore previous session",
            Self::FormatOnSave => "Format on save",
            Self::Minimap => "Show minimap",
            Self::BracketColors => "Color nested brackets",
            Self::ShowOutline => "Show outline",
        }
    }

    pub(crate) fn value(self, settings: &mut Settings) -> &mut bool {
        match self {
            Self::HighlightLine => &mut settings.highlight_line,
            Self::ShowWhitespace => &mut settings.show_whitespace,
            Self::InlineBlame => &mut settings.inline_blame,
            Self::ChangesTree => &mut settings.changes_tree,
            Self::RestoreSession => &mut settings.restore_previous_session,
            Self::FormatOnSave => &mut settings.format_on_save,
            Self::Minimap => &mut settings.minimap,
            Self::BracketColors => &mut settings.bracket_colors,
            Self::ShowOutline => &mut settings.show_outline,
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
    // Before Go to definition: egui also matches F12 when Shift is held.
    Entry::new(
        FindReferences,
        "Find all references",
        &[KeyboardShortcut::new(Modifiers::SHIFT, Key::F12)],
    ),
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
    Entry::new(
        SplitEditor,
        "Split editor (open the file to the side)",
        &[KeyboardShortcut::new(CMD, Key::Backslash)],
    ),
    Entry::new(ToggleSetting(Setting::HighlightLine), "Toggle current line highlight", &[]),
    Entry::new(ToggleSetting(Setting::ShowWhitespace), "Toggle whitespace", &[]),
    Entry::new(ToggleSetting(Setting::InlineBlame), "Toggle inline blame", &[]),
    Entry::new(ToggleSetting(Setting::ChangesTree), "Toggle source control tree view", &[]),
    Entry::new(ToggleSetting(Setting::RestoreSession), "Toggle restoring the previous session", &[]),
    Entry::new(ToggleSetting(Setting::FormatOnSave), "Toggle format on save", &[]),
    Entry::new(ToggleSetting(Setting::Minimap), "Toggle minimap", &[]),
    Entry::new(ToggleSetting(Setting::BracketColors), "Toggle bracket pair colors", &[]),
    Entry::new(ToggleSetting(Setting::ShowOutline), "Show outline", &[]),
    Entry::new(
        FormatDocument,
        "Format document",
        &[KeyboardShortcut::new(Modifiers::SHIFT.plus(Modifiers::ALT), Key::F)],
    ),
];

pub(crate) fn entry(id: Command) -> &'static Entry {
    COMMANDS.iter().find(|entry| entry.id == id).expect("command is registered")
}
