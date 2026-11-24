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
                Some(Tab::File(doc)) => {
                    // Past the end: the last line; past the line's end: its end.
                    let lines = doc.text.split('\n').count();
                    let line = line.min(lines.saturating_sub(1));
                    let len = doc
                        .text
                        .split('\n')
                        .nth(line)
                        .map_or(0, |l| l.trim_end_matches('\r').chars().count());
                    let at = crate::symbols::selection(&doc.text, line, (col.min(len)..col.min(len)).into()).0;
                    doc.goto = Some((at, at));
                }
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
    use super::parse;

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
