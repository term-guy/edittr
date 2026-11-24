use crate::goto::col16_to_char;
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum Severity {
    Error,
    Warning,
    Info,
    Hint,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct Diagnostic {
    /// Zero-based lines and character columns.
    pub line: usize,
    pub col_start: usize,
    pub end_line: usize,
    pub col_end: usize,
    pub severity: Severity,
    pub message: String,
    pub source: Option<String>,
}

pub(crate) type ByPath = HashMap<PathBuf, Vec<Diagnostic>>;

pub(crate) fn parse(items: &[Value], text: Option<&str>) -> Vec<Diagnostic> {
    let lines = text.map(|text| text.split('\n').collect::<Vec<_>>());
    let char_col = |line, col| {
        lines
            .as_ref()
            .and_then(|lines| lines.get(line).copied())
            .map_or(col, |text: &str| col16_to_char(text.strip_suffix('\r').unwrap_or(text), col))
    };
    let mut diagnostics: Vec<_> = items
        .iter()
        .filter_map(|item| {
            let start = &item["range"]["start"];
            let end = &item["range"]["end"];
            let line = usize::try_from(start["line"].as_u64()?).ok()?;
            let end_line = usize::try_from(end["line"].as_u64()?).ok()?;
            let col_start = char_col(line, usize::try_from(start["character"].as_u64()?).ok()?);
            let col_end = char_col(end_line, usize::try_from(end["character"].as_u64()?).ok()?);
            Some(Diagnostic {
                line,
                col_start,
                end_line,
                col_end,
                severity: match item["severity"].as_u64() {
                    Some(2) => Severity::Warning,
                    Some(3) => Severity::Info,
                    Some(4) => Severity::Hint,
                    _ => Severity::Error,
                },
                message: item["message"].as_str()?.to_owned(),
                source: item["source"].as_str().map(str::to_owned),
            })
        })
        .collect();
    diagnostics.sort_unstable();
    diagnostics
}

pub(crate) fn merge(snapshots: impl IntoIterator<Item = ByPath>) -> ByPath {
    let mut merged = ByPath::new();
    for snapshot in snapshots {
        for (path, diagnostics) in snapshot {
            merged.entry(path).or_default().extend(diagnostics);
        }
    }
    for diagnostics in merged.values_mut() {
        diagnostics.sort_unstable();
        diagnostics.dedup();
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn converts_utf16_ranges_and_severities() {
        let range = json!({"start": {"line": 0, "character": 3}, "end": {"line": 1, "character": 4}});
        let items: Vec<_> = (1..=5)
            .map(|n| {
                let mut item = json!({"range": range, "message": "message", "source": "server"});
                if n < 5 {
                    item["severity"] = json!(n);
                }
                item
            })
            .collect();
        let converted = parse(&items, Some("a😀b\r\n😀🚀z"));
        assert_eq!(
            converted.iter().map(|d| d.severity).collect::<Vec<_>>(),
            [Severity::Error, Severity::Error, Severity::Warning, Severity::Info, Severity::Hint]
        );
        for d in &converted {
            assert_eq!((d.line, d.col_start, d.end_line, d.col_end), (0, 2, 1, 2));
            assert_eq!(d.message, "message");
            assert_eq!(d.source.as_deref(), Some("server"));
        }
        let fallback = parse(&items, None);
        assert_eq!((fallback[0].col_start, fallback[0].col_end), (3, 4));
    }

    #[test]
    fn merges_files_and_preserves_distinct_diagnostics() {
        let diagnostic = |message: &str, line| Diagnostic {
            line,
            col_start: 0,
            end_line: line,
            col_end: 1,
            severity: Severity::Warning,
            message: message.into(),
            source: None,
        };
        let x = PathBuf::from("x.rs");
        let y = PathBuf::from("y.rs");
        let first = diagnostic("first", 0);
        let second = diagnostic("second", 1);
        let third = diagnostic("third", 4);
        let a = ByPath::from([(x.clone(), vec![second.clone()])]);
        let b = ByPath::from([(x.clone(), vec![first.clone(), second.clone()]), (y.clone(), vec![third.clone()])]);
        let merged = merge([a, b]);
        assert_eq!(merged[&x], [first, second]);
        assert_eq!(merged[&y], [third]);
        assert!(merge([]).is_empty());
    }
}
