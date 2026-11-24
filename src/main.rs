#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod bundled;
mod diff;
mod git;
mod goto;
mod graph;
mod icons;
mod installer;
mod lsp;
mod search;
mod settings;
mod tree;
mod workspace;

use std::path::PathBuf;

fn main() -> eframe::Result {
    // Launched from Finder / a desktop menu (no terminal): fix up PATH.
    #[cfg(unix)]
    if std::env::var_os("TERM").is_none() {
        import_login_shell_path();
    }

    if std::env::args().any(|a| a == "--list-servers") {
        list_servers();
        return Ok(());
    }

    // Old macOS versions pass "-psn_…" when launched from Finder.
    let arg = std::env::args().skip(1).find(|a| !a.starts_with("-psn")).map(PathBuf::from);
    // No argument: start with an empty project. A folder argument opens it; a file
    // argument opens just that file.
    let (root, file) = match arg {
        Some(p) if p.is_file() => (None, Some(std::fs::canonicalize(&p).unwrap_or(p))),
        Some(p) if p.is_dir() => (Some(std::fs::canonicalize(&p).unwrap_or(p)), None),
        _ => (None, None),
    };

    let mut viewport = eframe::egui::ViewportBuilder::default()
        .with_title("edittr")
        .with_app_id("edittr")
        .with_inner_size([1200.0, 780.0])
        .with_min_inner_size([480.0, 320.0]);
    if let Ok(icon) = eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon-256.png")) {
        viewport = viewport.with_icon(icon);
    }
    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };
    eframe::run_native("edittr", options, Box::new(move |cc| Ok(Box::new(app::App::new(cc, root, file)))))
}

/// GUI-launched apps on macOS/Linux get a minimal PATH, which hides tools like
/// Homebrew's `npm`/`go` or a user-installed language server. Ask the user's
/// login shell for its PATH (with a timeout) and use that instead.
#[cfg(unix)]
fn import_login_shell_path() {
    use std::process::{Command, Stdio};
    use std::time::Duration;
    const MARK: &str = "_EDITTR_PATH_";
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
    let Ok(child) = Command::new(shell)
        .args(["-ilc", &format!("printf '{MARK}%s{MARK}' \"$PATH\"")])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        return;
    };
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });
    let Ok(Ok(out)) = rx.recv_timeout(Duration::from_secs(3)) else {
        return;
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let path = text.split(MARK).nth(1).unwrap_or_default();
    if !path.is_empty() {
        // SAFETY: called at startup before any other threads read the environment.
        unsafe { std::env::set_var("PATH", path) };
    }
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
