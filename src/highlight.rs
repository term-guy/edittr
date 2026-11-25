//! Incremental syntax highlighting for the editor.
//!
//! egui_extras re-highlights the whole buffer whenever it changes (≈230 ms for a
//! 20k-line C file, i.e. on every keystroke). Here each line keeps its highlighted
//! spans plus the parser state after it; an edit re-highlights from the first
//! changed line only until the state matches the cached one again (usually one
//! line), and the laid-out galley is reused while the text doesn't change.

use eframe::egui::{self, Color32};
use std::hash::{Hash, Hasher};
use std::ops::Range;
use std::sync::OnceLock;
use syntect::highlighting::{FontStyle, HighlightIterator, HighlightState, Highlighter, Theme, ThemeSet};
use syntect::parsing::{ParseState, Scope, ScopeStack, SyntaxReference, SyntaxSet};

struct Assets {
    syntaxes: SyntaxSet,
    themes: ThemeSet,
    scopes: Scopes,
}

fn assets() -> &'static Assets {
    static A: OnceLock<Assets> = OnceLock::new();
    A.get_or_init(|| Assets {
        syntaxes: SyntaxSet::load_defaults_newlines(),
        themes: ThemeSet::load_defaults(),
        scopes: Scopes::new(),
    })
}

/// Load the syntax definitions on a background thread so opening the first file doesn't stall.
pub fn warm_up() {
    std::thread::spawn(|| {
        assets();
    });
}

fn theme(dark: bool) -> &'static Theme {
    // Same themes as egui_extras' CodeTheme defaults.
    &assets().themes.themes[if dark { "base16-mocha.dark" } else { "Solarized (light)" }]
}

fn constant_color(dark: bool) -> Color32 {
    if dark {
        Color32::from_rgb(247, 168, 208)
    } else {
        Color32::from_rgb(144, 32, 82)
    }
}

#[derive(Clone)]
pub struct Span {
    pub range: Range<usize>, // bytes, within the line
    pub color: Color32,
    pub italics: bool,
    pub underline: bool,
}

pub struct Line {
    hash: u64,
    pub spans: Vec<Span>,
    /// Brackets outside strings and comments: (byte in line, is an opener).
    pub brackets: Vec<(u32, bool)>,
    /// Parser + highlighter state after this line.
    end: (ParseState, HighlightState),
}

/// Files with more lines than this get their first full highlight (and the one after a
/// theme change) on a background thread, shown as plain text meanwhile, instead of
/// stalling the frame. At ~25-50 us per line (Rust's grammar is among the slowest), a
/// synchronous pass over a few hundred lines already cost a 10-20 ms frame.
const BACKGROUND_LINES: usize = 150;

#[derive(Default)]
pub struct Cache {
    key: Option<(String, bool)>, // language, dark
    lines: Vec<Line>,
    /// A background full highlight in progress, for `key`.
    pending: Option<std::sync::mpsc::Receiver<Vec<Line>>>,
    /// Document version the lines were last brought up to date with (`spans`).
    version: Option<u64>,
}

/// Grammars that have highlighted something already. A grammar compiles its regexes
/// on first use (Markdown, which embeds other languages, took ~40 ms), so the first
/// file of each language is highlighted in the background however short it is.
fn warmed_set() -> &'static std::sync::Mutex<std::collections::HashSet<String>> {
    static W: OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> = OnceLock::new();
    W.get_or_init(Default::default)
}

fn warmed(name: &str) -> bool {
    warmed_set().lock().is_ok_and(|w| w.contains(name))
}

fn mark_warmed(name: &str) {
    if let Ok(mut w) = warmed_set().lock() {
        w.insert(name.to_string());
    }
}

fn hash_str(s: &str) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

fn syntax(lang: &str) -> Option<&'static SyntaxReference> {
    let ss = &assets().syntaxes;
    ss.find_syntax_by_name(lang)
        .or_else(|| ss.find_syntax_by_extension(lang))
        .or_else(|| match lang {
            // The bundled syntax set has no TypeScript grammar.
            "ts" | "tsx" | "mts" | "cts" | "TypeScript" | "TypeScript React" | "jsx" | "mjs" | "cjs" | "JavaScript React" => {
                ss.find_syntax_by_extension("js")
            }
            _ => None,
        })
}

impl Cache {
    /// Start or collect the background first pass; false while it runs.
    fn ensure(&mut self, ctx: &egui::Context, text: &str, syntax: &'static SyntaxReference, lang: &str, dark: bool) -> bool {
        let key = (lang.to_string(), dark);
        if self.key.as_ref() != Some(&key) {
            self.lines.clear();
            self.pending = None;
            self.key = Some(key);
        }
        if let Some(rx) = &self.pending {
            match rx.try_recv() {
                Ok(lines) => {
                    self.lines = lines;
                    self.pending = None;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => return false,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => self.pending = None,
            }
        } else if self.lines.is_empty() && (!warmed(&syntax.name) || text.bytes().filter(|&b| b == b'\n').count() > BACKGROUND_LINES) {
            let (tx, rx) = std::sync::mpsc::channel();
            let (text, ctx) = (text.to_string(), ctx.clone());
            std::thread::spawn(move || {
                let mut c = Cache::default();
                c.update(&text, syntax, dark);
                mark_warmed(&syntax.name);
                let _ = tx.send(c.lines);
                ctx.request_repaint();
            });
            self.pending = Some(rx);
            return false;
        }
        true
    }

    /// Highlighted spans per line of `text` (version `version` of the document), updated
    /// incrementally; None while a big file's first pass runs in the background or when
    /// `lang` has no grammar.
    pub fn spans(&mut self, ctx: &egui::Context, version: u64, text: &str, lang: &str, dark: bool) -> Option<&[Line]> {
        let syntax = syntax(lang)?;
        let key = (lang.to_string(), dark);
        if self.version == Some(version) && self.key.as_ref() == Some(&key) && self.pending.is_none() {
            return Some(&self.lines);
        }
        if !self.ensure(ctx, text, syntax, lang, dark) {
            return None;
        }
        self.update(text, syntax, dark);
        self.version = Some(version);
        Some(&self.lines)
    }

    /// Bring `self.lines` in sync with `text`, re-highlighting as little as possible.
    fn update(&mut self, text: &str, syntax: &SyntaxReference, dark: bool) {
        let new: Vec<&str> = text.split_inclusive('\n').collect();
        let hashes: Vec<u64> = new.iter().map(|l| hash_str(l)).collect();
        let old = std::mem::take(&mut self.lines);
        // Unchanged lines at the start and at the end.
        let prefix = old.iter().zip(&hashes).take_while(|(o, h)| o.hash == **h).count();
        let max_suffix = old.len().min(new.len()) - prefix;
        let suffix = old
            .iter()
            .rev()
            .zip(hashes.iter().rev())
            .take(max_suffix)
            .take_while(|(o, h)| o.hash == **h)
            .count();

        let highlighter = Highlighter::new(theme(dark));
        let caps = matches!(
            syntax.name.as_str(),
            "C" | "C++" | "Objective-C" | "Objective-C++" | "Rust" | "Python" | "JavaScript" | "Go" | "Java" | "C#"
        );
        let constant_color = constant_color(dark);
        let mut old = old.into_iter();
        let mut lines: Vec<Line> = old.by_ref().take(prefix).collect();
        let mut state = match lines.last() {
            Some(l) => l.end.clone(),
            None => (ParseState::new(syntax), HighlightState::new(&highlighter, ScopeStack::new())),
        };
        // State entering the first changed line, in the old text too.
        let prefix_state = state.clone();
        // Old lines after the prefix; the last `suffix` of them can be reused once states match.
        let rest: Vec<Line> = old.collect();
        let reuse_from = rest.len() - suffix; // index into `rest` of the first reusable line
        let new_suffix_start = new.len() - suffix;
        let mut i = prefix;
        while i < new.len() {
            if i >= new_suffix_start {
                let k = reuse_from + (i - new_suffix_start);
                let old_entry = if k == 0 { &prefix_state } else { &rest[k - 1].end };
                // Same line, same state entering it: everything from here on is unchanged.
                if *old_entry == state {
                    lines.extend(rest.into_iter().skip(k));
                    break;
                }
            }
            let line = new[i];
            let (spans, brackets) = highlight_line(line, &mut state, &highlighter, caps, constant_color);
            lines.push(Line {
                hash: hashes[i],
                spans,
                brackets,
                end: state.clone(),
            });
            i += 1;
        }
        self.lines = lines;
    }
}

struct Scopes {
    literals: [Scope; 2],
    constants: [Scope; 9],
    identifiers: [Scope; 3],
}

impl Scopes {
    fn new() -> Self {
        let scope = |name| Scope::new(name).unwrap();
        Self {
            literals: ["string", "comment"].map(scope),
            constants: [
                "entity.name.constant",
                "entity.name.function.preprocessor",
                "support.macro",
                "variable.other.constant",
                "constant.other.macro",
                "constant.other.caps",
                "constant.other.rust",
                "constant.other.java",
                "constant.other.enum",
            ]
            .map(scope),
            identifiers: ["source", "meta", "variable"].map(scope),
        }
    }

    fn in_literal(&self, stack: &ScopeStack) -> bool {
        stack
            .as_slice()
            .iter()
            .any(|&s| self.literals.iter().any(|&literal| literal.is_prefix_of(s)))
    }
}

fn highlight_line(
    line: &str,
    state: &mut (ParseState, HighlightState),
    highlighter: &Highlighter,
    caps: bool,
    constant_color: Color32,
) -> (Vec<Span>, Vec<(u32, bool)>) {
    let assets = assets();
    let Ok(ops) = state.0.parse_line(line, &assets.syntaxes) else {
        return Default::default();
    };
    let scopes = &assets.scopes;
    let mut stack = state.1.path.clone();
    // Syntect's style iterator advances its own stack past each piece before returning it.
    // Inspect the same ranges with the stack in effect for the piece itself.
    let mut regions = syntect::easy::ScopeRangeIterator::new(&ops, line);
    let mut spans = Vec::new();
    let mut brackets = Vec::new();
    let mut pos = 0;
    let has_brackets = line.bytes().any(|b| matches!(b, b'(' | b')' | b'[' | b']' | b'{' | b'}'));
    let mut caps_names = cap_names(line, caps).peekable();

    for (style, piece) in HighlightIterator::new(&mut state.1, &ops, line, highlighter) {
        let end = pos + piece.len();
        let mut span = Span {
            range: pos..end,
            color: Color32::from_rgb(style.foreground.r, style.foreground.g, style.foreground.b),
            italics: style.font_style.contains(FontStyle::ITALIC),
            underline: style.font_style.contains(FontStyle::UNDERLINE),
        };
        for (range, op) in regions.by_ref() {
            let _ = stack.apply(op);
            if !range.is_empty() {
                break;
            }
        }
        let leaf = stack.as_slice().last().copied().unwrap_or_default();
        let scoped_constant = scopes.constants.iter().any(|&s| s.is_prefix_of(leaf));
        let has_caps = caps_names.peek().is_some_and(|name| name.start < end);
        let in_code = (has_brackets || scoped_constant || has_caps) && !scopes.in_literal(&stack);
        if in_code && has_brackets {
            for (i, b) in piece.bytes().enumerate() {
                match b {
                    b'(' | b'[' | b'{' => brackets.push(((pos + i) as u32, true)),
                    b')' | b']' | b'}' => brackets.push(((pos + i) as u32, false)),
                    _ => {}
                }
            }
        }
        if in_code && scoped_constant {
            span.color = constant_color;
        }
        if has_caps {
            // Only fill in plain identifiers. Explicit keyword, function and type scopes keep their styles.
            let color_caps =
                in_code && !scoped_constant && (leaf == Scope::default() || scopes.identifiers.iter().any(|&s| s.is_prefix_of(leaf)));
            while let Some(name) = caps_names.next_if(|name| name.start < end) {
                if !color_caps || name.start < pos || name.end > end {
                    continue;
                }
                if span.range.start < name.start {
                    spans.push(Span {
                        range: span.range.start..name.start,
                        ..span
                    });
                }
                span.range.start = name.end;
                spans.push(Span {
                    range: name,
                    color: constant_color,
                    ..span
                });
            }
        }
        if !span.range.is_empty() {
            spans.push(span);
        }
        pos = end;
    }
    (spans, brackets)
}

fn cap_names(line: &str, enabled: bool) -> impl Iterator<Item = Range<usize>> + '_ {
    let bytes = if enabled { line.as_bytes() } else { &[] };
    let mut from = 0;
    std::iter::from_fn(move || {
        while let Some(offset) = bytes[from..].iter().position(u8::is_ascii_uppercase) {
            let start = from + offset;
            let end = start
                + 1
                + bytes[start + 1..]
                    .iter()
                    .take_while(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || **b == b'_')
                    .count();
            from = end;
            if end - start >= 2
                && !line[..start].chars().next_back().is_some_and(identifier_char)
                && !line[end..].chars().next().is_some_and(identifier_char)
            {
                return Some(start..end);
            }
        }
        None
    })
    .fuse()
}

fn identifier_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '$') || (!c.is_ascii() && !c.is_whitespace())
}

/// Theme color for a scope, None when the theme only has its default foreground for it.
fn scope_color(theme: &Theme, scope: &str) -> Option<Color32> {
    let highlighter = Highlighter::new(theme);
    let default = highlighter.get_default().foreground;
    let style = highlighter.style_for_stack(&[Scope::new(scope).ok()?]);
    (style.foreground != default).then(|| Color32::from_rgb(style.foreground.r, style.foreground.g, style.foreground.b))
}

struct SemanticColors {
    function: Option<Color32>,
    kind: Option<Color32>,
    namespace: Option<Color32>,
    parameter: Option<Color32>,
    comment: Option<Color32>,
}

fn semantic_colors(dark: bool) -> &'static SemanticColors {
    static C: [OnceLock<SemanticColors>; 2] = [OnceLock::new(), OnceLock::new()];
    C[dark as usize].get_or_init(|| {
        let theme = theme(dark);
        let first = |scopes: &[&str]| scopes.iter().find_map(|s| scope_color(theme, s));
        SemanticColors {
            function: first(&["entity.name.function", "support.function"]),
            kind: first(&["entity.name.type", "entity.name.class", "support.type", "storage.type"]),
            namespace: first(&["entity.name.namespace", "entity.name.module", "entity.name.type"]),
            parameter: first(&["variable.parameter"]),
            comment: first(&["comment"]),
        }
    })
}

/// Color for a semantic token (type and modifiers as the server names them), or None to
/// keep the syntax color. Only kinds a grammar can't tell apart are recolored: macros,
/// enum members and constants (the constant color), functions, types, namespaces,
/// parameters, and code the preprocessor disables (clangd marks it as a comment).
pub fn semantic_color(kind: &str, modifiers: &[String], dark: bool) -> Option<Color32> {
    let has = |m: &str| modifiers.iter().any(|x| x == m);
    let c = semantic_colors(dark);
    match kind {
        "macro" | "enumMember" | "constant" => Some(constant_color(dark)),
        "variable" if has("readonly") && (has("static") || has("globalScope") || has("fileScope")) => Some(constant_color(dark)),
        "variable" if has("constant") => Some(constant_color(dark)),
        "function" | "method" => c.function,
        "type" | "class" | "struct" | "enum" | "interface" | "typeParameter" | "concept" | "typeAlias" | "union" => c.kind,
        "namespace" => c.namespace,
        "parameter" => c.parameter,
        "comment" => c.comment,
        _ => None,
    }
}

/// `spans` (covering a line of `len` bytes, gaps allowed) with the colors of `over`
/// (sorted, non-overlapping byte ranges) laid on top. Gaps take `Color32::PLACEHOLDER`
/// unless covered, so the caller's default text color still applies there.
pub fn overlay(spans: &[Span], over: &[(Range<usize>, Color32)], len: usize) -> Vec<Span> {
    let plain = |range: Range<usize>| Span {
        range,
        color: Color32::PLACEHOLDER,
        italics: false,
        underline: false,
    };
    // Full coverage of 0..len first.
    let mut base = Vec::with_capacity(spans.len() + 2);
    let mut pos = 0;
    for s in spans {
        let (a, b) = (s.range.start.min(len), s.range.end.min(len));
        if a < pos || a >= b {
            continue;
        }
        if a > pos {
            base.push(plain(pos..a));
        }
        base.push(Span { range: a..b, ..s.clone() });
        pos = b;
    }
    if pos < len {
        base.push(plain(pos..len));
    }
    let mut out = Vec::with_capacity(base.len() + over.len() * 2);
    let mut o = over.iter().peekable();
    for s in base {
        let mut start = s.range.start;
        while start < s.range.end {
            while o.next_if(|(r, _)| r.end <= start).is_some() {}
            match o.peek() {
                Some((r, color)) if r.start < s.range.end => {
                    if r.start > start {
                        out.push(Span {
                            range: start..r.start,
                            ..s.clone()
                        });
                        start = r.start;
                    }
                    let end = r.end.min(s.range.end);
                    out.push(Span {
                        range: start..end,
                        color: *color,
                        ..s.clone()
                    });
                    start = end;
                }
                _ => {
                    out.push(Span {
                        range: start..s.range.end,
                        ..s.clone()
                    });
                    start = s.range.end;
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod semantic_tests {
    use super::*;

    fn span(range: Range<usize>, color: Color32) -> Span {
        Span {
            range,
            color,
            italics: false,
            underline: false,
        }
    }

    #[test]
    fn overlays_semantic_colors_on_syntax_spans() {
        let (a, b, x) = (Color32::RED, Color32::GREEN, Color32::BLUE);
        // "int foo(bar)": syntax spans with a gap at 3..4.
        let spans = [span(0..3, a), span(4..12, b)];
        let out = overlay(&spans, &[(4..7, x), (8..11, x)], 12);
        let got: Vec<(Range<usize>, Color32)> = out.iter().map(|s| (s.range.clone(), s.color)).collect();
        assert_eq!(
            got,
            vec![
                (0..3, a),
                (3..4, Color32::PLACEHOLDER),
                (4..7, x),
                (7..8, b),
                (8..11, x),
                (11..12, b)
            ]
        );
        // A token across span boundaries and into a gap.
        let out = overlay(&spans, &[(2..6, x)], 12);
        let got: Vec<(Range<usize>, Color32)> = out.iter().map(|s| (s.range.clone(), s.color)).collect();
        assert_eq!(got, vec![(0..2, a), (2..3, x), (3..4, x), (4..6, x), (6..12, b)]);
        assert_eq!(overlay(&[], &[], 0).len(), 0);
    }

    #[test]
    fn semantic_colors_for_macros_constants_and_types() {
        for dark in [false, true] {
            assert_eq!(semantic_color("macro", &[], dark), Some(constant_color(dark)));
            assert_eq!(semantic_color("enumMember", &[], dark), Some(constant_color(dark)));
            let mods = ["readonly".to_string(), "fileScope".to_string()];
            assert_eq!(semantic_color("variable", &mods, dark), Some(constant_color(dark)));
            assert_eq!(semantic_color("variable", &[], dark), None);
            assert_eq!(semantic_color("keyword", &[], dark), None);
            assert!(semantic_color("function", &[], dark).is_some());
            assert!(semantic_color("comment", &[], dark).is_some());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_constant_scopes() {
        let c = "#define BUF_SIZE 256\n#define addOne(x) ((x) + 1)\nenum Size { small, LARGE };\nint local = BUF_SIZE;\n";
        for (lang, sample) in [
            ("c", c),
            ("cpp", c),
            ("m", c),
            ("mm", c),
            ("rs", "const MAX: u32 = 1;\nstatic lower: u32 = MAX;\nenum Size { Small, LARGE }\n"),
            ("py", "MAX_RETRIES = 3\nlocal = MAX_RETRIES\n"),
            ("js", "const max = 3, MAX = 4;\nlet local = MAX;\n"),
            ("ts", "const MAX: number = 3;\nenum Size { Small, LARGE }\nlet local = MAX;\n"),
            ("go", "package main\nconst max = 3\nconst MAX = max\nvar local = MAX\n"),
            (
                "java",
                "class Example { static final int max = 3, MAX = 4; int local = MAX; }\nenum Size { Small, LARGE }\n",
            ),
            (
                "cs",
                "class Example { const int max = 3, MAX = 4; int local = MAX; }\nenum Size { Small, LARGE }\n",
            ),
        ] {
            let syn = syntax(lang).unwrap();
            println!("{lang}: {}", syn.name);
            let mut parser = ParseState::new(syn);
            let mut stack = ScopeStack::new();
            for line in sample.split_inclusive('\n') {
                let ops = parser.parse_line(line, &assets().syntaxes).unwrap();
                for (range, op) in syntect::easy::ScopeRangeIterator::new(&ops, line) {
                    stack.apply(op).unwrap();
                    let piece = &line[range];
                    if piece.chars().any(|c| c.is_ascii_alphabetic()) {
                        println!("  {piece:?} {stack:?}");
                    }
                }
            }
        }
    }

    #[test]
    fn constants_and_macros_use_the_constant_color_in_both_themes() {
        let c = concat!(
            "#define BUF_SIZE 256\n#define addOne(x) ((x) + 1)\n#define lower 8\n",
            "enum Size { SMALL, LARGE };\nint get(void) { int local = BUF_SIZE; return local; }\n",
            "char *s = \"BUF_SIZE\"; /* BUF_SIZE\nBUF_SIZE */\n",
        );
        let macros = &[("BUF_SIZE", 2), ("addOne", 1), ("lower", 1), ("SMALL", 1), ("LARGE", 1)][..];
        for (lang, source, constants) in [
            ("h", c, macros),
            ("c", c, macros),
            ("cpp", c, macros),
            ("hpp", c, macros),
            ("m", c, macros),
            ("mm", c, macros),
            (
                "rs",
                "const MAX: u32 = 1;\nstatic SIZE: u32 = MAX;\nfn get() { let local = SIZE; let s = \"MAX\"; } // MAX\n",
                &[("MAX", 2), ("SIZE", 2)][..],
            ),
            (
                "py",
                "MAX_RETRIES = 3\nlocal = MAX_RETRIES\ns = \"MAX_RETRIES\" # MAX_RETRIES\n",
                &[("MAX_RETRIES", 2)][..],
            ),
            (
                "js",
                "const MAX = 3;\nlet local = MAX;\nlet s = \"MAX\"; // MAX\n",
                &[("MAX", 2)][..],
            ),
            (
                "ts",
                "const MAX: number = 3;\nlet local = MAX;\nlet s = \"MAX\"; // MAX\n",
                &[("MAX", 2)][..],
            ),
            ("go", "const MAX = 3\nvar local = MAX\nvar s = \"MAX\" // MAX\n", &[("MAX", 2)][..]),
            ("java", "enum Size { LARGE; int local = 3; } // LARGE\n", &[("LARGE", 1)][..]),
            (
                "cs",
                "enum Size { Small, LARGE } // Small\nclass Example { int local = 3; }\n",
                &[("Small", 1), ("LARGE", 1)][..],
            ),
        ] {
            let syn = syntax(lang).unwrap();
            for dark in [false, true] {
                let color = constant_color(dark);
                let mut cache = Cache::default();
                cache.update(source, syn, dark);
                let mut parser = ParseState::new(syn);
                let highlighter = Highlighter::new(theme(dark));
                let mut original = HighlightState::new(&highlighter, ScopeStack::new());
                let mut expected = Vec::new();
                for line in source.split_inclusive('\n') {
                    let ops = parser.parse_line(line, &assets().syntaxes).unwrap();
                    for (style, piece) in HighlightIterator::new(&mut original, &ops, line, &highlighter) {
                        let fg = style.foreground;
                        expected.extend(std::iter::repeat_n(Color32::from_rgb(fg.r, fg.g, fg.b), piece.len()));
                    }
                }
                // The fixtures list code occurrences first. Matches in strings and comments retain their theme colors.
                for &(name, count) in constants {
                    for (start, _) in source.match_indices(name).take(count) {
                        expected[start..start + name.len()].fill(color);
                    }
                }
                let actual: Vec<_> = cache
                    .lines
                    .iter()
                    .flat_map(|line| line.spans.iter().flat_map(|s| std::iter::repeat_n(s.color, s.range.len())))
                    .collect();
                assert_ne!(actual[source.find("local").unwrap()], color, "{lang} {dark}");
                assert_eq!(actual, expected, "{lang} {dark}");
            }
        }
    }

    #[test]
    fn fallback_requires_a_whole_uppercase_identifier() {
        let source = "int local = (BUF_SIZE + ID2 + X_ + X + xBUF + BUFfoo + _BUF + žBUF + 0xFF + 1E10);\n";
        let names: Vec<_> = cap_names(source, true).map(|name| &source[name]).collect();
        assert_eq!(names, ["BUF_SIZE", "ID2", "X_"]);
        assert!(cap_names(source, false).next().is_none());
    }

    #[test]
    #[ignore = "run in isolation with --release --ignored --nocapture for timings"]
    fn c_highlighting_per_line() {
        const LINES: usize = 20_000;
        let mut source = String::new();
        for i in 0..LINES / 5 {
            use std::fmt::Write as _;
            writeln!(
                source,
                "#define SIZE_{i} 256\nint get_{i}(int index) {{\n    int local = index + SIZE_{i};\n    return local;\n}}"
            )
            .unwrap();
        }
        let syn = syntax("c").unwrap();
        Cache::default().update(&source, syn, true);
        let mut samples: Vec<_> = (0..9)
            .map(|_| {
                let mut cache = Cache::default();
                let start = std::time::Instant::now();
                cache.update(&source, syn, true);
                let elapsed = start.elapsed();
                assert_eq!(cache.lines.len(), LINES);
                elapsed.div_f64(LINES as f64)
            })
            .collect();
        samples.sort_unstable();
        println!("20,000-line C file: {:?}/line median ({samples:?})", samples[samples.len() / 2]);
    }

    fn full(text: &str, lang: &str) -> Vec<Vec<(Range<usize>, Color32)>> {
        let mut c = Cache {
            key: Some((lang.into(), true)),
            ..Default::default()
        };
        c.update(text, syntax(lang).unwrap(), true);
        c.lines
            .iter()
            .map(|l| l.spans.iter().map(|s| (s.range.clone(), s.color)).collect())
            .collect()
    }

    fn incremental(steps: &[&str], lang: &str) -> Vec<Vec<(Range<usize>, Color32)>> {
        let mut c = Cache::default();
        for t in steps {
            c.update(t, syntax(lang).unwrap(), true);
        }
        c.lines
            .iter()
            .map(|l| l.spans.iter().map(|s| (s.range.clone(), s.color)).collect())
            .collect()
    }

    #[test]
    fn brackets_outside_strings_and_comments() {
        let mut c = Cache::default();
        c.update("let s = \"(\"; // [\nf(a[0]);\n", syntax("rs").unwrap(), true);
        assert!(c.lines[0].brackets.is_empty());
        assert_eq!(c.lines[1].brackets, vec![(1, true), (3, true), (5, false), (6, false)]);
    }

    #[test]
    fn incremental_matches_full_highlighting() {
        let base = "fn main() {\n    let x = MAX;\n    // MAX\n    call(x);\n}\n";
        let edits = [
            // typing inside a line
            "fn main() {\n    let xy = MAX;\n    // MAX\n    call(x);\n}\n",
            // opening a block comment changes everything after it
            "fn main() {\n    /* let x = MAX;\n    // MAX\n    call(x);\n}\n",
            // closing it again
            "fn main() {\n    /* let x = MAX; */\n    // MAX\n    call(x);\n}\n",
            // inserting and deleting lines
            "use a;\nfn main() {\n    let x = MAX;\n    call(x);\n}\n",
            "",
            "x",
        ];
        for e in edits {
            assert_eq!(incremental(&[base, e], "rs"), full(e, "rs"), "edit {e:?}");
        }
    }

    #[test]
    fn reuses_unchanged_lines_after_an_edit() {
        let mut lines: Vec<String> = (0..2000).map(|i| format!("int f{i}(void) {{ return {i}; }}\n")).collect();
        let mut c = Cache::default();
        let syn = syntax("c").unwrap();
        c.update(&lines.concat(), syn, true);
        lines[1000] = "int changed(void) { return 0; }\n".into();
        let t = std::time::Instant::now();
        c.update(&lines.concat(), syn, true);
        // Re-highlighting one line (plus hashing) is far below a full pass.
        assert!(t.elapsed() < std::time::Duration::from_millis(50), "{:?}", t.elapsed());
        assert_eq!(c.lines.len(), 2000);
    }
}
