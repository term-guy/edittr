//! Go to definition: ask a language server when one is available, otherwise
//! fall back to a heuristic search for definition-looking lines in the project.

use crate::{lsp, workspace};
use regex::Regex;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

const LSP_TIMEOUT: Duration = Duration::from_secs(5);

pub struct Request {
    pub root: PathBuf,
    pub files: Box<dyn FnOnce() -> Vec<String> + Send>,
    pub path: PathBuf,
    pub text: String,
    /// Ignored folders, relative to `root`: never a source of definitions.
    pub ignored: Vec<String>,
    pub char_idx: usize,
    pub client: Option<Arc<lsp::Launch>>,
    pub revision: lsp::Revision,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Target {
    pub path: PathBuf,
    pub line: usize,
    /// Char columns of the symbol within the line.
    pub cols: Range<usize>,
}

pub enum Outcome {
    Found { target: Target, via: String },
    NotFound(String),
}

pub fn run(req: Request) -> Outcome {
    let Some((_, word)) = word_at(&req.text, req.char_idx) else {
        return Outcome::NotFound("No symbol under the cursor".into());
    };
    let ignored = |p: &Path| workspace::relative_path(&req.root, p).is_some_and(|rel| workspace::is_ignored(&rel, &req.ignored));
    let mut lsp_note = String::new();
    if let Some(client) = req.client.as_ref().and_then(|launch| launch.wait()) {
        let (line, col16) = line_col16(&req.text, req.char_idx);
        let mut result = client.definition(&req.path, &req.text, req.revision, (line, col16), LSP_TIMEOUT);
        // Right after a file is opened, the TypeScript server answers with the
        // `import` line until it has loaded the project; ask again a few times.
        for _ in 0..6 {
            let points_at_import = matches!(&result, Ok(locs) if locs.first().is_some_and(|l| {
                l.path == req.path && req.text.split('\n').nth(l.line).is_some_and(is_import_line)
            }));
            if !points_at_import {
                break;
            }
            std::thread::sleep(Duration::from_millis(500));
            result = client.definition(&req.path, &req.text, req.revision, (line, col16), LSP_TIMEOUT);
        }
        match result {
            Ok(locs) => {
                // Servers that can't be told to exclude folders may still answer with one.
                if let Some(loc) = locs.into_iter().find(|l| !ignored(&l.path)) {
                    let line_text = if loc.path == req.path {
                        req.text.split('\n').nth(loc.line).map(str::to_string)
                    } else {
                        std::fs::read_to_string(&loc.path)
                            .ok()
                            .and_then(|t| t.split('\n').nth(loc.line).map(str::to_string))
                    }
                    .unwrap_or_default();
                    let cols = col16_to_char(&line_text, loc.col16.start)..col16_to_char(&line_text, loc.col16.end);
                    return Outcome::Found {
                        target: Target {
                            path: loc.path,
                            line: loc.line,
                            cols,
                        },
                        via: client.name.to_string(),
                    };
                }
                lsp_note = format!("{} found nothing; ", client.name);
            }
            Err(e) => lsp_note = format!("{e}; "),
        }
    }
    let (cursor_line, _) = line_col16(&req.text, req.char_idx);
    let current = (!ignored(&req.path)).then_some(req.text.as_str());
    match heuristic(&req.root, (req.files)(), &req.path, current, cursor_line, &word) {
        Some(target) => Outcome::Found {
            target,
            via: format!("{lsp_note}text search"),
        },
        None => Outcome::NotFound(format!("{lsp_note}no definition found for `{word}`")),
    }
}

/// `import { x } from "./y"` and friends (JS/TS, Python).
fn is_import_line(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with("import ") || t.starts_with("import{") || (t.starts_with("from ") && t.contains(" import ")) || t.starts_with("export {")
}

// ------------------------------------------------------------- positions

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

/// Identifier around char index `idx` (also accepts the position right after it).
pub fn word_at(text: &str, idx: usize) -> Option<(Range<usize>, String)> {
    let chars: Vec<char> = text.chars().collect();
    let mut i = idx.min(chars.len());
    if i == chars.len() || !is_word_char(chars[i]) {
        if i > 0 && is_word_char(chars[i - 1]) {
            i -= 1;
        } else {
            return None;
        }
    }
    let mut start = i;
    while start > 0 && is_word_char(chars[start - 1]) {
        start -= 1;
    }
    let mut end = i;
    while end < chars.len() && is_word_char(chars[end]) {
        end += 1;
    }
    let word: String = chars[start..end].iter().collect();
    (!word.chars().next()?.is_numeric()).then_some((start..end, word))
}

/// 0-based line and UTF-16 column of char index `idx`.
pub fn line_col16(text: &str, idx: usize) -> (usize, usize) {
    let (mut line, mut col) = (0, 0);
    for c in text.chars().take(idx) {
        if c == '\n' {
            line += 1;
            col = 0;
        } else {
            col += c.len_utf16();
        }
    }
    (line, col)
}

pub(crate) fn col16_to_char(line: &str, col16: usize) -> usize {
    let mut units = 0;
    for (i, c) in line.chars().enumerate() {
        if units >= col16 {
            return i;
        }
        units += c.len_utf16();
    }
    line.chars().count()
}

// ------------------------------------------------------------- heuristic

/// Extensions that can define symbols used from a file with extension `ext`.
fn related_exts(ext: &str) -> Option<&'static [&'static str]> {
    const GROUPS: &[&[&str]] = &[
        &["c", "h", "cc", "cpp", "cxx", "hpp", "hh", "hxx", "m", "mm"],
        &["ts", "tsx", "js", "jsx", "mjs", "cjs", "mts", "cts", "vue", "svelte"],
        &["py", "pyi"],
        &["rs"],
        &["go"],
        &["java", "kt", "kts", "scala"],
        &["rb"],
        &["php"],
        &["cs"],
        &["swift"],
        &["lua"],
        &["sh", "bash", "zsh"],
    ];
    GROUPS.iter().copied().find(|g| g.contains(&ext))
}

struct Pattern {
    regex: Regex,
    score: u8,
    kind: &'static str,
}

fn patterns(name: &str) -> Vec<Pattern> {
    let keywords = r"fn|function|def|class|struct|enum|trait|type|interface|const|let|var|val|mod|module|namespace|func|union|typedef|macro_rules!|record|object|protocol|static|alias|local";
    [
        // `fn name`, `class Name`, `let mut name`, `func (r *T) name`.
        (
            format!(r"(?:^|[^\w.$])(?P<keyword>{keywords})\s+(?:mut\s+|async\s+|\*\s*)?(?:\([^)]*\)\s*)?(?P<name>{name})\b"),
            4,
            "Definition",
        ),
        (format!(r"^\s*#\s*define\s+(?P<name>{name})\b"), 4, "Macro"),
        // C/C++/Java/C# style declarations.
        (
            format!(r"^\s*(?:[\w:<>,\[\]\*&]+\s+)+[\*&]*(?P<name>{name})\s*(?P<separator>\(|=[^=]|;|\[)"),
            2,
            "Definition",
        ),
        (format!(r"^\s*(?:export\s+)?(?P<name>{name})\s*(?::[^=]*)?=[^=>]"), 1, "Variable"),
        (format!(r"^\s*(?P<name>{name})\s*:\s*(?:function|\(|async)"), 1, "Function"),
        (
            format!(r"^\s*(?:async\s+|static\s+|get\s+|set\s+)*(?P<name>{name})\s*\([^)]*\)\s*\{{\s*$"),
            1,
            "Method",
        ),
    ]
    .into_iter()
    .filter_map(|(pattern, score, kind)| {
        let regex = Regex::new(&pattern).ok()?;
        Some(Pattern { regex, score, kind })
    })
    .collect()
}

const NOT_DEFINITIONS: &[&str] = &[
    "return ", "else ", "if ", "while ", "case ", "new ", "await ", "throw ", "yield ", "delete ",
];

pub(crate) struct Definition<'a> {
    pub(crate) name: &'a str,
    pub(crate) kind: &'static str,
    pub(crate) line: usize,
    pub(crate) cols: Range<usize>,
    score: u8,
}

fn definitions_in<'a>(patterns: &'a [Pattern], text: &'a str) -> impl Iterator<Item = Definition<'a>> {
    text.split('\n').enumerate().filter_map(move |(line_no, line)| {
        let t = line.trim_start();
        let is_comment = t.starts_with("//")
            || t.starts_with("/*")
            || t.starts_with("* ")
            || t.starts_with("--")
            || (t.starts_with('#') && !t.trim_start_matches(['#', ' ']).starts_with("define"));
        if is_comment || NOT_DEFINITIONS.iter().any(|k| t.starts_with(k)) {
            return None;
        }
        let mut matches = patterns.iter().filter_map(|pattern| {
            let captures = pattern.regex.captures(line)?;
            Some((captures, pattern))
        });
        let (mut captures, mut pattern) = matches.next()?;
        let mut name = captures.name("name")?;
        let after_name = line[name.end()..].trim_start();
        if matches!(captures.name("keyword").map(|m| m.as_str()), Some("const" | "static"))
            && (!after_name.starts_with(':') || after_name.starts_with("::"))
            && let Some((typed, declaration)) = matches.next()
            && typed.name("separator").is_some()
            && let Some(typed_name) = typed.name("name")
            && typed_name.as_str() != name.as_str()
        {
            // In `static int count()` the declaration supplies the name, not the keyword pattern.
            (captures, pattern, name) = (typed, declaration, typed_name);
        }
        let kind = match captures.name("keyword").map(|m| m.as_str()) {
            Some("fn" | "function" | "def" | "func") => "Function",
            Some("class") => "Class",
            Some("struct" | "record" | "union") => "Struct",
            Some("enum") => "Enum",
            Some("trait" | "interface" | "protocol") => "Interface",
            Some("const") => "Constant",
            Some("mod" | "module" | "namespace") => "Module",
            Some("type" | "typedef" | "alias") => "Type",
            Some("macro_rules!") => "Macro",
            Some("object") => "Object",
            Some(_) => "Variable",
            None => match captures.name("separator").map(|m| m.as_str()) {
                Some("(") => "Function",
                Some(_) => "Variable",
                None => pattern.kind,
            },
        };
        let start = line[..name.start()].chars().count();
        Some(Definition {
            name: name.as_str(),
            kind,
            line: line_no,
            cols: start..start + name.as_str().chars().count(),
            score: pattern.score,
        })
    })
}

pub(crate) fn definitions(text: &str) -> impl Iterator<Item = Definition<'_>> {
    static PATTERNS: std::sync::LazyLock<Vec<Pattern>> = std::sync::LazyLock::new(|| patterns(r"[\p{Alphabetic}_$][\w$]*"));
    definitions_in(&PATTERNS, text)
}

/// Definition-looking matches of the requested name in one file.
fn best_in_text(pats: &[Pattern], text: &str) -> Vec<(u8, usize, Range<usize>)> {
    definitions_in(pats, text).map(|def| (def.score, def.line, def.cols)).collect()
}

fn heuristic(root: &Path, files: Vec<String>, current: &Path, text: Option<&str>, cursor_line: usize, name: &str) -> Option<Target> {
    let pats = patterns(&regex::escape(name));

    // 1) Current buffer: the highest-scoring definition, preferring the closest one
    //    above the cursor (so local variables resolve to their nearest `let`).
    let local = text.map(|text| best_in_text(&pats, text)).unwrap_or_default();
    let pick = local.iter().max_by_key(|(score, line, _)| {
        let above = *line <= cursor_line;
        let dist = if above { cursor_line - line } else { usize::MAX / 2 + line };
        (*score, above, std::cmp::Reverse(dist))
    });
    if let Some((score, line, cols)) = pick
        && *score >= 2
    {
        return Some(Target {
            path: current.to_path_buf(),
            line: *line,
            cols: cols.clone(),
        });
    }

    // 2) Other files in the project with a related extension.
    let ext = current.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
    let related = related_exts(&ext);
    let mut best: Option<(u8, Target)> = None;
    for rel in files {
        let path = root.join(&rel);
        if path == current {
            continue;
        }
        let fext = Path::new(&rel).extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
        if let Some(group) = related
            && !group.contains(&fext.as_str())
        {
            continue;
        }
        if std::fs::metadata(&path).map(|m| m.len() > 2_000_000).unwrap_or(true) {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&path) else { continue };
        if !content.contains(name) {
            continue;
        }
        if let Some((score, line, cols)) = best_in_text(&pats, &content)
            .into_iter()
            .max_by_key(|(s, l, _)| (*s, std::cmp::Reverse(*l)))
            && best.as_ref().is_none_or(|(s, _)| score > *s)
        {
            best = Some((score, Target { path, line, cols }));
            if score >= 4 {
                break;
            }
        }
    }
    // 3) A weak match in the current file beats a weak match elsewhere.
    match (pick, best) {
        (Some((ls, line, cols)), Some((bs, _))) if *ls >= bs => Some(Target {
            path: current.to_path_buf(),
            line: *line,
            cols: cols.clone(),
        }),
        (_, Some((_, t))) => Some(t),
        (Some((_, line, cols)), None) => Some(Target {
            path: current.to_path_buf(),
            line: *line,
            cols: cols.clone(),
        }),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn find_local(src: &str, cursor_line: usize, name: &str) -> Option<(usize, Range<usize>)> {
        heuristic(
            Path::new("/nonexistent"),
            vec![],
            Path::new("/x/a.rs"),
            Some(src),
            cursor_line,
            name,
        )
        .map(|t| (t.line, t.cols))
    }

    #[test]
    fn recognizes_import_lines() {
        assert!(is_import_line("import { greet } from \"./util\";"));
        assert!(is_import_line("  from util import greet"));
        assert!(is_import_line("export { greet } from \"./util\";"));
        assert!(!is_import_line("const important = 1;"));
        assert!(!is_import_line("fn import_all() {}"));
    }

    #[test]
    fn word_and_positions() {
        let t = "let héllo_1 = 2;\nfoo(x)";
        assert_eq!(word_at(t, 5).unwrap().1, "héllo_1");
        assert_eq!(word_at(t, 11).unwrap().1, "héllo_1"); // right after the word
        assert!(word_at(t, 12).is_none());
        assert_eq!(word_at(t, 14), None); // "2" is a number
        assert_eq!(line_col16("a😀b\ncd", 2), (0, 3)); // emoji is 2 UTF-16 units
        assert_eq!(line_col16("a😀b\ncd", 5), (1, 1));
        assert_eq!(col16_to_char("a😀b", 3), 2);
    }

    #[test]
    fn finds_definitions_across_languages() {
        let rust = "struct Foo;\nfn main() {\n    let x = 1;\n    let x = 2;\n    use_it(x);\n}\nfn use_it(v: i32) {}\n";
        assert_eq!(find_local(rust, 4, "x"), Some((3, 8..9))); // nearest `let` above the cursor
        assert_eq!(find_local(rust, 4, "use_it"), Some((6, 3..9)));
        assert_eq!(find_local(rust, 4, "Foo"), Some((0, 7..10)));

        let py = "import os\n\nclass Thing:\n    def run(self):\n        return helper()\n\nhelper = lambda: 1\n";
        assert_eq!(find_local(py, 4, "run"), Some((3, 8..11)));
        assert_eq!(find_local(py, 4, "helper"), Some((6, 0..6)));

        let c = "#define MAX 10\nstatic int count(const char *s) {\n  return strlen(s);\n}\nint main() { return count(\"a\"); }\n";
        assert_eq!(find_local(c, 4, "MAX"), Some((0, 8..11)));
        assert_eq!(find_local(c, 4, "count"), Some((1, 11..16)));

        let go = "func (s *Server) Start() error {\n\treturn nil\n}\n";
        assert_eq!(find_local(go, 1, "Start"), Some((0, 17..22)));

        let js = "export const add = (a, b) => a + b;\nclass A {\n  render() {\n    return add(1, 2);\n  }\n}\n";
        assert_eq!(find_local(js, 3, "add"), Some((0, 13..16)));
        assert_eq!(find_local(js, 3, "render"), Some((2, 2..8)));
    }

    #[test]
    fn searches_other_files_of_related_languages() {
        let dir = std::env::temp_dir().join(format!("edittr-goto-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/util.rs"), "// fn helper is below\npub fn helper() -> u8 { 1 }\n").unwrap();
        std::fs::write(dir.join("notes.py"), "def helper(): pass\n").unwrap();
        let files = vec!["notes.py".to_string(), "src/util.rs".into()];
        let t = heuristic(&dir, files, &dir.join("src/main.rs"), Some("fn main() { helper(); }"), 0, "helper").unwrap();
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(t.path, dir.join("src/util.rs")); // skips the .py file and the comment
        assert_eq!((t.line, t.cols), (1, 7..13));
    }

    /// Real language server round trip. Needs the server installed; run with
    /// `cargo test -- --ignored lsp_round_trip_<name>`.
    fn lsp_round_trip(ext: &str, files: &[(&str, &str)], main: &str, symbol: &str, expect: (&str, usize)) {
        let dir = std::env::temp_dir().join(format!("edittr-lsp-{ext}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for (name, body) in files {
            let p = dir.join(name);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, body).unwrap();
        }
        let dir = std::fs::canonicalize(&dir).unwrap();
        let (spec, command) = lsp::servers_for(ext)
            .into_iter()
            .find_map(|spec| lsp::resolve(spec.command).map(|command| (spec, command)))
            .expect("server not installed");
        let launch = Arc::new(lsp::Launch::default());
        launch.finish(Some(lsp::Client::start(&spec, command, &dir, &[], None).unwrap()));
        let text = std::fs::read_to_string(dir.join(main)).unwrap();
        let idx = text.find(symbol).map(|b| text[..b].chars().count()).unwrap() + 1;
        // Indexing can take a moment: retry until the server answers with a location.
        let start = std::time::Instant::now();
        let target = loop {
            let req = Request {
                root: dir.clone(),
                files: Box::new(Vec::new),
                path: dir.join(main),
                text: text.clone(),
                ignored: Vec::new(),
                char_idx: idx,
                client: Some(launch.clone()),
                revision: (0, 0),
            };
            match run(req) {
                Outcome::Found { target, via } if via == spec.name => break target,
                _ if start.elapsed() < Duration::from_secs(90) => std::thread::sleep(Duration::from_millis(500)),
                _ => panic!("{} never answered", spec.name),
            }
        };
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!((target.path.strip_prefix(&dir).unwrap().to_str().unwrap(), target.line), expect);
    }

    #[test]
    #[ignore]
    fn lsp_round_trip_c() {
        lsp_round_trip(
            "c",
            &[
                ("util.h", "#pragma once\nint add(int a, int b);\n"),
                ("util.c", "#include \"util.h\"\n\nint add(int a, int b) { return a + b; }\n"),
                (
                    "main.c",
                    "#include \"util.h\"\nint main(void) {\n  int total = add(1, 2);\n  return total;\n}\n",
                ),
                ("compile_flags.txt", "-I.\n"),
            ],
            "main.c",
            "add(1",
            ("util.h", 1), // clangd: definition lives in another TU, so it reports the declaration
        );
    }

    #[test]
    #[ignore]
    fn lsp_round_trip_rust() {
        lsp_round_trip(
            "rs",
            &[
                ("Cargo.toml", "[package]\nname = \"t\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"),
                (
                    "src/util.rs",
                    "pub struct Point { pub x: i32 }\n\npub fn make() -> Point { Point { x: 1 } }\n",
                ),
                (
                    "src/main.rs",
                    "mod util;\nfn main() {\n    let p = util::make();\n    println!(\"{}\", p.x);\n}\n",
                ),
            ],
            "src/main.rs",
            "make()",
            ("src/util.rs", 2),
        );
    }

    #[test]
    #[ignore]
    fn lsp_round_trip_ts() {
        lsp_round_trip(
            "ts",
            &[
                ("tsconfig.json", "{ \"compilerOptions\": { \"strict\": true } }\n"),
                (
                    "util.ts",
                    "export const VERSION = 1;\n\nexport function greet(name: string) {\n  return `hi ${name}`;\n}\n",
                ),
                ("main.ts", "import { greet } from \"./util\";\n\nconsole.log(greet(\"x\"));\n"),
            ],
            "main.ts",
            "greet(\"x\")",
            ("util.ts", 2),
        );
    }

    #[test]
    #[ignore]
    fn lsp_round_trip_py() {
        lsp_round_trip(
            "py",
            &[
                ("util.py", "import os\n\n\ndef greet(name):\n    return f\"hi {name}\"\n"),
                ("main.py", "from util import greet\n\nprint(greet(\"x\"))\n"),
            ],
            "main.py",
            "greet(\"x\")",
            ("util.py", 3),
        );
    }

    #[test]
    #[ignore]
    fn lsp_round_trip_go() {
        lsp_round_trip(
            "go",
            &[
                ("go.mod", "module example.com/t\n\ngo 1.21\n"),
                (
                    "util.go",
                    "package main\n\n// Greet says hi.\nfunc Greet(name string) string { return \"hi \" + name }\n",
                ),
                (
                    "main.go",
                    "package main\n\nimport \"fmt\"\n\nfunc main() { fmt.Println(Greet(\"x\")) }\n",
                ),
            ],
            "main.go",
            "Greet(\"x\")",
            ("util.go", 3),
        );
    }
}
