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
/// Enclosing headers kept per line: more than are shown, so hidden ones can be skipped.
const CACHED_HEADERS: usize = 16;
type Headers = [usize; CACHED_HEADERS];
/// (header line, last line, kind) of the language server's declarations.
pub(crate) type Scopes = Arc<[(usize, usize, Kind)]>;

/// What a pinned line opens; each can be shown or hidden.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Kind {
    Function,
    Type,
    Conditional,
    Loop,
    Switch,
    Case,
    Exception,
}

impl Kind {
    pub(crate) const ALL: [Kind; 7] = [
        Kind::Function,
        Kind::Type,
        Kind::Conditional,
        Kind::Loop,
        Kind::Switch,
        Kind::Case,
        Kind::Exception,
    ];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Kind::Function => "Functions and methods",
            Kind::Type => "Types (class, struct, impl, namespace)",
            Kind::Conditional => "if / else",
            Kind::Loop => "Loops (for, while, do)",
            Kind::Switch => "switch / match",
            Kind::Case => "case / default",
            Kind::Exception => "try / catch, with",
        }
    }

    /// Name in the settings file.
    pub(crate) fn key(self) -> &'static str {
        match self {
            Kind::Function => "function",
            Kind::Type => "type",
            Kind::Conditional => "if",
            Kind::Loop => "loop",
            Kind::Switch => "switch",
            Kind::Case => "case",
            Kind::Exception => "try",
        }
    }

    pub(crate) fn from_key(key: &str) -> Option<Kind> {
        Kind::ALL.into_iter().find(|kind| kind.key() == key)
    }

    /// Hidden as a group by the "control flow" switch.
    pub(crate) fn is_control_flow(self) -> bool {
        !matches!(self, Kind::Function | Kind::Type)
    }

    fn of_declaration(kind: &str) -> Kind {
        if matches!(kind, "Function" | "Method" | "Constructor") {
            Kind::Function
        } else {
            Kind::Type
        }
    }
}

/// A set of kinds.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) struct Kinds(u8);

impl Kinds {
    pub(crate) const ALL: Kinds = Kinds((1 << Kind::ALL.len()) - 1);

    pub(crate) fn contains(self, kind: Kind) -> bool {
        self.0 & (1 << kind as u8) != 0
    }

    pub(crate) fn toggle(&mut self, kind: Kind) {
        self.0 ^= 1 << kind as u8;
    }

    pub(crate) fn iter(self) -> impl Iterator<Item = Kind> {
        Kind::ALL.into_iter().filter(move |&kind| self.contains(kind))
    }

    /// The kinds pinned: `shown`, without control flow when that switch is off.
    pub(crate) fn pinned(shown: Kinds, control_flow: bool) -> Kinds {
        Kinds(
            shown
                .iter()
                .filter(|kind| control_flow || !kind.is_control_flow())
                .fold(0, |bits, kind| bits | 1 << kind as u8),
        )
    }
}

impl FromIterator<Kind> for Kinds {
    fn from_iter<I: IntoIterator<Item = Kind>>(kinds: I) -> Self {
        Kinds(kinds.into_iter().fold(0, |bits, kind| bits | 1 << kind as u8))
    }
}

fn declaration(kind: &str) -> bool {
    matches!(
        kind,
        "Function" | "Method" | "Constructor" | "Class" | "Struct" | "Interface" | "Enum" | "Module" | "Namespace" | "Package" | "Object"
    )
}

pub(crate) fn outline_scopes<'a>(symbols: impl Iterator<Item = &'a Symbol>, text: &str) -> Scopes {
    let starts: BTreeMap<_, _> = symbols
        .filter(|symbol| declaration(&symbol.kind) && symbol.lines.end() > symbol.lines.start())
        .map(|symbol| {
            (
                *symbol.lines.start(),
                (symbol.line, *symbol.lines.end(), Kind::of_declaration(&symbol.kind)),
            )
        })
        .collect();
    let mut starts = starts.into_iter().peekable();
    let mut scopes = Vec::new();
    for (line, source) in text.lines().enumerate() {
        let Some(&(start, (name_line, end, kind))) = starts.peek() else {
            break;
        };
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
            scopes.push((line, end, kind));
        }
    }
    scopes.into()
}

/// A block header of control flow: `if`, `} else {`, loops, `switch`, `case X:` (indented case bodies are
/// fold regions too), `try` / `catch`...
fn control_flow(line: &str) -> Option<Kind> {
    let first = line
        .trim_start_matches(|c: char| c == '}' || c.is_whitespace())
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .next()
        .unwrap_or_default();
    Some(match first {
        "if" | "else" | "elif" | "elsif" | "unless" => Kind::Conditional,
        "for" | "foreach" | "while" | "until" | "do" | "loop" => Kind::Loop,
        "match" | "switch" | "select" | "when" => Kind::Switch,
        "case" | "default" => Kind::Case,
        "try" | "catch" | "except" | "finally" | "with" => Kind::Exception,
        _ => return None,
    })
}

/// Every kind's scopes: header line -> (last line, kind).
fn fold_scopes(text: &str, ix: &LineIndex, regions: &[Range<usize>]) -> BTreeMap<usize, (usize, Kind)> {
    let mut scopes = BTreeMap::new();
    let mut continuations = BTreeMap::new();
    for region in regions {
        let line = text[ix.line_range(text, region.start)].trim();
        let continued = line.starts_with(')').then(|| continuations.remove(&region.start)).flatten();
        let first = line.split(|c: char| !c.is_alphanumeric() && c != '_').next().unwrap_or_default();
        let (header, kind) = if let Some(continued) = continued {
            continued
        } else if let Some(kind) = control_flow(line) {
            (region.start, kind)
        } else if first == "impl" {
            (region.start, Kind::Type)
        } else if let Some(def) = goto::definitions(line)
            .next()
            .filter(|def| declaration(def.kind) || (def.kind == "Type" && line.ends_with('{')))
        {
            (region.start, Kind::of_declaration(def.kind))
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
        scopes.insert(header, (close.map_or(end - 1, |(line, _)| line), kind));
        if line.ends_with('(')
            && let Some((line, source)) = next
            && source.starts_with(')')
        {
            continuations.insert(line, (header, kind));
        }
    }
    scopes
}

#[derive(Default)]
pub(super) struct Cache {
    version: Option<u64>,
    folds: BTreeMap<usize, (usize, Kind)>,
    outline: Option<(u64, Scopes)>,
    /// Kinds pinned (the others are filtered out of `entries`).
    kinds: Kinds,
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
        if self.version == Some(version) {
            self.rebuild(version);
        }
    }

    pub(super) fn update(&mut self, version: u64, text: &str, ix: &LineIndex, regions: &[Range<usize>], kinds: Kinds) {
        if self.version != Some(version) {
            self.version = Some(version);
            self.kinds = kinds;
            self.folds = fold_scopes(text, ix, regions);
            self.rebuild(version);
        } else if self.kinds != kinds {
            self.kinds = kinds;
            self.rebuild(version);
        }
    }

    fn rebuild(&mut self, version: u64) {
        let mut scopes = self.folds.clone();
        if let Some((v, outline)) = &self.outline
            && *v == version
        {
            scopes.extend(outline.iter().map(|&(start, end, kind)| (start, (end, kind))));
        }
        let kinds = self.kinds;
        self.entries = index(
            scopes
                .into_iter()
                .filter(|(_, (_, kind))| kinds.contains(*kind))
                .map(|(start, (end, _))| (start, end))
                .collect(),
        );
    }

    fn headers(&self, line: usize) -> Headers {
        let i = self.entries.partition_point(|&(start, _)| start <= line);
        if i == 0 { [NO_LINE; CACHED_HEADERS] } else { self.entries[i - 1].1 }
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
            let mut headers = [NO_LINE; CACHED_HEADERS];
            for (slot, &header) in headers.iter_mut().zip(&active) {
                *slot = header;
            }
            (events[0].0, headers)
        })
        .collect()
}

/// Checkboxes for what sticky scroll pins; returns the command of the one clicked. The menu stays open,
/// so several can be changed in a row.
pub(crate) fn kinds_menu(ui: &mut egui::Ui, shown: Kinds, control_flow: bool) -> Option<crate::commands::Command> {
    use crate::commands::{Command, Setting};
    let mut command = None;
    let mut on = control_flow;
    if ui.checkbox(&mut on, "Pin control flow").clicked() {
        command = Some(Command::ToggleSetting(Setting::StickyControlFlow));
    }
    for kind in Kind::ALL {
        if kind == Kind::Conditional {
            ui.separator();
        }
        let mut on = shown.contains(kind);
        let enabled = control_flow || !kind.is_control_flow();
        if ui.add_enabled(enabled, egui::Checkbox::new(&mut on, kind.label())).clicked() {
            command = Some(Command::ToggleStickyKind(kind));
        }
    }
    command
}

/// What the user did on the pinned lines this frame.
#[derive(Default)]
pub(super) struct Interaction {
    /// Scroll offset to jump to (a pinned line was clicked).
    pub(super) scroll: Option<f32>,
    pub(super) command: Option<crate::commands::Command>,
    /// Stop pinning this line.
    pub(super) hide: Option<usize>,
    pub(super) show_hidden: bool,
}

pub(super) struct Overlay {
    viewport: Rect,
    headers: [usize; MAX_HEADERS],
    row_h: f32,
    available: usize,
}

impl Overlay {
    /// `hidden`: lines the user hid from the pinned ones (the next enclosing line takes their place).
    pub(super) fn new(
        cache: &Cache,
        view: &fold::View,
        viewport: Rect,
        row_h: f32,
        top_row: usize,
        hidden: impl Fn(usize) -> bool,
    ) -> Self {
        let mut headers = [NO_LINE; MAX_HEADERS];
        let available = ((viewport.height() / row_h).floor() as usize).saturating_sub(1).min(MAX_HEADERS);
        if top_row < view.count() {
            let active = cache.headers(view.line(top_row));
            let visible = active
                .into_iter()
                .take_while(|&line| line != NO_LINE)
                .filter(|&line| !view.is_hidden(line) && !hidden(line));
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
        shown: Kinds,
        control_flow: bool,
        hidden_lines: usize,
    ) -> Interaction {
        use crate::commands::{Command, Setting};
        let mut out = Interaction::default();
        for (line, rect) in self.rows() {
            let response = ui
                .interact(rect, id.with(("sticky_header", line)), Sense::click())
                .on_hover_cursor(egui::CursorIcon::PointingHand);
            if response.clicked() {
                // Leave space for any enclosing headers that will still be pinned after the jump.
                out.scroll = Some(view.row(line).saturating_sub(self.available) as f32 * self.row_h);
            }
            response.context_menu(|ui| {
                if ui
                    .button("Hide this line")
                    .on_hover_text("Stop pinning this line (and identical ones) in this file")
                    .clicked()
                {
                    out.hide = Some(line);
                    ui.close();
                }
                if hidden_lines > 0 && ui.button(format!("Show hidden lines ({hidden_lines})")).clicked() {
                    out.show_hidden = true;
                    ui.close();
                }
                ui.separator();
                out.command = out.command.or(kinds_menu(ui, shown, control_flow));
                ui.separator();
                if ui.button("Turn off sticky scroll").clicked() {
                    out.command = Some(Command::ToggleSetting(Setting::StickyScroll));
                    ui.close();
                }
            });
        }
        out
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
    let scopes = fold_scopes(text, &ix, &fold::regions(text, &ix));
    let (start, end) = scopes
        .into_iter()
        .filter(|(_, (_, kind))| !kind.is_control_flow())
        .map(|(start, (end, _))| (start, end))
        .max_by_key(|&(start, end)| end - start)?;
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
        cache.update(1, text, &ix, &fold::regions(text, &ix), Kinds::pinned(Kinds::ALL, false));
        assert_eq!(cache.headers(0), [NO_LINE; CACHED_HEADERS]);
        assert_eq!(cache.headers(1)[..3], [0, NO_LINE, NO_LINE]);
        assert_eq!(cache.headers(3)[..3], [0, 1, NO_LINE]);
        assert_eq!(cache.headers(6)[..3], [0, NO_LINE, NO_LINE]);
        assert_eq!(cache.headers(7), [NO_LINE; CACHED_HEADERS]);
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
        cache.update(1, text, &ix, &regions, Kinds::ALL);
        let pinned = |cache: &Cache, line| cache.headers(line).into_iter().take_while(|&l| l != NO_LINE).collect::<Vec<_>>();
        assert_eq!(pinned(&cache, 2), [0, 1]);
        assert_eq!(pinned(&cache, 7), [0, 3, 4, 5], "function, else, switch, case");
        assert_eq!(pinned(&cache, 9), [0, 3, 4, 8], "the next case replaces the previous one");
        cache.update(1, text, &ix, &regions, Kinds::pinned(Kinds::ALL, false));
        assert_eq!(pinned(&cache, 7), [0], "declarations only");
        let mut shown = Kinds::ALL;
        shown.toggle(Kind::Switch);
        cache.update(1, text, &ix, &regions, Kinds::pinned(shown, true));
        assert_eq!(pinned(&cache, 7), [0, 3, 5], "switch hidden, its case still pinned");
        shown.toggle(Kind::Function);
        cache.update(1, text, &ix, &regions, Kinds::pinned(shown, true));
        assert_eq!(pinned(&cache, 7), [3, 5]);
        assert_eq!(control_flow("} else if (x) {"), Some(Kind::Conditional));
        assert_eq!(control_flow("for x in y:"), Some(Kind::Loop));
        assert_eq!(control_flow("format(x);"), None);
    }

    #[test]
    fn outline_ranges_replace_fold_extents_only_for_their_version() {
        let text = "fn f(\n    a: usize,\n) {\n    work(a);\n}\n";
        let ix = LineIndex::new(text);
        let regions = fold::regions(text, &ix);
        let mut cache = Cache::default();
        cache.update(1, text, &ix, &regions, Kinds::ALL);
        assert_eq!(cache.headers(3)[0], 0);
        cache.set_outline(1, &Arc::from([(0, 2, Kind::Function)]));
        assert_eq!(cache.headers(3)[0], NO_LINE);
        cache.update(2, text, &ix, &regions, Kinds::ALL);
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
        let overlay = Overlay::new(&cache, &view, viewport, 15.0, 7, |_| false);
        assert_eq!(view.line(7), 14);
        assert_eq!(overlay.rows().map(|(line, _)| line).collect::<Vec<_>>(), [0, 1, 2, 3, 4]);
        // A hidden line gives its slot to the next enclosing one.
        let overlay = Overlay::new(&cache, &view, viewport, 15.0, 7, |line| line == 1);
        assert_eq!(overlay.rows().map(|(line, _)| line).collect::<Vec<_>>(), [0, 2, 3, 4, 5]);
    }
}
