//! The Windows Explorer right-click entries: "Edit with edittr" on files, "Open with edittr" on folders
//! and folder backgrounds. The installer adds them (optional task); the Settings switch adds them for the
//! current user (HKCU, no admin), which is how the portable exe gets them.
//!
//! Several copies (e.g. extracted 0.7.1 and 0.7.2 portables) share one set of entries: each records the
//! version that wrote it, and at startup a newer copy takes the entries over ([`follow_newest`]), as does any
//! copy when the registered exe is gone. Nothing is added unless the user turned the menu on.

use std::path::Path;

/// A registry key under `Software\Classes`, its menu label and the argument Explorer substitutes.
struct Entry {
    key: &'static str,
    label: &'static str,
    arg: &'static str,
}

/// Same keys as `packaging/windows/edittr.iss`, so an install and the switch replace each other.
const ENTRIES: [Entry; 3] = [
    Entry {
        key: r"Software\Classes\*\shell\edittr",
        label: "Edit with edittr",
        arg: "%1",
    },
    Entry {
        key: r"Software\Classes\Directory\shell\edittr",
        label: "Open with edittr",
        arg: "%1",
    },
    Entry {
        key: r"Software\Classes\Directory\Background\shell\edittr",
        label: "Open with edittr",
        arg: "%V",
    },
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum State {
    Off,
    /// Registered for the current user, opening this exe.
    This,
    /// Registered for the current user, opening another copy (e.g. a moved portable exe).
    Other(String),
    /// Only registered for all users (an all-users install); changing it needs that installer.
    AllUsers,
}

/// Value next to each entry's label: the edittr version that registered it.
const VERSION_VALUE: &str = "EdittrVersion";

fn command(exe: &Path, arg: &str) -> String {
    format!("\"{}\" \"{arg}\"", exe.display())
}

/// `(key, value name (None: default), data)` for every value `register` writes.
fn values(exe: &Path, version: &str) -> Vec<(String, Option<&'static str>, String)> {
    let icon = format!("\"{}\"", exe.display());
    ENTRIES
        .iter()
        .flat_map(|entry| {
            [
                (entry.key.to_string(), None, entry.label.to_string()),
                (entry.key.to_string(), Some("Icon"), icon.clone()),
                (entry.key.to_string(), Some(VERSION_VALUE), version.to_string()),
                (format!(r"{}\command", entry.key), None, command(exe, entry.arg)),
            ]
        })
        .collect()
}

/// `1.2.3` / `1.2.3-dev` as comparable numbers (a missing or odd version is the oldest).
fn version_key(version: Option<&str>) -> Vec<u64> {
    let Some(version) = version else { return Vec::new() };
    let numeric = version.split(['-', '+']).next().unwrap_or_default();
    numeric.split('.').map(|part| part.trim().parse().unwrap_or(0)).collect()
}

/// The exe of a registered command line (`"C:\...\edittr.exe" "%1"`).
fn command_exe(command: &str) -> Option<&str> {
    let rest = command.trim_start().strip_prefix('"')?;
    rest.split('"').next()
}

#[derive(Debug, PartialEq, Eq)]
enum Follow {
    Keep,
    /// Ours: only the recorded version is out of date (e.g. updated by the installer).
    RecordVersion,
    TakeOver,
}

/// What a starting copy does with the entries, given their state, the version recorded with them and
/// whether the exe they open still exists.
fn follow(state: &State, recorded: Option<&str>, ours: &str, registered_exe_exists: bool) -> Follow {
    match state {
        State::This if recorded != Some(ours) => Follow::RecordVersion,
        State::Other(_) if !registered_exe_exists || version_key(Some(ours)) > version_key(recorded) => Follow::TakeOver,
        _ => Follow::Keep,
    }
}

/// At startup (background thread): point the user's entries at this copy if it is newer than the one they
/// open, or that one is gone.
pub(crate) fn follow_newest() {
    let state = state();
    let folder_entry = ENTRIES[1].key;
    let recorded = reg::get(reg::Root::User, folder_entry, Some(VERSION_VALUE));
    let exists = match &state {
        State::Other(command) => command_exe(command).is_some_and(|exe| Path::new(exe).is_file()),
        _ => true,
    };
    let ours = env!("CARGO_PKG_VERSION");
    let result = match follow(&state, recorded.as_deref(), ours, exists) {
        Follow::Keep => Ok(()),
        Follow::RecordVersion => ENTRIES.iter().try_for_each(|entry| reg::set(entry.key, Some(VERSION_VALUE), ours)),
        Follow::TakeOver => register(),
    };
    if let Err(err) = result {
        eprintln!("edittr: couldn't update the Explorer menu: {err}");
    }
}

/// Decide the state from the per-user commands (one per entry) and whether all users have the entries.
fn classify(exe: &Path, user_commands: &[Option<String>], all_users: bool) -> State {
    let expected = ENTRIES.iter().map(|entry| command(exe, entry.arg));
    if user_commands
        .iter()
        .zip(expected)
        .all(|(found, expected)| found.as_deref().is_some_and(|found| found.eq_ignore_ascii_case(&expected)))
    {
        return State::This;
    }
    if let Some(other) = user_commands.iter().flatten().next() {
        return State::Other(other.clone());
    }
    if all_users { State::AllUsers } else { State::Off }
}

fn exe() -> std::io::Result<std::path::PathBuf> {
    std::env::current_exe().map(|exe| crate::workspace::plain_path(&exe))
}

pub(crate) fn state() -> State {
    let Ok(exe) = exe() else { return State::Off };
    let user: Vec<_> = ENTRIES
        .iter()
        .map(|entry| reg::get(reg::Root::User, &format!(r"{}\command", entry.key), None))
        .collect();
    let all_users = ENTRIES
        .iter()
        .any(|entry| reg::get(reg::Root::Machine, &format!(r"{}\command", entry.key), None).is_some());
    classify(&exe, &user, all_users)
}

/// Add the entries for the current user, pointing at this exe (replacing another copy's).
pub(crate) fn register() -> std::io::Result<()> {
    for (key, name, data) in values(&exe()?, env!("CARGO_PKG_VERSION")) {
        reg::set(&key, name, &data)?;
    }
    Ok(())
}

/// Remove the current user's entries (an all-users install's stay).
pub(crate) fn unregister() -> std::io::Result<()> {
    for entry in &ENTRIES {
        reg::delete_tree(entry.key)?;
    }
    Ok(())
}

#[cfg(windows)]
mod reg {
    use std::io;
    use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, WIN32_ERROR};
    use windows_sys::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, REG_SZ, RRF_RT_REG_SZ, RegDeleteTreeW, RegGetValueW, RegSetKeyValueW,
    };

    pub(super) enum Root {
        User,
        Machine,
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(Some(0)).collect()
    }

    fn check(code: WIN32_ERROR) -> io::Result<()> {
        if code == ERROR_SUCCESS {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(code as i32))
        }
    }

    /// A string value of `key` (`None`: its default value).
    pub(super) fn get(root: Root, key: &str, name: Option<&str>) -> Option<String> {
        let root: HKEY = match root {
            Root::User => HKEY_CURRENT_USER,
            Root::Machine => HKEY_LOCAL_MACHINE,
        };
        let key = wide(key);
        let name = name.map(wide);
        let mut buf = vec![0u16; 2048];
        let mut bytes = (buf.len() * 2) as u32;
        // SAFETY: `key` and `name` are NUL-terminated; `buf` holds `bytes` bytes, which the call updates to the size written.
        let code = unsafe {
            RegGetValueW(
                root,
                key.as_ptr(),
                name.as_ref().map_or(std::ptr::null(), |name| name.as_ptr()),
                RRF_RT_REG_SZ,
                std::ptr::null_mut(),
                buf.as_mut_ptr().cast(),
                &mut bytes,
            )
        };
        if code != ERROR_SUCCESS {
            return None;
        }
        let len = (bytes as usize / 2).min(buf.len());
        let text = &buf[..len];
        let end = text.iter().position(|&c| c == 0).unwrap_or(text.len());
        Some(String::from_utf16_lossy(&text[..end]))
    }

    /// Set a string value under HKCU, creating the key.
    pub(super) fn set(key: &str, name: Option<&str>, data: &str) -> io::Result<()> {
        let key = wide(key);
        let name = name.map(wide);
        let data = wide(data);
        // SAFETY: all strings are NUL-terminated and `data` is `data.len() * 2` bytes long.
        check(unsafe {
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                key.as_ptr(),
                name.as_ref().map_or(std::ptr::null(), |name| name.as_ptr()),
                REG_SZ,
                data.as_ptr().cast(),
                (data.len() * 2) as u32,
            )
        })
    }

    /// Delete an HKCU key and its subkeys (a missing key is fine).
    pub(super) fn delete_tree(key: &str) -> io::Result<()> {
        let key = wide(key);
        // SAFETY: `key` is NUL-terminated.
        match unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, key.as_ptr()) } {
            ERROR_FILE_NOT_FOUND => Ok(()),
            code => check(code),
        }
    }
}

#[cfg(not(windows))]
mod reg {
    use std::io;

    pub(super) enum Root {
        User,
        Machine,
    }

    pub(super) fn get(_: Root, _: &str, _: Option<&str>) -> Option<String> {
        None
    }

    pub(super) fn set(_: &str, _: Option<&str>, _: &str) -> io::Result<()> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "the Explorer menu is Windows only"))
    }

    pub(super) fn delete_tree(_: &str) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_quote_the_exe_and_match_the_installer() {
        let exe = Path::new(r"C:\Tools\edittr portable\edittr.exe");
        let values = values(exe, "0.7.2");
        assert_eq!(values.len(), 12);
        assert!(values.contains(&(
            r"Software\Classes\Directory\shell\edittr".into(),
            Some(VERSION_VALUE),
            "0.7.2".into()
        )));
        assert!(values.contains(&(
            r"Software\Classes\Directory\Background\shell\edittr\command".into(),
            None,
            r#""C:\Tools\edittr portable\edittr.exe" "%V""#.into()
        )));
        assert!(values.contains(&(
            r"Software\Classes\*\shell\edittr".into(),
            Some("Icon"),
            r#""C:\Tools\edittr portable\edittr.exe""#.into()
        )));
        let iss = include_str!("../packaging/windows/edittr.iss");
        for entry in &ENTRIES {
            assert!(iss.contains(entry.key), "{} missing from the installer", entry.key);
            assert!(iss.contains(entry.label));
            assert!(iss.contains(&format!(
                r#"Subkey: "{}"; ValueType: string; ValueName: "{VERSION_VALUE}""#,
                entry.key
            )));
        }
    }

    #[test]
    fn the_newest_copy_takes_the_menu_over() {
        let other = State::Other(r#""D:\0.7.1\edittr.exe" "%1""#.into());
        assert_eq!(
            command_exe(match &other {
                State::Other(c) => c,
                _ => unreachable!(),
            }),
            Some(r"D:\0.7.1\edittr.exe")
        );
        assert_eq!(follow(&other, Some("0.7.1"), "0.7.2", true), Follow::TakeOver);
        assert_eq!(follow(&other, Some("0.7.10"), "0.7.9", true), Follow::Keep);
        assert_eq!(follow(&other, Some("0.7.2"), "0.7.2", true), Follow::Keep);
        assert_eq!(follow(&other, Some("0.7.2"), "0.7.2-dev", true), Follow::Keep);
        assert_eq!(
            follow(&other, Some("0.8.0"), "0.7.2", false),
            Follow::TakeOver,
            "the registered copy was deleted"
        );
        assert_eq!(
            follow(&other, None, "0.7.2", true),
            Follow::TakeOver,
            "registered by a copy that didn't record its version"
        );
        assert_eq!(follow(&State::This, Some("0.7.1"), "0.7.2", true), Follow::RecordVersion);
        assert_eq!(follow(&State::This, Some("0.7.2"), "0.7.2", true), Follow::Keep);
        assert_eq!(
            follow(&State::Off, None, "0.7.2", true),
            Follow::Keep,
            "never added without the switch"
        );
        assert_eq!(follow(&State::AllUsers, None, "0.7.2", true), Follow::Keep);
    }

    #[test]
    fn classify_tells_this_copy_from_others() {
        let exe = Path::new(r"C:\a\edittr.exe");
        let ours: Vec<_> = ENTRIES.iter().map(|entry| Some(command(exe, entry.arg))).collect();
        assert_eq!(classify(exe, &ours, false), State::This);
        let upper: Vec<_> = ours.iter().map(|c| c.as_ref().map(|c| c.to_uppercase())).collect();
        assert_eq!(classify(exe, &upper, true), State::This);
        let mut partial = ours.clone();
        partial[2] = None;
        assert_eq!(classify(exe, &partial, false), State::Other(ours[0].clone().unwrap()));
        let other = vec![Some(r#""D:\old\edittr.exe" "%1""#.to_string()), None, None];
        assert_eq!(classify(exe, &other, false), State::Other(other[0].clone().unwrap()));
        assert_eq!(classify(exe, &[None, None, None], true), State::AllUsers);
        assert_eq!(classify(exe, &[None, None, None], false), State::Off);
    }
}
