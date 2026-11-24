//! Minimal Language Server Protocol client: just enough for go-to-definition.
//!
//! One server process per language per project folder, started lazily on first use.
//! Documents are synced lazily too: before each request we send the current buffer.

use crate::workspace;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

pub struct ServerSpec {
    pub name: &'static str,
    pub command: &'static str,
    pub args: &'static [&'static str],
    pub language_id: &'static str,
}

impl ServerSpec {
    fn initialization_options(&self, ignored: &[String]) -> Option<Value> {
        if ignored.is_empty() {
            return None;
        }
        match self.name {
            "rust-analyzer" => Some(json!({"files": {"excludeDirs": ignored}})),
            "gopls" => {
                let filters: Vec<_> = std::iter::once("-**/node_modules".to_owned())
                    .chain(ignored.iter().map(|dir| format!("-{dir}")))
                    .collect();
                Some(json!({"directoryFilters": filters}))
            }
            _ => None,
        }
    }
}

/// Candidate servers for a file extension, in order of preference.
pub fn servers_for(ext: &str) -> Vec<ServerSpec> {
    let s = |name, command, args, language_id| ServerSpec {
        name,
        command,
        args,
        language_id,
    };
    match ext {
        "rs" => vec![s("rust-analyzer", "rust-analyzer", &[][..], "rust")],
        "ts" | "tsx" | "mts" | "cts" => vec![s(
            "typescript",
            "typescript-language-server",
            &["--stdio"][..],
            if ext == "tsx" { "typescriptreact" } else { "typescript" },
        )],
        "js" | "jsx" | "mjs" | "cjs" => vec![s(
            "typescript",
            "typescript-language-server",
            &["--stdio"][..],
            if ext == "jsx" { "javascriptreact" } else { "javascript" },
        )],
        "py" | "pyi" => vec![
            s("pyright", "pyright-langserver", &["--stdio"][..], "python"),
            s("basedpyright", "basedpyright-langserver", &["--stdio"][..], "python"),
            s("pylsp", "pylsp", &[][..], "python"),
        ],
        "go" => vec![s("gopls", "gopls", &[][..], "go")],
        "c" | "h" => vec![s("clangd", "clangd", &[][..], "c")],
        "cc" | "cpp" | "cxx" | "hpp" | "hh" | "hxx" => vec![s("clangd", "clangd", &[][..], "cpp")],
        "java" => vec![s("jdtls", "jdtls", &[][..], "java")],
        "rb" => vec![s("ruby-lsp", "ruby-lsp", &[][..], "ruby")],
        "php" => vec![s("intelephense", "intelephense", &["--stdio"][..], "php")],
        "lua" => vec![s("lua-language-server", "lua-language-server", &[][..], "lua")],
        "zig" => vec![s("zls", "zls", &[][..], "zig")],
        "cs" => vec![s("csharp-ls", "csharp-ls", &[][..], "csharp")],
        _ => vec![],
    }
}

/// Find an executable on PATH (honouring PATHEXT on Windows).
pub fn which(cmd: &str) -> Option<PathBuf> {
    let exts: Vec<String> = if cfg!(windows) {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".EXE;.CMD;.BAT".into())
            .split(';')
            .map(|e| e.to_lowercase())
            .chain([String::new()])
            .collect()
    } else {
        vec![String::new()]
    };
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find_map(|dir| exts.iter().map(|e| dir.join(format!("{cmd}{e}"))).find(|p| p.is_file()))
}

/// Lookup order: PATH (the user's own), then a server bundled in a "full"
/// download, then one edittr installed on demand.
pub fn resolve(cmd: &str) -> Option<crate::bundled::Launch> {
    use crate::bundled::Launch;
    which(cmd)
        .map(|exe| Launch { exe, args: Vec::new() })
        .or_else(|| crate::bundled::find(cmd))
        .or_else(|| crate::installer::find_installed(cmd).map(|exe| Launch { exe, args: Vec::new() }))
}

pub struct Location {
    pub path: PathBuf,
    /// 0-based line and UTF-16 column range, as LSP reports them.
    pub line: usize,
    pub col16: std::ops::Range<usize>,
}

struct Synced {
    version: i64,
    text_hash: u64,
}

pub struct Client {
    pub name: &'static str,
    language_id: &'static str,
    root: PathBuf,
    ignored: Vec<String>,
    retired: AtomicBool,
    stdin: Mutex<ChildStdin>,
    child: Mutex<Child>,
    next_id: AtomicI64,
    pending: Arc<Mutex<HashMap<i64, Sender<Value>>>>,
    ready: OnceLock<Result<(), String>>,
    docs: Mutex<HashMap<String, Synced>>,
}

impl Drop for Client {
    fn drop(&mut self) {
        if let Ok(mut c) = self.child.lock() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

impl Client {
    /// Spawn the server and start the `initialize` handshake in the background.
    pub fn start(spec: &ServerSpec, root: &Path, ignored: &[String]) -> Result<Arc<Client>, String> {
        let launch = resolve(spec.command).ok_or_else(|| format!("{} is not installed", spec.command))?;
        let mut cmd = Command::new(&launch.exe);
        cmd.args(&launch.args)
            .args(spec.args)
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
        let mut child = cmd.spawn().map_err(|e| format!("failed to start {}: {e}", spec.command))?;
        let stdin = child.stdin.take().ok_or("no stdin")?;
        let stdout = child.stdout.take().ok_or("no stdout")?;

        let client = Arc::new(Client {
            name: spec.name,
            language_id: spec.language_id,
            root: root.to_path_buf(),
            ignored: ignored.to_vec(),
            retired: AtomicBool::new(false),
            stdin: Mutex::new(stdin),
            child: Mutex::new(child),
            next_id: AtomicI64::new(1),
            pending: Arc::new(Mutex::new(HashMap::new())),
            ready: OnceLock::new(),
            docs: Mutex::new(HashMap::new()),
        });

        let weak = Arc::downgrade(&client);
        let pending = client.pending.clone();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            while let Some(msg) = read_message(&mut reader) {
                let id = msg.get("id").cloned();
                let method = msg.get("method").and_then(Value::as_str).map(str::to_string);
                match (id, method) {
                    // Response to one of our requests.
                    (Some(id), None) => {
                        if let Some(tx) = id.as_i64().and_then(|i| pending.lock().unwrap().remove(&i)) {
                            let _ = tx.send(msg);
                        }
                    }
                    // Request from the server: answer so it doesn't block.
                    (Some(id), Some(method)) => {
                        let Some(c) = weak.upgrade() else { break };
                        let result = if method == "workspace/configuration" {
                            let items = msg["params"]["items"].as_array().map_or(&[][..], Vec::as_slice);
                            Value::Array(items.iter().map(|item| c.configuration(&item["section"])).collect())
                        } else {
                            Value::Null
                        };
                        c.send(&json!({"jsonrpc": "2.0", "id": id, "result": result}));
                    }
                    _ => {} // notifications (logs, progress, diagnostics) are ignored
                }
            }
        });

        let c = client.clone();
        let root_uri = path_to_uri(root);
        let root_name = root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let initialization_options = spec.initialization_options(ignored);
        std::thread::spawn(move || {
            let mut params = json!({
                "processId": std::process::id(),
                "clientInfo": {"name": "edittr"},
                "rootUri": root_uri,
                "workspaceFolders": [{"uri": root_uri, "name": root_name}],
                "capabilities": {
                    "general": {"positionEncodings": ["utf-16"]},
                    "textDocument": {
                        "synchronization": {"dynamicRegistration": false},
                        "definition": {"linkSupport": true},
                        "declaration": {"linkSupport": true},
                    },
                    "workspace": {"workspaceFolders": true, "configuration": true},
                },
            });
            if let Some(options) = initialization_options {
                params["initializationOptions"] = options;
            }
            let res = c
                .request("initialize", params, Duration::from_secs(60))
                .map(|_| c.notify("initialized", json!({})));
            let _ = c.ready.set(res);
        });
        Ok(client)
    }

    pub fn alive(&self) -> bool {
        self.child.lock().map(|mut c| matches!(c.try_wait(), Ok(None))).unwrap_or(false)
    }

    /// Prevent workers that still hold an Arc from sending after this client is replaced.
    pub fn retire(&self) {
        let mut pending = self.pending.lock().unwrap();
        self.retired.store(true, Ordering::Release);
        pending.clear();
    }

    fn configuration(&self, section: &Value) -> Value {
        if self.name == "pyright" && !self.ignored.is_empty() {
            // Pyright reads the parent section and enables autoSearchPaths when it is absent.
            let analysis = || json!({"exclude": self.ignored, "autoSearchPaths": true});
            match section.as_str() {
                Some("python") => return json!({"analysis": analysis()}),
                Some("python.analysis") => return analysis(),
                _ => {}
            }
        }
        Value::Null
    }

    fn send(&self, msg: &Value) {
        let body = msg.to_string();
        if let Ok(mut w) = self.stdin.lock()
            && !self.retired.load(Ordering::Acquire)
        {
            let _ = write!(w, "Content-Length: {}\r\n\r\n{body}", body.len());
            let _ = w.flush();
        }
    }

    fn notify(&self, method: &str, params: Value) {
        self.send(&json!({"jsonrpc": "2.0", "method": method, "params": params}));
    }

    fn request(&self, method: &str, params: Value, timeout: Duration) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = channel();
        {
            let mut pending = self.pending.lock().unwrap();
            if self.retired.load(Ordering::Acquire) {
                return Err(format!("{} was stopped", self.name));
            }
            pending.insert(id, tx);
        }
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        let res = rx.recv_timeout(timeout);
        self.pending.lock().unwrap().remove(&id);
        let msg = res.map_err(|_| format!("{} did not answer in time", self.name))?;
        if let Some(err) = msg.get("error") {
            return Err(err["message"].as_str().unwrap_or("error").to_string());
        }
        Ok(msg["result"].clone())
    }

    fn wait_ready(&self, timeout: Duration) -> Result<(), String> {
        let start = std::time::Instant::now();
        loop {
            if let Some(r) = self.ready.get() {
                return r.clone();
            }
            if start.elapsed() > timeout {
                return Err(format!("{} is still starting", self.name));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Make sure the server sees the current buffer contents.
    fn sync(&self, path: &Path, text: &str) {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        text.hash(&mut h);
        let hash = h.finish();
        let uri = path_to_uri(path);
        let mut docs = self.docs.lock().unwrap();
        match docs.get_mut(&uri) {
            Some(s) if s.text_hash == hash => {}
            Some(s) => {
                s.version += 1;
                s.text_hash = hash;
                self.notify(
                    "textDocument/didChange",
                    json!({"textDocument": {"uri": uri, "version": s.version}, "contentChanges": [{"text": text}]}),
                );
            }
            None => {
                self.notify(
                    "textDocument/didOpen",
                    json!({"textDocument": {"uri": uri, "languageId": self.language_id, "version": 1, "text": text}}),
                );
                docs.insert(
                    uri,
                    Synced {
                        version: 1,
                        text_hash: hash,
                    },
                );
            }
        }
    }

    /// Go to definition; falls back to declaration if the server returns nothing.
    pub fn definition(&self, path: &Path, text: &str, line: usize, col16: usize, timeout: Duration) -> Result<Vec<Location>, String> {
        if !self.ignored.is_empty()
            && workspace::relative_path(&self.root, path).is_some_and(|rel| workspace::is_ignored(&rel, &self.ignored))
        {
            return Ok(Vec::new());
        }
        self.wait_ready(timeout)?;
        self.sync(path, text);
        let params = json!({
            "textDocument": {"uri": path_to_uri(path)},
            "position": {"line": line, "character": col16},
        });
        let mut locs = parse_locations(&self.request("textDocument/definition", params.clone(), timeout)?);
        if locs.is_empty()
            && let Ok(v) = self.request("textDocument/declaration", params, timeout)
        {
            locs = parse_locations(&v);
        }
        Ok(locs)
    }
}

fn read_message(r: &mut impl BufRead) -> Option<Value> {
    let mut len = None;
    loop {
        let mut line = String::new();
        if r.read_line(&mut line).ok()? == 0 {
            return None; // server exited
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some(v) = line.strip_prefix("Content-Length:") {
            len = v.trim().parse::<usize>().ok();
        }
    }
    let mut body = vec![0; len?];
    r.read_exact(&mut body).ok()?;
    serde_json::from_slice(&body).ok()
}

/// Accepts `Location`, `Location[]`, `LocationLink[]` or null.
fn parse_locations(v: &Value) -> Vec<Location> {
    let items: Vec<&Value> = match v {
        Value::Array(a) => a.iter().collect(),
        Value::Object(_) => vec![v],
        _ => vec![],
    };
    items
        .into_iter()
        .filter_map(|item| {
            let uri = item.get("targetUri").or_else(|| item.get("uri"))?.as_str()?;
            let range = item.get("targetSelectionRange").or_else(|| item.get("range"))?;
            let start = &range["start"];
            let end = &range["end"];
            let line = start["line"].as_u64()? as usize;
            let c0 = start["character"].as_u64()? as usize;
            let c1 = if end["line"].as_u64() == Some(line as u64) {
                end["character"].as_u64()? as usize
            } else {
                c0
            };
            Some(Location {
                path: uri_to_path(uri)?,
                line,
                col16: c0..c1.max(c0),
            })
        })
        .collect()
}

pub fn path_to_uri(p: &Path) -> String {
    url::Url::from_file_path(p).map(|u| u.to_string()).unwrap_or_default()
}

fn uri_to_path(uri: &str) -> Option<PathBuf> {
    url::Url::parse(uri).ok()?.to_file_path().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_location_shapes() {
        let uri = path_to_uri(&std::env::temp_dir().join("x.rs"));
        let loc = json!({"uri": uri, "range": {"start": {"line": 3, "character": 4}, "end": {"line": 3, "character": 9}}});
        let link = json!([{"targetUri": uri, "targetRange": {}, "targetSelectionRange": {"start": {"line": 1, "character": 2}, "end": {"line": 1, "character": 5}}}]);
        let a = parse_locations(&loc);
        assert_eq!((a[0].line, a[0].col16.clone()), (3, 4..9));
        let b = parse_locations(&link);
        assert_eq!((b[0].line, b[0].col16.clone()), (1, 2..5));
        assert!(parse_locations(&Value::Null).is_empty());
    }

    #[test]
    fn reads_framed_messages() {
        let body = r#"{"jsonrpc":"2.0","id":1,"result":null}"#;
        let raw = format!("Content-Length: {}\r\nContent-Type: x\r\n\r\n{body}", body.len());
        let msg = read_message(&mut std::io::Cursor::new(raw)).unwrap();
        assert_eq!(msg["id"], 1);
    }
}
