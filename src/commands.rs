use crate::settings::Settings;
use eframe::egui::{self, Key, KeyboardShortcut, ModifierNames, Modifiers};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Command {
    Palette,
    OpenFolder,
    CloseFolder,
    QuickOpen,
    GoToLine,
    Find,
    Replace,
    FindInFiles,
    DocumentSymbols,
    WorkspaceSymbols,
    SwitchSourceHeader,
    GoToDefinition,
    FindReferences,
    RenameSymbol,
    CodeActions,
    SignatureHelp,
    ShowCallHierarchy,
    GoBack,
    GoForward,
    Explorer,
    ToggleSidebar,
    SourceControl,
    History,
    FileHistory,
    SelectionHistory,
    MarkdownPreview,
    SelectForComparison,
    CompareWithSelected,
    Refresh,
    Commit,
    Save,
    CloseTab,
    CloseAllTabs,
    ToggleTheme,
    SplitEditor,
    FormatDocument,
    CSettings,
    NewWindow,
    NewFile,
    SaveAs,
    NewFolder,
    Delete,
    RenameFile,
    OpenFolderInNewWindow,
    OpenSettings,
    GenerateCompileDatabase,
    ToggleSetting(Setting),
    TabSize(usize),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Setting {
    HighlightLine,
    ShowWhitespace,
    DiffIgnoreEol,
    InlineBlame,
    ChangesTree,
    RestoreSession,
    ReopenLastFolder,
    FormatOnSave,
    Minimap,
    BracketColors,
    SemanticHighlighting,
    StickyScroll,
    ShowOutline,
}

impl Setting {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::HighlightLine => "Highlight current line",
            Self::ShowWhitespace => "Show whitespace",
            Self::DiffIgnoreEol => "Diff: ignore line ending differences",
            Self::InlineBlame => "Inline blame (who changed the current line)",
            Self::ChangesTree => "Source Control: view as tree",
            Self::RestoreSession => "Restore previous session",
            Self::ReopenLastFolder => "Reopen last folder at startup",
            Self::FormatOnSave => "Format on save",
            Self::Minimap => "Show minimap",
            Self::BracketColors => "Color nested brackets",
            Self::SemanticHighlighting => "Semantic highlighting (colors from the language server)",
            Self::StickyScroll => "Sticky scroll",
            Self::ShowOutline => "Show outline",
        }
    }

    pub(crate) fn value(self, settings: &mut Settings) -> &mut bool {
        match self {
            Self::HighlightLine => &mut settings.highlight_line,
            Self::ShowWhitespace => &mut settings.show_whitespace,
            Self::DiffIgnoreEol => &mut settings.diff_ignore_eol,
            Self::InlineBlame => &mut settings.inline_blame,
            Self::ChangesTree => &mut settings.changes_tree,
            Self::RestoreSession => &mut settings.restore_previous_session,
            Self::ReopenLastFolder => &mut settings.reopen_last_folder,
            Self::FormatOnSave => &mut settings.format_on_save,
            Self::Minimap => &mut settings.minimap,
            Self::BracketColors => &mut settings.bracket_colors,
            Self::SemanticHighlighting => &mut settings.semantic_highlighting,
            Self::StickyScroll => &mut settings.sticky_scroll,
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
        if matches!(self.id, Command::SwitchSourceHeader | Command::ShowCallHierarchy) {
            // Option-letter shortcuts can also produce a Text event on macOS.
            let mut consumed = false;
            let mut paired_text = false;
            input.events.retain(|event| {
                let text = std::mem::take(&mut paired_text) && matches!(event, egui::Event::Text(_));
                paired_text = matches!(
                    event,
                    egui::Event::Key { key, pressed: true, modifiers, .. }
                        if self.shortcuts.iter().any(|shortcut| shortcut.logical_key == *key
                            && if self.id == Command::SwitchSourceHeader {
                                *modifiers == shortcut.modifiers
                            } else {
                                modifiers.matches_logically(shortcut.modifiers)
                            })
                );
                consumed |= paired_text;
                !paired_text && !text
            });
            return consumed;
        }
        self.shortcuts.iter().any(|shortcut| input.consume_shortcut(shortcut))
    }
}

const CMD: Modifiers = Modifiers::COMMAND;
const CMD_SHIFT: Modifiers = CMD.plus(Modifiers::SHIFT);
#[cfg(target_os = "macos")]
const GO_BACK: KeyboardShortcut = KeyboardShortcut::new(Modifiers::CTRL, Key::Minus);
#[cfg(target_os = "macos")]
const GO_FORWARD: KeyboardShortcut = KeyboardShortcut::new(Modifiers::CTRL.plus(Modifiers::SHIFT), Key::Minus);
#[cfg(not(target_os = "macos"))]
const GO_BACK: KeyboardShortcut = KeyboardShortcut::new(Modifiers::ALT, Key::ArrowLeft);
#[cfg(not(target_os = "macos"))]
const GO_FORWARD: KeyboardShortcut = KeyboardShortcut::new(Modifiers::ALT, Key::ArrowRight);

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
    Entry::new(NewFile, "New file", &[KeyboardShortcut::new(CMD, Key::N)]),
    Entry::new(NewFolder, "New folder…", &[]),
    Entry::new(Delete, "Delete file", &[]),
    Entry::new(RenameFile, "Rename file…", &[]),
    Entry::new(OpenFolder, "Open folder", &[]),
    Entry::new(OpenFolderInNewWindow, "Open folder in new window…", &[]),
    Entry::new(NewWindow, "New window", &[KeyboardShortcut::new(CMD_SHIFT, Key::N)]),
    Entry::new(CloseFolder, "Close folder (back to the start page)", &[]),
    Entry::new(QuickOpen, "Go to file", &[KeyboardShortcut::new(CMD, Key::P)]),
    // Ctrl+G on every platform (also on macOS, like VS Code).
    Entry::new(GoToLine, "Go to line…", &[KeyboardShortcut::new(Modifiers::CTRL, Key::G)]),
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
        ShowCallHierarchy,
        "Show Call Hierarchy",
        &[KeyboardShortcut::new(Modifiers::SHIFT.plus(Modifiers::ALT), Key::H)],
    ),
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
    Entry::new(RenameSymbol, "Rename symbol", &[KeyboardShortcut::new(Modifiers::NONE, Key::F2)]),
    Entry::new(CodeActions, "Quick fix / code actions", &[KeyboardShortcut::new(CMD, Key::Period)]),
    Entry::new(
        SignatureHelp,
        "Show parameter hints",
        &[KeyboardShortcut::new(CMD_SHIFT, Key::Space)],
    ),
    Entry::new(
        SwitchSourceHeader,
        "Switch between C/C++ source and header",
        &[KeyboardShortcut::new(Modifiers::ALT, Key::O)],
    ),
    Entry::new(GoBack, "Go back", &[GO_BACK]),
    Entry::new(GoForward, "Go forward", &[GO_FORWARD]),
    Entry::new(Explorer, "Show Explorer", &[]),
    Entry::new(ToggleSidebar, "Toggle sidebar", &[]),
    Entry::new(SourceControl, "Show source control", &[]),
    Entry::new(History, "Show Git graph / repository history", &[]),
    Entry::new(FileHistory, "File History", &[]),
    Entry::new(SelectionHistory, "File History for Selection", &[]),
    Entry::new(MarkdownPreview, "Open Preview to the Side", &[]),
    Entry::new(SelectForComparison, "Select for Comparison", &[]),
    Entry::new(CompareWithSelected, "Compare with Selected", &[]),
    Entry::new(Refresh, "Refresh project and source control", &[]),
    Entry::new(Commit, "Commit changes", &[KeyboardShortcut::new(CMD, Key::Enter)]),
    Entry::new(SaveAs, "Save as…", &[KeyboardShortcut::new(CMD_SHIFT, Key::S)]),
    Entry::new(Save, "Save", &[KeyboardShortcut::new(CMD, Key::S)]),
    Entry::new(CloseTab, "Close tab", &[KeyboardShortcut::new(CMD, Key::W)]),
    Entry::new(CloseAllTabs, "Close all tabs", &[]),
    Entry::new(OpenSettings, "Open settings", &[KeyboardShortcut::new(CMD, Key::Comma)]),
    Entry::new(ToggleTheme, "Toggle theme", &[]),
    Entry::new(
        SplitEditor,
        "Split editor (open the file to the side)",
        &[KeyboardShortcut::new(CMD, Key::Backslash)],
    ),
    Entry::new(ToggleSetting(Setting::HighlightLine), "Toggle current line highlight", &[]),
    Entry::new(ToggleSetting(Setting::ShowWhitespace), "Toggle whitespace", &[]),
    Entry::new(ToggleSetting(Setting::DiffIgnoreEol), "Diff: ignore line ending differences", &[]),
    Entry::new(ToggleSetting(Setting::InlineBlame), "Toggle inline blame", &[]),
    Entry::new(ToggleSetting(Setting::ChangesTree), "Toggle source control tree view", &[]),
    Entry::new(ToggleSetting(Setting::RestoreSession), "Toggle restoring the previous session", &[]),
    Entry::new(
        ToggleSetting(Setting::ReopenLastFolder),
        "Toggle reopening the last folder at startup (instead of the start page)",
        &[],
    ),
    Entry::new(ToggleSetting(Setting::FormatOnSave), "Toggle format on save", &[]),
    Entry::new(ToggleSetting(Setting::Minimap), "Toggle minimap", &[]),
    Entry::new(ToggleSetting(Setting::BracketColors), "Toggle bracket pair colors", &[]),
    Entry::new(ToggleSetting(Setting::SemanticHighlighting), "Toggle semantic highlighting", &[]),
    Entry::new(ToggleSetting(Setting::StickyScroll), "Toggle sticky scroll", &[]),
    Entry::new(ToggleSetting(Setting::ShowOutline), "Show outline", &[]),
    Entry::new(TabSize(2), "Tab size: 2", &[]),
    Entry::new(TabSize(4), "Tab size: 4", &[]),
    Entry::new(TabSize(8), "Tab size: 8", &[]),
    Entry::new(CSettings, "C/C++: Compile flags and build settings", &[]),
    Entry::new(GenerateCompileDatabase, "C/C++: Generate compile database", &[]),
    Entry::new(
        FormatDocument,
        "Format document",
        &[KeyboardShortcut::new(Modifiers::SHIFT.plus(Modifiers::ALT), Key::F)],
    ),
];

/// Commands in the order their shortcuts are tried: most modifiers first. egui also
/// matches a shortcut when Shift or Alt is held in addition, so Ctrl+F would otherwise
/// take Ctrl+Shift+F (Search in files) and Ctrl+Alt+F (Replace).
pub(crate) fn dispatch_order() -> &'static [&'static Entry] {
    static ORDER: std::sync::OnceLock<Vec<&'static Entry>> = std::sync::OnceLock::new();
    ORDER.get_or_init(|| {
        let count = |m: Modifiers| [m.alt, m.shift, m.command || m.ctrl || m.mac_cmd].iter().filter(|b| **b).count();
        let mut order: Vec<&Entry> = COMMANDS.iter().collect();
        order.sort_by_key(|e| std::cmp::Reverse(e.shortcuts.iter().map(|s| count(s.modifiers)).max().unwrap_or(0)));
        order
    })
}

pub(crate) fn entry(id: Command) -> &'static Entry {
    COMMANDS.iter().find(|entry| entry.id == id).expect("command is registered")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn more_specific_shortcuts_win() {
        let first = |modifiers: Modifiers, key: Key| {
            dispatch_order()
                .iter()
                .find(|e| {
                    e.shortcuts
                        .iter()
                        .any(|s| s.logical_key == key && modifiers.matches_logically(s.modifiers))
                })
                .map(|e| e.id)
        };
        assert!(first(CMD_SHIFT, Key::F) == Some(FindInFiles));
        assert!(first(CMD.plus(Modifiers::ALT), Key::F) == Some(Replace));
        assert!(first(CMD, Key::F) == Some(Find));
        assert!(first(CMD_SHIFT, Key::P) == Some(Palette));
        assert!(first(CMD, Key::P) == Some(QuickOpen));
        assert!(first(CMD_SHIFT, Key::O) == Some(DocumentSymbols));
        assert!(first(Modifiers::SHIFT, Key::F12) == Some(FindReferences));
        assert!(first(Modifiers::SHIFT.plus(Modifiers::ALT), Key::H) == Some(ShowCallHierarchy));
        assert!(first(Modifiers::NONE, Key::F12) == Some(GoToDefinition));
        assert!(first(Modifiers::SHIFT.plus(Modifiers::ALT), Key::F) == Some(FormatDocument));
        #[cfg(target_os = "macos")]
        {
            assert!(first(Modifiers::CTRL.plus(Modifiers::SHIFT), Key::Minus) == Some(GoForward));
            assert!(first(Modifiers::CTRL, Key::Minus) == Some(GoBack));
        }
        #[cfg(not(target_os = "macos"))]
        {
            assert!(first(Modifiers::ALT, Key::ArrowRight) == Some(GoForward));
            assert!(first(Modifiers::ALT, Key::ArrowLeft) == Some(GoBack));
        }
    }

    #[test]
    fn option_shortcuts_consume_paired_text() {
        for (command, key, modifiers, generated) in [
            (SwitchSourceHeader, Key::O, Modifiers::ALT, "ø"),
            (ShowCallHierarchy, Key::H, Modifiers::ALT | Modifiers::SHIFT, "Ó"),
        ] {
            let mut input = egui::InputState::default();
            input.events = vec![
                egui::Event::Key {
                    key,
                    physical_key: Some(key),
                    pressed: true,
                    repeat: false,
                    modifiers,
                },
                egui::Event::Text(generated.into()),
                egui::Event::Text("other input".into()),
            ];
            assert!(entry(command).consume(&mut input));
            assert_eq!(input.events, [egui::Event::Text("other input".into())]);
        }
    }
}
