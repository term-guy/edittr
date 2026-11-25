//! Language Server Protocol client: definitions, symbols and diagnostics.
//!
//! One server process per language per project folder, started lazily on first use.

pub(crate) mod call_hierarchy;
pub mod features;

use crate::diagnostics::{self, ByPath};
pub use crate::diagnostics::{Diagnostic, Severity};
use crate::{clangd, workspace};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

/// LSP error codes: the document changed / the server gave up on the request; both mean "ask again".
const CONTENT_MODIFIED: i64 = -32801;
const SERVER_CANCELLED: i64 = -32802;

pub struct ServerSpec {
    pub name: &'static str,
    pub command: &'static str,
    pub args: &'static [&'static str],
    pub language_id: &'static str,
}

impl ServerSpec {
    fn initialization_options(&self, ignored: &[String]) -> Option<Value> {
        match self.name {
            "rust-analyzer" => Some(json!({
                "files": {"excludeDirs": ignored},
                "cachePriming": {"enable": true},
                "cargo": {"targetDir": true},
            })),
            "gopls" => {
                // gopls advertises semantic tokens but sends none unless asked to.
                let mut options = json!({"semanticTokens": true});
                if !ignored.is_empty() {
                    let filters: Vec<_> = std::iter::once("-**/node_modules".to_owned())
                        .chain(ignored.iter().map(|dir| format!("-{dir}")))
                        .collect();
                    options["directoryFilters"] = json!(filters);
                }
                Some(options)
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
        "java" => vec![s("jdtls", "jdtls", &[][..], "java")],
        "rb" => vec![s("ruby-lsp", "ruby-lsp", &[][..], "ruby")],
        "php" => vec![s("intelephense", "intelephense", &["--stdio"][..], "php")],
        "lua" => vec![s("lua-language-server", "lua-language-server", &[][..], "lua")],
        "zig" => vec![s("zls", "zls", &[][..], "zig")],
        "cs" => vec![s("csharp-ls", "csharp-ls", &[][..], "csharp")],
        _ => clangd::language(ext)
            .map(|language| s("clangd", "clangd", clangd::ARGS, language))
            .into_iter()
            .collect(),
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

/// Document id and edit version, ordered across reopening the same file.
pub type Revision = (u64, u64);

type Position = (usize, usize);

pub(crate) struct Symbol {
    pub(crate) name: String,
    pub(crate) kind: &'static str,
    pub(crate) detail: Option<String>,
    pub(crate) range: Option<std::ops::Range<Position>>,
    pub(crate) container: Option<String>,
    pub(crate) location: Location,
    pub(crate) lines: std::ops::RangeInclusive<usize>,
    pub(crate) children: Vec<Symbol>,
}

struct Synced {
    version: i64,
    revision: Revision,
    text: Arc<str>,
}

#[derive(Clone, Copy)]
pub(crate) enum SyncKind {
    None,
    Full,
    Incremental,
}

impl SyncKind {
    fn from_initialize(result: &Value) -> Self {
        let sync = &result["capabilities"]["textDocumentSync"];
        match sync.as_u64().or_else(|| sync["change"].as_u64()) {
            Some(2) => Self::Incremental,
            Some(0) => Self::None,
            _ => Self::Full,
        }
    }
}

/// Shares a background launch between diagnostics and navigation.
#[derive(Default)]
pub struct Launch {
    result: OnceLock<Option<Arc<Client>>>,
    cancelled: AtomicBool,
}

impl Launch {
    pub fn finish(&self, client: Option<Arc<Client>>) {
        let _ = self.result.set(client);
        self.retire_if_cancelled();
    }

    pub fn wait(&self) -> Option<&Client> {
        let client = self.result.wait().as_deref()?;
        (!self.cancelled.load(Ordering::Acquire)).then_some(client)
    }

    pub fn get(&self) -> Option<&Option<Arc<Client>>> {
        self.result.get()
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    pub fn retire_if_cancelled(&self) {
        if self.is_cancelled()
            && let Some(Some(client)) = self.get()
        {
            client.retire();
        }
    }
}

#[derive(Eq, Hash, PartialEq)]
enum ProgressToken {
    Number(i64),
    String(String),
}

#[derive(Clone, Eq, Ord, PartialEq, PartialOrd)]
pub struct Progress {
    pub title: String,
    pub message: Option<String>,
    pub percentage: Option<u64>,
}

fn update_progress(active: &mut HashMap<ProgressToken, Progress>, params: &Value) -> bool {
    let token = &params["token"];
    let token = if let Some(number) = token.as_i64() {
        ProgressToken::Number(number)
    } else if let Some(string) = token.as_str() {
        ProgressToken::String(string.to_owned())
    } else {
        return false;
    };
    let value = &params["value"];
    let message = value["message"].as_str();
    let percentage = value["percentage"].as_u64();
    match value["kind"].as_str() {
        Some("begin") => {
            let Some(title) = value["title"].as_str() else { return false };
            active.insert(
                token,
                Progress {
                    title: title.to_owned(),
                    message: message.map(str::to_owned),
                    percentage,
                },
            );
            true
        }
        Some("report") => {
            let Some(progress) = active.get_mut(&token) else { return false };
            if let Some(message) = message {
                progress.message = Some(message.to_owned());
            }
            if percentage.is_some() {
                progress.percentage = percentage;
            }
            message.is_some() || percentage.is_some()
        }
        Some("end") => active.remove(&token).is_some(),
        _ => false,
    }
}

pub struct Client {
    pub name: &'static str,
    language_id: &'static str,
    root: PathBuf,
    ignored: Vec<String>,
    retired: AtomicBool,
    connected: AtomicBool,
    stdin: Mutex<ChildStdin>,
    child: Mutex<Child>,
    next_id: AtomicI64,
    pending: Arc<Mutex<HashMap<i64, Sender<Value>>>>,
    ready: OnceLock<Result<SyncKind, String>>,
    call_hierarchy: AtomicBool,
    progress: Mutex<HashMap<ProgressToken, Progress>>,
    sync_write: Mutex<()>,
    docs: Mutex<HashMap<PathBuf, Synced>>,
    diagnostics: Mutex<ByPath>,
    diagnostics_generation: AtomicU64,
    /// The server's `initialize` capabilities.
    capabilities: OnceLock<Value>,
    /// Diagnostics as the server sent them (code actions send them back).
    raw_diagnostics: Mutex<HashMap<PathBuf, Vec<Value>>>,
    /// Edits the server asked to apply (`workspace/applyEdit`) during a command.
    applied_edits: Mutex<Vec<Vec<features::FileEdit>>>,
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
    pub fn start(
        spec: &ServerSpec,
        launch: crate::bundled::Launch,
        root: &Path,
        ignored: &[String],
        repaint: Option<Arc<dyn Fn() + Send + Sync>>,
    ) -> Result<Arc<Client>, String> {
        let mut cmd = Command::new(&launch.exe);
        cmd.args(&launch.args)
            .args(spec.args)
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        if spec.name == "clangd" {
            let cores = std::thread::available_parallelism().map_or(1, usize::from);
            cmd.arg(format!("-j={}", (cores / 2).max(2)));
        }
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
            connected: AtomicBool::new(true),
            stdin: Mutex::new(stdin),
            child: Mutex::new(child),
            next_id: AtomicI64::new(1),
            pending: Arc::new(Mutex::new(HashMap::new())),
            ready: OnceLock::new(),
            call_hierarchy: AtomicBool::new(false),
            progress: Mutex::new(HashMap::new()),
            sync_write: Mutex::new(()),
            docs: Mutex::new(HashMap::new()),
            diagnostics: Mutex::new(HashMap::new()),
            diagnostics_generation: AtomicU64::new(0),
            capabilities: OnceLock::new(),
            raw_diagnostics: Mutex::new(HashMap::new()),
            applied_edits: Mutex::new(Vec::new()),
        });

        let weak = Arc::downgrade(&client);
        let pending = client.pending.clone();
        let reader_repaint = repaint.clone();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            while let Some(msg) = read_message(&mut reader) {
                let id = msg.get("id");
                let method = msg.get("method").and_then(Value::as_str);
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
                        } else if method == "workspace/applyEdit" {
                            // Collected by `execute_command`, which applies them in the editor.
                            let edits = features::parse_workspace_edit(&msg["params"]["edit"]);
                            c.applied_edits.lock().unwrap().push(edits);
                            json!({"applied": true})
                        } else {
                            // Includes window/workDoneProgress/create. Track the token when it begins.
                            Value::Null
                        };
                        c.send(&json!({"jsonrpc": "2.0", "id": id, "result": result}));
                    }
                    (None, Some("$/progress")) => {
                        let Some(c) = weak.upgrade() else { break };
                        let changed = update_progress(&mut c.progress.lock().unwrap(), &msg["params"]);
                        if changed && let Some(repaint) = &reader_repaint {
                            repaint();
                        }
                    }
                    (None, Some("textDocument/publishDiagnostics")) => {
                        let Some(c) = weak.upgrade() else { break };
                        if c.publish_diagnostics(&msg["params"])
                            && let Some(repaint) = &reader_repaint
                        {
                            repaint();
                        }
                    }
                    _ => {}
                }
            }
            if let Some(c) = weak.upgrade() {
                c.connected.store(false, Ordering::Release);
                pending.lock().unwrap().clear();
                if let Some(repaint) = reader_repaint {
                    repaint();
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
                    "window": {"workDoneProgress": true},
                    "textDocument": {
                        "synchronization": {"dynamicRegistration": false},
                        "definition": {"linkSupport": true},
                        "declaration": {"linkSupport": true},
                        "publishDiagnostics": {"versionSupport": true},
                        "documentSymbol": {"hierarchicalDocumentSymbolSupport": true},
                        "hover": {"contentFormat": ["plaintext", "markdown"]},
                        "completion": {"completionItem": {"snippetSupport": false, "documentationFormat": ["plaintext"]}},
                        "references": {},
                        "callHierarchy": {"dynamicRegistration": false},
                    },
                    "workspace": {"workspaceFolders": true, "configuration": true},
                },
            });
            let (text_document, workspace) = features::capabilities();
            for (section, extra) in [("textDocument", text_document), ("workspace", workspace)] {
                if let (Some(to), Value::Object(extra)) = (params["capabilities"][section].as_object_mut(), extra) {
                    to.extend(extra);
                }
            }
            if let Some(options) = initialization_options {
                params["initializationOptions"] = options;
            }
            let res = c.request("initialize", params, Duration::from_secs(60)).map(|result| {
                c.call_hierarchy.store(call_hierarchy::supported(&result), Ordering::Release);
                c.notify("initialized", json!({}));
                let _ = c.capabilities.set(result["capabilities"].clone());
                SyncKind::from_initialize(&result)
            });
            let _ = c.ready.set(res);
            if let Some(repaint) = repaint {
                repaint();
            }
        });
        Ok(client)
    }

    pub fn is_available(&self) -> bool {
        self.connected.load(Ordering::Acquire) && !self.retired.load(Ordering::Acquire) && !matches!(self.ready.get(), Some(Err(_)))
    }

    pub fn is_starting(&self) -> bool {
        self.is_available() && self.ready.get().is_none()
    }

    pub fn active_progress(&self) -> Vec<Progress> {
        if !self.is_available() {
            return Vec::new();
        }
        // A progress report can arrive while the UI is drawing. The reader will request another frame.
        let Ok(active) = self.progress.try_lock() else { return Vec::new() };
        let mut snapshot: Vec<_> = active.values().cloned().collect();
        drop(active);
        snapshot.sort_unstable();
        snapshot
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

    /// A request; answers saying the document or index changed while the server worked
    /// (rust-analyzer while it indexes) are retried until `timeout`, as the LSP spec suggests.
    fn request(&self, method: &str, params: Value, timeout: Duration) -> Result<Value, String> {
        // Commands can change things on the server: never send one twice.
        let retry = method != "workspace/executeCommand";
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            match self.request_once(method, params.clone(), left) {
                Err((Some(CONTENT_MODIFIED | SERVER_CANCELLED), _)) if retry && left > Duration::from_millis(250) => {
                    std::thread::sleep(Duration::from_millis(200));
                }
                result => return result.map_err(|(_, message)| message),
            }
        }
    }

    fn request_once(&self, method: &str, params: Value, timeout: Duration) -> Result<Value, (Option<i64>, String)> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = channel();
        {
            let mut pending = self.pending.lock().unwrap();
            if self.retired.load(Ordering::Acquire) || !self.connected.load(Ordering::Acquire) {
                return Err((None, format!("{} was stopped", self.name)));
            }
            pending.insert(id, tx);
        }
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        let res = rx.recv_timeout(timeout);
        self.pending.lock().unwrap().remove(&id);
        let msg = res.map_err(|_| (None, format!("{} did not answer in time", self.name)))?;
        if let Some(err) = msg.get("error") {
            return Err((err["code"].as_i64(), err["message"].as_str().unwrap_or("error").to_string()));
        }
        Ok(msg["result"].clone())
    }

    pub(crate) fn wait_ready(&self, timeout: Duration) -> Result<SyncKind, String> {
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

    pub fn diagnostics_generation(&self) -> u64 {
        self.diagnostics_generation.load(Ordering::Acquire)
    }

    pub(crate) fn diagnostics_snapshot(&self) -> ByPath {
        self.diagnostics.lock().unwrap().clone()
    }

    fn ignores(&self, path: &Path) -> bool {
        !self.ignored.is_empty() && workspace::relative_path(&self.root, path).is_some_and(|rel| workspace::is_ignored(&rel, &self.ignored))
    }

    fn publish_diagnostics(&self, params: &Value) -> bool {
        let Some(path) = params["uri"].as_str().and_then(uri_to_path) else {
            return false;
        };
        let Some(items) = params["diagnostics"].as_array() else {
            return false;
        };
        if self.ignores(&path) {
            return false;
        }
        let version = params["version"].as_i64();
        let stale = |doc: &Synced| version.is_some_and(|v| v < doc.version);
        let text = {
            let docs = self.docs.lock().unwrap();
            let doc = docs.get(&path);
            if doc.is_some_and(stale) {
                return false;
            }
            doc.map(|doc| doc.text.clone())
        };
        let disk = if text.is_none() && !items.is_empty() && path.is_file() {
            std::fs::read_to_string(&path).ok()
        } else {
            None
        };
        let diagnostics = diagnostics::parse(items, text.as_deref().or(disk.as_deref()));
        self.raw_diagnostics.lock().unwrap().insert(path.clone(), items.clone());
        let docs = self.docs.lock().unwrap();
        if docs.get(&path).is_some_and(stale) {
            return false;
        }
        let mut all = self.diagnostics.lock().unwrap();
        let changed = if diagnostics.is_empty() {
            all.remove(&path).is_some()
        } else if all.get(&path) == Some(&diagnostics) {
            false
        } else {
            all.insert(path, diagnostics);
            true
        };
        if changed {
            self.diagnostics_generation.fetch_add(1, Ordering::Release);
        }
        changed
    }

    pub fn sync_ready(&self, path: &Path, text: &str, revision: Revision) {
        if self.is_available()
            && let Some(Ok(kind)) = self.ready.get()
        {
            self.sync(path, text, revision, *kind);
        }
    }

    /// Serialize document notifications, releasing the document lock before writing to the pipe.
    fn sync(&self, path: &Path, text: &str, revision: Revision, kind: SyncKind) -> bool {
        let _write = self.sync_write.lock().unwrap();
        let uri = path_to_uri(path);
        let mut docs = self.docs.lock().unwrap();
        let (method, params) = match docs.get_mut(path) {
            Some(s) if s.revision > revision => return false,
            Some(s) if s.revision == revision => return true,
            Some(s) => {
                s.revision = revision;
                if s.text.as_ref() == text || matches!(kind, SyncKind::None) {
                    return true;
                }
                s.version += 1;
                let previous = std::mem::replace(&mut s.text, text.into());
                let version = s.version;
                drop(docs);
                let change = match kind {
                    SyncKind::Incremental => {
                        let line = previous.bytes().filter(|&b| b == b'\n').count();
                        let character = previous.rsplit('\n').next().unwrap_or_default().encode_utf16().count();
                        json!({"range": {"start": {"line": 0, "character": 0}, "end": {"line": line, "character": character}}, "text": text})
                    }
                    _ => json!({"text": text}),
                };
                (
                    "textDocument/didChange",
                    json!({"textDocument": {"uri": uri, "version": version}, "contentChanges": [change]}),
                )
            }
            None => {
                docs.insert(
                    path.to_path_buf(),
                    Synced {
                        version: 1,
                        revision,
                        text: text.into(),
                    },
                );
                drop(docs);
                let language_id = path
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .and_then(|ext| {
                        servers_for(&ext.to_ascii_lowercase())
                            .into_iter()
                            .find(|spec| spec.name == self.name)
                    })
                    .map_or(self.language_id, |spec| spec.language_id);
                (
                    "textDocument/didOpen",
                    json!({"textDocument": {"uri": uri, "languageId": language_id, "version": 1, "text": text}}),
                )
            }
        };
        self.notify(method, params);
        true
    }

    /// Go to definition; falls back to declaration if the server returns nothing.
    pub fn definition(
        &self,
        path: &Path,
        text: &str,
        revision: Revision,
        (line, col16): (usize, usize),
        timeout: Duration,
    ) -> Result<Vec<Location>, String> {
        if self.ignores(path) {
            return Ok(Vec::new());
        }
        let kind = self.wait_ready(timeout)?;
        if !self.sync(path, text, revision, kind) {
            return Err("The document changed".into());
        }
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

    /// Sync the document and send a request at a position.
    #[allow(clippy::too_many_arguments)]
    fn at_position(
        &self,
        method: &str,
        path: &Path,
        text: &str,
        revision: Revision,
        (line, col16): (usize, usize),
        extra: Value,
        timeout: Duration,
    ) -> Result<Value, String> {
        if self.ignores(path) {
            return Ok(Value::Null);
        }
        let kind = self.wait_ready(timeout)?;
        if !self.sync(path, text, revision, kind) {
            return Err("The document changed".into());
        }
        let mut params = json!({
            "textDocument": {"uri": path_to_uri(path)},
            "position": {"line": line, "character": col16},
        });
        if let (Some(p), Value::Object(extra)) = (params.as_object_mut(), extra) {
            p.extend(extra);
        }
        self.request(method, params, timeout)
    }

    /// Hover text (markdown or plain) at a position.
    pub fn hover(
        &self,
        path: &Path,
        text: &str,
        revision: Revision,
        pos: (usize, usize),
        timeout: Duration,
    ) -> Result<Option<String>, String> {
        let v = self.at_position("textDocument/hover", path, text, revision, pos, Value::Null, timeout)?;
        Ok(parse_hover(&v["contents"]))
    }

    pub fn completion(
        &self,
        path: &Path,
        text: &str,
        revision: Revision,
        pos: (usize, usize),
        timeout: Duration,
    ) -> Result<Vec<Completion>, String> {
        let v = self.at_position("textDocument/completion", path, text, revision, pos, Value::Null, timeout)?;
        Ok(parse_completions(&v))
    }

    pub fn references(
        &self,
        path: &Path,
        text: &str,
        revision: Revision,
        pos: (usize, usize),
        timeout: Duration,
    ) -> Result<Vec<Location>, String> {
        let extra = json!({"context": {"includeDeclaration": true}});
        let v = self.at_position("textDocument/references", path, text, revision, pos, extra, timeout)?;
        Ok(parse_locations(&v))
    }

    pub(crate) fn prepare_call_hierarchy(
        &self,
        path: &Path,
        text: &str,
        revision: Revision,
        pos: (usize, usize),
        timeout: Duration,
    ) -> Result<Vec<call_hierarchy::Item>, String> {
        self.wait_ready(timeout)?;
        if !self.call_hierarchy.load(Ordering::Acquire) {
            return Err(format!("{} does not support call hierarchy", self.name));
        }
        let result = self.at_position("textDocument/prepareCallHierarchy", path, text, revision, pos, Value::Null, timeout)?;
        Ok(call_hierarchy::parse_roots(result))
    }

    pub(crate) fn hierarchy_calls(
        &self,
        item: Value,
        direction: call_hierarchy::Direction,
        timeout: Duration,
    ) -> Result<Vec<call_hierarchy::Item>, String> {
        let result = self.request(direction.method(), json!({"item": item}), timeout)?;
        Ok(call_hierarchy::parse_calls(result, direction))
    }

    pub(crate) fn switch_source_header(&self, path: &Path, text: &str, revision: Revision, timeout: Duration) -> Option<PathBuf> {
        let kind = self.wait_ready(timeout).ok()?;
        self.sync(path, text, revision, kind).then_some(())?;
        // This clangd extension takes a TextDocumentIdentifier directly.
        let result = self
            .request("textDocument/switchSourceHeader", json!({"uri": path_to_uri(path)}), timeout)
            .ok()?;
        result.as_str().and_then(uri_to_path)
    }

    pub(crate) fn document_symbols(&self, path: &Path, text: &str, revision: Revision, timeout: Duration) -> Result<Vec<Symbol>, String> {
        let kind = self.wait_ready(timeout)?;
        if !self.sync(path, text, revision, kind) {
            return Err("The document changed".into());
        }
        let params = json!({"textDocument": {"uri": path_to_uri(path)}});
        let result = self.request("textDocument/documentSymbol", params, timeout)?;
        Ok(parse_symbols(&result, Some(path)))
    }

    pub(crate) fn workspace_symbols(&self, query: &str, timeout: Duration) -> Result<Vec<Symbol>, String> {
        self.wait_ready(timeout)?;
        let result = self.request("workspace/symbol", json!({"query": query}), timeout)?;
        Ok(parse_symbols(&result, None))
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
/// A completion suggestion.
#[derive(Clone, Debug, PartialEq)]
pub struct Completion {
    pub label: String,
    /// Kind name (Function, Variable, ...) or empty.
    pub kind: &'static str,
    pub detail: String,
    /// Text to insert in place of the word before the cursor.
    pub insert: String,
    /// What to match the typed prefix against.
    pub filter: String,
    pub sort: String,
}

fn completion_kind(n: u64) -> &'static str {
    match n {
        2 => "Method",
        3 => "Function",
        4 => "Constructor",
        5 => "Field",
        6 => "Variable",
        7 => "Class",
        8 => "Interface",
        9 => "Module",
        10 => "Property",
        13 => "Enum",
        14 => "Keyword",
        15 => "Snippet",
        20 => "EnumMember",
        21 => "Constant",
        22 => "Struct",
        25 => "TypeParameter",
        _ => "",
    }
}

/// Snippet syntax to plain text: `${1:x}` -> `x`, `$0` / `$1` -> nothing.
fn strip_snippet(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                if let Some(n) = chars.next() {
                    out.push(n);
                }
            }
            '$' if chars.peek().is_some_and(|c| c.is_ascii_digit()) => {
                while chars.peek().is_some_and(|c| c.is_ascii_digit()) {
                    chars.next();
                }
            }
            '$' if chars.peek() == Some(&'{') => {
                chars.next();
                while chars.peek().is_some_and(|c| c.is_ascii_digit()) {
                    chars.next();
                }
                if chars.peek() == Some(&':') {
                    chars.next();
                }
                let mut depth = 1;
                for c in chars.by_ref() {
                    match c {
                        '{' => depth += 1,
                        '}' => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        _ => {}
                    }
                    if depth > 0 {
                        out.push(c);
                    }
                }
            }
            c => out.push(c),
        }
    }
    out
}

pub(crate) fn parse_completions(v: &Value) -> Vec<Completion> {
    let items = v.get("items").unwrap_or(v);
    let Some(items) = items.as_array() else { return Vec::new() };
    items
        .iter()
        .filter_map(|it| {
            // clangd marks items that would also add an #include with a leading bullet.
            let label = it["label"].as_str()?.trim().trim_start_matches(['•', '·']).trim().to_string();
            let snippet = it["insertTextFormat"].as_u64() == Some(2);
            let raw = it["textEdit"]["newText"]
                .as_str()
                .or_else(|| it["insertText"].as_str())
                .unwrap_or(&label);
            let insert = if snippet { strip_snippet(raw) } else { raw.to_string() };
            Some(Completion {
                kind: completion_kind(it["kind"].as_u64().unwrap_or(0)),
                detail: it["detail"].as_str().unwrap_or_default().trim().to_string(),
                filter: it["filterText"].as_str().unwrap_or(&label).to_string(),
                sort: it["sortText"].as_str().unwrap_or(&label).to_string(),
                insert,
                label,
            })
        })
        .collect()
}

/// Hover contents (MarkupContent, MarkedString or an array of them) as plain text.
pub(crate) fn parse_hover(v: &Value) -> Option<String> {
    let part = |v: &Value| -> Option<String> {
        match v {
            Value::String(s) => Some(s.clone()),
            Value::Object(o) => o
                .get("value")
                .and_then(Value::as_str)
                .map(|s| match o.get("language").and_then(Value::as_str) {
                    Some(_) => s.to_string(),
                    None => s.to_string(),
                }),
            _ => None,
        }
    };
    let text = match v {
        Value::Array(a) => a.iter().filter_map(part).collect::<Vec<_>>().join("\n\n"),
        v => part(v)?,
    };
    // Markdown code fences and rules are noise in a plain tooltip.
    let cleaned: Vec<&str> = text
        .lines()
        .filter(|l| !l.trim_start().starts_with("```") && l.trim() != "---")
        .collect();
    let cleaned = cleaned.join("\n").trim().to_string();
    (!cleaned.is_empty()).then_some(cleaned)
}

fn parse_locations(v: &Value) -> Vec<Location> {
    let items: Vec<&Value> = match v {
        Value::Array(a) => a.iter().collect(),
        Value::Object(_) => vec![v],
        _ => vec![],
    };
    items.into_iter().filter_map(parse_location).collect()
}

fn parse_location(item: &Value) -> Option<Location> {
    let uri = item.get("targetUri").or_else(|| item.get("uri"))?.as_str()?;
    let range = item.get("targetSelectionRange").or_else(|| item.get("range"))?;
    location_at(uri_to_path(uri)?, range)
}

fn location_at(path: PathBuf, range: &Value) -> Option<Location> {
    let start = &range["start"];
    let end = &range["end"];
    let line = usize::try_from(start["line"].as_u64()?).ok()?;
    let c0 = usize::try_from(start["character"].as_u64()?).ok()?;
    let c1 = if end["line"].as_u64() == Some(line as u64) {
        usize::try_from(end["character"].as_u64()?).ok()?
    } else {
        c0
    };
    Some(Location {
        path,
        line,
        col16: c0..c1.max(c0),
    })
}

/// DocumentSymbol trees and the flat SymbolInformation / WorkspaceSymbol forms.
fn parse_symbols(value: &Value, document: Option<&Path>) -> Vec<Symbol> {
    let mut out = Vec::new();
    collect_symbols(value, document, None, &mut out);
    out
}

fn collect_symbols(value: &Value, document: Option<&Path>, parent: Option<&str>, out: &mut Vec<Symbol>) {
    let Some(items) = value.as_array() else { return };
    for item in items {
        let name = item["name"].as_str();
        let location = match item.get("location") {
            Some(location) => parse_location(location),
            None => document.and_then(|path| location_at(path.to_path_buf(), &item["selectionRange"])),
        };
        if let (Some(name), Some(location)) = (name, location) {
            let range = item.get("range").unwrap_or(&item["location"]["range"]);
            let mut children = Vec::new();
            collect_symbols(&item["children"], document, Some(name), &mut children);
            out.push(Symbol {
                name: name.to_owned(),
                kind: symbol_kind(item["kind"].as_u64()),
                detail: document.and_then(|_| item["detail"].as_str()).map(str::to_owned),
                range: document.and_then(|_| Some(symbol_position(&range["start"])?..symbol_position(&range["end"])?)),
                container: item["containerName"].as_str().or(parent).map(str::to_owned),
                lines: symbol_lines(range, location.line),
                location,
                children,
            });
        } else {
            collect_symbols(&item["children"], document, name.or(parent), out);
        }
    }
}

fn symbol_position(value: &Value) -> Option<Position> {
    Some((
        usize::try_from(value["line"].as_u64()?).ok()?,
        usize::try_from(value["character"].as_u64()?).ok()?,
    ))
}

fn symbol_lines(range: &Value, selection_line: usize) -> std::ops::RangeInclusive<usize> {
    let start = range["start"]["line"]
        .as_u64()
        .and_then(|n| usize::try_from(n).ok())
        .unwrap_or(selection_line);
    let mut end = range["end"]["line"]
        .as_u64()
        .and_then(|n| usize::try_from(n).ok())
        .unwrap_or(selection_line);
    if end > start && range["end"]["character"].as_u64() == Some(0) {
        end -= 1; // LSP positions are end-exclusive.
    }
    start.min(selection_line)..=end.max(selection_line)
}

fn symbol_kind(kind: Option<u64>) -> &'static str {
    const KINDS: &[&str] = &[
        "Symbol",
        "File",
        "Module",
        "Namespace",
        "Package",
        "Class",
        "Method",
        "Property",
        "Field",
        "Constructor",
        "Enum",
        "Interface",
        "Function",
        "Variable",
        "Constant",
        "String",
        "Number",
        "Boolean",
        "Array",
        "Object",
        "Key",
        "Null",
        "Enum member",
        "Struct",
        "Event",
        "Operator",
        "Type parameter",
    ];
    kind.and_then(|n| usize::try_from(n).ok())
        .and_then(|i| KINDS.get(i).copied())
        .unwrap_or(KINDS[0])
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
    fn parses_nested_document_symbols_at_selection_ranges() {
        let file = std::env::temp_dir().join("model.rs");
        let symbols = parse_symbols(
            &json!([{
                "name": "Model", "kind": 23,
                "range": {"start": {"line": 0, "character": 0}, "end": {"line": 6, "character": 1}},
                "selectionRange": {"start": {"line": 0, "character": 7}, "end": {"line": 0, "character": 12}},
                "children": [{
                    "name": "field", "kind": 8,
                    "selectionRange": {"start": {"line": 2, "character": 6}, "end": {"line": 2, "character": 11}}
                }]
            }]),
            Some(&file),
        );
        assert_eq!(symbols.len(), 1);
        assert_eq!((&*symbols[0].name, symbols[0].kind), ("Model", "Struct"));
        assert_eq!(symbols[0].location.col16, 7..12);
        assert_eq!(symbols[0].lines, 0..=6);
        let [field] = symbols[0].children.as_slice() else {
            panic!("expected one child")
        };
        assert_eq!(field.container.as_deref(), Some("Model"));
        assert_eq!(field.location.path, file);
        assert_eq!((field.location.line, field.location.col16.clone()), (2, 6..11));
        assert_eq!(field.lines, 2..=2);
    }

    #[test]
    fn parses_flat_document_and_workspace_symbols_with_locations() {
        let file = std::env::temp_dir().join("from server.rs");
        let result = json!([
            {
                "name": "read", "kind": 12, "containerName": "io",
                "location": {
                    "uri": path_to_uri(&file),
                    "range": {"start": {"line": 5, "character": 3}, "end": {"line": 7, "character": 0}}
                }
            },
            {"name": "unresolved", "kind": 99, "location": {"uri": path_to_uri(&file)}},
            {"name": "invalid", "location": {"uri": "https://example.com/other.rs"}}
        ]);
        let document = std::env::temp_dir().join("current.rs");
        for context in [None, Some(document.as_path())] {
            let symbols = parse_symbols(&result, context);
            assert_eq!(symbols.len(), 1);
            assert_eq!(
                (&*symbols[0].name, symbols[0].kind, symbols[0].container.as_deref()),
                ("read", "Function", Some("io"))
            );
            assert_eq!(symbols[0].location.path, file);
            assert_eq!((symbols[0].location.line, symbols[0].location.col16.clone()), (5, 3..3));
            assert_eq!(symbols[0].lines, 5..=6);
        }
        assert!(parse_symbols(&Value::Null, None).is_empty());
    }

    #[test]
    fn reads_framed_messages() {
        let body = r#"{"jsonrpc":"2.0","id":1,"result":null}"#;
        let raw = format!("Content-Length: {}\r\nContent-Type: x\r\n\r\n{body}", body.len());
        let msg = read_message(&mut std::io::Cursor::new(raw)).unwrap();
        assert_eq!(msg["id"], 1);
    }

    #[test]
    #[ignore = "requires clangd on PATH"]
    fn clangd_publishes_and_clears_diagnostics() {
        let dir = std::env::temp_dir().join(format!("edittr-clangd-diagnostics-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dir = dir.canonicalize().unwrap();
        let path = dir.join("main.c");
        let broken = "int main(void) { /* 😀 */ return missing; }\n";
        std::fs::write(&path, broken).unwrap();

        let spec = &servers_for("c")[0];
        let command = resolve(spec.command).expect("clangd is not installed");
        let (tx, rx) = channel();
        let client = Client::start(
            spec,
            command,
            &dir,
            &[],
            Some(Arc::new(move || {
                let _ = tx.send(());
            })),
        )
        .unwrap();
        client.wait_ready(Duration::from_secs(10)).unwrap();
        let wait_for = |generation| {
            let start = std::time::Instant::now();
            while client.diagnostics_generation() <= generation {
                let remaining = Duration::from_secs(10).saturating_sub(start.elapsed());
                rx.recv_timeout(remaining).expect("no diagnostics publication");
            }
        };

        client.sync_ready(&path, broken, (0, 0));
        wait_for(0);
        let snapshot = client.diagnostics_snapshot();
        let diagnostic = snapshot[&path].iter().find(|d| d.message.contains("missing")).unwrap();
        assert_eq!(diagnostic.col_start, broken.split("missing").next().unwrap().chars().count());

        let generation = client.diagnostics_generation();
        client.sync_ready(&path, "int main(void) { return 0; }\n", (0, 1));
        wait_for(generation);
        assert!(!client.diagnostics_snapshot().contains_key(&path));
        drop(client);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn parses_hover_and_completions() {
        assert_eq!(
            parse_hover(&json!({"kind": "markdown", "value": "```rust\nfn f()\n```\n---\nDocs"})).as_deref(),
            Some("fn f()\nDocs")
        );
        assert_eq!(
            parse_hover(&json!([{"language": "c", "value": "int x"}, "note"])).as_deref(),
            Some("int x\n\nnote")
        );
        assert_eq!(parse_hover(&json!("")), None);
        let items = parse_completions(&json!({"isIncomplete": false, "items": [
            {"label": "push", "kind": 2, "detail": "fn(&mut self, T)", "insertTextFormat": 2, "insertText": "push(${1:value})$0"},
            {"label": " len", "textEdit": {"newText": "len", "range": {}}},
        ]}));
        assert_eq!(items[0].insert, "push(value)");
        assert_eq!(items[0].kind, "Method");
        assert_eq!(items[1].label, "len");
        assert_eq!(items[1].insert, "len");
    }

    #[test]
    fn tracks_work_done_progress_by_token() {
        let mut active = HashMap::new();
        assert!(!update_progress(
            &mut active,
            &json!({"token": 1, "value": {"kind": "report", "percentage": 20}})
        ));
        assert!(update_progress(
            &mut active,
            &json!({"token": 1, "value": {"kind": "begin", "title": "Indexing", "message": "Scanning", "percentage": 0}})
        ));
        assert!(update_progress(
            &mut active,
            &json!({"token": "1", "value": {"kind": "begin", "title": "Checking"}})
        ));
        assert!(update_progress(
            &mut active,
            &json!({"token": 1, "value": {"kind": "report", "percentage": 45}})
        ));
        let indexing = &active[&ProgressToken::Number(1)];
        assert_eq!(indexing.title, "Indexing");
        assert_eq!(indexing.message.as_deref(), Some("Scanning"));
        assert_eq!(indexing.percentage, Some(45));
        assert!(update_progress(&mut active, &json!({"token": 1, "value": {"kind": "end"}})));
        assert_eq!(active.len(), 1);
        assert_eq!(active[&ProgressToken::String("1".into())].title, "Checking");
    }
}
