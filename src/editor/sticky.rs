//! Declaration headers for the editor's sticky scroll overlay.

use super::{LineIndex, Lines, fold};
use crate::{goto, symbols::Symbol};
use eframe::egui::{self, Align2, Pos2, Rect, Sense, Stroke, Vec2};
use std::{
    collections::{BTreeMap, BTreeSet},
    ops::Range,
    sync::Arc,
};

const MAX_HEADERS: usize = 5;
const NO_LINE: usize = usize::MAX;
type Headers = [usize; MAX_HEADERS];
pub(crate) type Scopes = Arc<[(usize, usize)]>;

fn declaration(kind: &str) -> bool {
    matches!(
        kind,
        "Function" | "Method" | "Constructor" | "Class" | "Struct" | "Interface" | "Enum" | "Module" | "Namespace" | "Package" | "Object"
    )
}

pub(crate) fn outline_scopes<'a>(symbols: impl Iterator<Item = &'a Symbol>, text: &str) -> Scopes {
    let starts: BTreeMap<_, _> = symbols
        .filter(|symbol| declaration(&symbol.kind) && symbol.lines.end() > symbol.lines.start())
        .map(|symbol| (*symbol.lines.start(), (symbol.line, *symbol.lines.end())))
        .collect();
    let mut starts = starts.into_iter().peekable();
    let mut scopes = Vec::new();
    for (line, source) in text.lines().enumerate() {
        let Some(&(start, (name_line, end))) = starts.peek() else { break };
        if line < start {
            continue;
        }
        let source = source.trim_start();
        // Some servers include doc comments and attributes in the symbol range.
        if line < name_line && (source.is_empty() || source.starts_with(['#', '@', '*']) || source.starts_with('/')) {
            continue;
        }
        starts.next();
        if line < end {
            scopes.push((line, end));
        }
    }
    scopes.into()
}

/// A block header of control flow: `if`, `} else {`, loops, `switch`, `case X:` (indented case bodies are
/// fold regions too), `try` / `catch`...
fn control_flow(line: &str) -> bool {
    let first = line
        .trim_start_matches(|c: char| c == '}' || c.is_whitespace())
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .next()
        .unwrap_or_default();
    matches!(
        first,
        "if" | "else"
            | "elif"
            | "elsif"
            | "unless"
            | "for"
            | "foreach"
            | "while"
            | "until"
            | "do"
            | "loop"
            | "match"
            | "switch"
            | "select"
            | "case"
            | "default"
            | "when"
            | "try"
            | "catch"
            | "except"
            | "finally"
            | "with"
    )
}

fn fold_scopes(text: &str, ix: &LineIndex, regions: &[Range<usize>], control: bool) -> BTreeMap<usize, usize> {
    let mut scopes = BTreeMap::new();
    let mut continuations = BTreeMap::new();
    for region in regions {
        let line = text[ix.line_range(text, region.start)].trim();
        let continued = line.starts_with(')').then(|| continuations.remove(&region.start)).flatten();
        let first = line.split(|c: char| !c.is_alphanumeric() && c != '_').next().unwrap_or_default();
        let flow = control_flow(line);
        if flow && !control {
            continue;
        }
        let header = if let Some(header) = continued {
            header
        } else if flow
            || first == "impl"
            || goto::definitions(line)
                .next()
                .is_some_and(|def| declaration(def.kind) || (def.kind == "Type" && line.ends_with('{')))
        {
            region.start
        } else {
            continue;
        };

        let end = region.end;
        let next = (end..ix.lines())
            .map(|line| (line, text[ix.line_range(text, line)].trim()))
            .find(|(_, source)| !source.is_empty());
        let close = next.filter(|(_, source)| {
            let token = source.split_whitespace().next().unwrap_or_default();
            matches!(token, "}" | "};" | "end")
        });
        scopes.insert(header, close.map_or(end - 1, |(line, _)| line));
        if line.ends_with('(')
            && let Some((line, source)) = next
            && source.starts_with(')')
        {
            continuations.insert(line, header);
        }
    }
    scopes
}

#[derive(Default)]
pub(super) struct Cache {
    /// (version, control flow included) the fold scopes were computed for.
    version: Option<(u64, bool)>,
    folds: BTreeMap<usize, usize>,
    outline: Option<(u64, Scopes)>,
    entries: Vec<(usize, Headers)>,
}

impl Cache {
    pub(super) fn set_outline(&mut self, version: u64, scopes: &Scopes) {
        if self
            .outline
            .as_ref()
            .is_some_and(|(v, current)| *v == version && Arc::ptr_eq(current, scopes))
        {
            return;
        }
        self.outline = Some((version, scopes.clone()));
        if self.version.is_some_and(|(v, _)| v == version) {
            self.rebuild(version);
        }
    }

    pub(super) fn update(&mut self, version: u64, text: &str, ix: &LineIndex, regions: &[Range<usize>], control: bool) {
        if self.version != Some((version, control)) {
            self.version = Some((version, control));
            self.folds = fold_scopes(text, ix, regions, control);
            self.rebuild(version);
        }
    }

    fn rebuild(&mut self, version: u64) {
        let mut scopes = self.folds.clone();
        if let Some((v, outline)) = &self.outline
            && *v == version
        {
            scopes.extend(outline.iter().copied());
        }
        self.entries = index(scopes);
    }

    fn headers(&self, line: usize) -> Headers {
        let i = self.entries.partition_point(|&(start, _)| start <= line);
        if i == 0 { [NO_LINE; MAX_HEADERS] } else { self.entries[i - 1].1 }
    }
}

fn index(scopes: BTreeMap<usize, usize>) -> Vec<(usize, Headers)> {
    let mut events = Vec::with_capacity(scopes.len() * 2);
    for (start, end) in scopes {
        if start < end {
            events.push((start + 1, start, true));
            events.push((end.saturating_add(1), start, false));
        }
    }
    events.sort_unstable();
    let mut active = BTreeSet::new();
    events
        .chunk_by(|a, b| a.0 == b.0)
        .map(|events| {
            for &(_, header, entering) in events {
                if entering {
                    active.insert(header);
                } else {
                    active.remove(&header);
                }
            }
            let mut headers = [NO_LINE; MAX_HEADERS];
            for (slot, &header) in headers.iter_mut().zip(&active) {
                *slot = header;
            }
            (events[0].0, headers)
        })
        .collect()
}

pub(super) struct Overlay {
    viewport: Rect,
    headers: Headers,
    row_h: f32,
    available: usize,
}

impl Overlay {
    pub(super) fn new(cache: &Cache, view: &fold::View, viewport: Rect, row_h: f32, top_row: usize) -> Self {
        let mut headers = [NO_LINE; MAX_HEADERS];
        let available = ((viewport.height() / row_h).floor() as usize).saturating_sub(1).min(MAX_HEADERS);
        if top_row < view.count() {
            let active = cache.headers(view.line(top_row));
            let visible = active
                .into_iter()
                .take_while(|&line| line != NO_LINE)
                .filter(|&line| !view.is_hidden(line));
            for (slot, line) in headers[..available].iter_mut().zip(visible) {
                *slot = line;
            }
        }
        Self {
            viewport,
            headers,
            row_h,
            available,
        }
    }

    fn rows(&self) -> impl Iterator<Item = (usize, Rect)> + '_ {
        self.headers
            .iter()
            .copied()
            .take_while(|&line| line != NO_LINE)
            .enumerate()
            .map(|(row, line)| {
                let min = self.viewport.min + Vec2::new(0.0, row as f32 * self.row_h);
                (line, Rect::from_min_size(min, Vec2::new(self.viewport.width(), self.row_h)))
            })
    }

    pub(super) fn height(&self) -> f32 {
        self.headers.iter().take_while(|&&line| line != NO_LINE).count() as f32 * self.row_h
    }

    pub(super) fn contains_pointer(&self, ui: &egui::Ui) -> bool {
        let height = self.height();
        height > 0.0
            && ui
                .input(|input| input.pointer.interact_pos())
                .is_some_and(|pos| pos.y < self.viewport.top() + height && self.viewport.contains(pos))
    }

    /// Clicks on the pinned lines: the scroll offset to jump to, and a command from their context menu.
    pub(super) fn interact(
        &self,
        ui: &egui::Ui,
        id: egui::Id,
        view: &fold::View,
        control_flow: bool,
    ) -> (Option<f32>, Option<crate::commands::Command>) {
        use crate::commands::{Command, Setting};
        let (mut clicked, mut command) = (None, None);
        for (line, rect) in self.rows() {
            let response = ui
                .interact(rect, id.with(("sticky_header", line)), Sense::click())
                .on_hover_cursor(egui::CursorIcon::PointingHand);
            if response.clicked() {
                // Leave space for any enclosing headers that will still be pinned after the jump.
                clicked = Some(view.row(line).saturating_sub(self.available) as f32 * self.row_h);
            }
            response.context_menu(|ui| {
                let mut on = control_flow;
                if ui.checkbox(&mut on, "Pin control flow (if, else, loops, switch, case)").clicked() {
                    command = Some(Command::ToggleSetting(Setting::StickyControlFlow));
                    ui.close();
                }
                if ui.button("Turn off sticky scroll").clicked() {
                    command = Some(Command::ToggleSetting(Setting::StickyScroll));
                    ui.close();
                }
            });
        }
        (clicked, command)
    }

    pub(super) fn paint(&self, lines: &Lines<'_>, text_x: f32, gutter_w: f32, char_w: f32) {
        let ui = lines.ui;
        let vis = ui.visuals();
        let painter = ui.painter().with_clip_rect(self.viewport);
        let gutter_right = self.viewport.left() + gutter_w;
        let text_painter = painter.with_clip_rect(Rect::from_min_max(Pos2::new(gutter_right, self.viewport.top()), self.viewport.max));
        let mut bottom = None;
        for (line, rect) in self.rows() {
            painter.rect_filled(rect, 0.0, lines.palette.color(crate::theme::Part::Background));
            painter.text(
                Pos2::new(gutter_right - 1.5 * char_w, rect.top()),
                Align2::RIGHT_TOP,
                (line + 1).to_string(),
                lines.font.clone(),
                lines.palette.color(crate::theme::Part::LineNumber),
            );
            text_painter.galley(
                Pos2::new(text_x, rect.top()),
                lines.galley(line),
                lines.palette.color(crate::theme::Part::Foreground),
            );
            bottom = Some(rect.bottom());
        }
        if let Some(y) = bottom {
            painter.hline(
                self.viewport.x_range(),
                y,
                Stroke::new(1.0, vis.widgets.noninteractive.bg_stroke.color),
            );
        }
    }
}

#[cfg(feature = "tour")]
pub(crate) fn tour_line(text: &str) -> Option<usize> {
    let ix = LineIndex::new(text);
    let scopes = fold_scopes(text, &ix, &fold::regions(text, &ix), false);
    let (start, end) = scopes.into_iter().max_by_key(|&(start, end)| end - start)?;
    (end > start).then_some((start + 3).min(end))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declarations_pin_after_the_header_and_stop_after_the_scope() {
        let text = "impl Example {\n    fn f() {\n        if condition {\n            work();\n        }\n    }\n}\nfn outside() {}\n";
        let ix = LineIndex::new(text);
        let mut cache = Cache::default();
        cache.update(1, text, &ix, &fold::regions(text, &ix), false);
        assert_eq!(cache.headers(0), [NO_LINE; MAX_HEADERS]);
        assert_eq!(cache.headers(1), [0, NO_LINE, NO_LINE, NO_LINE, NO_LINE]);
        assert_eq!(cache.headers(3), [0, 1, NO_LINE, NO_LINE, NO_LINE]);
        assert_eq!(cache.headers(6), [0, NO_LINE, NO_LINE, NO_LINE, NO_LINE]);
        assert_eq!(cache.headers(7), [NO_LINE; MAX_HEADERS]);
    }

    #[test]
    fn control_flow_is_pinned_only_when_enabled() {
        let text = "\
void something() {
    if (something) {
        asdf();
    } else {
        switch (something) {
            case ASDF:
                one();
                two();
            default:
                three();
        }
    }
}
";
        let ix = LineIndex::new(text);
        let regions = fold::regions(text, &ix);
        let mut cache = Cache::default();
        cache.update(1, text, &ix, &regions, true);
        let pinned = |cache: &Cache, line| cache.headers(line).into_iter().take_while(|&l| l != NO_LINE).collect::<Vec<_>>();
        assert_eq!(pinned(&cache, 2), [0, 1]);
        assert_eq!(pinned(&cache, 7), [0, 3, 4, 5], "function, else, switch, case");
        assert_eq!(pinned(&cache, 9), [0, 3, 4, 8], "the next case replaces the previous one");
        cache.update(1, text, &ix, &regions, false);
        assert_eq!(pinned(&cache, 7), [0], "declarations only");
        assert!(control_flow("} else if (x) {") && control_flow("for x in y:") && !control_flow("format(x);"));
    }

    #[test]
    fn outline_ranges_replace_fold_extents_only_for_their_version() {
        let text = "fn f(\n    a: usize,\n) {\n    work(a);\n}\n";
        let ix = LineIndex::new(text);
        let regions = fold::regions(text, &ix);
        let mut cache = Cache::default();
        cache.update(1, text, &ix, &regions, false);
        assert_eq!(cache.headers(3)[0], 0);
        cache.set_outline(1, &Arc::from([(0, 2)]));
        assert_eq!(cache.headers(3)[0], NO_LINE);
        cache.update(2, text, &ix, &regions, false);
        assert_eq!(cache.headers(3)[0], 0);
    }

    #[test]
    fn nested_headers_are_capped_and_follow_the_first_visible_document_line() {
        let cache = Cache {
            entries: index((0..7).map(|start| (start, 20 - start)).collect()),
            ..Cache::default()
        };
        let folded = BTreeSet::from([6]);
        let region = 6..14;
        let view = fold::View::new(24, std::slice::from_ref(&region), &folded);
        let viewport = Rect::from_min_size(Pos2::ZERO, egui::vec2(600.0, 300.0));
        let overlay = Overlay::new(&cache, &view, viewport, 15.0, 7);
        assert_eq!(view.line(7), 14);
        assert_eq!(overlay.rows().map(|(line, _)| line).collect::<Vec<_>>(), [0, 1, 2, 3, 4]);
    }
}
