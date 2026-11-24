use crate::{clangd, goto, lsp, symbol_cache, workspace};
use std::ops::{Range, RangeInclusive};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

mod c_family;

pub(crate) const LIMIT: usize = 200;
const TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone)]
pub(crate) enum Columns {
    Chars(Range<usize>),
    Utf16(Range<usize>),
}

impl From<Range<usize>> for Columns {
    fn from(range: Range<usize>) -> Self {
        Self::Chars(range)
    }
}

pub(crate) fn selection(text: &str, line: usize, cols: Columns) -> (usize, usize) {
    let mut lines = text.split('\n');
    let start: usize = lines.by_ref().take(line).map(|l| l.chars().count() + 1).sum();
    let max = text.chars().count();
    let cols = match cols {
        Columns::Chars(cols) => cols,
        Columns::Utf16(cols) => {
            let line = lines.next().unwrap_or_default();
            goto::col16_to_char(line, cols.start)..goto::col16_to_char(line, cols.end)
        }
    };
    (start.saturating_add(cols.start).min(max), start.saturating_add(cols.end).min(max))
}

pub(crate) struct Symbol {
    pub(crate) name: String,
    pub(crate) kind: String,
    pub(crate) detail: Option<String>,
    pub(crate) container: Option<String>,
    pub(crate) path: PathBuf,
    /// Location of the symbol name, used for navigation.
    pub(crate) line: usize,
    pub(crate) cols: Columns,
    /// Full symbol span, in zero-based lines.
    pub(crate) lines: RangeInclusive<usize>,
    pub(crate) children: Vec<Symbol>,
}

impl Symbol {
    fn from_definition(path: &Path, def: goto::Definition<'_>) -> Self {
        Self {
            name: def.name.to_owned(),
            kind: def.kind.to_owned(),
            detail: None,
            container: None,
            path: path.to_path_buf(),
            line: def.line,
            cols: Columns::Chars(def.cols),
            lines: def.line..=def.line,
            children: Vec::new(),
        }
    }

    fn from_cached(path: &Path, def: &symbol_cache::Definition) -> Self {
        Self {
            name: def.name.clone(),
            kind: def.kind.clone(),
            detail: None,
            container: None,
            path: path.to_path_buf(),
            line: def.line,
            cols: Columns::Chars(def.cols.clone()),
            lines: def.line..=def.line,
            children: Vec::new(),
        }
    }
}

impl From<lsp::Symbol> for Symbol {
    fn from(symbol: lsp::Symbol) -> Self {
        Self {
            name: symbol.name,
            kind: symbol.kind.to_owned(),
            detail: None,
            container: symbol.container,
            path: symbol.location.path,
            line: symbol.location.line,
            cols: Columns::Utf16(symbol.location.col16),
            lines: symbol.lines,
            children: symbol.children.into_iter().map(Self::from).collect(),
        }
    }
}

pub(crate) fn document(
    path: &Path,
    text: &str,
    revision: lsp::Revision,
    client: Option<Arc<lsp::Client>>,
    project: Option<(&Path, &[String])>,
    // The outline needs the hierarchy, which the flat cached definitions lack; re-parsing
    // one open file is cheap. (The cache serves workspace-symbol search.)
    _cache: Option<&symbol_cache::Snapshot>,
) -> Vec<Symbol> {
    let rel = project.and_then(|(root, _)| workspace::relative_path(root, path));
    if rel
        .as_deref()
        .zip(project)
        .is_some_and(|(rel, (_, ignored))| excluded(rel, ignored))
    {
        return Vec::new();
    }
    let symbols = client.and_then(|client| client.document_symbols(path, text, revision, TIMEOUT).ok());
    if let Some(header) = c_family::header(path) {
        return c_family::document(path, text, header, symbols);
    }
    if let Some(symbols) = symbols {
        let symbols = document_symbols(path, symbols);
        if !symbols.is_empty() {
            return symbols;
        }
    }
    fallback_document(path, text)
}

fn document_symbols(path: &Path, symbols: Vec<lsp::Symbol>) -> Vec<Symbol> {
    let mut result = Vec::new();
    for mut symbol in symbols {
        let children = document_symbols(path, std::mem::take(&mut symbol.children));
        if symbol.location.path == path {
            let mut symbol = Symbol::from(symbol);
            symbol.children = children;
            result.push(symbol);
        } else {
            result.extend(children);
        }
    }
    result
}

/// Approximate scope from indentation and aligned closing braces when no server supplies ranges.
fn fallback_document(path: &Path, text: &str) -> Vec<Symbol> {
    fn append(roots: &mut Vec<Symbol>, scopes: &mut [(usize, Symbol)], mut symbol: Symbol) {
        if let Some((_, parent)) = scopes.last_mut() {
            symbol.container = Some(parent.name.clone());
            parent.children.push(symbol);
        } else {
            roots.push(symbol);
        }
    }

    let mut definitions = goto::definitions(text).peekable();
    let mut roots = Vec::new();
    let mut scopes = Vec::<(usize, Symbol)>::new();
    let mut last_line = 0;
    for (line, source) in text.split('\n').enumerate() {
        let trimmed = source.trim();
        if trimmed.is_empty() {
            continue;
        }
        let indent = source.len() - source.trim_start().len();
        while scopes.last().is_some_and(|(level, _)| *level >= indent) {
            let (level, mut symbol) = scopes.pop().unwrap();
            let end = if level == indent && trimmed.starts_with('}') {
                line
            } else {
                last_line
            };
            symbol.lines = symbol.line..=end;
            append(&mut roots, &mut scopes, symbol);
        }
        if definitions.peek().is_some_and(|def| def.line == line) {
            let symbol = Symbol::from_definition(path, definitions.next().unwrap());
            let opens_scope = if trimmed.contains('{') {
                !trimmed.ends_with('}')
            } else {
                !trimmed.ends_with(';')
            };
            if opens_scope {
                scopes.push((indent, symbol));
            } else {
                append(&mut roots, &mut scopes, symbol);
            }
        }
        last_line = line;
    }
    while let Some((_, mut symbol)) = scopes.pop() {
        symbol.lines = symbol.line..=last_line;
        append(&mut roots, &mut scopes, symbol);
    }
    roots
}

pub(crate) fn flatten(symbols: Vec<Symbol>) -> Vec<Symbol> {
    fn append(symbols: Vec<Symbol>, rows: &mut Vec<Symbol>) {
        for mut symbol in symbols {
            let children = std::mem::take(&mut symbol.children);
            rows.push(symbol);
            append(children, rows);
        }
    }
    let mut rows = Vec::new();
    append(symbols, &mut rows);
    rows
}

fn excluded(rel: &str, ignored: &[String]) -> bool {
    workspace::is_ignored(rel, ignored) || clangd::is_index_path(Path::new(rel))
}

pub(crate) fn project(
    root: &Path,
    ignored: &[String],
    clients: Vec<Arc<lsp::Client>>,
    query: &str,
    cancel: &AtomicBool,
    cache: Option<&symbol_cache::Snapshot>,
    files: impl FnOnce() -> Vec<String>,
) -> Vec<Symbol> {
    if cancel.load(Ordering::Relaxed) {
        return Vec::new();
    }
    let server_results = std::thread::scope(|scope| {
        let requests: Vec<_> = clients
            .into_iter()
            .map(|client| scope.spawn(move || client.workspace_symbols(query, TIMEOUT)))
            .collect();
        requests
            .into_iter()
            .filter_map(|request| request.join().ok()?.ok())
            .collect::<Vec<_>>()
    });
    if cancel.load(Ordering::Relaxed) {
        return Vec::new();
    }
    let mut seen = std::collections::HashSet::new();
    let results: Vec<_> = server_results
        .into_iter()
        .flatten()
        .filter(|symbol| {
            let loc = &symbol.location;
            workspace::relative_path(root, &loc.path).is_some_and(|rel| !excluded(&rel, ignored))
                && seen.insert((loc.path.clone(), loc.line, loc.col16.start, symbol.name.clone()))
        })
        .take(LIMIT)
        .map(Symbol::from)
        .collect();
    if !results.is_empty() {
        return results;
    }

    let query = query.to_lowercase();
    let mut results = Vec::new();
    let matches_query = |name: &str| query.is_empty() || name.to_lowercase().contains(&query);
    if let Some(cache) = cache {
        for (rel, definitions) in cache.files() {
            if cancel.load(Ordering::Relaxed) {
                return Vec::new();
            }
            if excluded(rel, ignored) {
                continue;
            }
            let path = root.join(rel);
            results.extend(
                definitions
                    .iter()
                    .filter(|def| matches_query(&def.name))
                    .take(LIMIT - results.len())
                    .map(|def| Symbol::from_cached(&path, def)),
            );
            if results.len() == LIMIT {
                return results;
            }
        }
    }
    let remaining = cache.map_or_else(files, |cache| cache.overflow.clone());
    for rel in remaining {
        if cancel.load(Ordering::Relaxed) {
            return Vec::new();
        }
        if excluded(&rel, ignored) {
            continue;
        }
        let path = root.join(rel);
        if !std::fs::metadata(&path).is_ok_and(|meta| meta.is_file() && meta.len() <= symbol_cache::MAX_FILE_BYTES) {
            continue;
        }
        let Some(text) = symbol_cache::source(&path) else { continue };
        let matches = goto::definitions(&text)
            .take_while(|_| !cancel.load(Ordering::Relaxed))
            .filter(|def| matches_query(def.name))
            .take(LIMIT - results.len())
            .map(|def| Symbol::from_definition(&path, def));
        results.extend(matches);
        if results.len() == LIMIT {
            break;
        }
    }
    results
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_outline_shares_definition_patterns_and_char_columns() {
        let path = Path::new("fixture.txt");
        let source = "// fn fake() {}\nstruct Model;\nfn café() {}\n  let mut value = 1;\n#define MAX 10\nstatic int count(void) {\n  return count();\nconst MAX_COUNT: usize = 10;\n";
        let symbols = document(path, source, (0, 0), None, None, None);
        let outline: Vec<_> = symbols.iter().map(|s| (s.name.as_str(), s.kind.as_str(), s.line)).collect();
        assert_eq!(
            outline,
            [
                ("Model", "Struct", 1),
                ("café", "Function", 2),
                ("value", "Variable", 3),
                ("MAX", "Macro", 4),
                ("count", "Function", 5),
                ("MAX_COUNT", "Constant", 7),
            ]
        );
        let Columns::Chars(cols) = &symbols[1].cols else {
            panic!("expected char columns")
        };
        let (start, end) = selection(source, symbols[1].line, cols.clone().into());
        assert_eq!(source.chars().skip(start).take(end - start).collect::<String>(), "café");
    }

    #[test]
    fn workspace_fallback_filters_definitions_and_caps_results() {
        let root = std::env::temp_dir().join(format!("edittr-symbols-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("lib.rs");
        let text: String = (0..LIMIT + 10).map(|i| format!("fn symbol_{i}() {{}}\n")).collect();
        std::fs::write(&path, format!("fn skip_me() {{}}\n{text}")).unwrap();
        let results = project(&root, &[], Vec::new(), "SYM", &AtomicBool::new(false), None, || {
            vec!["lib.rs".into()]
        });
        std::fs::remove_dir_all(&root).unwrap();
        assert_eq!(results.len(), LIMIT);
        assert_eq!((&*results[0].name, results[0].line, &results[0].path), ("symbol_0", 1, &path));
        assert!(results.iter().all(|symbol| symbol.name.starts_with("symbol_")));
    }

    #[test]
    fn utf16_selection_accounts_for_surrogate_pairs() {
        assert_eq!(selection("header\n😀name", 1, Columns::Utf16(2..6)), (8, 12));
    }
}
