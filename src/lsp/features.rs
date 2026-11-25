//! Editing features from the language server: rename, code actions, signature help,
//! document highlights and semantic tokens. Requests block their (background) caller.

use super::{Client, Revision, path_to_uri, uri_to_path};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// LSP position: 0-based line and UTF-16 column.
pub type Pos16 = (usize, usize);
/// A start and end position.
pub type Range16 = (Pos16, Pos16);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextEdit {
    pub start: Pos16,
    pub end: Pos16,
    pub new_text: String,
}

/// The edits of a WorkspaceEdit for one file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileEdit {
    pub path: PathBuf,
    pub edits: Vec<TextEdit>,
}

#[derive(Clone, Debug)]
pub struct CodeAction {
    pub title: String,
    pub kind: String,
    pub preferred: bool,
    /// Why it can't run right now (`disabled.reason`).
    pub disabled: Option<String>,
    pub edit: Option<Vec<FileEdit>>,
    /// A command to run on the server (after the edit, if any).
    pub command: Option<Value>,
    /// The action as the server sent it (for `codeAction/resolve`).
    pub raw: Value,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Signature {
    pub label: String,
    /// Byte ranges of the parameters in `label`.
    pub params: Vec<std::ops::Range<usize>>,
    pub active_param: Option<usize>,
    pub doc: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignatureHelp {
    pub signatures: Vec<Signature>,
    pub active: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HighlightKind {
    Text,
    Read,
    Write,
}

/// A semantic token: line, UTF-16 start and length, type and modifier names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SemanticToken {
    pub line: usize,
    pub start16: usize,
    pub len16: usize,
    pub kind: String,
    pub modifiers: Vec<String>,
}

fn pos(v: &Value) -> Option<Pos16> {
    Some((v["line"].as_u64()? as usize, v["character"].as_u64()? as usize))
}

fn range(v: &Value) -> Option<(Pos16, Pos16)> {
    Some((pos(&v["start"])?, pos(&v["end"])?))
}

fn text_edits(v: &Value) -> Vec<TextEdit> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|e| {
                    let (start, end) = range(&e["range"])?;
                    // Snippet edits (`insertTextFormat`) are rare in workspace edits; take the text as is.
                    let new_text = e["newText"].as_str()?.to_string();
                    Some(TextEdit { start, end, new_text })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The text edits of a WorkspaceEdit (`changes` and/or `documentChanges`), grouped per
/// file in first-seen order. File create/rename/delete operations are skipped.
pub fn parse_workspace_edit(v: &Value) -> Vec<FileEdit> {
    let mut out: Vec<FileEdit> = Vec::new();
    let mut add = |path: PathBuf, edits: Vec<TextEdit>| {
        if edits.is_empty() {
            return;
        }
        match out.iter_mut().find(|f| f.path == path) {
            Some(f) => f.edits.extend(edits),
            None => out.push(FileEdit { path, edits }),
        }
    };
    if let Some(changes) = v["documentChanges"].as_array() {
        for c in changes {
            if c.get("kind").is_some() {
                continue; // create / rename / delete file
            }
            if let Some(path) = c["textDocument"]["uri"].as_str().and_then(uri_to_path) {
                add(path, text_edits(&c["edits"]));
            }
        }
    } else if let Some(changes) = v["changes"].as_object() {
        for (uri, edits) in changes {
            if let Some(path) = uri_to_path(uri) {
                add(path, text_edits(edits));
            }
        }
    }
    out
}

/// Code actions and bare commands from a `textDocument/codeAction` result.
/// Kinds that belong in the quick fix menu. Servers also list "source" actions that browse
/// documentation, show assembly or split packages (gopls offers seven of those on any line).
fn quick_fix_menu_kind(kind: &str) -> bool {
    let family = |prefix: &str| kind == prefix || kind.starts_with(&format!("{prefix}."));
    kind.is_empty() || family("quickfix") || family("refactor") || family("source.organizeImports") || family("source.fixAll")
}

pub fn parse_code_actions(v: &Value) -> Vec<CodeAction> {
    let Some(items) = v.as_array() else { return Vec::new() };
    items
        .iter()
        .filter_map(|item| {
            let title = item["title"].as_str()?.to_string();
            // A bare Command has a string `command`; a CodeAction has an object (or none).
            if item["command"].is_string() {
                return Some(CodeAction {
                    title,
                    kind: String::new(),
                    preferred: false,
                    disabled: None,
                    edit: None,
                    command: Some(item.clone()),
                    raw: item.clone(),
                });
            }
            Some(CodeAction {
                title,
                kind: item["kind"].as_str().unwrap_or_default().to_string(),
                preferred: item["isPreferred"].as_bool().unwrap_or(false),
                disabled: item["disabled"]["reason"].as_str().map(str::to_string),
                edit: item.get("edit").filter(|e| e.is_object()).map(parse_workspace_edit),
                command: item.get("command").filter(|c| c.is_object()).cloned(),
                raw: item.clone(),
            })
        })
        .collect()
}

/// Byte range of a parameter label in the signature label: either a substring or
/// `[start, end]` UTF-16 offsets.
fn param_range(label: &str, p: &Value, from: usize) -> Option<std::ops::Range<usize>> {
    match &p["label"] {
        Value::String(s) if !s.is_empty() => {
            let at = label[from.min(label.len())..].find(s.as_str())? + from;
            Some(at..at + s.len())
        }
        Value::Array(a) => {
            let to_byte = |c16: u64| {
                let mut n = 0u64;
                for (i, ch) in label.char_indices() {
                    if n >= c16 {
                        return i;
                    }
                    n += ch.len_utf16() as u64;
                }
                label.len()
            };
            Some(to_byte(a.first()?.as_u64()?)..to_byte(a.get(1)?.as_u64()?))
        }
        _ => None,
    }
}

fn documentation(v: &Value) -> Option<String> {
    let s = match v {
        Value::String(s) => s.clone(),
        Value::Object(_) => v["value"].as_str()?.to_string(),
        _ => return None,
    };
    let s = s.trim().to_string();
    (!s.is_empty()).then_some(s)
}

pub fn parse_signature_help(v: &Value) -> Option<SignatureHelp> {
    let sigs = v["signatures"].as_array()?;
    let outer_param = v["activeParameter"].as_u64().map(|n| n as usize);
    let signatures: Vec<Signature> = sigs
        .iter()
        .filter_map(|s| {
            let label = s["label"].as_str()?.to_string();
            let mut from = 0;
            let params = s["parameters"]
                .as_array()
                .map(|ps| {
                    ps.iter()
                        .filter_map(|p| {
                            let r = param_range(&label, p, from)?;
                            from = r.end;
                            Some(r)
                        })
                        .collect()
                })
                .unwrap_or_default();
            let active_param = s["activeParameter"].as_u64().map(|n| n as usize).or(outer_param);
            Some(Signature {
                label,
                params,
                active_param,
                doc: documentation(&s["documentation"]),
            })
        })
        .collect();
    if signatures.is_empty() {
        return None;
    }
    let active = (v["activeSignature"].as_u64().unwrap_or(0) as usize).min(signatures.len() - 1);
    Some(SignatureHelp { signatures, active })
}

pub fn parse_document_highlights(v: &Value) -> Vec<((Pos16, Pos16), HighlightKind)> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|h| {
                    let kind = match h["kind"].as_u64() {
                        Some(2) => HighlightKind::Read,
                        Some(3) => HighlightKind::Write,
                        _ => HighlightKind::Text,
                    };
                    Some((range(&h["range"])?, kind))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Decode `data` (5 integers per token, positions relative to the previous token).
pub fn decode_semantic_tokens(data: &[u64], types: &[String], modifiers: &[String]) -> Vec<SemanticToken> {
    let mut out = Vec::with_capacity(data.len() / 5);
    let (mut line, mut start) = (0usize, 0usize);
    for t in data.as_chunks::<5>().0 {
        let (dl, ds) = (t[0] as usize, t[1] as usize);
        if dl > 0 {
            line += dl;
            start = ds;
        } else {
            start += ds;
        }
        let Some(kind) = types.get(t[3] as usize) else { continue };
        let mods = modifiers
            .iter()
            .enumerate()
            .filter(|(i, _)| *i < 64 && t[4] & (1 << i) != 0)
            .map(|(_, m)| m.clone())
            .collect();
        out.push(SemanticToken {
            line,
            start16: start,
            len16: t[2] as usize,
            kind: kind.clone(),
            modifiers: mods,
        });
    }
    out
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|a| a.iter().filter_map(|s| s.as_str().map(str::to_string)).collect())
        .unwrap_or_default()
}

/// Token types this client understands (all of LSP's standard ones).
pub(super) const TOKEN_TYPES: &[&str] = &[
    "namespace",
    "type",
    "class",
    "enum",
    "interface",
    "struct",
    "typeParameter",
    "parameter",
    "variable",
    "property",
    "enumMember",
    "event",
    "function",
    "method",
    "macro",
    "keyword",
    "modifier",
    "comment",
    "string",
    "number",
    "regexp",
    "operator",
    "decorator",
];

pub(super) const TOKEN_MODIFIERS: &[&str] = &[
    "declaration",
    "definition",
    "readonly",
    "static",
    "deprecated",
    "abstract",
    "async",
    "modification",
    "documentation",
    "defaultLibrary",
];

/// Client capabilities for these features, merged into `initialize`.
pub(super) fn capabilities() -> (Value, Value) {
    let text_document = json!({
        "rename": {"prepareSupport": true},
        "codeAction": {
            "codeActionLiteralSupport": {"codeActionKind": {"valueSet": [
                "", "quickfix", "refactor", "refactor.extract", "refactor.inline", "refactor.rewrite",
                "source", "source.organizeImports", "source.fixAll"
            ]}},
            "isPreferredSupport": true,
            "disabledSupport": true,
            "dataSupport": true,
            "resolveSupport": {"properties": ["edit"]},
        },
        "signatureHelp": {"signatureInformation": {
            "documentationFormat": ["plaintext", "markdown"],
            "parameterInformation": {"labelOffsetSupport": true},
            "activeParameterSupport": true,
        }},
        "documentHighlight": {},
        "semanticTokens": {
            "requests": {"full": true},
            "tokenTypes": TOKEN_TYPES,
            "tokenModifiers": TOKEN_MODIFIERS,
            "formats": ["relative"],
            "multilineTokenSupport": false,
            "overlappingTokenSupport": false,
        },
    });
    let workspace = json!({
        "applyEdit": true,
        "workspaceEdit": {"documentChanges": true},
        "executeCommand": {},
    });
    (text_document, workspace)
}

/// A diagnostic on any line of the range: like VS Code's lightbulb, a quick fix is
/// offered anywhere on the line (clang reports a missing `;` at the next token).
fn on_lines(diagnostic: (Pos16, Pos16), range: (Pos16, Pos16)) -> bool {
    diagnostic.0.0 <= range.1.0 && range.0.0 <= diagnostic.1.0
}

impl Client {
    fn capability(&self, name: &str) -> Value {
        self.capabilities.get().map(|c| c[name].clone()).unwrap_or(Value::Null)
    }

    fn supports(&self, name: &str) -> bool {
        !matches!(self.capability(name), Value::Null | Value::Bool(false))
    }

    /// Sync the document, then send a request about it.
    fn about_document(
        &self,
        method: &str,
        path: &Path,
        text: &str,
        revision: Revision,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, String> {
        let kind = self.wait_ready(timeout)?;
        if !self.sync(path, text, revision, kind) {
            return Err("The document changed".into());
        }
        let mut p = json!({"textDocument": {"uri": path_to_uri(path)}});
        if let (Some(p), Value::Object(extra)) = (p.as_object_mut(), params) {
            p.extend(extra);
        }
        self.request(method, p, timeout)
    }

    /// The range and current name of the symbol to rename (None: the server doesn't say).
    pub fn prepare_rename(
        &self,
        path: &Path,
        text: &str,
        revision: Revision,
        at: Pos16,
        timeout: Duration,
    ) -> Result<Option<(Range16, Option<String>)>, String> {
        self.wait_ready(timeout)?;
        if !self.capability("renameProvider")["prepareProvider"].as_bool().unwrap_or(false) {
            return Ok(None);
        }
        let v = self.at_position("textDocument/prepareRename", path, text, revision, at, Value::Null, timeout)?;
        if v.is_null() {
            return Err("This symbol can't be renamed".into());
        }
        let r = range(&v).or_else(|| range(&v["range"]));
        Ok(r.map(|r| (r, v["placeholder"].as_str().map(str::to_string))))
    }

    pub fn rename(
        &self,
        path: &Path,
        text: &str,
        revision: Revision,
        at: Pos16,
        new_name: &str,
        timeout: Duration,
    ) -> Result<Vec<FileEdit>, String> {
        self.wait_ready(timeout)?;
        if !self.supports("renameProvider") {
            return Err(format!("{} can't rename symbols", self.name));
        }
        let v = self.at_position(
            "textDocument/rename",
            path,
            text,
            revision,
            at,
            json!({"newName": new_name}),
            timeout,
        )?;
        Ok(parse_workspace_edit(&v))
    }

    /// Code actions for a range; the server's diagnostics overlapping it go in the context.
    pub fn code_actions(
        &self,
        path: &Path,
        text: &str,
        revision: Revision,
        sel: (Pos16, Pos16),
        timeout: Duration,
    ) -> Result<Vec<CodeAction>, String> {
        self.wait_ready(timeout)?;
        if !self.supports("codeActionProvider") {
            return Err(format!("{} has no code actions", self.name));
        }
        let diagnostics: Vec<Value> = self
            .raw_diagnostics
            .lock()
            .unwrap()
            .get(path)
            .map(|items| {
                items
                    .iter()
                    .filter(|d| range(&d["range"]).is_some_and(|r| on_lines(r, sel)))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        let params = json!({
            "range": {"start": {"line": sel.0.0, "character": sel.0.1}, "end": {"line": sel.1.0, "character": sel.1.1}},
            "context": {"diagnostics": diagnostics, "triggerKind": 1},
        });
        let v = self.about_document("textDocument/codeAction", path, text, revision, params, timeout)?;
        let mut actions = parse_code_actions(&v);
        actions.retain(|a| quick_fix_menu_kind(&a.kind));
        // Preferred (usually the fix of the diagnostic) first, then quick fixes.
        actions.sort_by_key(|a| (!a.preferred, !a.kind.starts_with("quickfix")));
        Ok(actions)
    }

    /// Fill in a code action's edit when the server sends it lazily.
    pub fn resolve_code_action(&self, action: CodeAction, timeout: Duration) -> CodeAction {
        if action.edit.is_some() || !self.capability("codeActionProvider")["resolveProvider"].as_bool().unwrap_or(false) {
            return action;
        }
        match self.request("codeAction/resolve", action.raw.clone(), timeout) {
            Ok(v) => parse_code_actions(&Value::Array(vec![v])).pop().unwrap_or(action),
            Err(_) => action,
        }
    }

    /// Run a server command; returns the edits the server asked us to apply meanwhile
    /// (`workspace/applyEdit`).
    pub fn execute_command(&self, command: &Value, timeout: Duration) -> Result<Vec<FileEdit>, String> {
        self.applied_edits.lock().unwrap().clear();
        let params = json!({
            "command": command["command"],
            "arguments": command.get("arguments").cloned().unwrap_or(json!([])),
        });
        self.request("workspace/executeCommand", params, timeout)?;
        Ok(std::mem::take(&mut *self.applied_edits.lock().unwrap())
            .into_iter()
            .flatten()
            .collect())
    }

    pub fn signature_help(
        &self,
        path: &Path,
        text: &str,
        revision: Revision,
        at: Pos16,
        timeout: Duration,
    ) -> Result<Option<SignatureHelp>, String> {
        self.wait_ready(timeout)?;
        if !self.supports("signatureHelpProvider") {
            return Ok(None);
        }
        let v = self.at_position("textDocument/signatureHelp", path, text, revision, at, Value::Null, timeout)?;
        Ok(parse_signature_help(&v))
    }

    pub fn document_highlights(
        &self,
        path: &Path,
        text: &str,
        revision: Revision,
        at: Pos16,
        timeout: Duration,
    ) -> Result<Option<Vec<(Range16, HighlightKind)>>, String> {
        self.wait_ready(timeout)?;
        if !self.supports("documentHighlightProvider") {
            return Ok(None);
        }
        let v = self.at_position("textDocument/documentHighlight", path, text, revision, at, Value::Null, timeout)?;
        Ok(Some(parse_document_highlights(&v)))
    }

    /// All semantic tokens of the document; None when the server has none.
    pub fn semantic_tokens(
        &self,
        path: &Path,
        text: &str,
        revision: Revision,
        timeout: Duration,
    ) -> Result<Option<Vec<SemanticToken>>, String> {
        self.wait_ready(timeout)?;
        let provider = self.capability("semanticTokensProvider");
        if provider.is_null() || provider["full"].is_null() || provider["full"] == Value::Bool(false) {
            return Ok(None);
        }
        let legend = &provider["legend"];
        let (types, mods) = (strings(&legend["tokenTypes"]), strings(&legend["tokenModifiers"]));
        let v = self.about_document("textDocument/semanticTokens/full", path, text, revision, Value::Null, timeout)?;
        let data: Vec<u64> = v["data"]
            .as_array()
            .map(|a| a.iter().filter_map(Value::as_u64).collect())
            .unwrap_or_default();
        Ok(Some(decode_semantic_tokens(&data, &types, &mods)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_workspace_edits_in_both_shapes() {
        let uri = path_to_uri(Path::new("/p/a.c"));
        let edit = json!({"range": {"start": {"line": 1, "character": 2}, "end": {"line": 1, "character": 5}}, "newText": "bar"});
        let changes = parse_workspace_edit(&json!({"changes": {uri.clone(): [edit.clone()]}}));
        let expected = vec![FileEdit {
            path: PathBuf::from("/p/a.c"),
            edits: vec![TextEdit {
                start: (1, 2),
                end: (1, 5),
                new_text: "bar".into(),
            }],
        }];
        assert_eq!(changes, expected);
        let doc_changes = parse_workspace_edit(&json!({"documentChanges": [
            {"textDocument": {"uri": uri, "version": 3}, "edits": [edit]},
            {"kind": "create", "uri": "file:///p/new.c"},
        ]}));
        assert_eq!(doc_changes, expected);
    }

    #[test]
    fn parses_code_actions_and_commands() {
        let actions = parse_code_actions(&json!([
            {"title": "Run", "command": "x.run", "arguments": [1]},
            {"title": "Add include", "kind": "quickfix", "isPreferred": true, "edit": {"changes": {}}},
            {"title": "Extract", "kind": "refactor.extract", "disabled": {"reason": "Select an expression"}, "data": 7},
        ]));
        assert_eq!(actions.len(), 3);
        assert_eq!(actions[0].command.as_ref().unwrap()["command"], "x.run");
        assert!(actions[1].preferred && actions[1].edit.as_ref().is_some_and(Vec::is_empty));
        assert_eq!(actions[2].disabled.as_deref(), Some("Select an expression"));
        assert!(actions[2].edit.is_none());
    }

    #[test]
    fn parses_signature_help_with_string_and_offset_labels() {
        let help = parse_signature_help(&json!({
            "signatures": [
                {"label": "int add(int a, int b)", "parameters": [{"label": "int a"}, {"label": "int b"}], "documentation": {"kind": "markdown", "value": "Adds."}},
                {"label": "f(x, x)", "parameters": [{"label": [2, 3]}, {"label": [5, 6]}]},
            ],
            "activeSignature": 0,
            "activeParameter": 1,
        }))
        .unwrap();
        let s = &help.signatures[0];
        assert_eq!(&s.label[s.params[1].clone()], "int b");
        assert_eq!(s.active_param, Some(1));
        assert_eq!(s.doc.as_deref(), Some("Adds."));
        assert_eq!(help.signatures[1].params, vec![2..3, 5..6]);
        assert!(parse_signature_help(&json!({"signatures": []})).is_none());
        assert!(parse_signature_help(&Value::Null).is_none());
    }

    #[test]
    fn decodes_relative_semantic_tokens() {
        let types: Vec<String> = ["variable", "function", "macro"].map(String::from).to_vec();
        let mods: Vec<String> = ["declaration", "readonly"].map(String::from).to_vec();
        let tokens = decode_semantic_tokens(&[1, 4, 3, 2, 0, 0, 6, 2, 1, 3, 2, 1, 5, 0, 2, 0, 1, 1, 9, 0], &types, &mods);
        let t = |line, start16, len16, kind: &str, m: &[&str]| SemanticToken {
            line,
            start16,
            len16,
            kind: kind.into(),
            modifiers: m.iter().map(|s| s.to_string()).collect(),
        };
        assert_eq!(
            tokens,
            vec![
                t(1, 4, 3, "macro", &[]),
                t(1, 10, 2, "function", &["declaration", "readonly"]),
                t(3, 1, 5, "variable", &["readonly"]),
            ]
        );
    }

    #[test]
    fn quick_fix_menu_skips_browsing_actions() {
        for kind in [
            "",
            "quickfix",
            "refactor.extract.function",
            "source.organizeImports",
            "source.fixAll.eslint",
        ] {
            assert!(quick_fix_menu_kind(kind), "{kind}");
        }
        for kind in [
            "source.assembly",
            "source.doc",
            "gopls.doc.features",
            "source.organizeImportsX",
            "refactoring",
        ] {
            assert!(!quick_fix_menu_kind(kind), "{kind}");
        }
    }

    #[test]
    fn parses_document_highlights() {
        let h = parse_document_highlights(&json!([
            {"range": {"start": {"line": 0, "character": 4}, "end": {"line": 0, "character": 7}}, "kind": 3},
            {"range": {"start": {"line": 2, "character": 1}, "end": {"line": 2, "character": 4}}},
        ]));
        assert_eq!(
            h,
            vec![(((0, 4), (0, 7)), HighlightKind::Write), (((2, 1), (2, 4)), HighlightKind::Text)]
        );
    }
    /// Real clangd: rename, code actions (a fix-it), signature help, document highlights
    /// and semantic tokens. Needs `clangd` on PATH (or installed).
    #[test]
    #[ignore]
    fn lsp_round_trip_clangd_editing_features() {
        let spec = super::super::servers_for("c").into_iter().next().unwrap();
        let launch = super::super::resolve(spec.command).expect("clangd");
        let dir = std::env::temp_dir().join(format!("edittr-features-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        let path = dir.join("main.c");
        // Line 4 misses a `;` (clangd offers a fix-it), line 5 is inside a call.
        let text = "#define LIMIT 10\nstatic int add(int a, int b) { return a + b; }\nint main(void) {\n    int total = add(1, LIMIT);\n    total = total + 1\n    return add(total, \n";
        std::fs::write(&path, text).unwrap();
        std::fs::write(dir.join("compile_flags.txt"), "-std=c11\n").unwrap();
        let client = Client::start(&spec, launch, &dir, &[], None).unwrap();
        let t = Duration::from_secs(30);
        let rev = (1, 1);

        // Semantic tokens: LIMIT is a macro, add a function.
        let tokens = client.semantic_tokens(&path, text, rev, t).unwrap().expect("semantic tokens");
        assert!(tokens.iter().any(|k| k.line == 3 && k.kind == "macro"), "{tokens:?}");
        assert!(tokens.iter().any(|k| k.line == 3 && k.kind == "function"), "{tokens:?}");

        // Document highlights of `total` (line 4, col 4): its declaration and uses.
        let marks = client
            .document_highlights(&path, text, rev, (4, 5), t)
            .unwrap()
            .expect("highlights");
        assert!(marks.len() >= 3, "{marks:?}");
        assert!(marks.iter().any(|(_, k)| *k == HighlightKind::Write), "{marks:?}");

        // Signature help inside `add(total, ` on line 5.
        let help = client
            .signature_help(&path, text, rev, (5, 22), t)
            .unwrap()
            .expect("signature help");
        let s = &help.signatures[help.active];
        assert!(s.label.contains("add"), "{help:?}");
        assert_eq!(s.active_param, Some(1), "{help:?}");
        assert_eq!(&s.label[s.params[1].clone()], "int b");

        // Rename `add` everywhere in the file.
        let edits = client.rename(&path, text, rev, (1, 12), "sum", t).unwrap();
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].edits.len(), 3, "{edits:?}");
        assert!(edits[0].edits.iter().all(|e| e.new_text == "sum"));
        let prep = client.prepare_rename(&path, text, rev, (1, 12), t).unwrap();
        assert!(prep.is_some_and(|(r, _)| r == ((1, 11), (1, 14))));

        // A quick fix for the missing semicolon: wait for diagnostics first.
        let start = std::time::Instant::now();
        while client.raw_diagnostics.lock().unwrap().get(&path).is_none_or(Vec::is_empty) && start.elapsed() < t {
            std::thread::sleep(Duration::from_millis(100));
        }
        let actions = client.code_actions(&path, text, rev, ((5, 0), (5, 0)), t).unwrap();
        let fix = actions
            .iter()
            .find(|a| a.kind == "quickfix")
            .unwrap_or_else(|| panic!("no quick fix in {actions:?}"));
        let fix = client.resolve_code_action(fix.clone(), t);
        let edit = &fix.edit.as_ref().expect("fix edit")[0].edits[0];
        assert_eq!(edit.new_text, ";", "{fix:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// What a project's files look like and where to probe, for one real server.
    struct Probe {
        ext: &'static str,
        files: &'static [(&'static str, &'static str)],
        main: &'static str,
        /// A variable that is assigned again (document highlights).
        variable: Pos16,
        /// Inside `add(x, |1)`: the second argument (signature help).
        second_argument: Pos16,
        /// A use of `add` in `main` (rename across files).
        add_use: Pos16,
        /// Lines to ask code actions for.
        action_line: usize,
        /// `add`'s name in its own file (call hierarchy).
        add_file: &'static str,
        add_name: Pos16,
        /// Function that calls `add`.
        caller: &'static str,
    }

    /// Retries until the server has indexed the project (rust-analyzer answers empty meanwhile).
    fn until<T>(what: &str, mut f: impl FnMut() -> Option<T>) -> T {
        let start = std::time::Instant::now();
        loop {
            if let Some(v) = f() {
                return v;
            }
            assert!(start.elapsed() < Duration::from_secs(120), "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(500));
        }
    }

    fn editing_features_round_trip(probe: Probe) {
        use super::super::call_hierarchy::Direction;
        let spec = super::super::servers_for(probe.ext).into_iter().next().unwrap();
        let launch = super::super::resolve(spec.command).unwrap_or_else(|| panic!("{} is not installed", spec.command));
        let dir = std::env::temp_dir().join(format!("edittr-features-{}-{}", probe.ext, std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dir = crate::workspace::canonical(&dir).unwrap();
        for (name, text) in probe.files {
            std::fs::write(dir.join(name), text).unwrap();
        }
        let text = probe.files.iter().find(|(n, _)| *n == probe.main).unwrap().1;
        let path = dir.join(probe.main);
        let client = Client::start(&spec, launch, &dir, &[], None).unwrap();
        let t = Duration::from_secs(30);
        let rev = (1, 1);

        match client.semantic_tokens(&path, text, rev, t).unwrap() {
            Some(tokens) => {
                let kinds: std::collections::BTreeSet<_> = tokens.iter().map(|k| k.kind.as_str()).collect();
                eprintln!("{}: semantic token kinds {kinds:?}", spec.name);
                assert!(kinds.contains("function"), "{tokens:?}");
            }
            None => eprintln!("{}: no semantic tokens", spec.name),
        }

        let marks = until("document highlights", || {
            let m = client
                .document_highlights(&path, text, rev, probe.variable, t)
                .unwrap()
                .expect("highlights");
            (m.len() >= 3).then_some(m)
        });
        assert!(marks.iter().any(|(_, k)| *k == HighlightKind::Write), "{marks:?}");

        let help = until("signature help", || {
            client.signature_help(&path, text, rev, probe.second_argument, t).unwrap()
        });
        let sig = &help.signatures[help.active];
        assert!(sig.label.contains("add"), "{help:?}");
        assert_eq!(sig.active_param, Some(1), "{help:?}");
        eprintln!(
            "{}: signature {:?}, second parameter {:?}",
            spec.name,
            sig.label,
            &sig.label[sig.params[1].clone()]
        );

        let edits = until("rename", || {
            let e = client.rename(&path, text, rev, probe.add_use, "plus", t).unwrap();
            (!e.is_empty()).then_some(e)
        });
        let files: std::collections::BTreeSet<_> = edits
            .iter()
            .map(|e| e.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        let count: usize = edits.iter().map(|e| e.edits.len()).sum();
        eprintln!("{}: rename touches {files:?} with {count} edits", spec.name);
        assert!(files.contains(probe.add_file) && files.contains(probe.main), "{edits:?}");
        assert_eq!(count, 3, "{edits:?}");
        assert!(edits.iter().flat_map(|e| &e.edits).all(|e| e.new_text == "plus"));
        if let Some((range, _)) = client.prepare_rename(&path, text, rev, probe.add_use, t).unwrap() {
            assert_eq!(range.0, (probe.add_use.0, range.0.1));
            assert_eq!(range.1.1 - range.0.1, 3, "{range:?}");
        }

        let line_len = text.lines().nth(probe.action_line).unwrap().encode_utf16().count();
        let actions = until("code actions", || {
            let a = client
                .code_actions(&path, text, rev, ((probe.action_line, 0), (probe.action_line, line_len)), t)
                .unwrap();
            (!a.is_empty()).then_some(a)
        });
        eprintln!(
            "{}: code actions {:?}",
            spec.name,
            actions.iter().map(|a| (&a.title, &a.kind)).collect::<Vec<_>>()
        );
        let resolved: Vec<_> = actions.into_iter().map(|a| client.resolve_code_action(a, t)).collect();
        let with_edit = resolved.iter().filter(|a| a.edit.as_ref().is_some_and(|e| !e.is_empty())).count();
        let with_command = resolved.iter().filter(|a| a.command.is_some()).count();
        eprintln!(
            "{}: {with_edit} actions carry edits after resolve, {with_command} run a command",
            spec.name
        );
        assert!(with_edit + with_command > 0);

        let add_path = dir.join(probe.add_file);
        let add_text = probe.files.iter().find(|(n, _)| *n == probe.add_file).unwrap().1;
        let roots = until("call hierarchy", || {
            let r = client.prepare_call_hierarchy(&add_path, add_text, rev, probe.add_name, t).unwrap();
            (!r.is_empty()).then_some(r)
        });
        assert_eq!(roots[0].name, "add");
        let callers = client.hierarchy_calls(roots[0].wire.clone(), Direction::Incoming, t).unwrap();
        eprintln!(
            "{}: callers of add {:?}",
            spec.name,
            callers.iter().map(|c| &c.name).collect::<Vec<_>>()
        );
        assert!(callers.iter().any(|c| c.name.contains(probe.caller) && c.location.path == path));
        drop(client);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Real rust-analyzer (on PATH): `cargo test -- --ignored lsp_round_trip_rust_analyzer --nocapture`.
    #[test]
    #[ignore]
    fn lsp_round_trip_rust_analyzer_editing_features() {
        editing_features_round_trip(Probe {
            ext: "rs",
            files: &[
                (
                    "Cargo.toml",
                    "[package]\nname = \"probe\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[lib]\npath = \"lib.rs\"\n",
                ),
                (
                    "lib.rs",
                    "mod util;\npub const LIMIT: u32 = 10;\npub fn total(a: u32) -> u32 {\n    let mut sum = util::add(a, LIMIT);\n    sum += 1;\n    util::add(sum, 1)\n}\n",
                ),
                ("util.rs", "pub fn add(a: u32, b: u32) -> u32 {\n    a + b\n}\n"),
            ],
            main: "lib.rs",
            variable: (3, 13),
            second_argument: (5, 19),
            add_use: (5, 11),
            action_line: 3,
            add_file: "util.rs",
            add_name: (0, 8),
            caller: "total",
        });
    }

    /// Real gopls (on PATH): `cargo test -- --ignored lsp_round_trip_gopls --nocapture`.
    #[test]
    #[ignore]
    fn lsp_round_trip_gopls_editing_features() {
        editing_features_round_trip(Probe {
            ext: "go",
            files: &[
                ("go.mod", "module probe\n\ngo 1.22\n"),
                (
                    "main.go",
                    "package main\n\nconst Limit = 10\n\nfunc total(a int) int {\n\tsum := add(a, Limit)\n\tsum += 1\n\treturn add(sum, 1)\n}\n\nfunc main() { println(total(1)) }\n",
                ),
                ("util.go", "package main\n\nfunc add(a, b int) int { return a + b }\n"),
            ],
            main: "main.go",
            variable: (5, 2),
            second_argument: (7, 17),
            add_use: (7, 9),
            action_line: 5,
            add_file: "util.go",
            add_name: (2, 6),
            caller: "total",
        });
    }
}
