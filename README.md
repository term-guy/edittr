# edittr

A small, fast code editor with built-in git. It's a single native binary written in Rust with [egui](https://github.com/emilk/egui) (no Electron, no webview) and runs on macOS, Linux and Windows.

## Features

### Editor

- **Fast on any file size**: a custom editor widget that only lays out the visible lines, with incremental syntax highlighting for most languages, so typing stays smooth in files with tens of thousands of lines
- **Multiple cursors**: `Alt+click` adds a cursor, `Ctrl/Cmd+D` selects the next occurrence, and middle-click drag makes a column selection; typing edits every line at once
- **Editing helpers**: smart indentation (detected from the file or `.editorconfig`), `Tab` / `Shift+Tab` indent, `Ctrl/Cmd+/` toggles comments, undo/redo grouped by typing pauses
- **Code folding** by indentation (`Ctrl/Cmd+Alt+[` / `]`, or the gutter arrows)
- **Semantic highlighting**: with a language server, macros, enum members, constants, functions, types and parameters get colors from what the server knows, and code switched off by the preprocessor (`#if 0`, unset configuration macros) is greyed out
- **Sticky scroll**: the headers of the functions, types and blocks you're scrolling through stay pinned at the top; click one to jump to it
- **Other uses of the symbol** under the caret are highlighted (where it's written, a little stronger)
- **Syntax colors**: macros and constants (`#define` names, `const`, `ALL_CAPS` names) get their own color, apart from variables, in C/C++ and other languages
- **Minimap** (with a clear visible-area band), **split view** (`Ctrl/Cmd+\` or "Open to the Side"), **bracket pair colors**, **current line highlight** and optional **whitespace markers** (spaces as `·`, tabs as `→`)
- **Gutter change markers**: added, modified and deleted lines compared with the last commit (CRLF files too)
- **Inline blame**: a dimmed note at the end of the current line saying who changed it, when, and in which commit
- **Find and replace** in the file (`Ctrl/Cmd+F`, `Ctrl/Cmd+H`) with match case / whole word / regex, all matches highlighted
- **Format document** (`Shift+Alt+F`) with rustfmt, clang-format, prettier, black or gofmt when installed; **format on save** is optional (off by default)
- **Binary files** open in a **hex view** (offset, hex and ASCII columns, `Ctrl+G` to jump to an offset, copy as hex or text), even when they are gigabytes large. Binary changes in Source Control, and any two files (**Select for Comparison**, then **Compare with Selected**), are compared byte by byte with next/previous difference
- **Markdown preview** to the side (the eye icon or **Open Preview to the Side**), updated as you type and scrolling with the editor
- **Large files**: files of 64 MB or more open in a read-only viewer that indexes them in the background without loading them into memory, with find and go to line
- **Unicode**: Chinese, Japanese, Korean and other scripts render with the system fonts, and input methods (IME) work
- **Tab size**: click "Tab size: N" in the status bar (or the command palette) to set how wide tabs are drawn and how many spaces files without an indentation style of their own indent with
- **Files changed outside edittr**: nothing is reloaded behind your back. A bar offers **Reload**, **Compare** (the disk version against the editor text) or **Keep mine**
- **New file / New folder** (`Ctrl/Cmd+N`, the buttons in the Explorer header, or right-click a folder): type a name, subfolders included (`notes/todo.md`); the file is created and opened, and an existing file is never overwritten. With no folder open, New file asks where to save
- **Start page**: edittr opens on a start page with your recent folders; the ✕ in the Explorer header (or **Close folder**) goes back to it
- **Session restore**: reopens each folder's tabs, cursor positions and layout
- **Multiple windows**: **New window** (`Ctrl/Cmd+Shift+N`, the start page, or right-click the folder icon in the activity bar) opens another edittr window, so several projects can be open at once; **Open folder in new window…** and right-clicking a recent folder open a project directly in a new window

### Navigation

- **Command palette** (`Ctrl/Cmd+Shift+P` or `F1`): every command and setting, fuzzy searched
- **Go to file** (`Ctrl/Cmd+P`): searches the files git tracks (or all files outside a repo), closest match first: the exact file name, then names starting with or containing what you typed, then looser matches. Add a line or range to jump there: `main.c:183`, `main.c:183:5`, `main.c:183-190` (also with `–` or `—`), or just `:183` for the current file
- **Go to symbol** in the file (`Ctrl/Cmd+Shift+O`) or the workspace (`Ctrl/Cmd+T`)
- **Outline** below the Explorer, following the cursor: symbols grouped into collapsible **Macros**, **Constants**, **Variables**, **Types** and **Functions** (collapsed groups are remembered), or in source order. For C/C++ it lists macros with their values, global and `static` variables, and functions
- **Go to definition** (`Ctrl/Cmd+click` or `F12`; go back with `Alt+Left` (macOS `Ctrl+-`) and forward with `Alt+Right` (macOS `Ctrl+Shift+-`), or the mouse back/forward buttons): uses a language server when available, otherwise a built-in text search that understands common definition syntax (`fn`, `def`, `class`, `const`, `#define`, …)
- **Find all references** (`Shift+F12`), **hover info**, and **autocomplete** (as you type or `Ctrl+Space`) from the language server
- **Rename symbol** (`F2`): every use across the project, previewed line by line before it's applied; open files change in the editor (one undo step), the others on disk. Without a language server it renames whole words in the current file
- **Quick fixes and code actions** (`Ctrl/Cmd+.`, or right-click): the language server's fixes for the diagnostics on the current line (a missing `;`, a missing `#include`, …) and its refactorings
- **Parameter hints** while typing a call's arguments (or `Ctrl/Cmd+Shift+Space`), with the current parameter underlined; `Alt+Up/Down` switches between overloads
- **Call hierarchy** (`Shift+Alt+H`): who calls a function and what it calls, expanded level by level
- The editor's **right-click menu** has these too: go to definition, find references, call hierarchy, rename, quick fix and file history
- **Diagnostics**: errors and warnings as squiggles, in a Problems panel and as counts in the status bar
- **Switch between C/C++ source and header** (`Alt+O`)

### Search

- **Search in files** (`Ctrl/Cmd+Shift+F`, which searches the selected text right away): press **Enter** to search the whole folder in the background, open files first, with match case / whole word / regex toggles; results are grouped by file and clicking one opens the file with the match selected. The matches are also highlighted in the open file (blue, apart from the file's own Find matches in amber). Respects `.gitignore` and skips binary files and files over 4 MB
- **Find in file** (`Ctrl/Cmd+F`) starts with the selected text
- **Include / exclude filters**: comma-separated globs such as `*.c,*.h` or `build/**,*.min.js`, saved per folder
- **Replace in files**, previewed before anything is written
- **Ignored folders** per workspace: right-click a folder (in the Explorer or search results) and choose **Ignore folder**. Ignored folders are skipped by search and by the language servers, so their definitions never show up. In the Search panel's **Ignored** list each folder can be switched off (kept in the list) or removed with ✕

### Source control

- **Git and Subversion**, detected automatically. Large working copies stay fast: tested with 20,000 files and 8,000 pending changes
- Staged and unstaged changes as a list or a tree (remembered), stage/unstage/discard per file, per folder (with a confirmation) or all, commit / amend, switch branches, fetch / pull (fast-forward only) / push
- **Diffs** side by side or inline. For working-copy changes the right side is the real editor: edit, undo and save right there, with the changes recomputed as you type. **Ignore line endings** (on by default) hides CRLF/LF-only differences
- **Graph and history**: branch lanes, merges, branch and tag labels in their lane's color. Selecting a commit shows its changed files as a tree with each file's diff (side by side or inline). Right-click a commit or a branch/tag label to check out, create a branch or tag, reset (soft/mixed/hard), cherry-pick, revert, merge, delete a branch, or copy the hash, message, author or ref name. For Subversion, the log and blame are cached so the server isn't asked again, and history times are shown in local time
- **File history**: the commits that changed a file (following renames), or only the selected lines (`git log -L`); pick one to see its change side by side. From the Explorer, tab or editor right-click menu, or the command palette
- A **background watcher** keeps the Explorer colors and Source Control view up to date when files or the repository change outside the editor

Git runs through your installed `git` (and Subversion through `svn`), so your config, credential helpers, SSH keys and hooks all apply.

### Status bar

Shows the language of the current file, the cursor position, problem counts, and what is running in the background (search, indexing, language server startup, git).

### Settings

The gear in the activity bar (or `Ctrl/Cmd+,`) opens the **Settings** page: every option grouped by area, with a search box, switches, the tab size, the theme, fonts and the C/C++ build settings. Changes apply and save immediately. Each option is also in the command palette.

- **Themes**: built-in color themes, mostly dark (edittr Dark, Tidal Glass, Kiln Ash, Plum Circuit, Fern Relay, Graphite Signal; edittr Light and Oat Paper), with a live preview. The **theme editor** has a color for each part (background, selection, current line, line numbers, comments, keywords, strings, numbers, constants, macros, types, functions, variables, operators, disabled code and bracket depths); edits are kept as your custom theme and **Reset to preset** undoes them. Syntax and language-server colors both follow the theme, in the editor and in diffs
- **Fonts**: the **editor font** (code, line numbers, diffs) and the **UI font** (menus, panels, dialogs) can each be any installed font, with their own size; monospace fonts are listed first for the editor. **Default** goes back to the bundled fonts

| Setting | Default |
| --- | --- |
| Highlight current line | on |
| Show whitespace | off |
| Inline blame | on |
| Show minimap | on |
| Color nested brackets | on |
| Semantic highlighting (colors from the language server) | on |
| Sticky scroll | on |
| Show outline | on |
| Restore previous session (each folder's tabs) | on |
| Reopen last folder at startup (instead of the start page) | off |
| Format on save | off |
| Source Control: view as tree | off |
| Diff: ignore line endings | on |
| Outline: grouped by kind | on |
| Tab size | 4 |
| Theme | System (edittr Dark / edittr Light) |
| Editor font, UI font | Default (bundled), 13 pt |

## Language servers

edittr uses the standard Language Server Protocol, like VS Code and Zed. It uses a server on your `PATH` first, then one bundled in a **full** download (see Install), and otherwise, the first time you go to a definition, offers to install it (it always asks first):

| Language | Server | Installed from |
| --- | --- | --- |
| Rust | rust-analyzer | official GitHub release (SHA-256 verified) |
| C / C++ | clangd | official GitHub release (SHA-256 verified) |
| TypeScript / JavaScript | typescript-language-server | npm (needs Node.js) |
| Python | pyright | npm (needs Node.js) |
| Go | gopls | `go install` (needs Go) |

Run `edittr --list-servers` to see which server each language uses and where it comes from, and `edittr --check-servers` to test each one with a real go to definition (`--bundled` tests only the servers in a full download).

Servers start the first time you open a file of their language, not when you open a folder.

Servers edittr installs go into its own folder (`~/Library/Application Support/edittr/servers` on macOS, `~/.local/share/edittr/servers` on Linux, `%LOCALAPPDATA%\edittr\servers` on Windows; override it with `EDITTR_SERVERS_DIR`). edittr also starts servers it finds on `PATH` for Java (jdtls), Ruby (ruby-lsp), PHP (intelephense), Lua, Zig and C#.

### C and C++ compile flags

clangd needs each file's compile flags. edittr finds them in this order, and the status bar shows which one is in use (click it for the C/C++ settings):

1. **The project's `compile_commands.json`**: the workspace root, `build*/`, `out/`, `cmake-build-*/`, or the `CompileFlags.CompilationDatabase` path in `.clangd`. clangd gets a filtered copy in the OS cache directory under `edittr/clangd/`, without the ignored folders.
2. **A generated database**: **C/C++: Generate compile database** (command palette or the settings dialog) runs a CMake or Meson configure, your build under [bear](https://github.com/rizsotto/Bear), or a `make -n` dry run, after showing you the exact command. Output streams live and can be cancelled; the result goes into the cache, never into the project.
3. **clangd's own discovery**: `compile_flags.txt`, nested `compile_commands.json` files and `.clangd` settings.
4. **Inferred from the Makefiles**: edittr reads the Makefile and the `.mk` files it includes (without running anything) for `-I`, `-D`, `-std` and friends, adds include folders found from your `#include` lines, and writes a database so clangd can index the whole project.

**Embedded and automotive compilers**: commands for compilers clangd can't run (IAR, TI, Keil armcc and C51/C166, TASKING, Green Hills, Wind River Diab, Renesas CC-RH/RL/RX, Cosmic, SDCC, CodeWarrior) are converted to clang commands. Their include paths, defines and pre-includes are kept, their predefined macros and keyword extensions (`__no_init`, `__interrupt`, `__ramfunc`, ...) are defined, and their own C library headers are found from the install folder. For GCC cross compilers such as `arm-none-eabi-gcc`, clangd gets `--query-driver` so it finds their system headers.

The C/C++ settings (per folder) add extra flags, set the compiler and set the build command used for generating. edittr restarts clangd when the database, `.clangd` or the ignore list changes, or when a Makefile changes and the flags were inferred from it. A generated database is marked out of date instead. clangd stores its persistent index alongside the database it uses.

Rust-analyzer warms its analysis caches and uses a separate Cargo target subdirectory. gopls manages its own persistent cache. edittr also caches regex-based symbols from saved workspace files under `edittr/symbols/` in the OS cache directory, up to 16 MiB per workspace.

## Install

Download from the [Releases](https://github.com/term-guy/edittr/releases) page. Every platform comes in two flavors:

- **Light** (about 5–10 MB): just the editor. Language servers are installed on demand, only for the languages you use.
- **Full** (`-full` in the file name, about 100–220 MB): also bundles rust-analyzer, clangd, gopls, the TypeScript and Python servers and a Node.js runtime for the last two, so go to definition works offline from the first launch. clangd has no arm64 Linux build, so the Linux aarch64 full download leaves it out.

Files:

- **Windows**: `…-setup.exe` / `…-full-setup.exe` (installer: Start menu, optional "Open with edittr" right-click menu and PATH entry; no admin rights needed; installing one flavor over the other switches in place), `…-portable.exe` (light only) or `…-portable.zip` / `…-full-portable.zip` (no install)
- **macOS**: `…-macos-universal.dmg` / `…-macos-universal-full.dmg` (Apple Silicon and Intel). The app isn't notarized yet, so the first time, right-click the app, choose **Open**, then confirm
- **Linux**: `…-linux-x86_64.deb` / `…-linux-aarch64.deb` (Debian/Ubuntu, light) or `.tar.gz` / `-full.tar.gz` (any distro; run `./install.sh` to install into `~/.local`), for x86_64 and aarch64

Windows may show a SmartScreen warning because the binaries aren't code-signed yet.

## Releasing

Push a version tag and GitHub Actions builds and publishes everything:

```sh
git tag v0.1.0
git push origin v0.1.0
```

The version comes from the tag. Before publishing, the workflow installs or unpacks every full package on fresh Windows, macOS and Linux machines and checks each bundled language server with `edittr --check-servers --bundled`. Windows and Linux are published as soon as theirs pass; the macOS build takes longest and is added to the same release when it's done. Running the `release` workflow by hand, or pushing the `release-check` branch, builds and checks all packages without publishing a release. The app icon is generated by `packaging/make_icon.py`.

## Build and run

```sh
cargo run --release -- [folder-or-file]
```

On Linux you need the usual windowing dev packages (see `.github/workflows/build.yml`).

## Shortcuts

`Ctrl` on Windows and Linux, `Cmd` on macOS.

| Key | Action |
| --- | --- |
| `Ctrl/Cmd+Shift+P`, `F1` | Command palette |
| `Ctrl/Cmd+,` | Settings |
| `Ctrl/Cmd+Shift+N` | New window |
| `Ctrl/Cmd+N` | New file |
| `Ctrl/Cmd+P` | Go to file (`name:line`, `name:line-line`) |
| `Ctrl+G` | Go to line (`line` or `line:column`) |
| `Ctrl/Cmd+Shift+O` | Go to symbol in file |
| `Ctrl/Cmd+T` | Go to symbol in workspace |
| `Ctrl/Cmd+F` | Find in file (the selection, if any) |
| `Ctrl/Cmd+H`, `Ctrl/Cmd+Alt+F` | Replace in file |
| `Ctrl/Cmd+Shift+F` | Search in files (the selection, if any); `Enter` runs a search |
| `Ctrl/Cmd+click`, `F12` | Go to definition |
| `Shift+F12` | Find all references |
| `F2` | Rename symbol |
| `Ctrl/Cmd+.` | Quick fix / code actions |
| `Shift+Alt+H` | Call hierarchy |
| `Ctrl+Space` | Autocomplete |
| `Ctrl/Cmd+Shift+Space` | Parameter hints |
| `Alt+O` | Switch between C/C++ source and header |
| `Alt+Left` (macOS: `Ctrl+-`), mouse back button | Go back |
| `Alt+Right` (macOS: `Ctrl+Shift+-`), mouse forward button | Go forward |
| `Alt+click` | Add a cursor |
| `Ctrl/Cmd+D` | Select the next occurrence |
| Middle-click drag | Column selection |
| `Ctrl/Cmd+/` | Toggle comment |
| `Ctrl/Cmd+Alt+[` / `]` | Fold / unfold |
| `Ctrl/Cmd+\` | Split editor |
| `Shift+Alt+F` | Format document |
| `Ctrl/Cmd+S` | Save |
| `Ctrl/Cmd+W` | Close tab |
| `Ctrl/Cmd+Enter` | Commit (in the commit box) |
| `Ctrl/Cmd + / -` | Zoom |

Click the active sidebar icon to hide the sidebar.

## License

MIT, see [LICENSE](LICENSE). Icons come from [Phosphor](https://phosphoricons.com) (MIT, see `assets/PHOSPHOR-LICENSE`).
