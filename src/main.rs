#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod blame;
mod bundled;
mod commands;
mod diagnostics;
mod diff;
mod editor;
mod editorconfig;
mod formatter;
mod git;
mod goto;
mod graph;
mod highlight;
mod icons;
mod installer;
mod lsp;
mod replace;
mod search;
mod session;
mod settings;
mod svn;
mod symbols;
mod tree;
pub mod vcs;
mod watch;
mod workspace;

use std::path::PathBuf;

fn main() -> eframe::Result {
    // Launched from Finder / a desktop menu (no terminal): fix up PATH.
    #[cfg(unix)]
    if std::env::var_os("TERM").is_none() {
        import_login_shell_path();
    }

    if std::env::args_os().any(|a| a == "--list-servers") {
        list_servers();
        return Ok(());
    }

    // Old macOS versions pass "-psn_…" when launched from Finder.
    let arg = std::env::args_os()
        .skip(1)
        .find(|a| !a.to_string_lossy().starts_with("-psn"))
        .map(PathBuf::from);
    let settings = settings::Settings::load();
    let startup = session::Startup::new(arg, settings.restore_previous_session);

    let mut viewport = eframe::egui::ViewportBuilder::default()
        .with_title("edittr")
        .with_app_id("edittr")
        .with_inner_size([1200.0, 780.0])
        .with_min_inner_size([480.0, 320.0]);
    if let Ok(icon) = eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon-256.png")) {
        viewport = viewport.with_icon(icon);
    }
    let options = eframe::NativeOptions {
        viewport: startup.window.geometry.apply(viewport),
        ..Default::default()
    };
    let (settings_writer, worker) = settings::SettingsWriter::start();
    let (session_writer, session_worker) = session::Writer::start().unzip();
    let result = eframe::run_native(
        "edittr",
        options,
        Box::new(move |cc| Ok(Box::new(app::App::new(cc, startup, settings, settings_writer, session_writer)))),
    );
    // Finish queued writes once the event loop and its app have exited.
    let _ = worker.join();
    if let Some(worker) = session_worker {
        let _ = worker.join();
    }
    result
}

/// GUI-launched apps on macOS/Linux get a minimal PATH, which hides tools like
/// Homebrew's `npm`/`go` or a user-installed language server. Use the PATH the
/// user's login shell reported last time (instant), and ask the shell again in
/// the background for next launch; only the very first launch waits for it.
#[cfg(unix)]
fn import_login_shell_path() {
    let cache = dirs::cache_dir().map(|d| d.join("edittr").join("login-path.txt"));
    let cached = cache
        .as_ref()
        .and_then(|f| std::fs::read_to_string(f).ok())
        .filter(|p| !p.trim().is_empty());
    let path = match cached {
        Some(path) => {
            if let Some(cache) = cache {
                std::thread::spawn(move || {
                    if let Some(fresh) = login_shell_path() {
                        save_login_path(&cache, &fresh);
                    }
                });
            }
            path.trim().to_string()
        }
        None => {
            let Some(fresh) = login_shell_path() else { return };
            if let Some(cache) = cache {
                save_login_path(&cache, &fresh);
            }
            fresh
        }
    };
    // SAFETY: called at startup before any other threads read the environment
    // (the refresh thread above only runs the shell and writes the cache file).
    unsafe { std::env::set_var("PATH", path) };
}

#[cfg(unix)]
fn save_login_path(cache: &std::path::Path, path: &str) {
    if let Some(dir) = cache.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(cache, path);
}

/// The login shell's PATH (waits up to 3 s for the shell).
#[cfg(unix)]
fn login_shell_path() -> Option<String> {
    use std::process::{Command, Stdio};
    use std::time::Duration;
    const MARK: &str = "_EDITTR_PATH_";
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
    let child = Command::new(shell)
        .args(["-ilc", &format!("printf '{MARK}%s{MARK}' \"$PATH\"")])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });
    let out = rx.recv_timeout(Duration::from_secs(3)).ok()?.ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let path = text.split(MARK).nth(1).unwrap_or_default();
    (!path.is_empty()).then(|| path.to_string())
}

/// `edittr --list-servers`: which language server each language would use, and from where.
fn list_servers() {
    for (lang, ext) in [
        ("Rust", "rs"),
        ("C/C++", "c"),
        ("Go", "go"),
        ("TypeScript/JS", "ts"),
        ("Python", "py"),
    ] {
        let found = lsp::servers_for(ext)
            .into_iter()
            .find_map(|s| lsp::resolve(s.command).map(|l| (s.name, l)));
        match found {
            Some((name, l)) => {
                let args = if l.args.is_empty() {
                    String::new()
                } else {
                    format!(" {}", l.args.join(" "))
                };
                println!("{lang:<14} {name:<20} {}{args}", l.exe.display());
            }
            None => println!("{lang:<14} (none: go to definition uses text search; edittr offers to install one)"),
        }
    }
}
