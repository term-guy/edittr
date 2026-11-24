use crate::{goto, lsp, workspace};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

pub(crate) const LIMIT: usize = 200;
const TIMEOUT: Duration = Duration::from_secs(2);

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
    pub(crate) kind: &'static str,
    pub(crate) container: Option<String>,
    pub(crate) path: PathBuf,
    pub(crate) line: usize,
    pub(crate) cols: Columns,
}

impl Symbol {
    fn from_definition(path: &Path, def: goto::Definition<'_>) -> Self {
        Self {
            name: def.name.to_owned(),
            kind: def.kind,
            container: None,
            path: path.to_path_buf(),
            line: def.line,
            cols: Columns::Chars(def.cols),
        }
    }
}

impl From<lsp::Symbol> for Symbol {
    fn from(symbol: lsp::Symbol) -> Self {
        Self {
            name: symbol.name,
            kind: symbol.kind,
            container: symbol.container,
            path: symbol.location.path,
            line: symbol.location.line,
            cols: Columns::Utf16(symbol.location.col16),
        }
    }
}

pub(crate) fn document(path: &Path, text: &str, revision: lsp::Revision, client: Option<Arc<lsp::Client>>) -> Vec<Symbol> {
    if let Some(client) = client
        && let Ok(symbols) = client.document_symbols(path, text, revision, TIMEOUT)
    {
        let symbols: Vec<_> = symbols
            .into_iter()
            .filter(|symbol| symbol.location.path == path)
            .map(Symbol::from)
            .collect();
        if !symbols.is_empty() {
            return symbols;
        }
    }
    goto::definitions(text).map(|def| Symbol::from_definition(path, def)).collect()
}

pub(crate) fn project(
    root: &Path,
    ignored: &[String],
    clients: Vec<Arc<lsp::Client>>,
    query: &str,
    cancel: &AtomicBool,
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
            workspace::relative_path(root, &loc.path).is_some_and(|rel| !workspace::is_ignored(&rel, ignored))
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
    for rel in files() {
        if cancel.load(Ordering::Relaxed) {
            return Vec::new();
        }
        let path = root.join(rel);
        if !std::fs::metadata(&path).is_ok_and(|meta| meta.is_file() && meta.len() <= 2_000_000) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        if text.as_bytes().contains(&0) {
            continue;
        }
        let matches = goto::definitions(&text)
            .take_while(|_| !cancel.load(Ordering::Relaxed))
            .filter(|def| query.is_empty() || def.name.to_lowercase().contains(&query))
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
        let symbols = document(path, source, (0, 0), None);
        let outline: Vec<_> = symbols.iter().map(|s| (s.name.as_str(), s.kind, s.line)).collect();
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
        let results = project(&root, &[], Vec::new(), "SYM", &AtomicBool::new(false), || vec!["lib.rs".into()]);
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
