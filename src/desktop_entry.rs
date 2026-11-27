//! Linux desktop integration: a per-user `edittr.desktop` (app launcher, file managers' "Open with") and the icon,
//! for this copy of edittr. Wayland compositors take a window's icon from the desktop file named after its app id,
//! so without one a dev build or an extracted tar.gz shows a generic icon in the taskbar.
//!
//! The Settings switch writes `~/.local/share/applications/edittr.desktop` (with `Exec` set to this exe) and
//! `~/.local/share/icons/hicolor/256x256/apps/edittr.png`, the same names as the .deb and `install.sh`, so they
//! replace each other. Like the Windows Explorer menu ([`crate::shell_menu`]), the entry records the version that
//! wrote it, and at startup a newer copy takes it over ([`follow_newest`]), as does any copy when the registered
//! exe is gone. Nothing is added unless the user turned the switch on.

use std::path::{Path, PathBuf};

/// `packaging/linux/edittr.desktop`, shipped by the .deb and the tar.gz; only `Exec` and the version differ.
const TEMPLATE: &str = include_str!("../packaging/linux/edittr.desktop");
const ICON: &[u8] = include_bytes!("../assets/icon-256.png");
const FILE_NAME: &str = "edittr.desktop";
const VERSION_KEY: &str = "X-Edittr-Version";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum State {
    Off,
    /// The user's entry opens this exe.
    This,
    /// The user's entry opens another copy (its exe as written in `Exec`).
    Other(String),
    /// Only installed system-wide (the .deb); changing it needs the package manager.
    System,
}

fn data_home() -> Option<PathBuf> {
    dirs::data_dir()
}

fn entry_path(data: &Path) -> PathBuf {
    data.join("applications").join(FILE_NAME)
}

fn icon_path(data: &Path) -> PathBuf {
    data.join("icons/hicolor/256x256/apps/edittr.png")
}

/// `$XDG_DATA_DIRS` (default `/usr/local/share:/usr/share`).
fn system_data_dirs() -> Vec<PathBuf> {
    let dirs = std::env::var("XDG_DATA_DIRS").ok().filter(|dirs| !dirs.is_empty());
    dirs.as_deref()
        .unwrap_or("/usr/local/share:/usr/share")
        .split(':')
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
        .collect()
}

fn exe() -> std::io::Result<PathBuf> {
    std::env::current_exe().map(|exe| crate::workspace::plain_path(&exe))
}

/// The program of an `Exec` value, quoted per the Desktop Entry spec: reserved characters inside double quotes,
/// escaped with a backslash, and each backslash doubled once more by the string escaping of the file.
fn quote_exec(exe: &Path) -> String {
    let text = exe.to_string_lossy().replace('%', "%%");
    let plain = text.chars().all(|c| c.is_ascii_alphanumeric() || "/._+-,:@%".contains(c));
    if plain {
        return text;
    }
    let mut quoted = String::from("\"");
    for c in text.chars() {
        if matches!(c, '"' | '`' | '$' | '\\') {
            quoted.push('\\');
        }
        quoted.push(c);
    }
    quoted.push('"');
    quoted.replace('\\', "\\\\")
}

/// The program of an `Exec` value (the inverse of [`quote_exec`]).
fn exec_program(exec: &str) -> Option<String> {
    // Undo the file's string escaping first.
    let mut unescaped = String::new();
    let mut chars = exec.trim().chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            unescaped.push(c);
            continue;
        }
        match chars.next() {
            Some('s') => unescaped.push(' '),
            Some('n') => unescaped.push('\n'),
            Some('t') => unescaped.push('\t'),
            Some('r') => unescaped.push('\r'),
            Some(other) => unescaped.push(other),
            None => {}
        }
    }
    let mut program = String::new();
    let mut chars = unescaped.chars();
    if unescaped.starts_with('"') {
        chars.next();
        loop {
            match chars.next()? {
                '"' => break,
                '\\' => program.push(chars.next()?),
                c => program.push(c),
            }
        }
    } else {
        program.extend(chars.take_while(|c| !c.is_whitespace()));
    }
    let program = program.replace("%%", "%");
    (!program.is_empty()).then_some(program)
}

/// A `key=value` of the `[Desktop Entry]` group.
fn value<'a>(entry: &'a str, key: &str) -> Option<&'a str> {
    let mut in_group = false;
    for line in entry.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_group = line == "[Desktop Entry]";
        } else if in_group
            && let Some((k, v)) = line.split_once('=')
            && k.trim() == key
        {
            return Some(v.trim());
        }
    }
    None
}

/// The desktop file for `exe`: the packaged one with its `Exec` pointing there and the version recorded.
fn contents(exe: &Path, version: &str) -> String {
    let mut out = String::new();
    for line in TEMPLATE.lines() {
        match line.strip_prefix("Exec=") {
            Some(rest) => {
                let args = rest.split_once(' ').map_or("", |(_, args)| args);
                out.push_str(format!("Exec={} {args}", quote_exec(exe)).trim_end());
            }
            None => out.push_str(line),
        }
        out.push('\n');
    }
    out.push_str(&format!("{VERSION_KEY}={version}\n"));
    out
}

/// A program name without a slash, as the launcher finds it on PATH.
fn resolve(program: &str) -> Option<PathBuf> {
    if program.contains('/') {
        return Some(PathBuf::from(program));
    }
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(program))
        .find(|path| path.is_file())
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

fn classify(exe: &Path, user_entry: Option<&str>, system: bool) -> State {
    let Some(entry) = user_entry else {
        return if system { State::System } else { State::Off };
    };
    match value(entry, "Exec").and_then(exec_program) {
        Some(program) if resolve(&program).is_some_and(|path| same_file(&path, exe)) => State::This,
        Some(program) => State::Other(program),
        None => State::Other(String::new()),
    }
}

pub(crate) fn state() -> State {
    let (Ok(exe), Some(data)) = (exe(), data_home()) else {
        return State::Off;
    };
    let user = std::fs::read_to_string(entry_path(&data)).ok();
    let system = system_data_dirs().iter().any(|dir| entry_path(dir).is_file());
    classify(&exe, user.as_deref(), system)
}

/// Write the user's desktop file and icon, pointing at this exe (replacing another copy's).
pub(crate) fn register() -> std::io::Result<()> {
    let data = data_home().ok_or_else(|| std::io::Error::other("no home folder"))?;
    write(&entry_path(&data), contents(&exe()?, env!("CARGO_PKG_VERSION")).as_bytes())?;
    write(&icon_path(&data), ICON)?;
    refresh_caches(&data);
    Ok(())
}

/// Remove the user's desktop file and icon (a system-wide install's stay).
pub(crate) fn unregister() -> std::io::Result<()> {
    let data = data_home().ok_or_else(|| std::io::Error::other("no home folder"))?;
    for path in [entry_path(&data), icon_path(&data)] {
        match std::fs::remove_file(&path) {
            Err(err) if err.kind() != std::io::ErrorKind::NotFound => return Err(err),
            _ => {}
        }
    }
    refresh_caches(&data);
    Ok(())
}

/// Write through a temporary file so a launcher never reads half a desktop file.
fn write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("edittr-tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

/// Best effort, in the background: desktops that don't watch the folders pick the change up from these caches.
fn refresh_caches(data: &Path) {
    let applications = data.join("applications");
    let icons = data.join("icons/hicolor");
    std::thread::spawn(move || {
        let run = |program: &str, args: &[&std::ffi::OsStr]| {
            let _ = std::process::Command::new(program)
                .args(args)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        };
        run("update-desktop-database", &[applications.as_os_str()]);
        run("gtk-update-icon-cache", &["-q".as_ref(), "-t".as_ref(), icons.as_os_str()]);
        if std::env::var("XDG_CURRENT_DESKTOP").is_ok_and(|desktop| desktop.contains("KDE")) {
            run("kbuildsycoca6", &[]);
        }
    });
}

/// At startup (background thread): point the user's entry at this copy if it is newer than the one it opens, or
/// that one is gone. Entries this switch didn't write (no version: `install.sh`) are only replaced when their exe
/// is gone.
pub(crate) fn follow_newest() {
    let Some(data) = data_home() else { return };
    let Ok(entry) = std::fs::read_to_string(entry_path(&data)) else {
        return;
    };
    let Ok(exe) = exe() else { return };
    let recorded = value(&entry, VERSION_KEY);
    let ours = env!("CARGO_PKG_VERSION");
    let result = match classify(&exe, Some(&entry), false) {
        State::This if recorded.is_some_and(|recorded| recorded != ours) => register(),
        State::Other(program) => {
            let gone = resolve(&program).is_none_or(|path| !path.is_file());
            let newer = recorded.is_some_and(|recorded| version_key(ours) > version_key(recorded));
            if gone || newer { register() } else { Ok(()) }
        }
        _ => Ok(()),
    };
    if let Err(err) = result {
        eprintln!("edittr: couldn't update the desktop entry: {err}");
    }
}

/// `1.2.3` / `1.2.3-dev` as comparable numbers.
fn version_key(version: &str) -> Vec<u64> {
    let numeric = version.split(['-', '+']).next().unwrap_or_default();
    numeric.split('.').map(|part| part.trim().parse().unwrap_or(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exec_quoting_round_trips() {
        for path in [
            "/home/me/edittr/target/release/edittr",
            "/opt/my apps/edittr",
            "/tmp/we\"ird$`\\name/edittr",
            "/tmp/100%/edittr",
        ] {
            let quoted = quote_exec(Path::new(path));
            assert_eq!(exec_program(&format!("{quoted} %F")).as_deref(), Some(path), "{quoted}");
        }
        assert_eq!(quote_exec(Path::new("/opt/my apps/edittr")), "\"/opt/my apps/edittr\"");
        assert_eq!(quote_exec(Path::new("/a/b\\c")), r#""/a/b\\\\c""#);
        assert_eq!(exec_program("edittr %F").as_deref(), Some("edittr"));
    }

    #[test]
    fn contents_keep_the_packaged_entry() {
        let entry = contents(Path::new("/opt/my apps/edittr"), "0.9.0");
        assert_eq!(value(&entry, "Exec"), Some("\"/opt/my apps/edittr\" %F"));
        assert_eq!(value(&entry, "Icon"), Some("edittr"));
        assert_eq!(value(&entry, "StartupWMClass"), Some("edittr"));
        assert_eq!(value(&entry, VERSION_KEY), Some("0.9.0"));
        assert_eq!(entry.lines().filter(|line| line.starts_with("Exec=")).count(), 1);
    }

    #[test]
    fn classify_tells_this_copy_from_others() {
        let exe = Path::new("/nonexistent/a/edittr");
        let ours = contents(exe, "1.0.0");
        assert_eq!(classify(exe, Some(&ours), false), State::This);
        let other = contents(Path::new("/nonexistent/b/edittr"), "1.0.0");
        assert_eq!(classify(exe, Some(&other), true), State::Other("/nonexistent/b/edittr".into()));
        assert_eq!(classify(exe, None, true), State::System);
        assert_eq!(classify(exe, None, false), State::Off);
    }

    #[test]
    fn versions_compare_numerically() {
        assert!(version_key("0.7.10") > version_key("0.7.9"));
        assert!(version_key("0.7.2") == version_key("0.7.2-dev"));
    }
}
