//! Language servers shipped inside the "full" downloads.
//!
//! A bundle is a `servers/` folder with a `servers.json` manifest:
//! ```json
//! { "rust-analyzer": { "exe": "rust-analyzer/rust-analyzer" },
//!   "pyright-langserver": { "exe": "node/node", "args": ["@js/node_modules/pyright/langserver.index.js"] } }
//! ```
//! `exe` is relative to the bundle folder; an argument starting with `@` is a
//! path relative to it too. The folder is looked for next to the executable,
//! in `Contents/Resources` of a macOS app, and in `../lib/edittr` (Linux install).

use std::path::{Path, PathBuf};

/// How to start a server: the executable plus arguments that go before the server's own.
#[derive(Debug, Clone, PartialEq)]
pub struct Launch {
    pub exe: PathBuf,
    pub args: Vec<String>,
}

fn bundle_dirs() -> Vec<PathBuf> {
    let Some(exe_dir) = std::env::current_exe().ok().and_then(|e| e.parent().map(PathBuf::from)) else {
        return Vec::new();
    };
    vec![
        exe_dir.join("servers"),
        exe_dir.join("../Resources/servers"),
        exe_dir.join("../lib/edittr/servers"),
    ]
}

/// The bundled server for `command`, if this is a full download that ships it.
pub fn find(command: &str) -> Option<Launch> {
    bundle_dirs().iter().find_map(|dir| find_in(dir, command))
}

pub fn find_in(dir: &Path, command: &str) -> Option<Launch> {
    let manifest: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(dir.join("servers.json")).ok()?).ok()?;
    let entry = manifest.get(command)?;
    let exe = dir.join(entry.get("exe")?.as_str()?);
    if !exe.is_file() {
        return None;
    }
    let args = entry
        .get("args")
        .and_then(|a| a.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str())
                .map(|s| match s.strip_prefix('@') {
                    Some(rel) => dir.join(rel).to_string_lossy().into_owned(),
                    None => s.to_string(),
                })
                .collect()
        })
        .unwrap_or_default();
    Some(Launch { exe, args })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_manifest_with_relative_paths() {
        let dir = std::env::temp_dir().join(format!("edittr-bundle-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("node")).unwrap();
        std::fs::create_dir_all(dir.join("ra")).unwrap();
        std::fs::write(dir.join("node/node"), "").unwrap();
        std::fs::write(dir.join("ra/rust-analyzer"), "").unwrap();
        std::fs::write(
            dir.join("servers.json"),
            r#"{ "rust-analyzer": { "exe": "ra/rust-analyzer" },
                 "pyright-langserver": { "exe": "node/node", "args": ["@js/langserver.js", "--max-old-space-size=4096"] },
                 "gopls": { "exe": "gopls/gopls" } }"#,
        )
        .unwrap();
        let ra = find_in(&dir, "rust-analyzer").unwrap();
        assert_eq!(
            ra,
            Launch {
                exe: dir.join("ra/rust-analyzer"),
                args: vec![]
            }
        );
        let py = find_in(&dir, "pyright-langserver").unwrap();
        assert_eq!(py.exe, dir.join("node/node"));
        assert_eq!(
            py.args,
            vec![
                dir.join("js/langserver.js").to_string_lossy().into_owned(),
                "--max-old-space-size=4096".into()
            ]
        );
        assert!(find_in(&dir, "gopls").is_none(), "listed but missing on disk");
        assert!(find_in(&dir, "clangd").is_none(), "not listed");
        std::fs::remove_dir_all(&dir).ok();
    }
}
