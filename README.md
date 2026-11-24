# edittr

A small, fast code editor with built-in git. It's a single native binary written in Rust with [egui](https://github.com/emilk/egui) (no Electron, no webview) and runs on macOS, Linux and Windows.

## Features

### Editor

- **Fast on any file size**: a custom editor widget that only lays out the visible lines, with incremental syntax highlighting for most languages, so typing stays smooth in files with tens of thousands of lines
- **Multiple cursors**: `Alt+click` adds a cursor, `Ctrl/Cmd+D` selects the next occurrence, and middle-click drag makes a column selection; typing edits every line at once
- **Editing helpers**: smart indentation (detected from the file or `.editorconfig`), `Tab` / `Shift+Tab` indent, `Ctrl/Cmd+/` toggles comments, undo/redo grouped by typing pauses
- **Code folding** by indentation (`Ctrl/Cmd+Alt+[` / `]`, or the gutter arrows)
- **Minimap**, **split view** (`Ctrl/Cmd+\` or "Open to the Side"), **bracket pair colors**, **current line highlight** and optional **whitespace markers** (spaces as `·`, tabs as `→`)
- **Gutter change markers**: added, modified and deleted lines compared with the last commit
- **Inline blame**: a dimmed note at the end of the current line saying who changed it, when, and in which commit
- **Find and replace** in the file (`Ctrl/Cmd+F`, `Ctrl/Cmd+H`) with match case / whole word / regex, all matches highlighted
- **Format document** (`Shift+Alt+F`) with rustfmt, clang-format, prettier, black or gofmt when installed; **format on save** is optional (off by default)
- **Large files**: files of 64 MB or more open in a read-only viewer that indexes them in the background without loading them into memory, with find and go to line
- **Unicode**: Chinese, Japanese, Korean and other scripts render with the system fonts, and input methods (IME) work
- **Session restore**: reopens each folder's tabs, cursor positions and layout

### Navigation

- **Command palette** (`Ctrl/Cmd+Shift+P` or `F1`): every command and setting, fuzzy searched
- **Go to file** (`Ctrl/Cmd+P`): fuzzy search over files git tracks (or all files outside a repo)
- **Go to symbol** in the file (`Ctrl/Cmd+Shift+O`) or the workspace (`Ctrl/Cmd+T`)
- **Outline** below the Explorer: functions, types, constants etc. of the current file, following the cursor
- **Go to definition** (`Ctrl/Cmd+click` or `F12`, go back with `Alt+Left` / `Ctrl+-` on macOS): uses a language server when available, otherwise a built-in text search that understands common definition syntax (`fn`, `def`, `class`, `const`, `#define`, …)
- **Find all references** (`Shift+F12`), **hover info**, and **autocomplete** (as you type or `Ctrl+Space`) from the language server
- **Diagnostics**: errors and warnings as squiggles, in a Problems panel and as counts in the status bar
- **Switch between C/C++ source and header** (`Alt+O`)

### Search

- **Search in files** (`Ctrl/Cmd+Shift+F`): searches the whole folder in the background as you type, open files first, with match case / whole word / regex toggles; results are grouped by file and clicking one opens the file with the match selected. Respects `.gitignore` and skips binary files and files over 4 MB
- **Include / exclude filters**: comma-separated globs such as `*.c,*.h` or `build/**,*.min.js`
- **Replace in files**, previewed before anything is written
- **Ignored folders** per workspace: right-click a folder (in the Explorer or search results) and choose **Ignore folder**. Ignored folders are skipped by search and by the language servers, so their definitions never show up

### Source control

- **Git and Subversion**, detected automatically
- Staged and unstaged changes as a list or a tree (remembered), per-file diffs, stage/unstage/discard (single file or all), commit / amend, switch branches, fetch / pull (fast-forward only) / push
- **Graph and history**: branch lanes, merges, branch and tag badges; click a commit to see its stats and patch. For Subversion, the log and blame are cached so the server isn't asked again
- A **background watcher** keeps the Explorer colors and Source Control view up to date when files or the repository change outside the editor

Git runs through your installed `git` (and Subversion through `svn`), so your config, credential helpers, SSH keys and hooks all apply.

### Status bar

Shows the language of the current file, the cursor position, problem counts, and what is running in the background (search, indexing, language server startup, git).

### Settings

Toggle these from the command palette; they are saved in your user settings.

| Setting | Default |
| --- | --- |
| Highlight current line | on |
| Show whitespace | off |
| Inline blame | on |
| Show minimap | on |
| Color nested brackets | on |
| Show outline | on |
| Restore previous session | on |
| Format on save | off |
| Source Control: view as tree | off |

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

When a workspace has a compilation database, edittr gives clangd a filtered copy in the OS cache directory under `edittr/clangd/`. It checks the workspace root, `build*/`, `out/`, `cmake-build-*/`, and a workspace-wide `CompileFlags.CompilationDatabase` path in `.clangd`. Ignored folders are excluded from the copy, and edittr restarts clangd when the database or the workspace ignore list changes. clangd stores its persistent index alongside the database it uses. If clangd discovers a project-local database on its own, edittr hides its `.cache/clangd` directory from Explorer, search and untracked Source Control results.

Rust-analyzer warms its analysis caches and uses a separate Cargo target subdirectory. gopls manages its own persistent cache. edittr also caches regex-based symbols from saved workspace files under `edittr/symbols/` in the OS cache directory, up to 16 MiB per workspace.

## Install

Download from the [Releases](https://github.com/term-guy/edittr/releases) page. Every platform comes in two flavors:

- **Light** (about 5–10 MB): just the editor. Language servers are installed on demand, only for the languages you use.
- **Full** (`-full` in the file name, about 100–220 MB): also bundles rust-analyzer, clangd, gopls, the TypeScript and Python servers and a Node.js runtime for the last two, so go to definition works offline from the first launch. clangd has no arm64 Linux build, so the Linux aarch64 full download leaves it out.

Files:

- **Windows**: `…-setup.exe` / `…-full-setup.exe` (installer: Start menu, optional "Open with edittr" right-click menu and PATH entry; no admin rights needed; installing one flavor over the other switches in place), `…-portable.exe` (light only) or `…-portable.zip` / `…-full-portable.zip` (no install)
- **macOS**: `…-macos-universal.dmg` / `…-macos-universal-full.dmg` (Apple Silicon and Intel). The app isn't notarized yet, so the first time, right-click the app, choose **Open**, then confirm
- **Linux**: `.deb` (Debian/Ubuntu, light) or `.tar.gz` / `-full.tar.gz` (any distro; run `./install.sh` to install into `~/.local`), for x86_64 and aarch64

Windows may show a SmartScreen warning because the binaries aren't code-signed yet.

## Releasing

Push a version tag and GitHub Actions builds and publishes everything:

```sh
git tag v0.1.0
git push origin v0.1.0
```

The version comes from the tag. Before publishing, the workflow installs or unpacks every full package on fresh Windows, macOS and Linux machines and checks each bundled language server with `edittr --check-servers --bundled`. Running the `release` workflow by hand, or pushing the `release-check` branch, builds and checks all packages without publishing a release. The app icon is generated by `packaging/make_icon.py`.

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
| `Ctrl/Cmd+P` | Go to file |
| `Ctrl/Cmd+Shift+O` | Go to symbol in file |
| `Ctrl/Cmd+T` | Go to symbol in workspace |
| `Ctrl/Cmd+F` | Find in file |
| `Ctrl/Cmd+H`, `Ctrl/Cmd+Alt+F` | Replace in file |
| `Ctrl/Cmd+Shift+F` | Search in files |
| `Ctrl/Cmd+click`, `F12` | Go to definition |
| `Shift+F12` | Find all references |
| `Ctrl+Space` | Autocomplete |
| `Alt+O` | Switch between C/C++ source and header |
| `Alt+Left` (macOS: `Ctrl+-`) | Go back |
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
