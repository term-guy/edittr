use super::{Columns, Symbol};
use crate::{goto, lsp};
use regex::Regex;
use std::iter::Peekable;
use std::ops::RangeInclusive;
use std::path::Path;
use std::sync::LazyLock;

pub(super) fn header(path: &Path) -> Option<bool> {
    match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
        "h" | "hpp" | "hh" | "hxx" | "inl" => Some(true),
        "c" | "cc" | "cpp" | "cxx" | "m" | "mm" => Some(false),
        _ => None,
    }
}

pub(super) fn document(path: &Path, text: &str, header: bool, server: Option<Vec<lsp::Symbol>>) -> Vec<Symbol> {
    let source = Source {
        text,
        starts: std::iter::once(0)
            .chain(text.match_indices('\n').map(|(offset, _)| offset + 1))
            .collect(),
    };
    let server = server.filter(|symbols| !symbols.is_empty());
    let (macros, mut tokens) = scan(path, &source, server.is_none());
    let symbols = if let Some(server) = server {
        from_server(path, &source, header, false, server)
    } else {
        let end = tokens.len();
        pair_delimiters(&mut tokens);
        Fallback {
            path,
            source: &source,
            header,
            tokens: &tokens,
        }
        .scope(0, end, None)
    };
    merge_macros(symbols, &mut macros.into_iter().peekable(), usize::MAX)
}

struct Source<'a> {
    text: &'a str,
    starts: Vec<usize>,
}

impl Source<'_> {
    fn line(&self, offset: usize) -> usize {
        self.starts.partition_point(|&start| start <= offset) - 1
    }

    fn offset(&self, (line, col): (usize, usize)) -> Option<usize> {
        let start = *self.starts.get(line)?;
        let end = self.starts.get(line + 1).copied().unwrap_or(self.text.len());
        let text = self.text.get(start..end)?;
        if text.is_ascii() {
            return Some(start + col.min(text.len()));
        }
        let chars = goto::col16_to_char(text, col);
        Some(start + text.char_indices().nth(chars).map_or(text.len(), |(offset, _)| offset))
    }

    fn symbol(&self, path: &Path, name: &str, kind: &str, offset: usize, width: usize, lines: RangeInclusive<usize>) -> Symbol {
        let line = self.line(offset);
        let col = self.text[self.starts[line]..offset].chars().count();
        Symbol {
            name: name.to_owned(),
            kind: kind.to_owned(),
            detail: None,
            container: None,
            path: path.to_path_buf(),
            line,
            cols: Columns::Chars(col..col + width),
            lines,
            children: Vec::new(),
        }
    }
}

// Find directives in one pass, shielding comments and literals. Continuations are part of
// the same match, so a # at the beginning of a continued line is never a separate directive.
static SPECIAL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r#"(?m:^[\t \x0b\x0c]*\#(?:[^\r\n]*\\\r?\n)*[^\r\n]*)"#,
        r#"|//(?:[^\r\n]*\\\r?\n)*[^\r\n]*|(?s:/\*.*?(?:\*/|\z))"#,
        r#"|\b(?:u8|u|U|L)?R"[^\s()\\]{0,16}\(|"(?:\\(?:\r?\n|[^\r\n])|[^"\\\r\n])*"|'(?:\\(?:\r?\n|[^\r\n])|[^'\\\r\n])'"#,
    ))
    .unwrap()
});
static WORD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[\p{XID_Start}_]\p{XID_Continue}*").unwrap());
static TOKEN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[\p{XID_Start}_]\p{XID_Continue}*|[0-9][\w'.]*|::|[^\s]").unwrap());

fn special_parts(text: &str) -> impl Iterator<Item = (usize, &str)> {
    let mut offset = 0;
    std::iter::from_fn(move || {
        let part = SPECIAL.find_at(text, offset)?;
        offset = part.end();
        if let Some((_, delimiter)) = part.as_str().strip_suffix('(').and_then(|prefix| prefix.split_once("R\""))
            && !part.as_str().trim_start().starts_with(['#', '/'])
        {
            let close = format!("){delimiter}\"");
            offset = text[offset..].find(&close).map_or(text.len(), |end| offset + end + close.len());
        }
        Some((part.start(), &text[part.start()..offset]))
    })
}

fn scan<'a>(path: &Path, source: &Source<'a>, fallback: bool) -> (Vec<Symbol>, Vec<Token<'a>>) {
    let mut macros = Vec::new();
    let mut tokens = Vec::new();
    let mut branches = Vec::new();
    let mut disabled = 0;
    let mut guard = None;
    let mut end = 0;
    for (start, part) in special_parts(source.text) {
        if fallback && disabled == 0 {
            tokenize(source.text, end, start, &mut tokens);
        }
        end = start + part.len();
        let raw = part.trim_start();
        let Some(directive) = raw.strip_prefix('#') else {
            if fallback && disabled == 0 && !raw.starts_with('/') {
                tokens.push(Token::new(part, start));
            }
            continue;
        };
        let offset = end - raw.len();
        let line = source.line(offset);
        let last_line = source.line(end);
        let previous_guard = guard.take();
        let directive = directive.trim_start();
        let Some(keyword) = WORD.find(directive) else { continue };
        let args = directive[keyword.end()..].trim_start();
        match keyword.as_str() {
            "if" | "ifdef" | "ifndef" => {
                let off = keyword.as_str() == "if" && uncomment(args).trim_matches([' ', '\t', '(', ')']) == "0";
                disabled += usize::from(off);
                branches.push(off);
                if disabled == 0 && keyword.as_str() == "ifndef" {
                    guard = WORD.find(args).map(|name| (name.as_str(), last_line));
                }
            }
            "elif" | "else" => {
                if let Some(off) = branches.last_mut() {
                    disabled -= usize::from(*off);
                    *off = keyword.as_str() == "elif" && uncomment(args).trim_matches([' ', '\t', '(', ')']) == "0";
                    disabled += usize::from(*off);
                }
            }
            "endif" => disabled -= usize::from(branches.pop().unwrap_or(false)),
            "define" if disabled == 0 => {
                let Some(name) = WORD.find(args) else { continue };
                let name_offset = end - args.len();
                let name = name.as_str();
                let tail = &args[name.len()..];
                let joined = tail.replace("\\\r\n", "").replace("\\\n", "");
                let (label, value) = if joined.starts_with('(')
                    && let Some(close) = joined.find(')')
                {
                    let params = joined[1..close].split(',').map(str::trim).collect::<Vec<_>>().join(", ");
                    (format!("{name}({params})"), &joined[close + 1..])
                } else {
                    (name.to_owned(), joined.as_str())
                };
                let value = uncomment(value);
                let value = value.trim();
                if value.is_empty() && previous_guard == Some((label.as_str(), line.saturating_sub(1))) {
                    continue;
                }
                let mut symbol = source.symbol(path, &label, "Macro", name_offset, name.chars().count(), line..=last_line);
                let mut chars = value.char_indices();
                symbol.detail = (!value.is_empty()).then(|| match chars.nth(79) {
                    Some((cut, ch)) if cut + ch.len_utf8() < value.len() => format!("{}…", &value[..cut]),
                    _ => value.to_owned(),
                });
                macros.push(symbol);
            }
            _ => {}
        }
    }
    if fallback && disabled == 0 {
        tokenize(source.text, end, source.text.len(), &mut tokens);
    }
    (macros, tokens)
}

fn uncomment(text: &str) -> std::borrow::Cow<'_, str> {
    let mut clean = None;
    let mut end = 0;
    for (start, part) in special_parts(text).filter(|(_, part)| part.starts_with('/')) {
        let clean = clean.get_or_insert_with(String::new);
        clean.push_str(&text[end..start]);
        clean.push(' ');
        end = start + part.len();
    }
    if let Some(mut clean) = clean {
        clean.push_str(&text[end..]);
        clean.into()
    } else {
        text.into()
    }
}

fn from_server(path: &Path, source: &Source<'_>, header: bool, member: bool, symbols: Vec<lsp::Symbol>) -> Vec<Symbol> {
    let mut result = Vec::new();
    for mut symbol in symbols {
        if symbol.location.path != path {
            result.extend(from_server(path, source, header, member, symbol.children));
            continue;
        }
        let mut children = Vec::new();
        let detail = match symbol.kind {
            "Function" | "Method" | "Constructor" | "Operator" => {
                let span = symbol
                    .range
                    .as_ref()
                    .and_then(|range| source.text.get(source.offset(range.start)?..source.offset(range.end)?));
                if !header
                    && span.is_some_and(|span| {
                        let end = span.trim_end_matches(|ch: char| ch.is_whitespace() || ch == ';');
                        !end.ends_with('}')
                            && !end
                                .rsplit_once('=')
                                .is_some_and(|(_, suffix)| matches!(suffix.trim(), "default" | "delete"))
                    })
                {
                    continue;
                }
                None
            }
            "Variable" | "Constant" if !member => {
                let prefix = symbol.range.as_ref().and_then(|range| {
                    let name = (symbol.location.line, symbol.location.col16.start);
                    source.text.get(source.offset(range.start)?..source.offset(name)?)
                });
                prefix.is_some_and(has_static).then(|| "static".to_owned())
            }
            "Namespace" | "Class" | "Struct" => {
                if symbol.detail.as_deref() == Some("type alias") {
                    continue;
                }
                let class = symbol.kind != "Namespace";
                children = from_server(path, source, header, class, std::mem::take(&mut symbol.children));
                if symbol.kind == "Struct" || matches!(symbol.detail.as_deref(), Some("struct" | "union")) {
                    result.extend(children);
                    continue;
                }
                None
            }
            _ => continue,
        };
        symbol.children.clear();
        let mut row = Symbol::from(symbol);
        row.children = children;
        row.detail = detail;
        result.push(row);
    }
    result
}

fn has_static(prefix: &str) -> bool {
    let contains_static = |s| TOKEN.find_iter(s).any(|token| token.as_str() == "static");
    let mut end = 0;
    for (start, part) in special_parts(prefix) {
        if contains_static(&prefix[end..start]) {
            return true;
        }
        end = start + part.len();
    }
    contains_static(&prefix[end..])
}

fn merge_macros(mut symbols: Vec<Symbol>, macros: &mut Peekable<impl Iterator<Item = Symbol>>, end: usize) -> Vec<Symbol> {
    symbols.sort_by_key(|row| {
        let col = match &row.cols {
            Columns::Chars(cols) | Columns::Utf16(cols) => cols.start,
        };
        (row.line, col)
    });
    let mut rows = Vec::new();
    for mut symbol in symbols {
        let container = matches!(symbol.kind.as_str(), "Class" | "Namespace");
        let start = if container { *symbol.lines.start() } else { symbol.line };
        while macros.peek().is_some_and(|m| m.line < start && m.line <= end) {
            rows.extend(macros.next());
        }
        if container {
            symbol.children = merge_macros(symbol.children, macros, (*symbol.lines.end()).min(end));
            if symbol.children.is_empty() {
                continue;
            }
        }
        rows.push(symbol);
    }
    while macros.peek().is_some_and(|m| m.line <= end) {
        rows.extend(macros.next());
    }
    rows
}

#[derive(Clone, Copy)]
struct Token<'a> {
    text: &'a str,
    offset: usize,
    close: usize,
}

impl<'a> Token<'a> {
    fn new(text: &'a str, offset: usize) -> Self {
        Self { text, offset, close: 0 }
    }

    fn word(self) -> bool {
        WORD.is_match(self.text)
    }
}

fn tokenize<'a>(text: &'a str, start: usize, end: usize, tokens: &mut Vec<Token<'a>>) {
    tokens.extend(
        TOKEN
            .find_iter(&text[start..end])
            .map(|m| Token::new(m.as_str(), start + m.start())),
    );
}

fn pair_delimiters(tokens: &mut [Token<'_>]) {
    let mut open = Vec::new();
    for i in 0..tokens.len() {
        match tokens[i].text {
            "(" | "[" | "{" => open.push(i),
            ")" | "]" | "}" => {
                if let Some(start) = open.pop()
                    && matches!((tokens[start].text, tokens[i].text), ("(", ")") | ("[", "]") | ("{", "}"))
                {
                    tokens[start].close = i;
                }
            }
            _ => {}
        }
    }
}

struct Fallback<'a> {
    path: &'a Path,
    source: &'a Source<'a>,
    header: bool,
    tokens: &'a [Token<'a>],
}

impl Fallback<'_> {
    fn scope(&self, mut start: usize, end: usize, class: Option<&str>) -> Vec<Symbol> {
        let mut rows = Vec::new();
        let mut i = start;
        while i < end {
            let token = self.tokens[i];
            match token.text {
                "@" if self
                    .tokens
                    .get(i + 1)
                    .is_some_and(|t| matches!(t.text, "interface" | "implementation" | "protocol")) =>
                {
                    let forward = self.tokens[i + 1].text == "protocol"
                        && (i + 2..end)
                            .find(|&j| !self.tokens[j].word() && self.tokens[j].text != ",")
                            .is_some_and(|j| self.tokens[j].text == ";");
                    if !forward {
                        i = (i + 1..end)
                            .find(|&j| self.tokens[j].text == "@" && self.tokens.get(j + 1).is_some_and(|t| t.text == "end"))
                            .map_or(end, |j| j + 2);
                        start = i;
                        continue;
                    }
                }
                ":" if i == start + 1 && matches!(self.tokens[start].text, "public" | "protected" | "private") => start = i + 1,
                ";" => {
                    self.declaration(start, i, i, class, false, &mut rows);
                    start = i + 1;
                }
                "{" => {
                    let close = if token.close > i { token.close } else { end - 1 };
                    let head = &self.tokens[start..i];
                    if let Some((key, name)) = container(head) {
                        let children = if key == "enum" {
                            Vec::new()
                        } else {
                            self.scope(i + 1, close, (key != "namespace").then_some(name.text))
                        };
                        if matches!(key, "class" | "namespace") {
                            let kind = if key == "class" { "Class" } else { "Namespace" };
                            let mut row = self.row(name, kind, start, close);
                            row.children = children;
                            rows.push(row);
                        } else if key != "enum" {
                            rows.extend(children);
                        }
                        i = close + 1;
                        // A type definition can also declare variables after its closing brace.
                        if key == "namespace" {
                            start = i;
                        }
                        continue;
                    }
                    if matches!(head, [a, b] if a.text == "extern" && b.text.starts_with('"')) {
                        rows.extend(self.scope(i + 1, close, class));
                        start = close + 1;
                    } else if !(self.tokens.get(close + 1).is_some_and(|t| matches!(t.text, "{" | ","))
                        && head.iter().any(|t| t.text == ":"))
                        && self.declaration(start, i, close, class, true, &mut rows)
                    {
                        start = close + 1;
                    }
                    i = close + 1;
                    continue;
                }
                _ => {}
            }
            i = if token.close > i { token.close + 1 } else { i + 1 };
        }
        rows
    }

    // Split consecutive declarators at commas and inspect the part before an initializer.
    fn declaration(&self, start: usize, end: usize, last: usize, class: Option<&str>, body: bool, rows: &mut Vec<Symbol>) -> bool {
        if start == end || matches!(self.tokens[start].text, "typedef" | "using" | "static_assert" | "return" | "@") {
            return false;
        }
        let mut static_decl = false;
        let mut begin = start;
        let mut name: Option<Token<'_>> = None;
        let mut names = 0;
        let mut function = None;
        let mut initializer = false;
        let mut i = start;
        while i <= end {
            let token = self.tokens[i];
            match token.text {
                "<" if !initializer && i < end => {
                    if let Some(close) = self.template_end(i, end) {
                        i = close + 1;
                        continue;
                    }
                }
                "static" if !initializer && begin == start => static_decl = true,
                "{" if i < end && !initializer && container(&self.tokens[begin..i]).is_some() => {
                    name = None;
                    names = 1; // A declarator after the type's closing brace needs only a name.
                }
                "=" | "{" if i < end => initializer = true,
                "(" if !initializer && i < end => {
                    if token.close > i && self.tokens[i + 1].text == "*" && function.is_none() {
                        name = self.tokens[i + 2..token.close]
                            .iter()
                            .copied()
                            .find(|part| part.word() && !matches!(part.text, "const" | "volatile" | "restrict"));
                        names = 2;
                    } else if let Some(candidate) = name
                        && (names >= 2 || begin != start || class == Some(candidate.text))
                        && self.tokens[i - 1].offset == candidate.offset
                        && !matches!(candidate.text, "decltype" | "typeof" | "__typeof__" | "alignas")
                    {
                        initializer = true;
                        if self
                            .tokens
                            .get(i + 1)
                            .is_some_and(|arg| arg.text.starts_with(['"', '\'', '{']) || arg.text.as_bytes()[0].is_ascii_digit())
                        {
                            i = token.close.max(i) + 1;
                            continue;
                        }
                        let kind = if class == Some(candidate.text) && (i < 2 || self.tokens[i - 2].text != "~") {
                            "Constructor"
                        } else if class.is_some() {
                            "Method"
                        } else {
                            "Function"
                        };
                        function = Some((candidate, kind));
                    }
                }
                _ if i == end || (token.text == "," && !body) => {
                    if let Some((name, kind)) = function {
                        let defaulted_or_deleted = matches!(&self.tokens[begin..i], [.., equal, suffix]
                            if equal.text == "=" && matches!(suffix.text, "default" | "delete"));
                        if body || self.header || defaulted_or_deleted {
                            rows.push(self.row(name, kind, begin, if body { last } else { i }));
                        }
                    } else if !body
                        && class.is_none()
                        && let Some(name) = name
                        && (names >= 2 || begin != start)
                        && !type_only(&self.tokens[begin..i], name)
                        && !self.tokens[begin..i]
                            .windows(2)
                            .any(|pair| pair[0].text == "::" && pair[1].offset == name.offset)
                    {
                        let mut row = self.row(name, "Variable", begin, i);
                        row.detail = static_decl.then(|| "static".to_owned());
                        rows.push(row);
                    }
                    if body {
                        return function.is_some();
                    }
                    begin = i + 1;
                    initializer = false;
                    function = None;
                    names = 0;
                    name = None;
                }
                _ if !initializer && token.word() => {
                    name = Some(token);
                    names += 1;
                }
                _ => {}
            }
            i = if token.close > i { token.close + 1 } else { i + 1 };
        }
        false
    }

    fn template_end(&self, start: usize, end: usize) -> Option<usize> {
        let mut depth = 1;
        let mut i = start + 1;
        while i < end {
            let token = self.tokens[i];
            match token.text {
                "<" => depth += 1,
                ">" => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(i);
                    }
                }
                ";" | "{" | "}" => break,
                _ => {}
            }
            i = if token.close > i { token.close + 1 } else { i + 1 };
        }
        None
    }

    fn row(&self, name: Token<'_>, kind: &str, start: usize, end: usize) -> Symbol {
        let lines = self.source.line(self.tokens[start].offset)..=self.source.line(self.tokens[end].offset);
        self.source
            .symbol(self.path, name.text, kind, name.offset, name.text.chars().count(), lines)
    }
}

fn container<'a>(head: &[Token<'a>]) -> Option<(&'a str, Token<'a>)> {
    let i = head
        .iter()
        .rposition(|t| matches!(t.text, "namespace" | "class" | "struct" | "union" | "enum"))?;
    let key = if i > 0 && head[i - 1].text == "enum" {
        "enum"
    } else {
        head[i].text
    };
    let name = head.get(i + 1).copied().unwrap_or(head[i]);
    let rest = head.get(i + 2..).unwrap_or_default();
    if head[..i].iter().any(|t| matches!(t.text, "(" | "="))
        || (!matches!(name.text, ":" | "namespace" | "struct" | "union" | "enum") && !name.word())
        || rest.first().is_some_and(|t| !matches!(t.text, ":" | "::" | "final" | "<"))
    {
        return None;
    }
    Some((key, name))
}

fn type_only(tokens: &[Token<'_>], name: Token<'_>) -> bool {
    matches!(
        name.text,
        "void" | "char" | "short" | "int" | "long" | "float" | "double" | "bool" | "auto"
    ) || tokens
        .iter()
        .position(|t| matches!(t.text, "struct" | "class" | "union" | "enum"))
        .is_some_and(|i| tokens.get(i + 1).is_some_and(|tag| tag.offset == name.offset))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bundled::Launch;
    use std::time::Duration;

    const C_HEADER: &str = r#"#ifndef SAMPLE_H
#define SAMPLE_H /* include guard */
#include <stddef.h>
#define BUFFER_SIZE 256
int global, other;
static
int counter;
#define MAX(a,b) ((a) > (b) ? (a) : (b))
#define TWO (1 +\
  2)
#define FLAG
#if 0
#define INACTIVE 0
int inactive;
#else
#define ACTIVE 1
#endif
typedef struct Hidden {
int member;
} Hidden;
enum E { VALUE };
union Storage { int field; } storage;
void prototype(int parameter);
void run(int parameter)
{
static int local;
if (parameter) { int nested; }
}
#define LAST "// not a comment"
"#;

    fn rows(symbols: &[Symbol]) -> Vec<(&str, &str, Option<&str>)> {
        symbols
            .iter()
            .map(|s| (s.name.as_str(), s.kind.as_str(), s.detail.as_deref()))
            .collect()
    }

    const EXPECTED_C: &[(&str, &str, Option<&str>)] = &[
        ("BUFFER_SIZE", "Macro", Some("256")),
        ("global", "Variable", None),
        ("other", "Variable", None),
        ("counter", "Variable", Some("static")),
        ("MAX(a, b)", "Macro", Some("((a) > (b) ? (a) : (b))")),
        ("TWO", "Macro", Some("(1 +  2)")),
        ("FLAG", "Macro", None),
        ("ACTIVE", "Macro", Some("1")),
        ("storage", "Variable", None),
        ("prototype", "Function", None),
        ("run", "Function", None),
        ("LAST", "Macro", Some("\"// not a comment\"")),
    ];

    #[test]
    fn fallback_outline_c_family() {
        let path = Path::new("sample.h");
        let symbols = super::super::document(path, C_HEADER, (0, 0), None, None, None);
        assert_eq!(rows(&symbols), EXPECTED_C);
        let source = super::super::document(Path::new("sample.c"), C_HEADER, (0, 0), None, None, None);
        assert!(!source.iter().any(|symbol| symbol.name == "prototype"));
        assert!(source.iter().any(|symbol| symbol.name == "run"));
        let tree = crate::outline::Tree::new(symbols);
        let mac = tree.rows.iter().find(|row| row.symbol.name == "MAX(a, b)").unwrap();
        let (start, end) = super::super::selection(C_HEADER, mac.symbol.line, mac.symbol.cols.clone());
        assert_eq!(&C_HEADER[start..end], "MAX");
    }

    #[test]
    fn fallback_outline_keeps_nonempty_cpp_containers() {
        let text = r##"
namespace empty { class Fields { int member; }; }
namespace lib {
int value;
class Reader {
public:
Reader();
int get() { return value; }
static int member;
};
#define WITHIN 1
struct Hidden { int member; };
}
const char *raw = R"tag(
#define INSIDE_RAW 0
)tag";
void (*callback)(int);
std::array<int, 2> samples;
int initialized(42);
/*
#define INSIDE_COMMENT 0
*/
"##;
        let path = Path::new("sample.hpp");
        let symbols = document(path, text, true, None);
        assert_eq!(
            rows(&symbols),
            [
                ("lib", "Namespace", None),
                ("raw", "Variable", None),
                ("callback", "Variable", None),
                ("samples", "Variable", None),
                ("initialized", "Variable", None),
            ]
        );
        let children = &symbols[0].children;
        assert_eq!(
            rows(children),
            [
                ("value", "Variable", None),
                ("Reader", "Class", None),
                ("WITHIN", "Macro", Some("1"))
            ]
        );
        assert_eq!(
            rows(&children[1].children),
            [("Reader", "Constructor", None), ("get", "Method", None)]
        );
        let mut tree = crate::outline::Tree::new(symbols);
        let method = tree.selected(7).unwrap();
        assert_eq!(tree.rows[method].symbol.name, "get");
        let class = tree.selected(4).unwrap();
        assert_eq!(tree.rows[class].symbol.name, "Reader");
        tree.toggle(class);
        assert_eq!(tree.selected(7), Some(class));
        let mut refreshed = crate::outline::Tree::new(document(path, &format!("\n{text}"), true, None));
        refreshed.restore_collapsed(&mut tree);
        assert_eq!(refreshed.selected(8), Some(class));
    }

    fn lsp_symbol(path: &Path, text: &str, name: &str, kind: &'static str, start: usize, line: usize, end: usize) -> lsp::Symbol {
        let lines: Vec<_> = text.lines().collect();
        let col = lines[line].find(name).unwrap();
        lsp::Symbol {
            name: name.to_owned(),
            kind,
            detail: None,
            range: Some((start, 0)..(end, lines[end].len())),
            container: None,
            location: lsp::Location {
                path: path.to_path_buf(),
                line,
                col16: col..col + name.len(),
            },
            lines: start..=end,
            children: Vec::new(),
        }
    }

    #[test]
    fn lsp_outline_filters_and_merges_in_source_order() {
        let path = Path::new("sample.h");
        let s = |name, kind, start, line, end| lsp_symbol(path, C_HEADER, name, kind, start, line, end);
        let mut run = s("run", "Function", 23, 23, 27);
        run.children = vec![s("parameter", "Variable", 23, 23, 23), s("local", "Variable", 25, 25, 25)];
        let mut record = s("Hidden", "Struct", 17, 17, 19);
        record.children.push(s("member", "Field", 18, 18, 18));
        let raw = vec![
            s("counter", "Variable", 5, 6, 6),
            s("global", "Variable", 4, 4, 4),
            s("other", "Variable", 4, 4, 4),
            s("storage", "Variable", 21, 21, 21),
            s("prototype", "Function", 22, 22, 22),
            s("E", "Enum", 20, 20, 20),
            record,
            run,
        ];
        let source = document(path, C_HEADER, false, Some(vec![s("prototype", "Function", 22, 22, 22)]));
        assert!(!source.iter().any(|symbol| symbol.name == "prototype"));
        let symbols = document(path, C_HEADER, true, Some(raw));
        assert_eq!(rows(&symbols), EXPECTED_C);
        let mut tree = crate::outline::Tree::new(symbols);
        let continued = tree.selected(9).unwrap();
        assert_eq!(tree.rows[continued].symbol.name, "TWO");
        let inside_fn = tree.selected(25).unwrap();
        assert_eq!(tree.rows[inside_fn].symbol.name, "run");
    }

    #[test]
    fn outline_macro_detail_truncates_on_a_character_boundary() {
        let value = "猫".repeat(90);
        let text = format!("# define LONG {value}\r\n#define CRLF(x, \\\r\ny) x + y\r\n");
        let symbols = document(Path::new("sample.mm"), &text, false, None);
        assert_eq!(symbols[0].detail.as_deref(), Some(format!("{}…", "猫".repeat(79)).as_str()));
        assert_eq!(rows(&symbols[1..]), [("CRLF(x, y)", "Macro", Some("x + y"))]);
        assert_eq!(symbols[1].lines, 1..=2);
    }

    #[test]
    fn outline_macro_scan_20k_lines() {
        let text = "int value; // a comment\n".repeat(20_000) + "#define LAST 1\n";
        let starts = std::iter::once(0)
            .chain(text.match_indices('\n').map(|(offset, _)| offset + 1))
            .collect();
        let source = Source { text: &text, starts };
        let path = Path::new("large.c");
        scan(path, &source, false); // Exclude one-time regex compilation.
        let start = std::time::Instant::now();
        let (macros, _) = scan(path, &source, false);
        eprintln!("scanned 20,001 lines for macros in {:?}", start.elapsed());
        assert_eq!(rows(&macros), [("LAST", "Macro", Some("1"))]);
    }

    #[test]
    #[ignore = "requires clangd on PATH"]
    fn clangd_outline_round_trip() {
        let dir = std::env::temp_dir().join(format!("edittr-clangd-outline-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dir = dir.canonicalize().unwrap();
        let path = dir.join("sample.h");
        std::fs::write(&path, C_HEADER).unwrap();
        let spec = &lsp::servers_for("h")[0];
        let exe = lsp::which("clangd").expect("clangd is not on PATH");
        let client = lsp::Client::start(spec, Launch { exe, args: Vec::new() }, &dir, &[], None).unwrap();
        let raw = client.document_symbols(&path, C_HEADER, (0, 0), Duration::from_secs(10)).unwrap();
        drop(client);
        std::fs::remove_dir_all(dir).unwrap();
        assert!(raw.iter().any(|s| s.name == "Hidden"));
        assert!(raw.iter().any(|s| s.name == "prototype"));
        assert!(
            !raw.iter()
                .any(|s| ["BUFFER_SIZE", "MAX", "TWO", "FLAG", "ACTIVE", "LAST"].contains(&s.name.as_str()))
        );
        let symbols = document(&path, C_HEADER, true, Some(raw));
        assert_eq!(rows(&symbols), EXPECTED_C);
    }
}
