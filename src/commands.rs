use crate::editor::Action;
use crate::encoding::Encoding;
use crate::settings::Settings;
use eframe::egui::{self, Key, KeyboardShortcut, ModifierNames, Modifiers};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Command {
    Palette,
    /// Show / hide one kind of sticky scroll line.
    ToggleStickyKind(crate::editor::sticky::Kind),
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
    ToggleHighlight,
    ClearHighlights,
    GoBack,
    GoForward,
    Explorer,
    ToggleSidebar,
    SourceControl,
    NextChange,
    PreviousChange,
    StageHunk,
    UnstageHunk,
    RevertHunk,
    StageSelectedLines,
    UnstageSelectedLines,
    RevertSelectedLines,
    History,
    FileHistory,
    SelectionHistory,
    MarkdownPreview,
    SelectForComparison,
    CompareWithSelected,
    Refresh,
    Commit,
    StashChanges,
    ShowStashes,
    StashEntry(crate::vcs::StashAction),
    ViewStash,
    Save,
    ReopenWithEncoding(Encoding),
    SaveWithEncoding(Encoding),
    ChangeLineEndings(bool),
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
    /// Line editing, bookmarks and merge conflicts in the active editor.
    Editor(crate::editor::Action),
    Bookmarks,
    AffixLines,
}

impl Command {
    pub(crate) fn is_stash(self) -> bool {
        matches!(self, Self::StashChanges | Self::ShowStashes | Self::StashEntry(_) | Self::ViewStash)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Setting {
    ColorWindow,
    HighlightLine,
    ShowWhitespace,
    DiffIgnoreEol,
    InlineBlame,
    ChangesTree,
    RestoreSession,
    ReopenLastFolder,
    FormatOnSave,
    TrimTrailingWhitespaceOnSave,
    InsertFinalNewlineOnSave,
    Minimap,
    BracketColors,
    SemanticHighlighting,
    StickyScroll,
    StickyControlFlow,
    ShowOutline,
    ShowFps,
    HeaderLines,
    WordWrap,
}

impl Setting {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::ColorWindow => "Color the whole window from the theme",
            Self::HighlightLine => "Highlight current line",
            Self::ShowWhitespace => "Show whitespace",
            Self::DiffIgnoreEol => "Diff: ignore line ending differences",
            Self::InlineBlame => "Inline blame (who changed the current line)",
            Self::ChangesTree => "Source Control: view as tree",
            Self::RestoreSession => "Restore previous session",
            Self::ReopenLastFolder => "Reopen last folder at startup",
            Self::FormatOnSave => "Format on save",
            Self::TrimTrailingWhitespaceOnSave => "Trim trailing whitespace on save",
            Self::InsertFinalNewlineOnSave => "Insert final newline on save",
            Self::Minimap => "Show minimap",
            Self::BracketColors => "Color nested brackets",
            Self::SemanticHighlighting => "Semantic highlighting (colors from the language server)",
            Self::StickyScroll => "Sticky scroll",
            Self::StickyControlFlow => "Sticky scroll: control flow (if, else, loops, switch, case)",
            Self::ShowOutline => "Show outline",
            Self::ShowFps => "Show frames per second in the status bar",
            Self::HeaderLines => "Lines under the breadcrumbs and sticky scroll",
            Self::WordWrap => "Word wrap",
        }
    }

    pub(crate) fn value(self, settings: &mut Settings) -> &mut bool {
        match self {
            Self::ColorWindow => &mut settings.color_window,
            Self::HighlightLine => &mut settings.highlight_line,
            Self::ShowWhitespace => &mut settings.show_whitespace,
            Self::DiffIgnoreEol => &mut settings.diff_ignore_eol,
            Self::InlineBlame => &mut settings.inline_blame,
            Self::ChangesTree => &mut settings.changes_tree,
            Self::RestoreSession => &mut settings.restore_previous_session,
            Self::ReopenLastFolder => &mut settings.reopen_last_folder,
            Self::FormatOnSave => &mut settings.format_on_save,
            Self::TrimTrailingWhitespaceOnSave => &mut settings.trim_trailing_whitespace_on_save,
            Self::InsertFinalNewlineOnSave => &mut settings.insert_final_newline_on_save,
            Self::Minimap => &mut settings.minimap,
            Self::BracketColors => &mut settings.bracket_colors,
            Self::SemanticHighlighting => &mut settings.semantic_highlighting,
            Self::StickyScroll => &mut settings.sticky_scroll,
            Self::StickyControlFlow => &mut settings.sticky_control_flow,
            Self::ShowOutline => &mut settings.show_outline,
            Self::ShowFps => &mut settings.show_fps,
            Self::HeaderLines => &mut settings.header_lines,
            Self::WordWrap => &mut settings.word_wrap,
        }
    }
}

pub(crate) struct Entry {
    pub(crate) id: Command,
    pub(crate) title: &'static str,
    shortcuts: &'static [KeyboardShortcut],
    /// The app runs the command when its shortcut is pressed (false: the editor handles the
    /// keys itself, only while it has the focus; they are listed for the palette).
    dispatch: bool,
}

impl Entry {
    const fn new(id: Command, title: &'static str, shortcuts: &'static [KeyboardShortcut]) -> Self {
        Self {
            id,
            title,
            shortcuts,
            dispatch: true,
        }
    }

    /// A command whose shortcuts the editor widget handles.
    const fn editor(action: crate::editor::Action, title: &'static str, shortcuts: &'static [KeyboardShortcut]) -> Self {
        Self {
            id: Command::Editor(action),
            title,
            shortcuts,
            dispatch: false,
        }
    }

    pub(crate) fn dispatches(&self) -> bool {
        self.dispatch
    }

    /// Position in [`COMMANDS`] (entries are only ever borrowed from it).
    pub(crate) fn index(&self) -> usize {
        (self as *const Entry as usize - COMMANDS.as_ptr() as usize) / std::mem::size_of::<Entry>()
    }

    /// The shortcuts in use: the default ones unless changed in Settings > Keyboard shortcuts.
    pub(crate) fn shortcuts(&self) -> Vec<KeyboardShortcut> {
        keymap().shortcuts[self.index()].clone()
    }

    pub(crate) fn default_shortcuts(&self) -> &'static [KeyboardShortcut] {
        self.shortcuts
    }

    pub(crate) fn shortcut_text(&self, ctx: &egui::Context) -> String {
        self.shortcuts()
            .iter()
            .map(|shortcut| shortcut.format(&ModifierNames::NAMES, ctx.os().is_mac()))
            .collect::<Vec<_>>()
            .join(" / ")
    }

    pub(crate) fn consume(&self, input: &mut egui::InputState) -> bool {
        let shortcuts = self.shortcuts();
        if shortcuts.is_empty() {
            return false;
        }
        // Option-letter shortcuts also type a character on macOS (Option+Z types "Ω").
        let alt_letter = shortcuts.iter().any(|s| s.modifiers.alt && s.logical_key.name().len() == 1);
        if matches!(self.id, Command::SwitchSourceHeader | Command::ShowCallHierarchy) || alt_letter {
            // Option-letter shortcuts can also produce a Text event on macOS.
            let mut consumed = false;
            let mut paired_text = false;
            input.events.retain(|event| {
                let text = std::mem::take(&mut paired_text) && matches!(event, egui::Event::Text(_));
                paired_text = matches!(
                    event,
                    egui::Event::Key { key, pressed: true, modifiers, .. }
                        if shortcuts.iter().any(|shortcut| shortcut.logical_key == *key
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
        shortcuts.iter().any(|shortcut| input.consume_shortcut(shortcut))
    }
}

/// Shortcut changes from the settings, by command title: the command's whole list (empty: none).
pub(crate) type Overrides = std::collections::BTreeMap<String, Vec<KeyboardShortcut>>;

/// The shortcuts in use, per entry of [`COMMANDS`], and the order to try them in.
pub(crate) struct Keymap {
    shortcuts: Vec<Vec<KeyboardShortcut>>,
    order: Vec<usize>,
}

#[cfg(not(test))]
static KEYMAP: std::sync::RwLock<Option<std::sync::Arc<Keymap>>> = std::sync::RwLock::new(None);

// Tests run in parallel: each thread has its own keymap, so changing one can't affect another.
#[cfg(test)]
thread_local! {
    static KEYMAP: std::cell::RefCell<Option<std::sync::Arc<Keymap>>> = const { std::cell::RefCell::new(None) };
}

fn stored_keymap() -> Option<std::sync::Arc<Keymap>> {
    #[cfg(not(test))]
    return KEYMAP.read().ok().and_then(|k| k.clone());
    #[cfg(test)]
    KEYMAP.with(|k| k.borrow().clone())
}

fn store_keymap(keymap: std::sync::Arc<Keymap>) {
    #[cfg(not(test))]
    if let Ok(mut slot) = KEYMAP.write() {
        *slot = Some(keymap);
    }
    #[cfg(test)]
    KEYMAP.with(|k| *k.borrow_mut() = Some(keymap));
}

impl Keymap {
    fn new(overrides: &Overrides) -> Self {
        let shortcuts: Vec<Vec<KeyboardShortcut>> = COMMANDS
            .iter()
            .map(|entry| overrides.get(entry.title).cloned().unwrap_or_else(|| entry.shortcuts.to_vec()))
            .collect();
        // Most modifiers first: egui also matches a shortcut when Shift or Alt is held in addition,
        // so Ctrl+F would otherwise take Ctrl+Shift+F (Search in files) and Ctrl+Alt+F (Replace).
        let count = |m: Modifiers| [m.alt, m.shift, m.command || m.ctrl || m.mac_cmd].iter().filter(|b| **b).count();
        let mut order: Vec<usize> = (0..COMMANDS.len()).collect();
        order.sort_by_key(|&i| std::cmp::Reverse(shortcuts[i].iter().map(|s| count(s.modifiers)).max().unwrap_or(0)));
        Self { shortcuts, order }
    }
}

fn keymap() -> std::sync::Arc<Keymap> {
    stored_keymap().unwrap_or_else(|| {
        let keymap = std::sync::Arc::new(Keymap::new(&Overrides::new()));
        store_keymap(keymap.clone());
        keymap
    })
}

/// Use the shortcut changes from the settings.
pub(crate) fn set_overrides(overrides: &Overrides) {
    store_keymap(std::sync::Arc::new(Keymap::new(overrides)));
}

/// The changes that turn the defaults into `shortcuts` (one list per entry of [`COMMANDS`]).
pub(crate) fn overrides_for(shortcuts: &[Vec<KeyboardShortcut>]) -> Overrides {
    COMMANDS
        .iter()
        .zip(shortcuts)
        .filter(|(entry, list)| entry.shortcuts != list.as_slice())
        .map(|(entry, list)| (entry.title.to_string(), list.clone()))
        .collect()
}

/// The shortcuts in use for every entry of [`COMMANDS`].
pub(crate) fn all_shortcuts() -> Vec<Vec<KeyboardShortcut>> {
    keymap().shortcuts.clone()
}

/// Pressed `m` is exactly `pattern` (no extra Shift / Alt, unlike egui's logical matching).
pub(crate) fn exact(m: Modifiers, pattern: Modifiers) -> bool {
    m.alt == pattern.alt
        && m.shift == pattern.shift
        && m.command == pattern.command
        && (m.ctrl && !m.command) == (pattern.ctrl && !pattern.command)
}

/// The editor action bound to `key` with exactly `m` (Settings > Keyboard shortcuts can change them).
pub(crate) fn editor_action(key: Key, m: Modifiers) -> Option<Action> {
    let keymap = keymap();
    COMMANDS
        .iter()
        .zip(&keymap.shortcuts)
        .find_map(|(entry, shortcuts)| match entry.id {
            Command::Editor(action) if !entry.dispatch => shortcuts
                .iter()
                .any(|s| s.logical_key == key && exact(m, s.modifiers))
                .then_some(action),
            _ => None,
        })
}

/// "Cmd+Shift+K" (Cmd: Ctrl on Windows and Linux) for the settings file.
pub(crate) fn shortcut_spec(shortcut: &KeyboardShortcut) -> String {
    let m = shortcut.modifiers;
    let mut parts = Vec::new();
    if m.ctrl && !m.command {
        parts.push("Ctrl");
    }
    if m.command || m.mac_cmd {
        parts.push("Cmd");
    }
    if m.alt {
        parts.push("Alt");
    }
    if m.shift {
        parts.push("Shift");
    }
    parts.push(shortcut.logical_key.name());
    parts.join("+")
}

pub(crate) fn parse_shortcut(spec: &str) -> Option<KeyboardShortcut> {
    let mut modifiers = Modifiers::NONE;
    let mut key = None;
    for part in spec.trim().split('+') {
        match part.trim() {
            "Ctrl" => modifiers = modifiers.plus(Modifiers::CTRL),
            "Cmd" => modifiers = modifiers.plus(Modifiers::COMMAND),
            "Alt" => modifiers = modifiers.plus(Modifiers::ALT),
            "Shift" => modifiers = modifiers.plus(Modifiers::SHIFT),
            name => key = Some(Key::from_name(name)?),
        }
    }
    Some(KeyboardShortcut::new(modifiers, key?))
}

/// Keys the editor handles itself (not commands): assigning one to a command takes it over.
pub(crate) fn editor_keys() -> Vec<(KeyboardShortcut, &'static str)> {
    let k = KeyboardShortcut::new;
    let mut keys = vec![
        (k(CMD, Key::Z), "Undo"),
        (k(CMD_SHIFT, Key::Z), "Redo"),
        (k(CMD, Key::A), "Select all"),
        (k(CMD, Key::C), "Copy"),
        (k(CMD, Key::X), "Cut"),
        (k(CMD, Key::V), "Paste"),
        (k(CMD, Key::Slash), "Toggle line comment"),
        (k(CMD.plus(Modifiers::ALT), Key::OpenBracket), "Fold"),
        (k(CMD.plus(Modifiers::ALT), Key::CloseBracket), "Unfold"),
        (k(Modifiers::NONE, Key::Tab), "Indent"),
        (k(Modifiers::SHIFT, Key::Tab), "Outdent"),
        (k(Modifiers::NONE, Key::Escape), "Single cursor"),
    ];
    if !cfg!(target_os = "macos") {
        keys.push((k(CMD, Key::Y), "Redo"));
    }
    keys
}

const CMD: Modifiers = Modifiers::COMMAND;
const ALT_SHIFT: Modifiers = Modifiers::ALT.plus(Modifiers::SHIFT);
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

pub(crate) static COMMANDS: &[Entry] = &[
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
    // Before Replace: egui also matches Cmd+H when Shift is held.
    Entry::new(
        ToggleHighlight,
        "Highlight all occurrences (toggle)",
        &[KeyboardShortcut::new(CMD_SHIFT, Key::H)],
    ),
    Entry::new(ClearHighlights, "Clear all highlights", &[]),
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
    Entry::new(NextChange, "Go to next change", &[KeyboardShortcut::new(Modifiers::ALT, Key::F5)]),
    Entry::new(
        PreviousChange,
        "Go to previous change",
        &[KeyboardShortcut::new(Modifiers::SHIFT.plus(Modifiers::ALT), Key::F5)],
    ),
    Entry::new(StageHunk, "Stage hunk at caret", &[]),
    Entry::new(UnstageHunk, "Unstage current hunk", &[]),
    Entry::new(RevertHunk, "Revert hunk at caret", &[]),
    Entry::new(StageSelectedLines, "Stage selected lines", &[]),
    Entry::new(UnstageSelectedLines, "Unstage selected lines", &[]),
    Entry::new(RevertSelectedLines, "Revert selected lines", &[]),
    Entry::new(History, "Show Git graph / repository history", &[]),
    Entry::new(FileHistory, "File History", &[]),
    Entry::new(SelectionHistory, "File History for Selection", &[]),
    Entry::new(MarkdownPreview, "Open Preview to the Side", &[]),
    Entry::new(SelectForComparison, "Select for Comparison", &[]),
    Entry::new(CompareWithSelected, "Compare with Selected", &[]),
    Entry::new(Refresh, "Refresh project and source control", &[]),
    Entry::new(Commit, "Commit changes", &[KeyboardShortcut::new(CMD, Key::Enter)]),
    Entry::new(StashChanges, "Git: Stash changes…", &[]),
    Entry::new(ShowStashes, "Git: Show stashes", &[]),
    Entry::new(StashEntry(crate::vcs::StashAction::Apply), "Git: Apply stash…", &[]),
    Entry::new(StashEntry(crate::vcs::StashAction::Pop), "Git: Pop stash…", &[]),
    Entry::new(StashEntry(crate::vcs::StashAction::Drop), "Git: Drop stash…", &[]),
    Entry::new(ViewStash, "Git: View stash diff…", &[]),
    Entry::new(SaveAs, "Save as…", &[KeyboardShortcut::new(CMD_SHIFT, Key::S)]),
    Entry::new(Save, "Save", &[KeyboardShortcut::new(CMD, Key::S)]),
    Entry::new(ReopenWithEncoding(Encoding::Utf8), "Reopen with encoding: UTF-8", &[]),
    Entry::new(ReopenWithEncoding(Encoding::Utf8Bom), "Reopen with encoding: UTF-8 with BOM", &[]),
    Entry::new(ReopenWithEncoding(Encoding::Utf16Le), "Reopen with encoding: UTF-16 LE", &[]),
    Entry::new(ReopenWithEncoding(Encoding::Utf16Be), "Reopen with encoding: UTF-16 BE", &[]),
    Entry::new(ReopenWithEncoding(Encoding::Windows1252), "Reopen with encoding: Windows-1252", &[]),
    Entry::new(ReopenWithEncoding(Encoding::Latin1), "Reopen with encoding: ISO-8859-1", &[]),
    Entry::new(SaveWithEncoding(Encoding::Utf8), "Save with encoding: UTF-8", &[]),
    Entry::new(SaveWithEncoding(Encoding::Utf8Bom), "Save with encoding: UTF-8 with BOM", &[]),
    Entry::new(SaveWithEncoding(Encoding::Utf16Le), "Save with encoding: UTF-16 LE", &[]),
    Entry::new(SaveWithEncoding(Encoding::Utf16Be), "Save with encoding: UTF-16 BE", &[]),
    Entry::new(SaveWithEncoding(Encoding::Windows1252), "Save with encoding: Windows-1252", &[]),
    Entry::new(SaveWithEncoding(Encoding::Latin1), "Save with encoding: ISO-8859-1", &[]),
    Entry::new(ChangeLineEndings(false), "Change line endings to LF", &[]),
    Entry::new(ChangeLineEndings(true), "Change line endings to CRLF", &[]),
    Entry::new(CloseTab, "Close tab", &[KeyboardShortcut::new(CMD, Key::W)]),
    Entry::new(CloseAllTabs, "Close all tabs", &[]),
    Entry::new(OpenSettings, "Open settings", &[KeyboardShortcut::new(CMD, Key::Comma)]),
    Entry::new(ToggleTheme, "Toggle theme", &[]),
    Entry::new(ToggleSetting(Setting::ColorWindow), "Toggle whole window theme colors", &[]),
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
    Entry::new(
        ToggleSetting(Setting::TrimTrailingWhitespaceOnSave),
        "Toggle trim trailing whitespace on save",
        &[],
    ),
    Entry::new(
        ToggleSetting(Setting::InsertFinalNewlineOnSave),
        "Toggle insert final newline on save",
        &[],
    ),
    Entry::new(ToggleSetting(Setting::Minimap), "Toggle minimap", &[]),
    Entry::new(
        ToggleSetting(Setting::WordWrap),
        "Toggle word wrap",
        &[KeyboardShortcut::new(Modifiers::ALT, Key::Z)],
    ),
    Entry::new(ToggleSetting(Setting::BracketColors), "Toggle bracket pair colors", &[]),
    Entry::new(ToggleSetting(Setting::SemanticHighlighting), "Toggle semantic highlighting", &[]),
    Entry::new(ToggleSetting(Setting::StickyScroll), "Toggle sticky scroll", &[]),
    Entry::new(ToggleSetting(Setting::StickyControlFlow), "Toggle sticky scroll control flow", &[]),
    Entry::new(ToggleSetting(Setting::ShowOutline), "Show outline", &[]),
    Entry::new(ToggleSetting(Setting::ShowFps), "Toggle frames per second (status bar)", &[]),
    Entry::new(
        ToggleSetting(Setting::HeaderLines),
        "Toggle lines under the breadcrumbs and sticky scroll",
        &[],
    ),
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
    Entry::editor(
        Action::Duplicate,
        "Duplicate line or selection",
        &[KeyboardShortcut::new(CMD, Key::D)],
    ),
    Entry::editor(
        Action::MoveLinesUp,
        "Move line up",
        &[KeyboardShortcut::new(Modifiers::ALT, Key::ArrowUp)],
    ),
    Entry::editor(
        Action::MoveLinesDown,
        "Move line down",
        &[KeyboardShortcut::new(Modifiers::ALT, Key::ArrowDown)],
    ),
    Entry::editor(
        Action::CopyLinesUp,
        "Copy line up",
        &[KeyboardShortcut::new(ALT_SHIFT, Key::ArrowUp)],
    ),
    Entry::editor(
        Action::CopyLinesDown,
        "Copy line down",
        &[KeyboardShortcut::new(ALT_SHIFT, Key::ArrowDown)],
    ),
    Entry::editor(Action::DeleteLines, "Delete line", &[KeyboardShortcut::new(CMD_SHIFT, Key::K)]),
    Entry::editor(Action::JoinLines, "Join lines", &[KeyboardShortcut::new(CMD, Key::J)]),
    Entry::editor(
        Action::AddNextOccurrence,
        "Add next occurrence to the selection",
        &[KeyboardShortcut::new(Modifiers::ALT, Key::J)],
    ),
    Entry::editor(
        Action::RemoveLastOccurrence,
        "Remove the last added occurrence",
        &[KeyboardShortcut::new(ALT_SHIFT, Key::J)],
    ),
    Entry::new(AffixLines, "Add text to the start / end of lines…", &[]),
    Entry::editor(Action::ToggleBookmark, "Toggle bookmark", &[KeyboardShortcut::new(CMD, Key::F2)]),
    Entry::editor(
        Action::NextBookmark,
        "Next bookmark",
        &[KeyboardShortcut::new(Modifiers::ALT, Key::PageDown)],
    ),
    Entry::editor(
        Action::PreviousBookmark,
        "Previous bookmark",
        &[KeyboardShortcut::new(Modifiers::ALT, Key::PageUp)],
    ),
    Entry::editor(Action::ClearBookmarks, "Clear bookmarks in this file", &[]),
    Entry::new(Bookmarks, "Show bookmarks", &[]),
    Entry::editor(Action::NextConflict, "Merge conflict: go to next", &[]),
    Entry::editor(Action::PreviousConflict, "Merge conflict: go to previous", &[]),
    Entry::editor(Action::AcceptCurrent, "Merge conflict: accept current change", &[]),
    Entry::editor(Action::AcceptIncoming, "Merge conflict: accept incoming change", &[]),
    Entry::editor(Action::AcceptBoth, "Merge conflict: accept both changes", &[]),
];

/// Commands in the order their shortcuts are tried: most modifiers first. egui also
/// matches a shortcut when Shift or Alt is held in addition, so Ctrl+F would otherwise
/// take Ctrl+Shift+F (Search in files) and Ctrl+Alt+F (Replace).
pub(crate) fn dispatch_order() -> Vec<&'static Entry> {
    keymap().order.iter().map(|&i| &COMMANDS[i]).collect()
}

pub(crate) fn entry(id: Command) -> &'static Entry {
    COMMANDS.iter().find(|entry| entry.id == id).expect("command is registered")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_are_unique_and_shortcuts_round_trip() {
        // Shortcut changes are saved by title.
        for (i, entry) in COMMANDS.iter().enumerate() {
            assert!(COMMANDS[..i].iter().all(|other| other.title != entry.title), "{}", entry.title);
            assert!(!entry.title.contains('='), "{}", entry.title);
            assert_eq!(entry.index(), i);
            for shortcut in entry.default_shortcuts() {
                assert_eq!(parse_shortcut(&shortcut_spec(shortcut)).as_ref(), Some(shortcut), "{}", entry.title);
            }
        }
    }

    #[test]
    fn more_specific_shortcuts_win() {
        let first = |modifiers: Modifiers, key: Key| {
            dispatch_order()
                .iter()
                .find(|e| {
                    e.shortcuts()
                        .iter()
                        .any(|s| s.logical_key == key && modifiers.matches_logically(s.modifiers))
                })
                .map(|e| e.id)
        };
        assert!(first(CMD_SHIFT, Key::F) == Some(FindInFiles));
        assert!(first(CMD.plus(Modifiers::ALT), Key::F) == Some(Replace));
        assert!(first(CMD, Key::F) == Some(Find));
        assert!(first(CMD_SHIFT, Key::H) == Some(ToggleHighlight));
        assert!(first(CMD, Key::H) == Some(Replace));
        assert!(first(CMD_SHIFT, Key::P) == Some(Palette));
        assert!(first(CMD, Key::P) == Some(QuickOpen));
        assert!(first(CMD_SHIFT, Key::O) == Some(DocumentSymbols));
        assert!(first(Modifiers::SHIFT, Key::F12) == Some(FindReferences));
        assert!(first(Modifiers::SHIFT.plus(Modifiers::ALT), Key::H) == Some(ShowCallHierarchy));
        assert!(first(Modifiers::NONE, Key::F12) == Some(GoToDefinition));
        assert!(first(Modifiers::SHIFT.plus(Modifiers::ALT), Key::F) == Some(FormatDocument));
        assert!(first(Modifiers::ALT, Key::F5) == Some(NextChange));
        assert!(first(Modifiers::SHIFT.plus(Modifiers::ALT), Key::F5) == Some(PreviousChange));
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
