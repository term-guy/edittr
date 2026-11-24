//! "Generate compile database": produce a `compile_commands.json` for clangd from
//! the project's own build system, into edittr's cache (nothing is written to the
//! project). Runs on a background thread with streamed output and cancellation.
//!
//! - CMake: configure only, with CMAKE_EXPORT_COMPILE_COMMANDS, into a cache build dir.
//! - Meson: `meson setup` into a cache build dir.
//! - Make (or a custom build command): under `bear` when installed (a real build, so
//!   every invocation is captured), otherwise `make -n -w -k -B` (a dry run) whose
//!   output is parsed here. A dry run still runs `$(shell ...)`, `+` lines and
//!   recursive `$(MAKE)` recipes, so this is only ever started by the user.

use serde_json::{Value, json};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Method {
    Cmake,
    Meson,
    Bear,
    MakeDryRun,
}

impl Method {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Cmake => "CMake",
            Self::Meson => "Meson",
            Self::Bear => "bear",
            Self::MakeDryRun => "make dry run",
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Plan {
    pub method: Method,
    pub program: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    /// Where the finished database ends up.
    pub output: PathBuf,
}

impl Plan {
    /// The command as shown to the user before it runs.
    pub(crate) fn command_line(&self) -> String {
        std::iter::once(&self.program)
            .chain(&self.args)
            .map(|w| quote(w))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

fn quote(word: &str) -> String {
    if !word.is_empty() && word.chars().all(|c| c.is_ascii_alphanumeric() || "-_./=:,+@%~".contains(c)) {
        word.to_owned()
    } else {
        format!("'{}'", word.replace('\'', r"'\''"))
    }
}

/// Edittr's cache folder for this workspace's clangd data.
pub(crate) fn cache_dir(root: &Path) -> Option<PathBuf> {
    Some(dirs::cache_dir()?.join("edittr/clangd").join(crate::workspace::root_hash(root)))
}

/// Where a generated database for `root` lives (whichever method made it).
pub(crate) fn generated_database(root: &Path) -> Option<PathBuf> {
    Some(cache_dir(root)?.join("generated").join(crate::clangd::DATABASE))
}

fn which(program: &str) -> bool {
    crate::lsp::which(program).is_some()
}

/// What "Generate compile database" would run for this project; `build` is the
/// per-workspace build command (empty: detect).
pub(crate) fn plan(root: &Path, build: &str) -> Result<Plan, String> {
    let generated = cache_dir(root).ok_or("edittr cache directory unavailable")?.join("generated");
    let output = generated.join(crate::clangd::DATABASE);
    let custom = split_words(build.trim());
    if custom.is_empty() {
        if root.join("CMakeLists.txt").is_file() {
            if !which("cmake") {
                return Err("This is a CMake project, but cmake is not on PATH".into());
            }
            let dir = generated.join("cmake");
            return Ok(Plan {
                method: Method::Cmake,
                program: "cmake".into(),
                args: vec![
                    "-S".into(),
                    root.display().to_string(),
                    "-B".into(),
                    dir.display().to_string(),
                    "-DCMAKE_EXPORT_COMPILE_COMMANDS=ON".into(),
                ],
                cwd: root.to_owned(),
                output,
            });
        }
        if root.join("meson.build").is_file() {
            if !which("meson") {
                return Err("This is a Meson project, but meson is not on PATH".into());
            }
            let dir = generated.join("meson");
            let mut args = vec!["setup".into()];
            if dir.join("build.ninja").is_file() {
                args.push("--reconfigure".into());
            }
            args.extend([dir.display().to_string(), root.display().to_string()]);
            return Ok(Plan {
                method: Method::Meson,
                program: "meson".into(),
                args,
                cwd: root.to_owned(),
                output,
            });
        }
        if !["GNUmakefile", "makefile", "Makefile"].iter().any(|m| root.join(m).is_file()) {
            return Err("No CMakeLists.txt, meson.build or Makefile at the project root. Set a build command first.".into());
        }
    }
    let mut build = if custom.is_empty() { vec!["make".to_owned()] } else { custom };
    let make = Path::new(&build[0])
        .file_stem()
        .and_then(|s| s.to_str())
        .is_some_and(|s| s == "make" || s == "gmake");
    if which("bear") {
        let mut args = vec!["--output".to_owned(), output.display().to_string(), "--".into()];
        if make && !build.iter().any(|a| a.starts_with("-k") || a == "--keep-going") {
            build.insert(1, "-k".into());
        }
        args.extend(build);
        return Ok(Plan {
            method: Method::Bear,
            program: "bear".into(),
            args,
            cwd: root.to_owned(),
            output,
        });
    }
    if !make {
        return Err(format!(
            "`{}` isn't make: install bear (github.com/rizsotto/Bear) to capture its compiler calls",
            build[0]
        ));
    }
    let program = build.remove(0);
    let mut args = vec!["-n".to_owned(), "-w".into(), "-k".into(), "-B".into()];
    args.extend(build);
    Ok(Plan {
        method: Method::MakeDryRun,
        program,
        args,
        cwd: root.to_owned(),
        output,
    })
}

pub(crate) enum Event {
    Line(String),
    /// The database path and how many files it covers, or an error.
    Done(Result<(PathBuf, usize), String>),
}

pub(crate) struct Job {
    pub plan: Plan,
    pub rx: Receiver<Event>,
    cancel: Arc<AtomicBool>,
    pid: Arc<Mutex<Option<u32>>>,
}

impl Job {
    pub(crate) fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
        if let Some(pid) = *self.pid.lock().unwrap() {
            kill_tree(pid);
        }
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        self.cancel();
    }
}

fn kill_tree(pid: u32) {
    #[cfg(unix)]
    let _ = Command::new("kill").args(["-TERM", &format!("-{pid}")]).status();
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let _ = Command::new("taskkill")
            .args(["/T", "/F", "/PID", &pid.to_string()])
            .creation_flags(0x0800_0000)
            .status();
    }
}

pub(crate) fn start(plan: Plan, wake: impl Fn() + Send + 'static) -> Job {
    let (tx, rx) = channel();
    let cancel = Arc::new(AtomicBool::new(false));
    let pid = Arc::new(Mutex::new(None));
    let (p, c, id) = (plan.clone(), cancel.clone(), pid.clone());
    std::thread::spawn(move || {
        let result = run(&p, &tx, &c, &id, &wake);
        let _ = tx.send(Event::Done(if c.load(Ordering::Relaxed) {
            Err("Cancelled".into())
        } else {
            result
        }));
        wake();
    });
    Job { plan, rx, cancel, pid }
}

fn run(
    plan: &Plan,
    tx: &Sender<Event>,
    cancel: &AtomicBool,
    pid: &Mutex<Option<u32>>,
    wake: &dyn Fn(),
) -> Result<(PathBuf, usize), String> {
    let dir = plan.output.parent().unwrap();
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let mut cmd = Command::new(&plan.program);
    cmd.args(&plan.args)
        .current_dir(&plan.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // English "Entering directory" messages for the dry-run parser.
    cmd.env("LC_ALL", "C");
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let mut child = cmd.spawn().map_err(|e| format!("Couldn't start {}: {e}", plan.program))?;
    *pid.lock().unwrap() = Some(child.id());
    let stderr = child.stderr.take().unwrap();
    let err_tx = tx.clone();
    let err_thread = std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            let _ = err_tx.send(Event::Line(line));
        }
    });
    let mut captured = Vec::new();
    let mut last_wake = std::time::Instant::now();
    for line in BufReader::new(child.stdout.take().unwrap()).lines().map_while(Result::ok) {
        if plan.method == Method::MakeDryRun {
            captured.push(line.clone());
        }
        let _ = tx.send(Event::Line(line));
        if last_wake.elapsed().as_millis() > 100 {
            last_wake = std::time::Instant::now();
            wake();
        }
    }
    let status = child.wait().map_err(|e| e.to_string())?;
    let _ = err_thread.join();
    *pid.lock().unwrap() = None;
    if cancel.load(Ordering::Relaxed) {
        return Err("Cancelled".into());
    }
    let produced = match plan.method {
        Method::MakeDryRun => {
            let records = parse_make_dry_run(&captured.join("\n"), &plan.cwd);
            if records.is_empty() {
                return Err(format!("make printed no compiler commands (exit {status})"));
            }
            let json = serde_json::to_string_pretty(&records).map_err(|e| e.to_string())?;
            crate::workspace::write_config(&plan.output, &json).map_err(|e| e.to_string())?;
            plan.output.clone()
        }
        Method::Cmake | Method::Meson => {
            if !status.success() {
                return Err(format!("{} failed ({status})", plan.program));
            }
            let build_dir = PathBuf::from(&plan.args[if plan.method == Method::Cmake { 3 } else { plan.args.len() - 2 }]);
            let made = build_dir.join(crate::clangd::DATABASE);
            let text = std::fs::read_to_string(&made).map_err(|e| format!("{}: {e}", made.display()))?;
            crate::workspace::write_config(&plan.output, &text).map_err(|e| e.to_string())?;
            plan.output.clone()
        }
        // bear writes the output itself; a failing build still leaves what it captured.
        Method::Bear => plan.output.clone(),
    };
    let text = std::fs::read_to_string(&produced).map_err(|e| format!("No database was written ({status}): {e}"))?;
    let count = serde_json::from_str::<Vec<Value>>(&text).map_err(|e| e.to_string())?.len();
    if count == 0 {
        return Err(format!(
            "The build ran no compiler commands ({status}); try `make clean` first or a different build command"
        ));
    }
    Ok((produced, count))
}

/// Shell-like word splitting: whitespace, single and double quotes, backslashes.
pub(crate) fn split_words(s: &str) -> Vec<String> {
    let (mut words, mut word, mut in_word) = (Vec::new(), String::new(), false);
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_word = true;
                word.extend(chars.by_ref().take_while(|&c| c != '\''));
            }
            '"' => {
                in_word = true;
                while let Some(c) = chars.next() {
                    match c {
                        '"' => break,
                        '\\' => word.extend(chars.next()),
                        c => word.push(c),
                    }
                }
            }
            '\\' => {
                in_word = true;
                word.extend(chars.next());
            }
            c if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            c => {
                in_word = true;
                word.push(c);
            }
        }
    }
    if in_word {
        words.push(word);
    }
    words
}

const SOURCES: &[&str] = &["c", "cc", "cpp", "cxx", "c++", "m", "mm", "s", "S"];

fn is_compiler(word: &str) -> bool {
    crate::ccompilers::is_compiler(word)
}

/// Compile commands from `make -n -w` output: tracks "Entering/Leaving directory"
/// and `cd dir &&` prefixes, splits `&&` / `;` chains, and keeps commands whose
/// compiler compiles exactly one C/C++ source.
pub(crate) fn parse_make_dry_run(output: &str, root: &Path) -> Vec<Value> {
    let mut dirs = vec![root.to_path_buf()];
    let mut records = Vec::new();
    let mut seen = std::collections::HashSet::new();
    // Join backslash-continued lines.
    let mut logical = Vec::new();
    let mut current = String::new();
    for line in output.lines() {
        if let Some(head) = line.strip_suffix('\\') {
            current.push_str(head);
            current.push(' ');
        } else {
            current.push_str(line);
            logical.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        logical.push(current);
    }
    for line in logical {
        let trimmed = line.trim();
        if let Some(rest) = make_message(trimmed) {
            if let Some(dir) = rest.strip_prefix("Entering directory ") {
                dirs.push(PathBuf::from(dir.trim_matches(|c| matches!(c, '\'' | '`' | '"'))));
            } else if rest.starts_with("Leaving directory ") && dirs.len() > 1 {
                dirs.pop();
            }
            continue;
        }
        let mut dir = dirs.last().unwrap().clone();
        for part in split_commands(trimmed) {
            let words = split_words(&part);
            let Some(first) = words.first() else { continue };
            if first == "cd" {
                if let Some(target) = words.get(1) {
                    dir = dir.join(target);
                }
                continue;
            }
            // Skip wrappers and echo/@ prefixes: ccache gcc ..., @gcc ...
            let start = words.iter().position(|w| is_compiler(w.trim_start_matches('@')));
            let Some(start) = start else { continue };
            let mut args: Vec<String> = words[start..].to_vec();
            args[0] = args[0].trim_start_matches('@').to_owned();
            // GCC-style drivers link unless given -c; IAR, TI, Keil and others compile by default.
            let native = crate::ccompilers::family(&args[0]).is_some_and(|f| f.native());
            if native && !args.iter().any(|a| a == "-c" || a == "/c") {
                continue; // linking or preprocessing only
            }
            let sources: Vec<&String> = args
                .iter()
                .skip(1)
                .filter(|a| {
                    !a.starts_with('-')
                        && Path::new(a.as_str())
                            .extension()
                            .and_then(|e| e.to_str())
                            .is_some_and(|e| SOURCES.contains(&e))
                })
                .collect();
            let [file] = sources[..] else { continue };
            let abs = dir.join(file);
            if seen.insert(abs.clone()) {
                records.push(json!({"directory": dir, "file": abs, "arguments": args}));
            }
        }
    }
    records
}

/// The text after `make: ` / `make[2]: ` / `gmake[1]: `.
fn make_message(line: &str) -> Option<&str> {
    let (head, rest) = line.split_once(": ")?;
    let name = head.split('[').next()?;
    let name = Path::new(name).file_stem()?.to_str()?;
    (name == "make" || name == "gmake" || name == "mingw32-make").then_some(rest)
}

/// Split a shell line at `&&`, `||` and `;` outside quotes.
fn split_commands(line: &str) -> Vec<String> {
    let (mut parts, mut cur, mut quote) = (Vec::new(), String::new(), None);
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(q), c) if c == q => {
                quote = None;
                cur.push(c);
            }
            (Some(_), c) => cur.push(c),
            (None, '\'' | '"') => {
                quote = Some(c);
                cur.push(c);
            }
            (None, ';') => parts.push(std::mem::take(&mut cur)),
            (None, '&' | '|') if chars.peek() == Some(&c) => {
                chars.next();
                parts.push(std::mem::take(&mut cur));
            }
            (None, c) => cur.push(c),
        }
    }
    parts.push(cur);
    parts.into_iter().map(|p| p.trim().to_owned()).filter(|p| !p.is_empty()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_recursive_make_dry_run() {
        let root = Path::new("/p");
        let out = "make: Entering directory '/p'\n\
            echo CC main.c\n\
            ccache gcc -Iinclude -DX=1 -c main.c -o main.o\n\
            make[1]: Entering directory '/p/lib'\n\
            arm-none-eabi-gcc -mcpu=cortex-m4 \\\n  -I../include -c 'util file.c' -o util.o\n\
            gcc -o app main.o util.o\n\
            make[1]: Leaving directory '/p/lib'\n\
            cd drivers && clang++-18 -std=c++17 -c uart.cpp -o uart.o; echo done\n\
            gcc -c main.c -o main.o\n\
            make: Leaving directory '/p'\n";
        let records = parse_make_dry_run(out, root);
        let files: Vec<&str> = records.iter().map(|r| r["file"].as_str().unwrap()).collect();
        assert_eq!(files, ["/p/main.c", "/p/lib/util file.c", "/p/drivers/uart.cpp"]);
        assert_eq!(records[0]["arguments"][0], "gcc");
        assert_eq!(records[1]["directory"], "/p/lib");
        assert_eq!(records[1]["arguments"][1], "-mcpu=cortex-m4");
        assert_eq!(records[2]["directory"], "/p/drivers");
    }

    #[test]
    fn splits_shell_words() {
        assert_eq!(
            split_words(r#"make -C "fw dir" 'a b' c\ d"#),
            ["make", "-C", "fw dir", "a b", "c d"]
        );
        assert!(split_words("  ").is_empty());
        assert!(is_compiler("/usr/bin/x86_64-w64-mingw32-g++.exe"));
        assert!(!is_compiler("ar") && !is_compiler("ld") && !is_compiler("clang-format"));
        // IAR compiles without -c; its dry-run line still counts.
        let iar = parse_make_dry_run("/opt/iar/arm/bin/iccarm src/a.c -Iinc -o obj/\n", Path::new("/p"));
        assert_eq!(iar[0]["file"], "/p/src/a.c");
    }

    #[test]
    fn plans_make_projects_and_rejects_unknown_builds() {
        let root = std::env::temp_dir().join(format!("edittr-compdb-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        assert!(plan(&root, "").is_err());
        std::fs::write(root.join("Makefile"), "all:\n").unwrap();
        let p = plan(&root, "").unwrap();
        assert!(matches!(p.method, Method::Bear | Method::MakeDryRun));
        assert!(p.command_line().contains("make"));
        assert!(p.output.ends_with("generated/compile_commands.json"));
        if p.method == Method::MakeDryRun {
            assert_eq!(p.args[..4], ["-n", "-w", "-k", "-B"]);
            assert!(plan(&root, "ninja -C out").is_err());
        }
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn make_dry_run_end_to_end() {
        if crate::lsp::which("make").is_none() {
            return; // no make on this machine
        }
        let root = std::env::temp_dir().join(format!("edittr-compdb-run-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("lib")).unwrap();
        let root = root.canonicalize().unwrap();
        std::fs::write(
            root.join("Makefile"),
            "all:\n\tcc -Iinc -DA=1 -c main.c -o main.o\n\t$(MAKE) -C lib\n",
        )
        .unwrap();
        std::fs::write(root.join("lib/Makefile"), "all:\n\tcc -DLIB -c util.c -o util.o\n").unwrap();
        let output = root.join("out/compile_commands.json");
        let plan = Plan {
            method: Method::MakeDryRun,
            program: "make".into(),
            args: vec!["-n".into(), "-w".into(), "-k".into(), "-B".into()],
            cwd: root.clone(),
            output: output.clone(),
        };
        let job = start(plan, || {});
        let result = loop {
            match job.rx.recv_timeout(std::time::Duration::from_secs(20)).unwrap() {
                Event::Done(result) => break result,
                Event::Line(_) => {}
            }
        };
        assert_eq!(result.unwrap(), (output.clone(), 2));
        let records: Vec<Value> = serde_json::from_str(&std::fs::read_to_string(&output).unwrap()).unwrap();
        assert_eq!(records[1]["directory"], root.join("lib").display().to_string());
        // Nothing was built.
        assert!(!root.join("main.o").exists() && !root.join("lib/util.o").exists());
        std::fs::remove_dir_all(&root).ok();
    }
}
