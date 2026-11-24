//! Go to line (Ctrl+G): a small box at the top of the editor, `line` or `line:column`.

use super::{App, Tab};
use eframe::egui::{self, Key, Modifiers};

#[derive(Default)]
pub(super) struct GotoLine {
    text: String,
    focus: bool,
}

/// `"12"` or `"12:5"` (1-based) to a 0-based (line, column).
fn parse(text: &str) -> Option<(usize, usize)> {
    let mut parts = text.trim().splitn(2, [':', ',']);
    let line = parts.next()?.trim().parse::<usize>().ok()?;
    let col = match parts.next().map(str::trim) {
        Some("") | None => 1,
        Some(col) => col.parse::<usize>().ok()?,
    };
    Some((line.max(1) - 1, col.max(1) - 1))
}

/// A line (and column, or line range) at the end of a Go to file query, 0-based:
/// `file.c:183`, `file.c:183:5`, `file.c:183-190` (also `–` and `—`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct LineTarget {
    pub(super) line: usize,
    pub(super) col: usize,
    /// Last line of a range (inclusive).
    pub(super) end: Option<usize>,
}

/// Split `query` into the file-name part and an optional line target. Only a suffix
/// after the last `:` that is all line syntax counts, so `C:\x` stays a name.
pub(super) fn split_line_target(query: &str) -> (&str, Option<LineTarget>) {
    let query = query.trim();
    let Some(colon) = query.rfind(':') else { return (query, None) };
    // `file:183:5`: the column comes after a second colon.
    let (name, rest) = match query[..colon].rfind(':') {
        Some(first) if query[first + 1..colon].trim().parse::<usize>().is_ok() => (&query[..first], &query[first + 1..]),
        _ => (&query[..colon], &query[colon + 1..]),
    };
    let rest = rest.trim();
    if rest.is_empty() {
        return (name, None);
    }
    let number = |s: &str| s.trim().parse::<usize>().ok().map(|n| n.max(1) - 1);
    let (head, end) = match rest.split_once(['-', '\u{2013}', '\u{2014}']) {
        Some((head, end)) => match number(end) {
            Some(end) => (head, Some(end)),
            None => return (query, None),
        },
        None => (rest, None),
    };
    let (line, col) = match head.split_once(':') {
        Some((line, col)) => (number(line), number(col)),
        None => (number(head), Some(0)),
    };
    match (line, col) {
        (Some(line), Some(col)) => (
            name,
            Some(LineTarget {
                line,
                col,
                end: end.filter(|&e| e > line),
            }),
        ),
        _ => (query, None),
    }
}

/// Select `target` in `doc`: a caret at line:col, or whole lines for a range.
/// Past the end: the last line; past a line's end: its end.
pub(super) fn select_target(doc: &mut super::Doc, target: LineTarget) {
    let lines = doc.text.split('\n').count();
    let last = lines.saturating_sub(1);
    let line = target.line.min(last);
    let len_of = |l: usize| doc.text.split('\n').nth(l).map_or(0, |s| s.trim_end_matches('\r').chars().count());
    let at = |l: usize, c: usize| crate::symbols::selection(&doc.text, l, (c..c).into()).0;
    doc.goto = Some(match target.end {
        Some(end) => {
            let end = end.min(last);
            (at(line, 0), at(end, len_of(end)))
        }
        None => {
            let c = at(line, target.col.min(len_of(line)));
            (c, c)
        }
    });
}

impl App {
    pub(super) fn open_goto_line(&mut self) {
        if matches!(self.tabs.get(self.active), Some(Tab::File(_) | Tab::Large(_))) {
            self.goto_line = Some(GotoLine {
                text: String::new(),
                focus: true,
            });
        }
    }

    pub(super) fn goto_line_dialog(&mut self, ctx: &egui::Context) {
        let Some(state) = &mut self.goto_line else { return };
        let (current, total) = match self.tabs.get(self.active) {
            Some(Tab::File(doc)) => (doc.line_col().0 + 1, doc.text.lines().count().max(1)),
            Some(Tab::Large(v)) => (v.window.0 + 1, v.index.lines()),
            _ => {
                self.goto_line = None;
                return;
            }
        };
        let mut close = ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape));
        let mut go = None;
        egui::Window::new("Go to line")
            .id(egui::Id::unique("goto_line"))
            .title_bar(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_TOP, [0.0, 60.0])
            .fixed_size([340.0, 0.0])
            .show(ctx, |ui| {
                let r = ui.add(
                    egui::TextEdit::singleline(&mut state.text)
                        .hint_text(format!("Line (1-{total}), or line:column. Now on line {current}"))
                        .desired_width(f32::INFINITY),
                );
                if std::mem::take(&mut state.focus) {
                    r.request_focus();
                }
                let target = parse(&state.text);
                if !state.text.trim().is_empty() && target.is_none() {
                    ui.colored_label(ui.visuals().error_fg_color, "Type a line number, e.g. 120 or 120:8");
                }
                if r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                    match target {
                        Some(t) => go = Some(t),
                        None => close = true,
                    }
                }
            });
        if let Some((line, col)) = go {
            match self.tabs.get_mut(self.active) {
                Some(Tab::File(doc)) => select_target(doc, LineTarget { line, col, end: None }),
                Some(Tab::Large(v)) => v.scroll_to = Some(line.min(v.index.lines().saturating_sub(1))),
                _ => {}
            }
            close = true;
        }
        if close {
            self.goto_line = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{LineTarget, parse, split_line_target};

    #[test]
    fn splits_line_targets_off_file_queries() {
        let t = |line, col, end| Some(LineTarget { line, col, end });
        assert_eq!(split_line_target("main.c"), ("main.c", None));
        assert_eq!(split_line_target("main.c:183"), ("main.c", t(182, 0, None)));
        assert_eq!(split_line_target("main.c:183:5"), ("main.c", t(182, 4, None)));
        assert_eq!(split_line_target("main.c:183-190"), ("main.c", t(182, 0, Some(189))));
        assert_eq!(split_line_target("main.c:183\u{2013}190"), ("main.c", t(182, 0, Some(189))));
        assert_eq!(split_line_target("main.c:183\u{2014}190"), ("main.c", t(182, 0, Some(189))));
        assert_eq!(split_line_target("main.c:183 - 190"), ("main.c", t(182, 0, Some(189))));
        assert_eq!(split_line_target("src/ui/main.c:7"), ("src/ui/main.c", t(6, 0, None)));
        assert_eq!(split_line_target(":42"), ("", t(41, 0, None)));
        // Not line syntax: the whole query is a name.
        assert_eq!(split_line_target(r"C:\proj\main.c"), (r"C:\proj\main.c", None));
        assert_eq!(split_line_target("main.c:abc"), ("main.c:abc", None));
        assert_eq!(split_line_target("main.c:"), ("main.c", None));
        // A range ending before it starts is just its first line.
        assert_eq!(split_line_target("a.c:10-3"), ("a.c", t(9, 0, None)));
    }

    #[test]
    fn parses_line_and_column() {
        assert_eq!(parse("12"), Some((11, 0)));
        assert_eq!(parse(" 12:5 "), Some((11, 4)));
        assert_eq!(parse("12,5"), Some((11, 4)));
        assert_eq!(parse("12:"), Some((11, 0)));
        assert_eq!(parse("0"), Some((0, 0)));
        assert_eq!(parse("x"), None);
        assert_eq!(parse("3:y"), None);
    }
}
