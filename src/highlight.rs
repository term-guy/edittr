//! Incremental syntax highlighting for the editor.
//!
//! egui_extras re-highlights the whole buffer whenever it changes (≈230 ms for a
//! 20k-line C file, i.e. on every keystroke). Here each line keeps its highlighted
//! spans plus the parser state after it; an edit re-highlights from the first
//! changed line only until the state matches the cached one again (usually one
//! line), and the laid-out galley is reused while the text doesn't change.

use crate::theme::{Palette, Part};
use eframe::egui::{self, Color32};
use std::collections::HashMap;
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

static ASSETS: OnceLock<Assets> = OnceLock::new();

fn assets() -> &'static Assets {
    ASSETS.get_or_init(|| Assets {
        syntaxes: SyntaxSet::load_defaults_newlines(),
        themes: ThemeSet::load_defaults(),
        scopes: Scopes::new(),
    })
}

/// Load the syntax definitions on a background thread so opening the first file doesn't stall.
pub fn warm_up(ctx: egui::Context) {
    std::thread::spawn(move || {
        assets();
        ctx.request_repaint();
    });
}

fn theme(dark: bool) -> &'static Theme {
    // Same themes as egui_extras' CodeTheme defaults.
    &assets().themes.themes[if dark { "base16-mocha.dark" } else { "Solarized (light)" }]
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Span {
    pub range: Range<usize>, // bytes, within the line
    pub part: Part,
    legacy_color: Option<Color32>,
    pub italics: bool,
    pub underline: bool,
}

impl Span {
    pub fn color(&self, palette: &Palette) -> Color32 {
        self.legacy_color
            .filter(|_| palette.uses_legacy_color(self.part))
            .unwrap_or_else(|| palette.color(self.part))
    }
}

pub type TokenLine = Vec<(Range<usize>, SemanticRole)>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SemanticRole {
    part: Part,
    legacy: LegacySemantic,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LegacySemantic {
    Both,
    Light,
    Neither,
}

impl SemanticRole {
    #[cfg(test)]
    pub(crate) fn new(part: Part) -> Self {
        Self {
            part,
            legacy: LegacySemantic::Both,
        }
    }

    fn applies(self, palette: &Palette) -> bool {
        !palette.uses_legacy_color(self.part)
            || self.legacy == LegacySemantic::Both
            || (self.legacy == LegacySemantic::Light && palette.preset().base == egui::Theme::Light)
    }
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
/// light/dark base changes) on a background thread, shown as plain text meanwhile, instead of
/// stalling the frame. At ~25-50 us per line (Rust's grammar is among the slowest), a
/// synchronous pass over a few hundred lines already cost a 10-20 ms frame.
const BACKGROUND_LINES: usize = 150;

struct Pending {
    version: u64,
    result: std::sync::mpsc::Receiver<(Vec<Line>, ScopeCache)>,
}

#[derive(Default)]
pub struct Cache {
    key: Option<(&'static str, bool)>, // syntax name, legacy style base
    lines: Vec<Line>,
    /// A background full highlight in progress, for `key`.
    pending: Option<Pending>,
    /// Document version the lines were last brought up to date with (`spans`).
    version: Option<u64>,
    categories: ScopeCache,
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

#[cfg(test)]
pub(crate) fn mark_syntax_warmed_for_test(lang: &str) {
    mark_warmed(&syntax(lang).unwrap().name);
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

/// Lines above this are freed on a background thread: each keeps the parser state at
/// its start, and freeing ~20k of them took ~60 ms on the UI thread (on a theme switch
/// or when closing the tab).
const BACKGROUND_DROP_LINES: usize = 2000;

fn drop_in_background(lines: Vec<Line>) {
    if lines.len() > BACKGROUND_DROP_LINES {
        std::thread::spawn(move || drop(lines));
    }
}

impl Drop for Cache {
    fn drop(&mut self) {
        drop_in_background(std::mem::take(&mut self.lines));
    }
}

impl Cache {
    /// Start or collect the background first pass; false while it runs.
    fn ensure(&mut self, ctx: &egui::Context, version: u64, text: &str, syntax: &'static SyntaxReference, dark: bool) -> bool {
        let key = (syntax.name.as_str(), dark);
        if self.key.as_ref() != Some(&key) {
            drop_in_background(std::mem::take(&mut self.lines));
            self.version = None;
            self.pending = None;
            self.key = Some(key);
        }
        if let Some(pending) = &self.pending {
            match pending.result.try_recv() {
                Ok((lines, categories)) => {
                    self.lines = lines;
                    self.categories = categories;
                    self.version = Some(pending.version);
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
                let _ = tx.send((std::mem::take(&mut c.lines), std::mem::take(&mut c.categories)));
                ctx.request_repaint();
            });
            self.pending = Some(Pending { version, result: rx });
            return false;
        }
        true
    }

    /// Highlighted spans per line of `text` (version `version` of the document), updated
    /// incrementally; None while a big file's first pass runs in the background or when
    /// `lang` has no grammar.
    pub fn spans(&mut self, ctx: &egui::Context, version: u64, text: &str, lang: &str, dark: bool) -> Option<&[Line]> {
        let _ = ASSETS.get()?;
        let syntax = syntax(lang)?;
        let key = (syntax.name.as_str(), dark);
        if self.version == Some(version) && self.key.as_ref() == Some(&key) && self.pending.is_none() {
            return Some(&self.lines);
        }
        if !self.ensure(ctx, version, text, syntax, dark) {
            return None;
        }
        if self.version != Some(version) {
            self.update(text, syntax, dark);
            self.version = Some(version);
        }
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
        let constant_color = Palette::defaults(dark).color(Part::Constant);
        let caps = matches!(
            syntax.name.as_str(),
            "C" | "C++" | "Objective-C" | "Objective-C++" | "Rust" | "Python" | "JavaScript" | "Go" | "Java" | "C#"
        );

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
            let (spans, brackets) = highlight_line(line, &mut state, &highlighter, caps, constant_color, &mut self.categories);
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

#[derive(Clone, Copy)]
struct ScopeStyle {
    part: Option<Part>,
    legacy_constant: bool,
}

type ScopeCache = HashMap<Scope, ScopeStyle>;

struct Scopes {
    categories: Vec<(Scope, Part)>,
    legacy_constants: [Scope; 9],
}

impl Scopes {
    fn new() -> Self {
        use Part::*;
        let categories = [
            ("comment.block.preprocessor", Disabled),
            ("comment.block.disabled", Disabled),
            ("meta.preprocessor.disabled", Disabled),
            ("comment", Comment),
            ("string", String),
            ("constant.numeric", Number),
            ("entity.name.constant.preprocessor", Macro),
            ("entity.name.function.preprocessor", Macro),
            ("keyword.control.import.define", Macro),
            ("keyword.control.import.include", Macro),
            ("entity.name.function.macro", Macro),
            ("entity.name.macro", Macro),
            ("support.macro", Macro),
            ("support.function.macro", Macro),
            ("constant.other.macro", Macro),
            ("variable.other.macro", Macro),
            ("keyword.control.import", Keyword),
            ("keyword.control.preprocessor", Macro),
            ("keyword.operator", Operator),
            ("keyword", Keyword),
            ("storage", Keyword),
            ("entity.name.constant", Constant),
            ("variable.other.constant", Constant),
            ("constant", Constant),
            ("entity.name.type", Type),
            ("entity.name.class", Type),
            ("entity.name.enum", Type),
            ("entity.name.struct", Type),
            ("entity.name.namespace", Type),
            ("entity.name.module", Type),
            ("support.type", Type),
            ("support.class", Type),
            ("entity.name.function", Function),
            ("support.function", Function),
            ("variable", Variable),
            ("punctuation", Operator),
            ("meta.preprocessor", Macro),
            ("meta.macro", Macro),
        ]
        .into_iter()
        .map(|(scope, part)| (Scope::new(scope).unwrap(), part))
        .collect();
        let legacy_constants = [
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
        .map(|name| Scope::new(name).unwrap());
        Self {
            categories,
            legacy_constants,
        }
    }

    fn category(&self, stack: &ScopeStack, memo: &mut ScopeCache) -> (Part, bool) {
        let mut part = Part::Foreground;
        let mut legacy_constant = false;
        for &scope in stack.as_slice() {
            let style = memo.entry(scope).or_insert_with(|| ScopeStyle {
                part: self
                    .categories
                    .iter()
                    .find(|(prefix, _)| prefix.is_prefix_of(scope))
                    .map(|&(_, part)| part),
                legacy_constant: self.legacy_constants.iter().any(|&prefix| prefix.is_prefix_of(scope)),
            });
            legacy_constant = style.legacy_constant;
            match style.part {
                // Literal interiors keep their color even if a punctuation scope is nested inside.
                Some(literal @ (Part::Comment | Part::String | Part::Disabled)) => return (literal, false),
                Some(category) => part = category,
                None => {}
            }
        }
        (part, legacy_constant)
    }
}

fn highlight_line(
    line: &str,
    state: &mut (ParseState, HighlightState),
    highlighter: &Highlighter,
    caps: bool,
    constant_color: Color32,
    categories: &mut ScopeCache,
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
            part: Part::Foreground,
            legacy_color: Some(Color32::from_rgb(style.foreground.r, style.foreground.g, style.foreground.b)),
            italics: style.font_style.contains(FontStyle::ITALIC),
            underline: style.font_style.contains(FontStyle::UNDERLINE),
        };
        for (range, op) in regions.by_ref() {
            let _ = stack.apply(op);
            if !range.is_empty() {
                break;
            }
        }
        let (part, legacy_constant) = scopes.category(&stack, categories);
        span.part = part;
        if legacy_constant {
            span.legacy_color = Some(constant_color);
        }
        let has_caps = caps_names.peek().is_some_and(|name| name.start < end);
        let in_code = !matches!(part, Part::String | Part::Comment | Part::Disabled);
        if in_code && has_brackets {
            for (i, b) in piece.bytes().enumerate() {
                match b {
                    b'(' | b'[' | b'{' => brackets.push(((pos + i) as u32, true)),
                    b')' | b']' | b'}' => brackets.push(((pos + i) as u32, false)),
                    _ => {}
                }
            }
        }
        if has_caps {
            // Only fill in plain identifiers. Explicit keyword, function and type scopes keep their colors.
            let color_caps = matches!(part, Part::Foreground | Part::Variable);
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
                    part: Part::Constant,
                    legacy_color: Some(constant_color),
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

/// Map server tokens to the same roles used by syntax highlighting.
pub fn semantic_role(kind: &str, modifiers: &[String], clangd: bool) -> Option<SemanticRole> {
    use LegacySemantic::{Both, Light, Neither};
    use Part::*;
    let has = |m: &str| modifiers.iter().any(|x| x == m);
    let (part, legacy) = match kind {
        "macro" => (Macro, Both),
        "enumMember" | "constant" => (Constant, Both),
        "variable" if has("readonly") && (has("static") || has("globalScope") || has("fileScope")) => (Constant, Both),
        "variable" if has("constant") => (Constant, Both),
        "function" | "method" => (Function, Both),
        "type" | "class" | "struct" | "enum" | "interface" | "typeParameter" | "concept" | "typeAlias" | "union" => (Type, Both),
        "namespace" => (Type, Light),
        "parameter" => (Variable, Both),
        "variable" | "property" => (Variable, Neither),
        "keyword" | "modifier" => (Keyword, Neither),
        "string" | "regexp" => (String, Neither),
        "number" => (Number, Neither),
        "operator" => (Operator, Neither),
        // clangd reports inactive preprocessor branches as semantic comments.
        "comment" if clangd || has("inactive") || has("disabled") => (Disabled, Both),
        "comment" => (Comment, Both),
        _ => return None,
    };
    Some(SemanticRole { part, legacy })
}

/// Overlay semantic token roles (sorted, non-overlapping byte ranges) on a line of syntax spans.
pub fn overlay(spans: &[Span], over: &[(Range<usize>, SemanticRole)], len: usize, palette: &Palette) -> Vec<Span> {
    let plain = |range: Range<usize>| Span {
        range,
        part: Part::Foreground,
        legacy_color: None,
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
    let mut o = over.iter().filter(|(_, role)| role.applies(palette)).peekable();
    for s in base {
        let mut start = s.range.start;
        while start < s.range.end {
            while o.next_if(|(r, _)| r.end <= start).is_some() {}
            match o.peek() {
                Some((r, role)) if r.start < s.range.end => {
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
                        part: role.part,
                        legacy_color: None,
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

    fn span(range: Range<usize>, part: Part) -> Span {
        Span {
            range,
            part,
            legacy_color: None,
            italics: false,
            underline: false,
        }
    }

    #[test]
    fn overlays_semantic_colors_on_syntax_spans() {
        use Part::*;
        let macro_role = SemanticRole::new(Macro);
        let palette = Palette::defaults(true);
        let spans = [span(0..3, Type), span(4..12, Variable)];
        let ranges = |out: Vec<Span>| out.into_iter().map(|s| (s.range, s.part)).collect::<Vec<_>>();
        assert_eq!(
            ranges(overlay(&spans, &[(4..7, macro_role), (8..11, macro_role)], 12, &palette)),
            vec![
                (0..3, Type),
                (3..4, Foreground),
                (4..7, Macro),
                (7..8, Variable),
                (8..11, Macro),
                (11..12, Variable)
            ]
        );
        assert_eq!(
            ranges(overlay(&spans, &[(2..6, macro_role)], 12, &palette)),
            vec![(0..2, Type), (2..3, Macro), (3..4, Macro), (4..6, Macro), (6..12, Variable)]
        );
        assert!(overlay(&[], &[], 0, &palette).is_empty());
    }

    #[test]
    fn semantic_roles_for_macros_constants_and_types() {
        let part = |kind, modifiers: &[String]| semantic_role(kind, modifiers, true).map(|role| role.part);
        assert_eq!(part("macro", &[]), Some(Part::Macro));
        assert_eq!(part("enumMember", &[]), Some(Part::Constant));
        let mods = ["readonly".to_string(), "fileScope".to_string()];
        assert_eq!(part("variable", &mods), Some(Part::Constant));
        assert_eq!(part("variable", &[]), Some(Part::Variable));
        assert_eq!(part("keyword", &[]), Some(Part::Keyword));
        assert_eq!(part("function", &[]), Some(Part::Function));
        assert_eq!(part("comment", &[]), Some(Part::Disabled));
        assert_eq!(semantic_role("comment", &[], false).unwrap().part, Part::Comment);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn syntect_scopes_map_to_palette_roles() {
        use Part::*;
        let scopes = Scopes::new();
        let mut categories = HashMap::new();
        for (names, expected) in [
            (&["source.c"][..], Foreground),
            (&["source.c", "comment.line.c", "punctuation.definition.comment.c"], Comment),
            (&["string.quoted.double", "constant.character.escape"], String),
            (&["keyword.control.c"], Keyword),
            (&["storage.modifier.rs"], Keyword),
            (&["constant.numeric.integer.c"], Number),
            (&["constant.other.enum.cs"], Constant),
            (&["entity.name.function.preprocessor.c"], Macro),
            (&["meta.preprocessor.c", "keyword.control.preprocessor.c"], Macro),
            (&["entity.name.type.rust"], Type),
            (&["support.type.c"], Type),
            (&["entity.name.function.c"], Function),
            (&["support.function.c"], Function),
            (&["variable.parameter.js"], Variable),
            (&["keyword.operator.arithmetic"], Operator),
            (&["punctuation.separator.c"], Operator),
            (&["comment.block.preprocessor.c"], Disabled),
        ] {
            let stack = ScopeStack::from_vec(names.iter().map(|name| Scope::new(name).unwrap()).collect());
            assert_eq!(scopes.category(&stack, &mut categories).0, expected, "{names:?}");
        }
    }

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
    fn constants_and_macros_have_distinct_editable_roles() {
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
            let mut cache = Cache::default();
            cache.update(source, syn, true);
            let actual: Vec<_> = cache
                .lines
                .iter()
                .flat_map(|line| line.spans.iter().flat_map(|s| std::iter::repeat_n(s.part, s.range.len())))
                .collect();
            for &(name, count) in constants {
                for (start, _) in source.match_indices(name).take(count) {
                    let part = if source[..start].ends_with("#define ") {
                        Part::Macro
                    } else {
                        Part::Constant
                    };
                    assert!(
                        actual[start..start + name.len()].iter().all(|&p| p == part),
                        "{lang} {name} at {start}: {:?}",
                        actual[start]
                    );
                }
            }
            assert_ne!(actual[source.find("local").unwrap()], Part::Constant);
            for name in ["\"MAX\"", "\"BUF_SIZE\"", "\"MAX_RETRIES\""] {
                if let Some(start) = source.find(name) {
                    assert_eq!(actual[start + 1], Part::String);
                }
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

    fn full(text: &str, lang: &str) -> Vec<Vec<Span>> {
        let mut c = Cache::default();
        c.update(text, syntax(lang).unwrap(), true);
        c.lines.iter().map(|l| l.spans.clone()).collect()
    }

    fn incremental(steps: &[&str], lang: &str) -> Vec<Vec<Span>> {
        let mut c = Cache::default();
        for t in steps {
            c.update(t, syntax(lang).unwrap(), true);
        }
        c.lines.iter().map(|l| l.spans.clone()).collect()
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
