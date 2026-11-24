//! Find ranges for the editor and build replacements from an immutable text snapshot.

use crate::search::{self, Options};
use regex::{Captures, Regex};
use std::ops::Range;

const LARGE_FILE_BYTES: usize = 2 * 1024 * 1024;

pub fn needs_background(text: &str) -> bool {
    text.len() > LARGE_FILE_BYTES
}

pub fn find(text: &str, opts: &Options) -> Result<Vec<Range<usize>>, String> {
    if opts.query.is_empty() {
        return Ok(Vec::new());
    }
    let re = search::build_regex(opts)?;
    let (mut byte, mut chars) = (0, 0);
    Ok(re
        .find_iter(text)
        .map(|m| {
            let start = chars + text[byte..m.start()].chars().count();
            byte = m.end();
            chars = start + m.as_str().chars().count();
            start..chars
        })
        .collect())
}

pub struct Edit {
    pub text: String,
    pub count: usize,
    pub char_len: usize,
    /// Character position to start looking for the next match after replacing one.
    pub next: Option<usize>,
}

pub fn document_edit(text: &str, opts: &Options, replacement: &str, index: Option<usize>) -> Result<Option<Edit>, String> {
    if opts.query.is_empty() {
        return Ok(None);
    }
    let re = search::build_regex(opts)?;
    let mut edit = Builder::new(text, replacement, opts.regex);
    if let Some(index) = index {
        if let Some(caps) = re.captures_iter(text).nth(index) {
            let m = caps.get_match();
            let start = text[..m.start()].chars().count();
            edit.add(&caps, 0);
            let inserted = edit.output[m.start()..].chars().count();
            return Ok(edit.finish(Some(start + inserted + usize::from(m.is_empty()))));
        }
    } else {
        for caps in re.captures_iter(text) {
            edit.add(&caps, 0);
        }
    }
    Ok(edit.finish(None))
}

pub fn project_count(text: &str, re: &Regex, opts: &Options) -> usize {
    if !opts.regex && !re.is_match(text) {
        return 0;
    }
    text.split_inclusive('\n')
        .map(search::line_body)
        .map(|line| re.find_iter(line).filter(|m| !m.is_empty()).count())
        .sum()
}

/// Use the same line boundaries and nonempty matches as the Search panel.
/// The builder copies line endings directly from the original text.
pub fn project_edit(text: &str, re: &Regex, opts: &Options, replacement: &str) -> Option<Edit> {
    if !opts.regex && !re.is_match(text) {
        return None;
    }
    let mut edit = Builder::new(text, replacement, opts.regex);
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        for caps in re.captures_iter(search::line_body(line)) {
            if !caps.get_match().is_empty() {
                edit.add(&caps, offset);
            }
        }
        offset += line.len();
    }
    edit.finish(None)
}

struct Builder<'a> {
    original: &'a str,
    replacement: &'a str,
    expand: bool,
    output: String,
    copied: usize,
    count: usize,
}

impl<'a> Builder<'a> {
    fn new(original: &'a str, replacement: &'a str, expand: bool) -> Self {
        Self {
            original,
            replacement,
            expand,
            output: String::new(),
            copied: 0,
            count: 0,
        }
    }

    fn add(&mut self, caps: &Captures<'_>, offset: usize) {
        let m = caps.get_match();
        if self.count == 0 {
            self.output.reserve(self.original.len());
        }
        self.output.push_str(&self.original[self.copied..offset + m.start()]);
        if self.expand {
            caps.expand(self.replacement, &mut self.output);
        } else {
            self.output.push_str(self.replacement);
        }
        self.copied = offset + m.end();
        self.count += 1;
    }

    fn finish(mut self, next: Option<usize>) -> Option<Edit> {
        if self.count == 0 {
            return None;
        }
        self.output.push_str(&self.original[self.copied..]);
        Some(Edit {
            char_len: self.output.chars().count(),
            text: self.output,
            count: self.count,
            next,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(query: &str) -> Options {
        Options {
            query: query.into(),
            ..Default::default()
        }
    }

    #[test]
    fn expands_groups_from_the_current_match() {
        let mut o = opts(r"\b(\w+)-(\w+)\b");
        o.regex = true;
        let edit = document_edit("é-猫 foo-bar", &o, "$2/$1", Some(0)).unwrap().unwrap();
        assert_eq!(edit.text, "猫/é foo-bar");
        assert_eq!(edit.next, Some(3));
        let edit = document_edit("é-猫 foo-bar", &o, "$2/$1", None).unwrap().unwrap();
        assert_eq!(edit.text, "猫/é bar/foo");
        assert_eq!(edit.count, 2);
    }

    #[test]
    fn treats_pattern_and_replacement_literally() {
        let edit = document_edit("a.b axb", &opts("a.b"), r"$1\n", None).unwrap().unwrap();
        assert_eq!(edit.text, r"$1\n axb");
    }

    #[test]
    fn honors_word_boundaries_and_case_with_unicode_offsets() {
        let mut o = opts("café");
        o.whole_word = true;
        o.match_case = true;
        assert_eq!(find("猫 café cafés CAFÉ\r\ncafé", &o).unwrap(), [2..6, 19..23]);
    }

    #[test]
    fn preserves_mixed_line_endings_and_project_match_boundaries() {
        let mut o = opts(r"^(world)$");
        o.regex = true;
        let text = "world\r\nworld\nhello world\r\nworld";
        let edit = project_edit(text, &search::build_regex(&o).unwrap(), &o, "<$1>").unwrap();
        assert_eq!(edit.text, "<world>\r\n<world>\nhello world\r\n<world>");
        assert_eq!(edit.count, 3);
    }

    #[test]
    fn advances_past_zero_width_matches() {
        let mut o = opts("^");
        o.regex = true;
        assert_eq!(find("a\nb", &o).unwrap(), [0..0, 2..2]);
        let edit = document_edit("a\nb", &o, "猫", Some(0)).unwrap().unwrap();
        assert_eq!(edit.text, "猫a\nb");
        assert_eq!(edit.next, Some(2));
        assert!(project_edit("a\nb", &search::build_regex(&o).unwrap(), &o, "猫").is_none());
    }
}
