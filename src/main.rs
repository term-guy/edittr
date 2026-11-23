#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod git;
mod goto;
mod graph;
mod icons;
mod installer;
mod lsp;
mod search;

use std::path::PathBuf;

fn main() -> eframe::Result {
    let launched_from_gui = std::env::var_os("TERM").is_none();
    #[cfg(unix)]
    if launched_from_gui {
        import_login_shell_path();
    }

    // Old macOS versions pass "-psn_…" when launched from Finder.
    let arg = std::env::args().skip(1).find(|a| !a.starts_with("-psn")).map(PathBuf::from);
    let (root, file) = match arg {
        Some(p) if p.is_file() => (p.parent().map(PathBuf::from).unwrap_or_else(|| ".".into()), Some(p)),
        Some(p) => (p, None),
        None => {
            let cwd = std::env::current_dir().ok();
            // Apps started from Finder / a desktop menu / the Start menu get "/" or
            // their install folder as working directory: open the home folder instead.
            let exe_dir = std::env::current_exe().ok().and_then(|e| e.parent().map(PathBuf::from));
            let useless = cwd.as_ref().is_none_or(|c| c.parent().is_none() || Some(c) == exe_dir.as_ref())
                || (cfg!(unix) && launched_from_gui);
            let home = dirs::home_dir();
            (if useless { home.or(cwd) } else { cwd }.unwrap_or_else(|| ".".into()), None)
        }
    };
    let root = std::fs::canonicalize(&root).unwrap_or(root);

    let mut viewport = eframe::egui::ViewportBuilder::default()
        .with_title("edittr")
        .with_app_id("edittr")
        .with_inner_size([1200.0, 780.0])
        .with_min_inner_size([480.0, 320.0]);
    if let Ok(icon) = eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon-256.png")) {
        viewport = viewport.with_icon(icon);
    }
    let options = eframe::NativeOptions { viewport, ..Default::default() };
    eframe::run_native(
        "edittr",
        options,
        Box::new(move |cc| Ok(Box::new(app::App::new(cc, root, file)))),
    )
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
    let Ok(Ok(out)) = rx.recv_timeout(Duration::from_secs(3)) else { return };
    let text = String::from_utf8_lossy(&out.stdout);
    let path = text.split(MARK).nth(1).unwrap_or_default();
    if !path.is_empty() {
        // SAFETY: called at startup before any other threads read the environment.
        unsafe { std::env::set_var("PATH", path) };
    }
}
