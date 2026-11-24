//! Static Makefile inference for use on the language-server launch worker.
use crate::workspace;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};

const SOURCES: &[&str] = &["c", "cc", "cpp", "cxx", "m", "mm"];
const HEADERS: &[&str] = &["h", "hh", "hpp", "hxx", "inl", "ipp", "tpp"];
const MAKEFILE_NAMES: &[&str] = &["GNUmakefile", "makefile", "Makefile"];
const FLAG_VARIABLES: &[&str] = &[
    "CPPFLAGS",
    "CFLAGS",
    "CXXFLAGS",
    "INCLUDES",
    "INCLUDE",
    "INC",
    "INCS",
    "INC_DIRS",
    "INCLUDE_DIRS",
    "DEFINES",
    "DEFS",
];
const MAX_MAKEFILES: usize = 200;
const MAX_MAKE_BYTES: usize = 2 * 1024 * 1024;
const MAX_EXPANSION: usize = 64 * 1024;
const MAX_ALTERNATIVES: usize = 16;
// Keep unexpanded references in their original word, so the whole flag can be discarded.
const UNRESOLVED: char = '\0';

#[derive(Default)]
pub(crate) struct Inferred {
    pub flags: Vec<String>,
    pub compiler: Option<String>,
    pub cxx_compiler: Option<String>,
    pub inputs: Vec<PathBuf>,
    pub partial: bool,
    pub guessed_includes: usize,
}

fn extension(file: &str) -> &str {
    Path::new(file).extension().and_then(OsStr::to_str).unwrap_or_default()
}

fn has_extension(ext: &str, extensions: &[&str]) -> bool {
    extensions.iter().any(|candidate| ext.eq_ignore_ascii_case(candidate))
}

fn absolute(base: &Path, path: impl AsRef<Path>) -> PathBuf {
    let mut normalized = PathBuf::new();
    for part in base.join(path).components() {
        match part {
            Component::ParentDir => {
                if normalized.file_name().is_some() {
                    normalized.pop();
                }
            }
            Component::CurDir => {}
            _ => normalized.push(part),
        }
    }
    normalized
}

fn read_file(path: &Path, limit: usize) -> io::Result<Vec<u8>> {
    if !fs::metadata(path)?.is_file() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "expected a regular file"));
    }
    let mut bytes = Vec::new();
    File::open(path)?.take(limit as u64).read_to_end(&mut bytes)?;
    Ok(bytes)
}

#[derive(Clone, Default)]
struct Expansion {
    text: String,
    // Possible replacements from undecidable conditionals. Only include paths may be collected from them.
    alternatives: Vec<String>,
    limited: bool,
}

impl Expansion {
    fn unresolved() -> Self {
        Self {
            text: UNRESOLVED.into(),
            ..Self::default()
        }
    }

    fn is_literal(&self) -> bool {
        self.alternatives.is_empty() && !self.text.contains(UNRESOLVED)
    }

    fn push(&mut self, text: &str) {
        if self.limited {
            return;
        }
        if std::iter::once(&self.text)
            .chain(&self.alternatives)
            .any(|head| head.len() + text.len() > MAX_EXPANSION)
        {
            self.limit();
            return;
        }
        self.text.push_str(text);
        for alternative in &mut self.alternatives {
            alternative.push_str(text);
        }
    }

    fn append(&mut self, tail: Self) {
        if self.limited || tail.limited {
            self.limit();
            return;
        }
        if tail.alternatives.is_empty() || self.alternatives.len() == MAX_ALTERNATIVES {
            self.push(&tail.text);
            return;
        }
        let head_len = std::iter::once(&self.text)
            .chain(&self.alternatives)
            .map(String::len)
            .max()
            .unwrap_or_default();
        let tail_len = std::iter::once(&tail.text)
            .chain(&tail.alternatives)
            .map(String::len)
            .max()
            .unwrap_or_default();
        if head_len + tail_len > MAX_EXPANSION {
            self.limit();
            return;
        }
        let heads = std::iter::once(&self.text).chain(&self.alternatives);
        let combinations = heads
            .flat_map(|head| tail.alternatives.iter().map(move |suffix| format!("{head}{suffix}")))
            .take(MAX_ALTERNATIVES);
        let mut alternatives: Vec<_> = self.alternatives.iter().map(|head| format!("{head}{}", tail.text)).collect();
        alternatives.extend(combinations.take(MAX_ALTERNATIVES - alternatives.len()));
        self.text.push_str(&tail.text);
        self.alternatives = alternatives;
    }

    fn limit(&mut self) {
        *self = Self {
            limited: true,
            ..Self::unresolved()
        };
    }
}

enum Binding {
    Recursive(String),
    Simple(Expansion),
}

#[derive(Default)]
struct Variable {
    certain: Option<Binding>,
    possible: Vec<Binding>,
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Skip,
    Certain,
    Possible,
}

impl Mode {
    fn branch(self, test: Option<bool>) -> Self {
        match (self, test) {
            (Self::Skip, _) | (_, Some(false)) => Self::Skip,
            (_, None) => Self::Possible,
            (mode, Some(true)) => mode,
        }
    }
}

struct Conditional {
    remaining: Mode,
    outer: Mode,
}

#[derive(Clone, Copy, PartialEq)]
enum Operator {
    Recursive,
    Simple,
    Default,
    Append,
}

fn directive(line: &str) -> (&str, &str) {
    line.split_once(char::is_whitespace)
        .map_or((line, ""), |(word, rest)| (word, rest.trim()))
}

fn variable_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}

fn assignment(mut line: &str) -> Option<(&str, Operator, &str)> {
    while let ("override" | "export", rest) = directive(line) {
        line = rest;
    }
    let (left, value) = line.split_once('=')?;
    let (name, op) = [
        ("::", Operator::Simple),
        (":", Operator::Simple),
        ("?", Operator::Default),
        ("+", Operator::Append),
    ]
    .into_iter()
    .find_map(|(suffix, op)| Some((left.strip_suffix(suffix)?, op)))
    .unwrap_or((left, Operator::Recursive));
    let name = name.trim_end();
    variable_name(name).then(|| (name, op, value.trim_start()))
}

fn comment(line: &str) -> String {
    let mut result = String::new();
    let mut chars = line.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '#' => break,
            '\\' if chars.peek() == Some(&'#') => result.push(chars.next().unwrap()),
            '\\' if chars.peek() == Some(&'\\') => {
                result.push(ch);
                result.push(chars.next().unwrap());
            }
            _ => result.push(ch),
        }
    }
    result
}

// Enough shell syntax for quoted paths and flag arguments, without any shell evaluation.
fn words(text: &str) -> impl Iterator<Item = String> + '_ {
    let mut chars = text.chars().peekable();
    std::iter::from_fn(move || {
        while chars.next_if(|ch| ch.is_whitespace()).is_some() {}
        chars.peek()?;
        let mut quote = None;
        let mut word = String::new();
        while let Some(ch) = chars.next() {
            if Some(ch) == quote {
                quote = None;
            } else if quote.is_none() && matches!(ch, '\'' | '"') {
                quote = Some(ch);
            } else if ch == '\\'
                && quote != Some('\'')
                && chars
                    .peek()
                    .is_some_and(|next| matches!(next, '\'' | '"' | '\\') || quote.is_none() && next.is_whitespace())
            {
                word.push(chars.next().unwrap());
            } else if ch.is_whitespace() && quote.is_none() {
                break;
            } else {
                word.push(ch);
            }
        }
        if quote.is_some() {
            word.push(UNRESOLVED);
        }
        Some(word)
    })
}

fn make_reference(text: &str) -> (Option<&str>, &str) {
    let mut chars = text.char_indices();
    let Some((_, open)) = chars.next() else { return (None, "") };
    let close = match open {
        '(' => ')',
        '{' => '}',
        _ => return (None, &text[open.len_utf8()..]),
    };
    let mut nesting = 0;
    for (index, ch) in chars {
        if ch == open {
            nesting += 1;
        } else if ch == close {
            if nesting == 0 {
                let name = &text[1..index];
                return (variable_name(name).then_some(name), &text[index + 1..]);
            }
            nesting -= 1;
        }
    }
    (None, "")
}

// A wildcard can backtrack within its path component, but never crosses a slash.
fn glob_matches(mut pattern: &str, mut path: &str) -> bool {
    let mut star = None::<(&str, &str)>;
    while let Some(ch) = path.chars().next() {
        if let Some(rest) = pattern.strip_prefix('*') {
            pattern = rest;
            star = Some((rest, path));
        } else if let Some(rest) = pattern
            .strip_prefix(ch)
            .or_else(|| (ch != '/').then(|| pattern.strip_prefix('?')).flatten())
        {
            pattern = rest;
            path = &path[ch.len_utf8()..];
            if ch == '/' {
                star = None;
            }
        } else if let Some((after_star, remaining)) = star
            && let Some(next) = remaining.chars().next().filter(|ch| *ch != '/')
        {
            pattern = after_star;
            path = &remaining[next.len_utf8()..];
            star = Some((pattern, path));
        } else {
            return false;
        }
    }
    pattern.bytes().all(|byte| byte == b'*')
}

struct Makefiles<'a> {
    root: &'a Path,
    files: &'a [String],
    sorted_files: Option<Vec<&'a str>>,
    cancelled: &'a dyn Fn() -> bool,
    active: HashSet<PathBuf>,
    inputs: Vec<PathBuf>,
    remaining_files: usize,
    remaining_bytes: usize,
    vars: HashMap<String, Variable>,
    flag_order: Vec<String>,
    partial: bool,
}

impl<'a> Makefiles<'a> {
    fn new(root: &'a Path, files: &'a [String], cancelled: &'a dyn Fn() -> bool) -> Self {
        Self {
            root,
            files,
            cancelled,
            sorted_files: None,
            active: HashSet::new(),
            inputs: Vec::new(),
            remaining_files: MAX_MAKEFILES,
            remaining_bytes: MAX_MAKE_BYTES,
            vars: HashMap::new(),
            flag_order: Vec::new(),
            partial: false,
        }
    }

    fn expand(&self, text: &str) -> Expansion {
        let mut budget = MAX_EXPANSION;
        self.expand_text(text, 0, &mut budget)
    }

    fn expand_value(&self, binding: &Binding) -> Expansion {
        let mut budget = MAX_EXPANSION;
        self.expand_binding(binding, 0, &mut budget)
    }

    fn expand_text(&self, mut text: &str, depth: usize, budget: &mut usize) -> Expansion {
        if depth >= 24 || text.len() > *budget {
            return Expansion::unresolved();
        }
        *budget -= text.len();
        let mut result = Expansion::default();
        while let Some((prefix, reference)) = text.split_once('$') {
            result.push(prefix);
            let (name, rest) = make_reference(reference);
            let value = name.map_or_else(Expansion::unresolved, |name| self.expand_variable(name, depth + 1, budget));
            result.append(value);
            if result.limited {
                break;
            }
            text = rest;
        }
        result.push(text);
        result
    }

    fn expand_binding(&self, binding: &Binding, depth: usize, budget: &mut usize) -> Expansion {
        match binding {
            Binding::Recursive(text) => self.expand_text(text, depth, budget),
            Binding::Simple(value) if value.text.len() <= *budget => {
                *budget -= value.text.len();
                value.clone()
            }
            Binding::Simple(_) => Expansion::unresolved(),
        }
    }

    fn expand_variable(&self, name: &str, depth: usize, budget: &mut usize) -> Expansion {
        if matches!(name, "CURDIR" | "PWD") {
            return Expansion {
                text: self.root.to_string_lossy().into_owned(),
                ..Expansion::default()
            };
        }
        let Some(var) = self.vars.get(name) else {
            return Expansion::unresolved();
        };
        let mut value = var
            .certain
            .as_ref()
            .map_or_else(Expansion::unresolved, |binding| self.expand_binding(binding, depth, budget));
        for binding in &var.possible {
            if value.alternatives.len() == MAX_ALTERNATIVES {
                break;
            }
            let variant = self.expand_binding(binding, depth, budget);
            let remaining = MAX_ALTERNATIVES - value.alternatives.len();
            value
                .alternatives
                .extend(std::iter::once(variant.text).chain(variant.alternatives).take(remaining));
        }
        value
    }

    fn assign(&mut self, name: &str, op: Operator, value: &str, mode: Mode) {
        let existing = self.vars.get(name);
        if op == Operator::Default && existing.is_some_and(|var| var.certain.is_some()) {
            return;
        }
        let mode = if op == Operator::Default && existing.is_some_and(|var| !var.possible.is_empty()) {
            Mode::Possible
        } else {
            mode
        };
        let simple = op == Operator::Simple
            || op == Operator::Append
                && existing
                    .and_then(|var| var.certain.as_ref())
                    .is_some_and(|binding| matches!(binding, Binding::Simple(_)));
        let binding = if simple {
            Binding::Simple(self.expand(value))
        } else {
            Binding::Recursive(value.to_owned())
        };
        if existing.is_none() && (FLAG_VARIABLES.contains(&name) || directory_variable(name)) {
            self.flag_order.push(name.to_owned());
        }
        let var = self.vars.entry(name.to_owned()).or_default();
        match (mode, op, &mut var.certain, binding) {
            (Mode::Possible, _, _, binding) => {
                if var.possible.len() < MAX_ALTERNATIVES {
                    var.possible.push(binding);
                }
            }
            (_, Operator::Append, Some(Binding::Recursive(text)), Binding::Recursive(extra)) => {
                text.push(' ');
                text.push_str(&extra);
            }
            (_, Operator::Append, Some(Binding::Simple(text)), Binding::Simple(extra)) => {
                text.push(" ");
                text.append(extra);
            }
            (_, op, slot, binding) => {
                *slot = Some(binding);
                if op != Operator::Append {
                    var.possible.clear();
                }
            }
        }
    }

    fn test(&mut self, word: &str, value: &str, mode: Mode) -> Option<bool> {
        if mode == Mode::Skip {
            return Some(false);
        }
        let result = match word {
            "ifdef" | "ifndef" if variable_name(value) => {
                let defined = if matches!(value, "CURDIR" | "PWD") {
                    Some(true)
                } else {
                    match self.vars.get(value) {
                        Some(var) if !var.possible.is_empty() => None,
                        Some(var) => match &var.certain {
                            Some(Binding::Simple(value)) => value.is_literal().then_some(!value.text.is_empty()),
                            Some(Binding::Recursive(value)) => Some(!value.is_empty()),
                            None => Some(false),
                        },
                        None => Some(false),
                    }
                };
                defined.map(|defined| defined == (word == "ifdef"))
            }
            _ => None,
        };
        self.partial |= result.is_none();
        result
    }

    fn read(&mut self, path: PathBuf, optional: bool, mode: Mode) {
        if (self.cancelled)() {
            return;
        }
        if self.remaining_files == 0 || self.remaining_bytes == 0 {
            self.partial = true;
            return;
        }
        let canonical = match path.canonicalize() {
            Ok(canonical) => canonical,
            Err(err) => {
                self.partial |= !optional || err.kind() != io::ErrorKind::NotFound;
                return;
            }
        };
        if !self.active.insert(canonical.clone()) {
            self.partial = true;
            return;
        }
        self.remaining_files -= 1;
        match read_file(&path, self.remaining_bytes + 1) {
            Ok(mut bytes) => {
                if !self.inputs.contains(&path) {
                    self.inputs.push(path.clone());
                }
                if bytes.len() > self.remaining_bytes {
                    bytes.truncate(self.remaining_bytes);
                    // Exclude any truncated assignment.
                    bytes.truncate(bytes.iter().rposition(|byte| *byte == b'\n').map_or(0, |end| end + 1));
                    self.remaining_bytes = 0;
                    self.partial = true;
                } else {
                    self.remaining_bytes -= bytes.len();
                }
                self.partial |= std::str::from_utf8(&bytes).is_err();
                self.parse(&String::from_utf8_lossy(&bytes), path.parent().unwrap_or(self.root), mode);
            }
            Err(err) => self.partial |= !optional || err.kind() != io::ErrorKind::NotFound,
        }
        self.active.remove(&canonical);
    }

    fn parse(&mut self, text: &str, dir: &Path, mut mode: Mode) {
        let mut conditions = Vec::<Conditional>::new();
        let mut defines = 0;
        let mut lines = text.lines();
        while let Some(line) = lines.next() {
            if (self.cancelled)() {
                return;
            }
            let recipe = line.starts_with('\t');
            let mut logical = String::new();
            let mut line = line;
            // In Make an odd number of final backslashes joins the following physical line.
            while line.bytes().rev().take_while(|byte| *byte == b'\\').count() % 2 == 1 {
                logical.push_str(line[..line.len() - 1].trim_end());
                logical.push(' ');
                line = lines.next().unwrap_or_default().trim_start();
            }
            if recipe {
                continue;
            }
            logical.push_str(line);
            let line = comment(&logical);
            let line = line.trim_start();
            let (word, rest) = directive(line);
            let definition = matches!(word, "define") || matches!(word, "override" | "export") && directive(rest).0 == "define";
            if definition && (defines > 0 || mode != Mode::Skip) {
                defines += 1;
                self.partial = true;
                continue;
            }
            if defines > 0 {
                if word == "endef" {
                    defines -= 1;
                }
                continue;
            }
            match word {
                "ifeq" | "ifneq" | "ifdef" | "ifndef" => {
                    let test = self.test(word, rest, mode);
                    conditions.push(Conditional {
                        remaining: mode.branch(test.map(|val| !val)),
                        outer: mode,
                    });
                    mode = mode.branch(test);
                }
                "else" => {
                    if let Some(condition) = conditions.last_mut() {
                        let (word, value) = directive(rest);
                        let test = if word.is_empty() {
                            Some(true)
                        } else {
                            self.test(word, value, condition.remaining)
                        };
                        mode = condition.remaining.branch(test);
                        condition.remaining = condition.remaining.branch(test.map(|val| !val));
                    } else {
                        self.partial = true;
                    }
                }
                "endif" => {
                    if let Some(condition) = conditions.pop() {
                        mode = condition.outer;
                    } else {
                        self.partial = true;
                    }
                }
                _ if mode == Mode::Skip => {}
                "include" | "-include" | "sinclude" if assignment(line).is_none() => {
                    self.include(rest, dir, word != "include", mode);
                }
                _ => {
                    if let Some((name, op, value)) = assignment(line) {
                        self.assign(name, op, value, mode);
                    }
                }
            }
        }
        self.partial |= !conditions.is_empty();
    }

    fn include(&mut self, value: &str, dir: &Path, optional: bool, mode: Mode) {
        let value = self.expand(value);
        self.partial |= !value.is_literal();
        for word in words(&value.text) {
            if (self.cancelled)() {
                return;
            }
            if word.contains(UNRESOLVED) || word.is_empty() {
                self.partial = true;
                continue;
            }
            if word.contains(['*', '?']) {
                self.include_glob(&word, dir, optional, mode);
            } else {
                let local = absolute(dir, &word);
                let path = if local.is_file() { local } else { absolute(self.root, &word) };
                self.read(path, optional, mode);
            }
        }
    }

    fn include_glob(&mut self, word: &str, dir: &Path, optional: bool, mode: Mode) {
        for base in [dir, self.root] {
            let path = absolute(base, word);
            let Some(relative) = workspace::relative_path(self.root, &path) else {
                continue;
            };
            let files = self.sorted_files.get_or_insert_with(|| {
                let mut paths: Vec<_> = self.files.iter().map(String::as_str).collect();
                paths.sort_unstable();
                paths
            });
            let prefix = relative.split(['*', '?']).next().unwrap_or_default();
            let suffix = relative.rsplit(['*', '?']).next().unwrap_or_default();
            let start = files.partition_point(|file| *file < prefix);
            let mut matching = Vec::new();
            for (index, file) in files[start..].iter().take_while(|file| file.starts_with(prefix)).enumerate() {
                if index % 256 == 0 && (self.cancelled)() {
                    return;
                }
                if file.ends_with(suffix) && glob_matches(&relative, file) {
                    matching.push(self.root.join(file));
                    if matching.len() > self.remaining_files {
                        break;
                    }
                }
            }
            if !matching.is_empty() {
                for path in matching {
                    self.read(path, optional, mode);
                }
                return;
            }
            if dir == self.root {
                break;
            }
        }
        self.partial |= !optional;
    }

    fn compiler(&self, name: &str, partial: &mut bool) -> Option<String> {
        let var = self.vars.get(name)?;
        let binding = var.certain.as_ref()?;
        let value = self.expand_value(binding);
        if !value.is_literal() || !var.possible.is_empty() {
            *partial = true;
            return None;
        }
        let mut words = words(&value.text).skip_while(|word| is_wrapper(word));
        let compiler = words.next()?;
        if words.next().is_some() || compiler.is_empty() || compiler.contains([UNRESOLVED, ';', '|', '&', '<', '>', '`']) {
            *partial = true;
            return None;
        }
        Some(compiler)
    }

    fn collect(&self, flags: &mut Flags<'_>, partial: &mut bool) {
        for name in &self.flag_order {
            if (self.cancelled)() {
                return;
            }
            let var = &self.vars[name];
            for (binding, conditional) in var
                .certain
                .iter()
                .map(|binding| (binding, false))
                .chain(var.possible.iter().map(|binding| (binding, true)))
            {
                let value = self.expand_value(binding);
                *partial |= !value.is_literal();
                let directories = directory_variable(name);
                flags.collect(&value.text, directories, conditional, partial);
                for variant in value.alternatives {
                    flags.collect(&variant, directories, true, partial);
                }
            }
        }
    }
}

fn directory_variable(name: &str) -> bool {
    name.split_once("INC").is_some_and(|(_, rest)| rest.contains("DIRS"))
}

struct Flags<'a> {
    root: &'a Path,
    ordered: Vec<String>,
    emitted: HashSet<String>,
    include_dirs: Vec<PathBuf>,
}

impl<'a> Flags<'a> {
    fn new(root: &'a Path) -> Self {
        Self {
            root,
            ordered: Vec::new(),
            emitted: HashSet::new(),
            include_dirs: Vec::new(),
        }
    }

    fn insert(&mut self, flag: String) -> bool {
        if self.emitted.insert(flag.clone()) {
            self.ordered.push(flag);
            true
        } else {
            false
        }
    }

    fn include_path(&mut self, option: &str, path: &str) -> bool {
        let path = absolute(self.root, path);
        let display = path.to_string_lossy();
        if option == "-include" {
            if self.emitted.insert(format!("{option}\0{display}")) {
                self.ordered.extend([option.to_owned(), display.into_owned()]);
                return true;
            }
        } else if self.insert(format!("{option}{display}")) {
            if option == "-I" {
                self.include_dirs.push(path);
            }
            return true;
        }
        false
    }

    fn collect(&mut self, text: &str, directories: bool, conditional: bool, partial: &mut bool) {
        let words: Vec<_> = words(text).collect();
        let mut words = words.iter().map(String::as_str).peekable();
        while let Some(word) = words.next() {
            let supported = ["-isystem", "-iquote", "-include", "-I", "-D", "-U", "/I", "/D"];
            let flag = supported.iter().find_map(|&option| {
                let value = word.strip_prefix(option)?;
                // Longer options like -include-pch are not -include <file>.
                (option != "-include" || value.is_empty()).then_some((option, value))
            });
            let (option, value) = match flag {
                Some((option, "")) => {
                    if words
                        .peek()
                        .is_none_or(|word| word.starts_with('-') || matches!(*word, "/I" | "/D"))
                    {
                        *partial = true;
                        continue;
                    }
                    (option, words.next().unwrap())
                }
                Some(flag) => flag,
                None if word.starts_with("-std=") => ("-std=", &word[5..]),
                None if directories && !word.starts_with('-') => ("-I", word),
                None => {
                    *partial |= word.contains(UNRESOLVED);
                    continue;
                }
            };
            if value.contains(UNRESOLVED) || value.is_empty() {
                *partial = true;
                continue;
            }
            match option {
                "-I" | "/I" => {
                    self.include_path("-I", value);
                }
                "-isystem" | "-iquote" if !conditional => {
                    self.include_path(option, value);
                }
                "-include" if !conditional => {
                    self.include_path(option, value);
                }
                "-D" | "/D" if !conditional => {
                    self.insert(format!("-D{value}"));
                }
                "-U" | "-std=" if !conditional => {
                    self.insert(format!("{option}{value}"));
                }
                _ => {}
            }
        }
    }
}

fn quoted_include(line: &str) -> Option<&str> {
    let line = line.trim_start().strip_prefix('#')?.trim_start().strip_prefix("include")?;
    let path = line.trim_start().strip_prefix('"')?.split_once('"')?.0;
    (!path.is_empty()).then_some(path)
}

fn header_includes(
    root: &Path,
    files: &[String],
    ignored: &[String],
    cancelled: &dyn Fn() -> bool,
    flags: &mut Flags<'_>,
    result: &mut Inferred,
) {
    const SCAN_FILES: usize = 400;
    const SCAN_BYTES: usize = 256 * 1024;
    const MAX_SPELLINGS: usize = 16_384;
    let mut seen = HashSet::new();
    let mut spellings = Vec::new();
    let mut count = 0;
    'sources: for file in files {
        if cancelled() {
            return;
        }
        let ext = extension(file);
        if !has_extension(ext, SOURCES) && !has_extension(ext, HEADERS) || workspace::is_ignored(file, ignored) {
            continue;
        }
        if count == SCAN_FILES {
            result.partial = true;
            break;
        }
        count += 1;
        let path = root.join(file);
        let dir = path.parent().unwrap_or(root);
        let Ok(bytes) = read_file(&path, SCAN_BYTES + 1) else { continue };
        let bytes = if bytes.len() > SCAN_BYTES {
            result.partial = true;
            &bytes[..SCAN_BYTES]
        } else {
            &bytes
        };
        for (index, spelling) in String::from_utf8_lossy(bytes).lines().filter_map(quoted_include).enumerate() {
            if index % 256 == 0 && cancelled() {
                return;
            }
            if Path::new(spelling).is_absolute() || seen.contains(spelling) || dir.join(spelling).is_file() {
                continue;
            }
            if spellings.len() == MAX_SPELLINGS {
                result.partial = true;
                break 'sources;
            }
            seen.insert(spelling.to_owned());
            spellings.push(spelling.to_owned());
        }
    }
    if spellings.is_empty() {
        return;
    }
    // Index only requested suffixes. Each project path is visited once, regardless of
    // how many quoted includes were found in the sample.
    let mut matches: HashMap<&str, Vec<&str>> = spellings.iter().map(|spelling| (spelling.as_str(), Vec::new())).collect();
    for (index, file) in files.iter().enumerate() {
        if index % 256 == 0 && cancelled() {
            return;
        }
        if let Some(dirs) = matches.get_mut(file.as_str()) {
            dirs.push("");
        }
        for (index, _) in file.match_indices('/') {
            if let Some(dirs) = matches.get_mut(&file[index + 1..]) {
                dirs.push(&file[..index]);
            }
        }
    }
    let mut guessed_dirs = HashSet::new();
    let mut guesses = Vec::new();
    for spelling in &spellings {
        if cancelled() {
            return;
        }
        let candidates = &matches[spelling.as_str()];
        let Some(&dir) = candidates.iter().min_by_key(|dir| (dir.len(), **dir)) else {
            continue;
        };
        if candidates.iter().any(|dir| guessed_dirs.contains(dir)) || flags.include_dirs.iter().any(|dir| dir.join(spelling).is_file()) {
            continue;
        }
        result.partial |= candidates.iter().any(|candidate| *candidate != dir);
        if guessed_dirs.insert(dir) {
            guesses.push(dir);
        }
    }
    for dir in guesses {
        if flags.include_path("-I", dir) {
            result.guessed_includes += 1;
        }
    }
}

pub(crate) fn infer(root: &Path, ignored: &[String], files: &[String], cancelled: &dyn Fn() -> bool) -> Option<Inferred> {
    if cancelled() {
        return None;
    }
    let root = std::path::absolute(root).ok()?;
    let root = absolute(&root, "");
    let makefile = MAKEFILE_NAMES.iter().map(|name| root.join(name)).find(|path| path.is_file());
    if makefile.is_none()
        && !files
            .iter()
            .any(|file| has_extension(extension(file), SOURCES) && !workspace::is_ignored(file, ignored))
    {
        return None;
    }
    let mut result = Inferred::default();
    let mut flags = Flags::new(&root);
    if let Some(path) = makefile {
        let mut parser = Makefiles::new(&root, files, cancelled);
        parser.read(path, false, Mode::Certain);
        if cancelled() {
            return None;
        }
        result.partial = parser.partial;
        parser.collect(&mut flags, &mut result.partial);
        result.compiler = parser.compiler("CC", &mut result.partial);
        result.cxx_compiler = parser.compiler("CXX", &mut result.partial);
        result.inputs = parser.inputs;
    }
    header_includes(&root, files, ignored, cancelled, &mut flags, &mut result);
    result.flags = flags.ordered;
    (!cancelled()).then_some(result)
}

pub(crate) fn synthesize_database(root: &Path, inferred: &Inferred, files: &[String], ignored: &[String]) -> Vec<Value> {
    let Ok(root) = std::path::absolute(root) else { return Vec::new() };
    files
        .iter()
        .filter_map(|file| {
            let ext = extension(file);
            if !has_extension(ext, SOURCES) || workspace::is_ignored(file, ignored) {
                return None;
            }
            let path = absolute(&root, file);
            let compiler = if ext != "C" && has_extension(ext, &["c", "m"]) {
                inferred.compiler.as_deref().unwrap_or("cc")
            } else {
                inferred.cxx_compiler.as_deref().unwrap_or("c++")
            };
            let mut arguments = Vec::with_capacity(inferred.flags.len() + 3);
            let file_argument = path.to_string_lossy();
            arguments.push(compiler);
            arguments.extend(inferred.flags.iter().map(String::as_str));
            arguments.extend(["-c", &file_argument]);
            Some(json!({"directory": root, "file": path, "arguments": arguments}))
        })
        .take(200_000)
        .collect()
}

fn is_wrapper(word: &str) -> bool {
    let name = word.rsplit(['/', '\\']).next().unwrap_or(word);
    matches!(
        name.strip_suffix(".exe").unwrap_or(name),
        "ccache" | "sccache" | "distcc" | "icecc" | "gomacc" | "--"
    )
}

fn gcc_driver(compiler: &str) -> bool {
    let name = compiler.rsplit(['/', '\\']).next().unwrap_or(compiler).to_ascii_lowercase();
    let name = name.strip_suffix(".exe").unwrap_or(&name);
    let name = name
        .rsplit_once('-')
        .filter(|(_, suffix)| {
            suffix.as_bytes().first().is_some_and(u8::is_ascii_digit) && suffix.bytes().all(|byte| byte.is_ascii_digit() || byte == b'.')
        })
        .map_or(name, |(stem, _)| stem);
    matches!(name, "cc" | "c++" | "gcc" | "g++") || name.ends_with("-gcc") || name.ends_with("-g++")
}

fn resolve_driver(compiler: &str, search_path: Option<&OsStr>) -> Option<String> {
    if !gcc_driver(compiler) {
        return None;
    }
    let path = Path::new(compiler);
    if path.is_absolute() || path.components().count() > 1 {
        return existing_driver(path);
    }
    std::env::split_paths(search_path?).find_map(|dir| existing_driver(&dir.join(path)))
}

fn existing_driver(path: &Path) -> Option<String> {
    let path = if cfg!(windows) && path.extension() != Some(OsStr::new("exe")) && !path.is_file() {
        let mut executable = path.as_os_str().to_owned();
        executable.push(".exe");
        PathBuf::from(executable)
    } else {
        path.to_owned()
    };
    let absolute = std::path::absolute(path).ok()?;
    let absolute = absolute.to_str()?;
    // clangd interprets these characters as glob syntax or pattern separators.
    (!absolute.contains(['*', '?', '[', ']', ',']) && Path::new(absolute).is_file()).then(|| absolute.to_owned())
}

pub(crate) fn query_driver(compiler: &str) -> Option<String> {
    query_driver_in(compiler, std::env::var_os("PATH").as_deref())
}

fn query_driver_in(compiler: &str, search_path: Option<&OsStr>) -> Option<String> {
    // Database arguments already contain an unquoted executable path.
    if compiler.contains(['/', '\\'])
        && let Some(path) = resolve_driver(compiler, search_path)
    {
        return Some(path);
    }
    let driver = words(compiler).find(|word| !is_wrapper(word))?;
    resolve_driver(&driver, search_path)
}

pub(crate) fn database_compiler(records: &[Value]) -> Option<String> {
    let mut counts = HashMap::<String, (usize, usize)>::new();
    for (index, record) in records.iter().enumerate() {
        let from_arguments = record["arguments"]
            .as_array()
            .and_then(|args| args.iter().map_while(Value::as_str).find(|word| !is_wrapper(word)));
        let compiler = from_arguments
            .map(str::to_owned)
            .or_else(|| words(record["command"].as_str()?).find(|word| !is_wrapper(word)));
        if let Some(compiler) = compiler.filter(|compiler| !compiler.is_empty() && !compiler.contains(UNRESOLVED)) {
            counts.entry(compiler).or_insert((0, index)).0 += 1;
        }
    }
    counts
        .into_iter()
        .max_by_key(|(_, (count, first))| (*count, std::cmp::Reverse(*first)))
        .map(|(compiler, _)| compiler)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Project(PathBuf);

    impl Project {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let id = NEXT.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!("edittr-makeflags-{}-{id}", std::process::id()));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn write(&self, name: &str, contents: &str) {
            let path = self.0.join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, contents).unwrap();
        }

        fn flag(&self, option: &str, path: &str) -> String {
            format!("{option}{}", self.0.join(path).display())
        }

        fn infer(&self, files: &[&str], ignored: &[&str]) -> Inferred {
            infer(&self.0, &strings(ignored), &strings(files), &|| false).unwrap()
        }
    }

    impl Drop for Project {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn reads_makefiles_in_order_and_detects_include_cycles() {
        let p = Project::new();
        p.write("Makefile", "CPPFLAGS = -DLOWER_PRIORITY");
        p.write(
            "GNUmakefile",
            "DIR = config\ninclude ${DIR}/one.mk\ninclude extras/*.mk\nsinclude missing.mk\nCPPFLAGS += -I './include'\n",
        );
        p.write(
            "config/one.mk",
            "include child.mk root.mk\ninclude ../GNUmakefile\nCPPFLAGS += -Iinclude\n",
        );
        p.write("config/child.mk", "CPPFLAGS += -Ichild\n");
        p.write("root.mk", "CPPFLAGS += -DROOT\n");
        p.write("extras/b.mk", "CPPFLAGS += -DB\n");
        p.write("extras/a.mk", "CPPFLAGS += -DA\n");
        let result = p.infer(&["extras/b.mk", "extras/a.mk", "other/config.mk", "extras/nested/ignored.mk"], &[]);
        assert_eq!(
            result.inputs,
            [
                "GNUmakefile",
                "config/one.mk",
                "config/child.mk",
                "root.mk",
                "extras/a.mk",
                "extras/b.mk"
            ]
            .map(|path| p.0.join(path))
        );
        assert_eq!(
            result.flags,
            [
                p.flag("-I", "child"),
                "-DROOT".into(),
                p.flag("-I", "include"),
                "-DA".into(),
                "-DB".into()
            ]
        );
        assert!(result.partial);
    }

    #[test]
    fn respects_make_assignment_flavors_and_collects_supported_flags() {
        let p = Project::new();
        p.write(
            "Makefile",
            r#"DIR = first
override CPPFLAGS ::= -I$(DIR)
CPPFLAGS += -isystem "$(PWD)/system dir"
CPPFLAGS ?= -DUNUSED
DEFS ?= -D DEFAULT=1
DEFS ?= -DUNUSED
DIR = second
CC = ccache arm-none-eabi-gcc
CXX := clang++
export CFLAGS = -I ${CURDIR}/$(DIR) \
  -std=c11 -include config.h -U OLD /DVALUE=a\#b # ignored
CFLAGS += -iquote quoted
CFLAGS += /I"extra dir" -Werror
INC_DIRS = inc ./inc vendor/include
other: CFLAGS += -DTARGET
	task CPPFLAGS += -DRECIPE
"#,
        );
        let result = p.infer(&[], &["vendor"]);
        assert_eq!(
            result.flags,
            [
                p.flag("-I", "first"),
                p.flag("-isystem", "system dir"),
                "-DDEFAULT=1".into(),
                p.flag("-I", "second"),
                "-std=c11".into(),
                "-include".into(),
                p.0.join("config.h").to_string_lossy().into_owned(),
                "-UOLD".into(),
                "-DVALUE=a#b".into(),
                p.flag("-iquote", "quoted"),
                p.flag("-I", "extra dir"),
                p.flag("-I", "inc"),
                p.flag("-I", "vendor/include"),
            ]
        );
        assert_eq!(result.compiler.as_deref(), Some("arm-none-eabi-gcc"));
        assert_eq!(result.cxx_compiler.as_deref(), Some("clang++"));
        assert!(!result.partial);
    }

    #[test]
    fn collects_includes_from_uncertain_branches_and_skips_dynamic_tokens() {
        let p = Project::new();
        p.write(
            "Makefile",
            r#"GIVEN = yes
CFLAGS = -DCOMMON -Iknown -I$(shell touch SHOULD_NOT_EXIST) -D${UNDEFINED}
ifeq ($(CONFIG),host)
  CFLAGS += -I host/inc -DHOST=1 -std=c99
  SUBDIR = host/extra
else
  CFLAGS += -Itarget/inc -DTARGET -UCOMMON
  SUBDIR = target/extra
endif
CPPFLAGS = -I$(SUBDIR) -DKEPT
ifdef GIVEN
  CPPFLAGS += -DDEFINED
else
  CPPFLAGS += -DBAD
endif
ifndef MISSING
  CPPFLAGS += -DUNDEFINED_OK
endif
"#,
        );
        let result = p.infer(&[], &[]);
        for path in ["known", "host/inc", "target/inc", "host/extra", "target/extra"] {
            assert!(result.flags.contains(&p.flag("-I", path)), "{:?}", result.flags);
        }
        let macros: Vec<_> = result
            .flags
            .iter()
            .filter(|flag| flag.starts_with("-D"))
            .map(String::as_str)
            .collect();
        assert_eq!(macros, ["-DCOMMON", "-DKEPT", "-DDEFINED", "-DUNDEFINED_OK"]);
        assert!(result.partial);
        assert!(!p.0.join("SHOULD_NOT_EXIST").exists());
    }

    #[test]
    fn guesses_header_paths_with_the_shortest_match_and_preserves_known_paths() {
        let p = Project::new();
        let files = [
            "src/main.c",
            "src/local.h",
            "configured/ready.h",
            "lib/include/api/detail.h",
            "lib/include/api/extra.h",
            "some/longer/include/api/detail.h",
            "skip/project.cc",
            "unwanted/unused.h",
        ];
        p.write("Makefile", "CPPFLAGS = -Iconfigured\n");
        p.write(
            files[0],
            "#include \"local.h\"\n# include \"ready.h\"\n#include \"api/detail.h\"\n#include \"api/extra.h\"\n#include <unused.h>\n",
        );
        p.write("skip/project.cc", "#include \"unused.h\"");
        for path in &files[1..] {
            if *path != "skip/project.cc" {
                p.write(path, "");
            }
        }
        let result = p.infer(&files, &["skip"]);
        assert_eq!(result.flags, [p.flag("-I", "configured"), p.flag("-I", "lib/include")]);
        assert_eq!(result.guessed_includes, 1);
        assert!(result.partial);
    }

    #[test]
    fn infers_without_a_makefile_and_honors_cancellation() {
        let p = Project::new();
        assert!(infer(&p.0, &[], &strings(&["only.h", "other.rs"]), &|| false).is_none());
        p.write("main.c", "#include \"api.h\"");
        p.write("inc/api.h", "");
        let files = strings(&["main.c", "inc/api.h"]);
        assert_eq!(infer(&p.0, &[], &files, &|| false).unwrap().flags, [p.flag("-I", "inc")]);
        assert!(infer(&p.0, &[], &files, &|| true).is_none());
    }

    #[test]
    fn synthesizes_only_visible_translation_units() {
        let p = Project::new();
        let inferred = Inferred {
            compiler: Some("arm-none-eabi-gcc".into()),
            flags: vec!["-DVALUE=1".into()],
            ..Inferred::default()
        };
        let files = strings(&[
            "src/a.c",
            "src/b.cc",
            "src/c.cpp",
            "src/d.cxx",
            "src/e.m",
            "src/f.mm",
            "vendor/g.c",
            "src/h.hpp",
        ]);
        let records = synthesize_database(&p.0, &inferred, &files, &strings(&["vendor"]));
        assert_eq!(records.len(), 6);
        for (index, record) in records.iter().enumerate() {
            let file = p.0.join(&files[index]);
            let compiler = if matches!(index, 0 | 4) { "arm-none-eabi-gcc" } else { "c++" };
            assert_eq!(
                *record,
                json!({"directory": p.0, "file": file, "arguments": [compiler, "-DVALUE=1", "-c", file]})
            );
        }
    }

    #[test]
    fn resolves_gcc_drivers_exactly_and_rejects_clang() {
        let p = Project::new();
        let executable = if cfg!(windows) {
            "arm-none-eabi-gcc.exe"
        } else {
            "arm-none-eabi-gcc"
        };
        p.write(executable, "");
        let path = p.0.join(executable);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let search_path = std::env::join_paths([&p.0]).unwrap();
        assert_eq!(
            query_driver_in("ccache distcc arm-none-eabi-gcc", Some(&search_path)).as_deref(),
            path.to_str()
        );
        assert_eq!(query_driver(path.to_str().unwrap()).as_deref(), path.to_str());
        for clang in ["clang", "clang++", "clang-cl", "cl", "clang-20"] {
            p.write(clang, "");
            assert!(query_driver_in(clang, Some(&search_path)).is_none(), "{clang}");
        }
    }

    #[test]
    fn counts_database_compilers_after_wrappers_and_handles_quoted_paths() {
        let compiler = "/SDK Tools/bin/arm-none-eabi-gcc";
        let records = [
            json!({"command": format!("/bin/ccache '{compiler}' -c main.c")}),
            json!({"arguments": ["sccache", "distcc", compiler, "-c", "other.c"]}),
            json!({"arguments": ["clang++", "-c", "main.cpp"]}),
        ];
        assert_eq!(database_compiler(&records).as_deref(), Some(compiler));
        assert_eq!(database_compiler(&[]), None);
    }
}
