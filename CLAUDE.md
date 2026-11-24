# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Priorities

- The editor must be as fast and lightweight to use as possible. Binary size is not a concern; UI responsiveness is.
- Never block the UI thread: git, search, language servers and downloads run on background threads and report back over channels polled in `App::poll_background`. Long lists use virtualized `ScrollArea::show_rows`.
- egui/eframe are 0.37 and their API differs from most docs/examples (`App::ui` not `update`, `egui::Panel::left(..).show(ui)`, `egui::Modal`, `Ui::close()`). Check the crate source under `~/.cargo/registry/src/` instead of relying on memory.

## Commands

- Build/test: `cargo build --release`, `cargo test --release`. CI's lint job runs `cargo test --locked` in debug, which enables extra egui/epaint debug asserts: run it too before pushing.
- Real-network / real-server tests are `#[ignore]`d. Run them with a throwaway servers dir, never the real one: `EDITTR_SERVERS_DIR=<tmp dir> cargo test --release -- --ignored` (downloads rust-analyzer and clangd; `lsp_round_trip_c` needs `clangd` on PATH).
- App icon assets are generated: `python3 packaging/make_icon.py` (rewrites `assets/icon*` and `packaging/macos/edittr.icns`).
- Formatting/lint: `rustfmt.toml` sets `max_width = 140` (a hook formats edited `.rs` files). CI fails on `cargo fmt --check` or `cargo clippy --all-targets -- -D warnings`; run both before pushing.

## Local GUI testing (macOS)

- Keep at most ONE edittr window open while testing or taking screenshots, and close it when done (hooks in `.claude/settings.json` enforce this). Never touch copies the user launched themselves.
- `target/release/edittr` with no argument starts an empty project; pass a folder (or a file) to open it.
- `screencapture` usually doesn't show the window (it opens on another Space); don't retry in a loop.
- The tour opens `src/ui/graph.rs` of the demo repo; `EDITTR_TOUR_FILE=<path relative to the demo>` opens another file (e.g. a C file to see clangd hover/completion/diagnostics).
- For screenshots, use the built-in tour instead of `screencapture`: `cargo build --release --features tour && EDITTR_TOUR_OUT=<dir> EDITTR_TOUR_DEMO=<git repo> target/release/edittr` saves one PNG per feature and quits (steps in `src/app/tour.rs`). Rebuild without `--features tour` afterwards.
- Temporary test hooks (env var `EDITTR_TEST`) must be removed before finishing: `grep -n EDITTR_TEST src/`.

## Git and releases

- Commit identity for this repo is `termguy <hello@termg.com>` (local git config); keep it.
- Commit directly on `main`; push only when asked. The remote uses the SSH host alias `github-term-guy`.
- Release = push a `vX.Y.Z` tag. `.github/workflows/release.yml` builds the Windows installer + portable exe, macOS universal dmg/zip and Linux deb/tar.gz, and publishes them; the version comes from the tag (`.github/scripts/version.sh`).
- `gh` is logged in as term-guy: `gh run list`, `gh run view <id> --log-failed`. Pushing the `release-check` branch builds every package and runs `verify` (bundled language servers on fresh Windows/macOS/Linux runners) without publishing.

## Language servers

- Lookup order (`lsp::resolve`): PATH, then a bundle (`src/bundled.rs`: `servers/servers.json` next to the exe, in `Contents/Resources`, or `../lib/edittr`), then on-demand installs (`src/installer.rs`). `edittr --list-servers` prints what each language resolves to.
- Full downloads are assembled by `.github/scripts/bundle-servers.sh <platform> <dest>` (runs on macOS locally too). To test a bundle against the real servers: symlink it to `target/release/deps/servers` and run `cargo test --release -- --ignored lsp_round_trip`.
- clangd flags are resolved in `clangd::CompilationDatabase::prepare` (project database → generated one in the cache → clangd's own compile_flags.txt/nested discovery → inferred from Makefiles by `makeflags.rs`). Commands of compilers clangd can't use (IAR, TI, Keil, ...) are rewritten by `ccompilers::translate`; "Generate compile database" is `compdb.rs`. Real-clangd tests: `cargo test --release -- --ignored lsp_round_trip_clangd`.
- `typescript` must stay pinned to `@6` (installer + bundle script): TypeScript 7 dropped the `tsserver.js` that typescript-language-server needs.

## Gotchas

- The editor is a custom widget (`src/editor/`: `buffer.rs` text model + undo, `fold.rs`, `mod.rs` widget) that lays out only visible lines; don't reintroduce `egui::TextEdit` for code. Its keyboard behavior is covered by headless widget tests in `src/editor/mod.rs` (drive the real widget with injected events). Headless `ctx.run_ui` calls must `textures_delta.clear()` the output, or epaint panics in debug builds.
- Editor text changes must bump `Doc::version` (use `Doc::reload` / `Doc::mark_saved`): highlighting, blame, dirty state and the gutter are cached per version. Syntax highlighting is incremental (`src/highlight.rs`), not egui_extras' whole-buffer highlighter.
- VCS calls go through `src/vcs.rs` (Git or Subversion) on background workers; refreshes are driven by the file watcher (`src/watch.rs`), not a timer. `svn` is not installed on the dev Mac: svn code is only unit-tested against sample XML.
- The release workflow builds macOS on `macos-15-intel` (Apple Silicon runners were often not assigned in time); it still produces the universal app, and the `verify` job runs it on Apple Silicon. A run that fails only because a runner wasn't assigned or `hdiutil` was busy can be re-run with `gh run rerun <id> --failed`.
- To measure frame times, time `App::ui` temporarily and run the tour against a demo repo containing a ~20k-line file; remove the timer afterwards.

- Paths: use `workspace::canonical` / `workspace::plain_path`, never bare `canonicalize()` for paths that reach the UI, tabs or comparisons: on Windows it returns `\\?\C:\...`, which doesn't match the `C:\...` paths from language servers, dialogs and search, so the same file opened twice.
- The editor keeps LF text for CRLF files (`Doc::crlf`); anything that compares the buffer with git/svn content must restore or strip CR (`blame::git_with_endings`, the gutter base, the diff "ignore line endings" option).
- egui matches shortcuts even with extra Shift/Alt held: commands are dispatched most-modifiers-first (`commands::dispatch_order`); don't loop over `COMMANDS` to consume shortcuts.
- eframe is built with `default-features = false`: keep the `clipboard` feature (without it copy/paste silently stays inside the app).
- Every spawned process needs `CREATE_NO_WINDOW` on Windows (see `git::cmd`), or a console window flashes.
- Downloads use ureq with `RootCerts::PlatformVerifier` (OS trust store). Don't switch back to bundled webpki roots: that broke clangd downloads.
- `rfd` is pinned to `=0.18.0` because Linux portal-failure detection in `app.rs` hooks its internal log target.
- egui's default fonts lack most symbols (they render as boxes); use the Phosphor constants in `src/icons.rs`.
