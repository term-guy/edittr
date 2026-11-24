//! On-demand installation of language servers into edittr's own data folder
//! (no admin rights needed). Only runs after the user agrees in the UI.
//!
//! - Native binaries come from the project's official GitHub release, and are
//!   checked against the SHA-256 digest GitHub publishes for the asset.
//! - npm / Go based servers are installed with the user's own `npm` / `go`.

use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

pub enum Method {
    /// Asset of the latest GitHub release whose name `matches`; `.gz` or `.zip`.
    Github {
        repo: &'static str,
        matches: fn(&str) -> bool,
    },
    Npm {
        packages: &'static [&'static str],
    },
    Go {
        package: &'static str,
    },
}

pub struct Installable {
    pub command: &'static str,
    pub method: Method,
    /// Human description of where it comes from and roughly how big it is.
    pub source: &'static str,
}

#[derive(Default)]
pub struct Progress {
    pub done: AtomicU64,
    pub total: AtomicU64,
    pub stage: Mutex<String>,
}

impl Progress {
    fn stage(&self, s: impl Into<String>) {
        *self.stage.lock().unwrap() = s.into();
    }
    pub fn describe(&self) -> String {
        let stage = self.stage.lock().unwrap().clone();
        let (done, total) = (self.done.load(Ordering::Relaxed), self.total.load(Ordering::Relaxed));
        if total > 0 && done < total {
            format!("{stage} {}%", done * 100 / total)
        } else {
            stage
        }
    }
}

pub fn installable(command: &str) -> Option<Installable> {
    let i = |command, method, source| Some(Installable { command, method, source });
    match command {
        "rust-analyzer" => i(
            "rust-analyzer",
            Method::Github {
                repo: "rust-lang/rust-analyzer",
                matches: rust_analyzer_asset,
            },
            "GitHub release of rust-lang/rust-analyzer (about 15 MB)",
        ),
        "clangd" => i(
            "clangd",
            Method::Github {
                repo: "clangd/clangd",
                matches: clangd_asset,
            },
            "GitHub release of clangd/clangd (about 30-120 MB)",
        ),
        "typescript-language-server" => i(
            "typescript-language-server",
            Method::Npm {
                // TypeScript 7 (the native port) no longer ships the tsserver.js this server needs.
                packages: &["typescript-language-server", "typescript@6"],
            },
            "npm (needs Node.js installed)",
        ),
        "pyright-langserver" => i(
            "pyright-langserver",
            Method::Npm { packages: &["pyright"] },
            "npm (needs Node.js installed)",
        ),
        "gopls" => i(
            "gopls",
            Method::Go {
                package: "golang.org/x/tools/gopls@latest",
            },
            "`go install` (needs Go installed)",
        ),
        _ => None,
    }
}

fn rust_analyzer_asset(name: &str) -> bool {
    let triple = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "aarch64-apple-darwin.gz",
        ("macos", _) => "x86_64-apple-darwin.gz",
        ("linux", "aarch64") => "aarch64-unknown-linux-gnu.gz",
        ("linux", _) => "x86_64-unknown-linux-gnu.gz",
        ("windows", "aarch64") => "aarch64-pc-windows-msvc.zip",
        ("windows", _) => "x86_64-pc-windows-msvc.zip",
        _ => return false,
    };
    name == format!("rust-analyzer-{triple}")
}

fn clangd_asset(name: &str) -> bool {
    let os = match std::env::consts::OS {
        "macos" => "mac",
        "windows" => "windows",
        _ => "linux",
    };
    name.starts_with(&format!("clangd-{os}-")) && name.ends_with(".zip")
}

/// `EDITTR_SERVERS_DIR` overrides the location (e.g. for a portable setup).
pub fn servers_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("EDITTR_SERVERS_DIR") {
        return PathBuf::from(d);
    }
    dirs::data_local_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("edittr")
        .join("servers")
}

fn marker(command: &str) -> PathBuf {
    servers_dir().join(format!("{command}.path"))
}

/// Executable previously installed by edittr, if it's still there.
pub fn find_installed(command: &str) -> Option<PathBuf> {
    let p = PathBuf::from(std::fs::read_to_string(marker(command)).ok()?.trim());
    p.is_file().then_some(p)
}

fn exe(name: &str) -> String {
    if cfg!(windows) { format!("{name}.exe") } else { name.to_string() }
}

/// Install `inst`, blocking. Returns the executable's path.
pub fn install(inst: &Installable, progress: &Progress) -> Result<PathBuf, String> {
    let dir = servers_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let path = match &inst.method {
        Method::Github { repo, matches } => install_github(inst.command, repo, *matches, &dir, progress)?,
        Method::Npm { packages } => {
            let npm = crate::lsp::which("npm").ok_or("npm was not found. Install Node.js first, then try again")?;
            progress.stage(format!("Installing {} with npm…", inst.command));
            let prefix = dir.join("node");
            run(Command::new(npm)
                .arg("install")
                .arg("--prefix")
                .arg(&prefix)
                .args(["--no-fund", "--no-audit", "--loglevel=error"])
                .args(*packages))?;
            let bin = prefix.join("node_modules").join(".bin");
            let p = if cfg!(windows) {
                bin.join(format!("{}.cmd", inst.command))
            } else {
                bin.join(inst.command)
            };
            p.is_file()
                .then_some(p)
                .ok_or("npm finished but the server executable is missing")?
        }
        Method::Go { package } => {
            let go = crate::lsp::which("go").ok_or("go was not found. Install Go first, then try again")?;
            progress.stage(format!("Building {} with go install…", inst.command));
            let bin = dir.join("go").join("bin");
            run(Command::new(go).arg("install").arg(package).env("GOBIN", &bin))?;
            let p = bin.join(exe(inst.command));
            p.is_file().then_some(p).ok_or("go install finished but the binary is missing")?
        }
    };
    std::fs::write(marker(inst.command), path.to_string_lossy().as_bytes()).map_err(|e| e.to_string())?;
    progress.stage(format!("Installed {}", inst.command));
    Ok(path)
}

fn run(cmd: &mut Command) -> Result<(), String> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    let out = cmd.output().map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        let err = String::from_utf8_lossy(&out.stderr);
        Err(err
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("install command failed")
            .to_string())
    }
}

/// HTTP client that trusts the operating system's certificate store (like browsers,
/// git and curl do), so corporate TLS proxies and newly issued roots work.
fn agent() -> &'static ureq::Agent {
    static AGENT: std::sync::OnceLock<ureq::Agent> = std::sync::OnceLock::new();
    AGENT.get_or_init(|| {
        use ureq::tls::{RootCerts, TlsConfig};
        ureq::Agent::config_builder()
            .tls_config(TlsConfig::builder().root_certs(RootCerts::PlatformVerifier).build())
            .build()
            .into()
    })
}

fn http_get(url: &str) -> Result<ureq::http::Response<ureq::Body>, String> {
    agent()
        .get(url)
        .header("User-Agent", "edittr-editor")
        .header("Accept", "application/vnd.github+json, application/octet-stream")
        .call()
        .map_err(|e| {
            let msg = e.to_string();
            if msg.contains("certificate") {
                format!(
                    "download failed: {msg}. Your network may be intercepting HTTPS; \
                         make sure its certificate is trusted by the system, or set HTTPS_PROXY"
                )
            } else {
                format!("download failed: {msg}")
            }
        })
}

fn install_github(command: &str, repo: &str, matches: fn(&str) -> bool, dir: &Path, progress: &Progress) -> Result<PathBuf, String> {
    progress.stage(format!("Looking up latest {command} release…"));
    let api = format!("https://api.github.com/repos/{repo}/releases/latest");
    let json: serde_json::Value = {
        let text = http_get(&api)?.into_body().read_to_string().map_err(|e| e.to_string())?;
        serde_json::from_str(&text).map_err(|e| e.to_string())?
    };
    let asset = json["assets"]
        .as_array()
        .and_then(|a| a.iter().find(|x| x["name"].as_str().is_some_and(matches)))
        .ok_or_else(|| format!("no {command} build for {}/{}", std::env::consts::OS, std::env::consts::ARCH))?;
    let name = asset["name"].as_str().unwrap_or_default().to_string();
    let url = asset["browser_download_url"].as_str().ok_or("release asset has no download URL")?;
    let expected = asset["digest"]
        .as_str()
        .and_then(|d| d.strip_prefix("sha256:"))
        .map(str::to_lowercase);

    // Download to a temp file while hashing.
    let target = dir.join(command);
    let _ = std::fs::remove_dir_all(&target);
    std::fs::create_dir_all(&target).map_err(|e| e.to_string())?;
    let tmp = target.join(format!("{name}.part"));
    progress.stage(format!("Downloading {command}"));
    let resp = http_get(url)?;
    progress.total.store(resp.body().content_length().unwrap_or(0), Ordering::Relaxed);
    let mut reader = resp.into_body().into_with_config().limit(1_000_000_000).reader();
    let mut file = std::fs::File::create(&tmp).map_err(|e| e.to_string())?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0; 256 * 1024];
    loop {
        let n = reader.read(&mut buf).map_err(|e| format!("download failed: {e}"))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        file.write_all(&buf[..n]).map_err(|e| e.to_string())?;
        progress.done.fetch_add(n as u64, Ordering::Relaxed);
    }
    drop(file);
    let actual: String = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
    if let Some(expected) = expected
        && expected != actual
    {
        let _ = std::fs::remove_dir_all(&target);
        return Err(format!("checksum mismatch for {name}; not installing"));
    }

    progress.stage(format!("Unpacking {command}…"));
    let bin_name = exe(command);
    let result = if name.ends_with(".gz") {
        let out = target.join(&bin_name);
        let mut gz = flate2::read::GzDecoder::new(std::fs::File::open(&tmp).map_err(|e| e.to_string())?);
        let mut f = std::fs::File::create(&out).map_err(|e| e.to_string())?;
        std::io::copy(&mut gz, &mut f).map_err(|e| format!("unpack failed: {e}"))?;
        Ok(out)
    } else {
        let f = std::fs::File::open(&tmp).map_err(|e| e.to_string())?;
        let mut zip = zip::ZipArchive::new(f).map_err(|e| format!("unpack failed: {e}"))?;
        zip.extract(&target).map_err(|e| format!("unpack failed: {e}"))?;
        find_file(&target, &bin_name).ok_or_else(|| format!("{bin_name} not found in {name}"))
    };
    let _ = std::fs::remove_file(&tmp);
    let out = result?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&out, std::fs::Permissions::from_mode(0o755));
    }
    Ok(out)
}

fn find_file(dir: &Path, name: &str) -> Option<PathBuf> {
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).ok()?.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.file_name().is_some_and(|n| n == name) {
                return Some(p);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_assets_for_this_platform() {
        let names = [
            "rust-analyzer-aarch64-apple-darwin.gz",
            "rust-analyzer-x86_64-apple-darwin.gz",
            "rust-analyzer-x86_64-unknown-linux-gnu.gz",
            "rust-analyzer-x86_64-pc-windows-msvc.zip",
            "rust-analyzer-linux-x64.vsix",
        ];
        assert_eq!(names.iter().filter(|n| rust_analyzer_asset(n)).count(), 1);
        let clangd = [
            "clangd-mac-23.1.0.zip",
            "clangd-linux-23.1.0.zip",
            "clangd-windows-23.1.0.zip",
            "clangd_indexing_tools-mac-23.1.0.zip",
        ];
        assert_eq!(clangd.iter().filter(|n| clangd_asset(n)).count(), 1);
    }

    /// Real download from GitHub. Run with:
    /// EDITTR_SERVERS_DIR=/some/tmp cargo test -- --ignored downloads_rust_analyzer
    #[test]
    #[ignore]
    fn downloads_rust_analyzer() {
        assert!(std::env::var_os("EDITTR_SERVERS_DIR").is_some(), "set EDITTR_SERVERS_DIR");
        let p = Progress::default();
        let path = install(&installable("rust-analyzer").unwrap(), &p).unwrap();
        assert_eq!(find_installed("rust-analyzer"), Some(path.clone()));
        let out = std::process::Command::new(&path).arg("--version").output().unwrap();
        assert!(String::from_utf8_lossy(&out.stdout).starts_with("rust-analyzer"));
    }

    /// TLS check against GitHub's release download host. Run with --ignored.
    #[test]
    #[ignore]
    fn tls_to_release_assets() {
        let url = "https://github.com/clangd/clangd/releases/download/23.1.0/clangd-windows-23.1.0.zip";
        let resp = http_get(url).expect("request failed");
        let mut buf = [0u8; 4];
        resp.into_body().into_reader().read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"PK\x03\x04");
    }

    /// Real clangd download (~100 MB). Run with EDITTR_SERVERS_DIR set and --ignored.
    #[test]
    #[ignore]
    fn downloads_clangd() {
        assert!(std::env::var_os("EDITTR_SERVERS_DIR").is_some(), "set EDITTR_SERVERS_DIR");
        let path = install(&installable("clangd").unwrap(), &Progress::default()).unwrap();
        let out = std::process::Command::new(&path).arg("--version").output().unwrap();
        assert!(String::from_utf8_lossy(&out.stdout).contains("clangd version"));
    }
}
