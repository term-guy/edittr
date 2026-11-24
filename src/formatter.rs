//! Run external formatters on a buffer snapshot without touching the source file.
use std::fs::{self, DirBuilder, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

const TIMEOUT: Duration = Duration::from_secs(5);

pub(crate) fn format(path: &Path, text: &str) -> Result<Option<String>, String> {
    let extension = path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let name = match extension.as_str() {
        "rs" => "rustfmt",
        "js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx" | "mts" | "cts" | "json" | "jsonc" | "css" | "scss" | "less" | "html" | "htm"
        | "md" | "markdown" | "mdx" | "yaml" | "yml" => "prettier",
        "py" | "pyi" => "black",
        "go" => "gofmt",
        "c" | "cc" | "cpp" | "cxx" | "h" | "hh" | "hpp" | "hxx" | "ipp" | "inl" | "m" | "mm" => "clang-format",
        _ => return Ok(None),
    };
    let path = std::path::absolute(path).map_err(|err| format!("{name}: {err}"))?;
    let dir = path.parent().ok_or_else(|| format!("{name}: file has no parent directory"))?;
    let launch = crate::lsp::which(name)
        .map(|exe| crate::bundled::Launch { exe, args: Vec::new() })
        .or_else(|| crate::bundled::find(name))
        .ok_or_else(|| format!("{name} was not found on PATH or in the bundle"))?;
    let exe = std::path::absolute(&launch.exe).map_err(|err| format!("{name}: {err}"))?;
    let mut cmd = Command::new(exe);
    cmd.current_dir(dir).args(launch.args);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    match name {
        "rustfmt" => {
            cmd.args(["--emit", "stdout"]);
            if let Some(edition) = rust_edition(dir) {
                cmd.args(["--edition", &edition]);
            }
        }
        "prettier" => {
            cmd.arg("--stdin-filepath").arg(&path);
        }
        "black" => {
            cmd.args(["-q", "--stdin-filename"]).arg(&path).arg("-");
        }
        "clang-format" => {
            let mut filename = std::ffi::OsString::from("--assume-filename=");
            filename.push(&path);
            cmd.arg(filename);
        }
        _ => {}
    }
    let result = run(&mut cmd, text, TIMEOUT).map_err(|err| format!("{name}: {err}"))?;
    if result.status.success() {
        String::from_utf8(result.stdout)
            .map(|text| Some(text.replace("\r\n", "\n")))
            .map_err(|err| format!("{name} returned invalid UTF-8: {err}"))
    } else {
        let detail = result.stderr.lines().find(|line| !line.trim().is_empty()).unwrap_or("").trim();
        if detail.is_empty() {
            Err(format!("{name} failed ({})", result.status))
        } else {
            let detail: String = detail.chars().take(240).collect();
            Err(format!("{name}: {detail}"))
        }
    }
}

fn rust_edition(dir: &Path) -> Option<String> {
    let mut inherited = false;
    for dir in dir.ancestors() {
        let manifest = dir.join("Cargo.toml");
        let Ok(text) = fs::read_to_string(&manifest) else {
            continue;
        };
        let table = text.parse::<toml::Table>().ok()?;
        if !inherited {
            let edition = table.get("package")?.get("edition");
            if let Some(value) = edition.and_then(toml::Value::as_str) {
                return Some(value.to_owned());
            }
            if edition.and_then(|value| value.get("workspace")).and_then(toml::Value::as_bool) != Some(true) {
                return None;
            }
            inherited = true;
        }
        if let Some(workspace) = table.get("workspace") {
            return workspace.get("package")?.get("edition")?.as_str().map(str::to_owned);
        }
    }
    None
}

struct Output {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: String,
}

fn run(cmd: &mut Command, text: &str, timeout: Duration) -> io::Result<Output> {
    // Files used as stdio prevent pipe backpressure and let the worker return
    // after killing a timed-out formatter even if a descendant inherited stdio.
    let scratch = Scratch::new()?;
    let input = scratch.0.join("stdin");
    let output = scratch.0.join("stdout");
    let errors = scratch.0.join("stderr");
    File::create(&input)?.write_all(text.as_bytes())?;
    let mut child = cmd
        .stdin(File::open(input)?)
        .stdout(File::create(&output)?)
        .stderr(File::create(&errors)?)
        .spawn()?;
    // Command retains its copies of stdio handles, so release them before cleanup.
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if start.elapsed() < timeout => std::thread::sleep(Duration::from_millis(20)),
            result => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(result
                    .err()
                    .unwrap_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "formatting timed out")));
            }
        }
    };
    Ok(Output {
        status,
        stdout: if status.success() { fs::read(output)? } else { Vec::new() },
        stderr: if status.success() {
            String::new()
        } else {
            let file = File::open(errors)?;
            let mut bytes = Vec::new();
            file.take(4096).read_to_end(&mut bytes)?;
            String::from_utf8_lossy(&bytes).into_owned()
        },
    })
}

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> io::Result<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let builder = DirBuilder::new();
        #[cfg(unix)]
        let builder = {
            use std::os::unix::fs::DirBuilderExt;
            let mut builder = builder;
            builder.mode(0o700);
            builder
        };
        let temp = std::env::temp_dir();
        loop {
            let unique = NEXT.fetch_add(1, Ordering::Relaxed);
            let path = temp.join(format!("edittr-fmt-{}-{unique}", std::process::id()));
            match builder.create(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
                Err(err) => return Err(err),
            }
        }
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_cargo_package_and_inherited_editions() {
        let dir = Scratch::new().unwrap();
        let member = dir.0.join("member");
        let source = member.join("src");
        fs::create_dir_all(&source).unwrap();
        fs::write(dir.0.join("Cargo.toml"), "[workspace.package]\nedition = '2024'").unwrap();
        fs::write(member.join("Cargo.toml"), "[package]\nedition.workspace = true").unwrap();
        assert_eq!(rust_edition(&source).as_deref(), Some("2024"));
        fs::write(member.join("Cargo.toml"), "[package]\nedition = '2021'").unwrap();
        assert_eq!(rust_edition(&source).as_deref(), Some("2021"));
    }

    #[cfg(unix)]
    #[test]
    fn formatter_stdio_and_timeout() {
        let buffer = "unsaved text \n".repeat(100_000);
        let output = run(&mut Command::new("/bin/cat"), &buffer, TIMEOUT).unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, buffer.as_bytes());

        let mut sleep = Command::new("/bin/sleep");
        sleep.arg("10");
        let start = Instant::now();
        let err = run(&mut sleep, "", Duration::from_millis(50)).err().unwrap();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        assert!(start.elapsed() < Duration::from_secs(5));
    }
}
