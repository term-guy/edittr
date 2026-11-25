//! The code editor widget. Unlike `egui::TextEdit` it lays out and paints only the
//! visible lines (each line is its own small galley, cached by egui), so frame cost
//! doesn't grow with file size, and it supports multiple cursors natively.
//!
//! The text stays a plain `String` owned by the caller (the rest of the app reads it);
//! positions are byte offsets (see [`buffer`]).

pub mod buffer;
pub mod fold;
mod navigation_menu;
pub(crate) mod sticky;

use crate::highlight;
use crate::theme::{Palette, Part};
use buffer::{History, Kind, LineIndex, Sel};
use eframe::egui::{self, Color32, Event, Key, Pos2, Rect, Sense, Stroke, Vec2, text::CCursor, text::LayoutJob};
use std::collections::BTreeSet;
use std::ops::Range;
use std::sync::Arc;

#[derive(Clone, Copy, PartialEq)]
enum Unit {
    Char,
    Word,
    Line,
}

enum Drag {
    /// Primary-button selection; `origin` is the word/line first clicked (for word/line snapping).
    Select { origin: Range<usize>, unit: Unit },
    /// Middle-button box selection: anchor line and x (content coordinates).
    Column { line: usize, x: f32 },
}

/// Editor state for one document (selections, undo history, scroll requests).
#[derive(Default)]
pub struct State {
    /// Selections; the last one is the primary (it scrolls into view, gets the IME).
    pub sels: Vec<Sel>,
    pub history: History,
    /// Shared so a frame can hold it without copying ~16 bytes per line.
    index: Arc<LineIndex>,
    index_version: Option<u64>,
    /// x the caret tries to keep while moving up/down.
    preferred_x: Option<f32>,
    drag: Option<Drag>,
    /// Cmd/Ctrl was held on press: a click (without dragging) goes to the definition.
    goto_press: Option<usize>,
    last_press: Option<(f64, Pos2, u8)>,
    preedit: Option<String>,
    /// Scroll the primary caret into view next frame (true: centered).
    reveal: Option<bool>,
    /// Where the pointer rests (byte, since when), for hover requests.
    rest: Option<(usize, f64)>,
    /// Folded regions, by header line.
    pub folded: BTreeSet<usize>,
    regions: Vec<Range<usize>>,
    view: Arc<fold::View>,
    sticky: sticky::Cache,
    /// (version, folded) the regions/view were computed for.
    view_key: Option<(u64, Vec<usize>)>,
    /// Vertical scroll offset to apply next frame (minimap clicks).
    scroll_to: Option<f32>,
    /// Line (fractional) to put at the top next frame (`scroll_to_line`).
    scroll_line: Option<f32>,
    minimap_drag: bool,
    /// A scroll bar arrow is held: when it scrolls another line.
    arrow_repeat: Option<f64>,
    /// Bracket nesting depth at the start of each line, for a version of the text.
    depth: (Option<u64>, Arc<Vec<u32>>),
}

impl State {
    pub(crate) fn set_sticky_outline(&mut self, version: u64, scopes: &sticky::Scopes) {
        self.sticky.set_outline(version, scopes);
    }

    /// Scroll so that `line` (fractional) is at the top on the next frame.
    pub fn scroll_to_line(&mut self, line: f32) {
        self.scroll_line = Some(line.max(0.0));
    }

    pub fn primary(&self) -> Sel {
        self.sels.last().copied().unwrap_or(Sel::caret(0))
    }

    /// The line index as a shared handle, for holding it while `self` changes.
    fn index_arc(&mut self, text: &str, version: u64) -> Arc<LineIndex> {
        self.index(text, version);
        self.index.clone()
    }

    /// Line index for `text` at `version` (rebuilt after any change).
    pub fn index(&mut self, text: &str, version: u64) -> &LineIndex {
        if self.index_version != Some(version) {
            self.index = Arc::new(LineIndex::new(text));
            self.index_version = Some(version);
            // The text may have been changed elsewhere (reload, replace): keep selections valid.
            let fix = |mut b: usize| {
                b = b.min(text.len());
                while !text.is_char_boundary(b) {
                    b -= 1;
                }
                b
            };
            for s in &mut self.sels {
                s.anchor = fix(s.anchor);
                s.head = fix(s.head);
            }
        }
        &self.index
    }

    /// Fold regions and the row view for the current text (recomputed after edits or fold changes).
    fn update_view(&mut self, text: &str, version: u64) {
        let folded: Vec<usize> = self.folded.iter().copied().collect();
        if self.view_key.as_ref().is_some_and(|(v, f)| *v == version && *f == folded) {
            return;
        }
        let ix = self.index_arc(text, version);
        if self.view_key.as_ref().is_none_or(|(v, _)| *v != version) {
            self.regions = fold::regions(text, &ix);
            let starts: BTreeSet<usize> = self.regions.iter().map(|r| r.start).collect();
            self.folded.retain(|l| starts.contains(l));
        }
        self.view = Arc::new(fold::View::new(ix.lines(), &self.regions, &self.folded));
        let folded: Vec<usize> = self.folded.iter().copied().collect();
        self.view_key = Some((version, folded));
    }

    /// Unfold any region hiding a caret (after moves, edits or jumps).
    fn unfold_carets(&mut self) {
        if self.folded.is_empty() {
            return;
        }
        let lines: Vec<usize> = self.sels.iter().map(|s| self.index.line_of_byte(s.head)).collect();
        let regions = &self.regions;
        self.folded.retain(|&start| {
            let Some(r) = regions.iter().find(|r| r.start == start) else {
                return false;
            };
            !lines.iter().any(|&l| l > r.start && l < r.end)
        });
    }

    /// Fold (or unfold) the innermost region containing the primary caret's line.
    fn fold_at_caret(&mut self, fold: bool) {
        let l = self.index.line_of_byte(self.primary().head);
        let region = self
            .regions
            .iter()
            .filter(|r| r.start <= l && l < r.end)
            .max_by_key(|r| r.start)
            .cloned();
        if let Some(r) = region {
            if fold {
                self.folded.insert(r.start);
                // Keep the caret visible: move it to the header line.
                let head = self.index.line_start(r.start);
                if l != r.start {
                    self.sels = vec![Sel::caret(head)];
                }
            } else {
                self.folded.remove(&r.start);
            }
        }
    }

    /// Replace `range` (bytes) with `insert` as one undoable edit; the caret goes after it.
    pub fn replace(&mut self, text: &mut String, range: Range<usize>, insert: &str) {
        let before = self.sels.clone();
        self.sels = buffer::edit(text, &mut self.history, &before, vec![(range, insert.to_string())], Kind::Other);
        self.reveal = Some(false);
        self.preferred_x = None;
    }

    /// Apply several replacements (bytes, in the current text) as one undoable edit,
    /// keeping the selections on the same text (e.g. a rename or a quick fix).
    pub fn apply_edits(&mut self, text: &mut String, edits: Vec<(Range<usize>, String)>) {
        if edits.is_empty() {
            return;
        }
        let before = self.sels.clone();
        self.sels = buffer::edit_keep(text, &mut self.history, &before, edits);
        self.history.seal();
        self.preferred_x = None;
    }

    /// Select a range (byte offsets) and scroll it to the center.
    pub fn select(&mut self, range: Range<usize>) {
        self.sels = vec![Sel {
            anchor: range.start,
            head: range.end,
        }];
        self.reveal = Some(true);
        self.preferred_x = None;
        self.history.seal();
    }
}

pub struct Options<'a> {
    pub palette: &'a Palette,
    pub lang: &'a str,
    pub highlight: bool,
    pub whitespace: bool,
    pub current_line: bool,
    /// Text inserted by Tab (e.g. four spaces or "\t").
    pub indent: &'a str,
    /// Line comment token for Cmd+/ (None: no toggle).
    pub comment: Option<&'a str>,
    pub minimap: bool,
    /// Color brackets by nesting depth.
    pub bracket_colors: bool,
    pub sticky_scroll: bool,
    /// Sticky scroll also pins control flow blocks (if / else / loops / switch / case).
    pub sticky_control_flow: bool,
    pub navigation_menu: bool,
    /// Selecting, copying, finding and scrolling only (e.g. the base side of a diff).
    pub read_only: bool,
}

/// Decorations supplied by the app.
#[derive(Default)]
pub struct Extras<'a> {
    /// Dimmed text after the end of the primary caret's line (inline blame).
    pub inline: Option<&'a str>,
    /// Squiggly underlines (byte ranges), e.g. diagnostics.
    pub squiggles: &'a [(Range<usize>, Color32)],
    /// Background marks (byte ranges), e.g. find matches.
    pub marks: &'a [Range<usize>],
    /// A second kind of marks in their own color (byte ranges): search-in-files results.
    pub global_marks: &'a [Range<usize>],
    /// Gutter markers per line (e.g. added / modified lines vs. git); `true` draws a
    /// "lines deleted above" triangle instead of a bar.
    pub gutter: &'a [(usize, Color32, bool)],
    /// Full-width line backgrounds (e.g. changed lines in a diff).
    pub line_bg: &'a [(usize, Color32)],
    /// Other uses of the symbol under the caret (byte ranges; `true`: written there).
    pub symbol_marks: &'a [(Range<usize>, bool)],
    /// Sorted ranges and color roles from the language server, per line.
    pub semantic: &'a [highlight::TokenLine],
}

#[derive(Default)]
pub struct Output {
    pub changed: bool,
    /// Cmd/Ctrl+click on a symbol: go to its definition (byte offset).
    pub goto: Option<usize>,
    pub context_command: Option<crate::commands::Command>,
    /// The pointer has rested on this byte offset for a while (hover info).
    pub hover: Option<(usize, Pos2)>,
    /// Screen position of the primary caret (for popups such as completion).
    pub caret_rect: Option<Rect>,
    pub focused: bool,
    /// The (fractional) line at the top of the view, for keeping two editors in sync.
    pub top_line: f32,
}

struct Layout<'a> {
    view: &'a fold::View,
    row_h: f32,
    /// Screen x where text starts (after the gutter), at the current horizontal scroll.
    text_x: f32,
    /// Screen y of line 0.
    top: f32,
}

impl Layout<'_> {
    fn line_y(&self, l: usize) -> f32 {
        self.top + self.view.row(l) as f32 * self.row_h
    }
}

/// Bracket colors for one line: depth at its start, the code brackets in it, palette.
struct Brackets<'a> {
    depth: u32,
    marks: &'a [(u32, bool)],
    colors: [Color32; 3],
}

#[allow(clippy::too_many_arguments)]
fn line_job(
    text: &str,
    ix: &LineIndex,
    l: usize,
    spans: Option<&[highlight::Line]>,
    semantic: &[(Range<usize>, highlight::SemanticRole)],
    font: &egui::FontId,
    palette: &Palette,
    brackets: Option<Brackets>,
) -> LayoutJob {
    let color = palette.color(Part::Foreground);
    let range = ix.line_range(text, l);
    let line = &text[range.clone()];
    let mut job = LayoutJob::default();
    job.wrap.max_width = f32::INFINITY;
    // Bracket color overrides: (byte in line, color).
    let mut overrides: Vec<(usize, Color32)> = Vec::new();
    if let Some(b) = brackets {
        let mut d = b.depth;
        for &(at, open) in b.marks {
            if open {
                overrides.push((at as usize, b.colors[d as usize % 3]));
                d += 1;
            } else {
                d = d.saturating_sub(1);
                overrides.push((at as usize, b.colors[d as usize % 3]));
            }
        }
    }
    let mut next = 0;
    let mut push = |job: &mut LayoutJob, from: usize, to: usize, fmt: egui::TextFormat| {
        let mut a = from;
        while a < to {
            while next < overrides.len() && overrides[next].0 < a {
                next += 1;
            }
            match overrides.get(next) {
                Some(&(at, c)) if at < to => {
                    if at > a {
                        job.append(&line[a..at], 0.0, fmt.clone());
                    }
                    job.append(&line[at..at + 1], 0.0, egui::TextFormat { color: c, ..fmt.clone() });
                    a = at + 1;
                    next += 1;
                }
                _ => {
                    job.append(&line[a..to], 0.0, fmt.clone());
                    a = to;
                }
            }
        }
    };
    let spans = spans.and_then(|s| s.get(l)).map(|l| l.spans.as_slice()).unwrap_or_default();
    let merged;
    let spans = if semantic.is_empty() {
        spans
    } else {
        merged = highlight::overlay(spans, semantic, line.len(), palette);
        &merged[..]
    };
    let mut pos = 0;
    for s in spans {
        let (a, b) = (s.range.start.min(line.len()), s.range.end.min(line.len()));
        if a >= b || a < pos {
            continue;
        }
        if a > pos {
            push(&mut job, pos, a, egui::TextFormat::simple(font.clone(), color));
        }
        let c = s.color(palette);
        let fmt = egui::TextFormat {
            font_id: font.clone(),
            color: c,
            italics: s.italics,
            underline: if s.underline { Stroke::new(1.0, c) } else { Stroke::NONE },
            ..Default::default()
        };
        push(&mut job, a, b, fmt);
        pos = b;
    }
    if pos < line.len() {
        push(&mut job, pos, line.len(), egui::TextFormat::simple(font.clone(), color));
    } else if line.is_empty() {
        job.append("", 0.0, egui::TextFormat::simple(font.clone(), color));
    }
    job
}

/// Lays out lines on demand (egui caches galleys between frames).
struct Lines<'a> {
    ui: &'a egui::Ui,
    text: &'a str,
    ix: &'a LineIndex,
    spans: Option<&'a [highlight::Line]>,
    /// Semantic token colors per line (may be shorter than the file, or empty).
    semantic: &'a [highlight::TokenLine],
    font: egui::FontId,
    palette: &'a Palette,
    /// Depth at each line start + palette, when bracket colors are on.
    depth: Option<(&'a [u32], [Color32; 3])>,
}

impl Lines<'_> {
    fn galley(&self, l: usize) -> Arc<egui::Galley> {
        let brackets = match (self.depth, self.spans.and_then(|s| s.get(l))) {
            (Some((depth, colors)), Some(hl)) if !hl.brackets.is_empty() => Some(Brackets {
                depth: depth.get(l).copied().unwrap_or(0),
                marks: &hl.brackets,
                colors,
            }),
            _ => None,
        };
        let semantic = self.semantic.get(l).map_or(&[][..], Vec::as_slice);
        let job = line_job(self.text, self.ix, l, self.spans, semantic, &self.font, self.palette, brackets);
        // The layout cache hashes TextFormat, so palette changes invalidate affected galleys
        // while syntax and semantic token ranges remain reusable.
        self.ui.fonts_mut(|f| f.layout_job(job))
    }

    /// x (relative to text start) of byte offset `b`.
    fn x_of(&self, b: usize) -> f32 {
        let (l, col) = self.ix.line_col(b);
        let range = self.ix.line_range(self.text, l);
        let col = col.min(range.len());
        let chars = self.text[range.start..range.start + col].chars().count();
        self.galley(l).pos_from_cursor(CCursor::new(chars)).min.x
    }

    /// Byte offset on line `l` closest to x (relative to text start).
    fn byte_at_x(&self, l: usize, x: f32) -> usize {
        let range = self.ix.line_range(self.text, l);
        let g = self.galley(l);
        let c = g.cursor_from_pos(Vec2::new(x, 0.0)).index.0;
        let line = &self.text[range.clone()];
        range.start + line.char_indices().nth(c).map_or(line.len(), |(i, _)| i)
    }
}

/// Show the editor. `version` is bumped when the text changes.
#[allow(clippy::too_many_arguments)]
pub fn show(
    ui: &mut egui::Ui,
    id_salt: egui::Id,
    st: &mut State,
    text: &mut String,
    version: &mut u64,
    hl: &mut highlight::Cache,
    opts: &Options,
    extras: &Extras,
) -> Output {
    let mut out = Output::default();
    let id = id_salt;
    if st.sels.is_empty() {
        st.sels.push(Sel::caret(0));
    }
    let font = crate::fonts::editor_font(ui.style());
    let (row_h, char_w, line_char_w) = ui.fonts_mut(|f| {
        let char_w = f.glyph_width(&font, 'x');
        if matches!(font.family, egui::FontFamily::Name(_)) {
            let digit_w = ('0'..='9').map(|c| f.glyph_width(&font, c)).fold(char_w, f32::max);
            (f.row_height(&font), digit_w, digit_w.max(f.glyph_width(&font, 'W')))
        } else {
            (f.row_height(&font), char_w, char_w)
        }
    });
    let is_mac = ui.ctx().os() == egui::os::OperatingSystem::Mac;
    let focused = ui.memory(|m| m.has_focus(id));
    out.focused = focused;
    // Semantic colors are per line of the text the app saw: an edit this frame that
    // adds or removes lines would shift them, so skip them until the app catches up.
    let lines_before = st.index(text, *version).lines();

    // ---- keyboard (before layout, so this frame shows the result)
    if focused {
        ui.memory_mut(|m| {
            m.set_focus_lock_filter(
                id,
                egui::EventFilter {
                    tab: true,
                    horizontal_arrows: true,
                    vertical_arrows: true,
                    // Esc collapses multiple cursors; the editor keeps focus (like VS Code).
                    escape: true,
                },
            )
        });
        let viewport_rows = (ui.available_height() / row_h).max(1.0) as usize;
        let events = ui.input(|i| i.events.clone());
        let ctx = KeyCtx {
            font: &font,
            is_mac,
            viewport_rows,
            opts,
        };
        for ev in events {
            if handle_event(ui, st, text, version, &ctx, &ev) {
                out.changed = true;
            }
        }
    }

    // ---- layout
    st.index(text, *version);
    st.update_view(text, *version);
    st.unfold_carets();
    st.update_view(text, *version);
    let ix = st.index_arc(text, *version);
    let view = st.view.clone();
    if opts.sticky_scroll {
        st.sticky.update(*version, text, &ix, &st.regions, opts.sticky_control_flow);
    }
    let spans = if opts.highlight {
        hl.spans(ui.ctx(), *version, text, opts.lang, opts.palette.preset().base == egui::Theme::Dark)
    } else {
        None
    };
    if opts.bracket_colors
        && let Some(sp) = spans
        && st.depth.0 != Some(*version)
    {
        let mut depth = Vec::with_capacity(sp.len() + 1);
        let mut d = 0u32;
        for line in sp {
            depth.push(d);
            for &(_, open) in &line.brackets {
                d = if open { d + 1 } else { d.saturating_sub(1) };
            }
        }
        depth.push(d);
        st.depth = (Some(*version), Arc::new(depth));
    }
    let palette = opts.palette;
    let bracket_palette = [Part::Bracket1, Part::Bracket2, Part::Bracket3].map(|part| palette.color(part));
    let use_depth = opts.bracket_colors && spans.is_some() && st.depth.0 == Some(*version);
    let depth_arc = st.depth.1.clone();
    let digits = ix.lines().to_string().len().max(3);
    // Fold arrows, line numbers, change markers.
    let fold_w = 1.6 * char_w;
    let gutter_w = fold_w + (digits as f32 + 2.5) * char_w;
    let content = Vec2::new(
        gutter_w + (ix.max_line_bytes as f32 + 8.0) * line_char_w + 200.0,
        // Let the last line scroll up to the middle of the view.
        (view.count() as f32) * row_h + ui.available_height() * 0.5,
    );
    let text_color = palette.color(Part::Foreground);

    // The minimap takes a strip on the right.
    let full = ui.available_rect_before_wrap();
    ui.painter().rect_filled(full, 0.0, palette.color(Part::Background));
    let mini_w = if opts.minimap && full.width() > 400.0 { 80.0 } else { 0.0 };
    let main_rect = Rect::from_min_max(full.min, Pos2::new(full.max.x - mini_w, full.max.y));
    let mini_rect = Rect::from_min_max(Pos2::new(full.max.x - mini_w, full.min.y), full.max);
    let mut main_ui = ui.new_child(egui::UiBuilder::new().max_rect(main_rect));
    let mut area = egui::ScrollArea::both().id_salt(id.with("scroll")).auto_shrink([false, false]);
    if let Some(line) = st.scroll_line.take() {
        let whole = (line.floor() as usize).min(view.count().saturating_sub(1));
        let row = if view.is_hidden(whole) { whole } else { view.row(whole) };
        st.scroll_to = Some((row as f32 + line.fract()) * row_h);
    }
    if let Some(y) = st.scroll_to.take() {
        area = area.vertical_scroll_offset(y.max(0.0));
    }
    // Arrows at both ends of the vertical scroll bar (when there is something to scroll); the bar runs between them.
    let arrow = ui.spacing().scroll.bar_width.max(14.0);
    let arrows = content.y > main_rect.height() && main_rect.height() > 4.0 * arrow;
    if arrows {
        // The horizontal bar stops before the arrows' column, or it takes presses on the down arrow (it senses drags).
        let mut bars = main_rect.shrink2(Vec2::new(0.0, arrow));
        bars.max.x -= arrow;
        area = area.scroll_bar_rect(bars);
    }
    let scroll = area.show_viewport(&mut main_ui, |ui, vp| {
        let (rect, _) = ui.allocate_exact_size(content, Sense::hover());
        let resp = ui.interact(rect, id, Sense::click_and_drag());
        let lay = Layout {
            view: &view,
            row_h,
            text_x: rect.min.x + gutter_w,
            top: rect.min.y,
        };
        let lines = Lines {
            ui,
            text,
            ix: &ix,
            spans,
            semantic: if ix.lines() == lines_before { extras.semantic } else { &[] },
            font: font.clone(),
            palette,
            depth: use_depth.then(|| (depth_arc.as_slice(), bracket_palette)),
        };
        let visible_rows = (vp.min.y / row_h).floor().max(0.0) as usize..((vp.max.y / row_h).ceil() as usize + 1).min(view.count());
        let vis_lines: Vec<usize> = visible_rows.clone().map(|r| view.line(r)).collect();
        let shown = |l: usize| !view.is_hidden(l) && visible_rows.contains(&view.row(l));
        let clip = Rect::from_min_max(
            Pos2::new(rect.min.x + vp.min.x + gutter_w, rect.min.y + vp.min.y),
            rect.min + vp.max.to_vec2(),
        );

        let sticky = opts.sticky_scroll.then(|| {
            let viewport = vp.translate(rect.min.to_vec2()).intersect(ui.clip_rect());
            sticky::Overlay::new(&st.sticky, &view, viewport, row_h, visible_rows.start)
        });
        if let Some(overlay) = &sticky {
            let (offset, command) = overlay.interact(ui, id, &view, opts.sticky_control_flow);
            if let Some(offset) = offset {
                st.scroll_to = Some(offset);
                resp.request_focus();
                ui.ctx().request_repaint();
            }
            out.context_command = out.context_command.or(command);
        }
        let over_sticky = sticky.as_ref().is_some_and(|overlay| overlay.contains_pointer(ui));
        if opts.navigation_menu && (!over_sticky || resp.context_menu_opened()) {
            out.context_command = navigation_menu::show(&resp, st, &lines, &lay, *version);
        }

        // A drag started in the text can still finish over a pinned header.
        let ptr = if over_sticky && st.drag.is_none() && st.goto_press.is_none() {
            None
        } else {
            mouse(ui, st, &resp, &lines, &lay, &mut out, rect.min.x + vp.min.x + fold_w)
        };

        // ---- paint
        let painter = ui.painter().with_clip_rect(clip);
        let primary = st.primary();
        let (pl, _) = ix.line_col(primary.head);
        let single_line = ix.line_of_byte(primary.anchor) == pl;
        let galleys: Vec<(usize, Arc<egui::Galley>)> = vis_lines.iter().map(|&l| (l, lines.galley(l))).collect();

        for &(l, color) in extras.line_bg {
            if shown(l) {
                let y = lay.line_y(l);
                let band = Rect::from_min_max(Pos2::new(clip.min.x - gutter_w, y), Pos2::new(clip.max.x, y + row_h));
                ui.painter().with_clip_rect(ui.clip_rect()).rect_filled(band, 0.0, color);
            }
        }
        if opts.current_line && single_line && st.sels.len() == 1 && shown(pl) {
            let y = lay.line_y(pl);
            let band = Rect::from_min_max(Pos2::new(clip.min.x - gutter_w, y), Pos2::new(clip.max.x, y + row_h));
            ui.painter()
                .with_clip_rect(ui.clip_rect())
                .rect_filled(band, 0.0, palette.color(Part::CurrentLine));
        }
        let x_in = |l: usize, g: &egui::Galley, b: usize| {
            let range = ix.line_range(text, l);
            let b = b.clamp(range.start, range.end);
            lay.text_x + g.pos_from_cursor(CCursor::new(text[range.start..b].chars().count())).min.x
        };
        // Marks (find matches), then selections.
        let paint_ranges = |ranges: &mut dyn Iterator<Item = Range<usize>>, color: Color32, extend_eol: bool| {
            for r in ranges {
                if r.is_empty() {
                    continue;
                }
                let (l0, l1) = (ix.line_of_byte(r.start), ix.line_of_byte(r.end));
                for (l, g) in &galleys {
                    if *l < l0 || *l > l1 {
                        continue;
                    }
                    let lr = ix.line_range(text, *l);
                    let a = x_in(*l, g, r.start.max(lr.start));
                    let mut b = x_in(*l, g, r.end.min(lr.end));
                    if extend_eol && r.end > lr.end {
                        b += char_w * 0.6;
                    }
                    let y = lay.line_y(*l);
                    painter.rect_filled(Rect::from_min_max(Pos2::new(a, y), Pos2::new(b, y + row_h)), 2.0, color);
                }
            }
        };
        let mark = ui.visuals().warn_fg_color.gamma_multiply(0.25);
        // Search-in-files results in blue, under the file's own find matches (amber).
        let global = Color32::from_rgb(0x3d, 0x8b, 0xfd).gamma_multiply(if ui.visuals().dark_mode { 0.35 } else { 0.28 });
        // Other uses of the symbol under the caret: neutral, a bit stronger where written.
        let uses = ui.visuals().text_color();
        let (read, write) = if ui.visuals().dark_mode { (0.13, 0.22) } else { (0.10, 0.17) };
        paint_ranges(
            &mut extras.symbol_marks.iter().filter(|m| !m.1).map(|m| m.0.clone()),
            uses.gamma_multiply(read),
            false,
        );
        paint_ranges(
            &mut extras.symbol_marks.iter().filter(|m| m.1).map(|m| m.0.clone()),
            uses.gamma_multiply(write),
            false,
        );
        paint_ranges(&mut extras.global_marks.iter().cloned(), global, false);
        paint_ranges(&mut extras.marks.iter().cloned(), mark, false);
        let sel_color = palette.color(Part::Selection).gamma_multiply(if focused { 0.7 } else { 0.4 });
        paint_ranges(&mut st.sels.iter().map(|s| s.range()), sel_color, true);

        // Text.
        for (l, g) in &galleys {
            painter.galley(Pos2::new(lay.text_x, lay.line_y(*l)), g.clone(), text_color);
        }
        if opts.whitespace {
            let color = ui.visuals().weak_text_color().gamma_multiply(0.6);
            for (l, g) in &galleys {
                paint_whitespace(&painter, g, Pos2::new(lay.text_x, lay.line_y(*l)), color);
            }
        }
        // Folded regions: a "⋯" chip after the header line.
        for (l, g) in &galleys {
            if st.folded.contains(l) {
                let x = lay.text_x + g.size().x + char_w;
                let r = Rect::from_min_size(Pos2::new(x, lay.line_y(*l) + 2.0), Vec2::new(3.0 * char_w, row_h - 4.0));
                painter.rect_filled(r, 3.0, ui.visuals().widgets.inactive.weak_bg_fill);
                painter.text(
                    r.center(),
                    egui::Align2::CENTER_CENTER,
                    "...",
                    font.clone(),
                    ui.visuals().weak_text_color(),
                );
            }
        }
        // Squiggles.
        for (r, color) in extras.squiggles {
            let (l0, l1) = (ix.line_of_byte(r.start), ix.line_of_byte(r.end));
            for (l, g) in &galleys {
                if *l < l0 || *l > l1 {
                    continue;
                }
                let lr = ix.line_range(text, *l);
                let a = x_in(*l, g, r.start.max(lr.start));
                let b = x_in(*l, g, r.end.min(lr.end)).max(a + char_w);
                squiggle(&painter, a, b, lay.line_y(*l) + row_h - 1.5, *color);
            }
        }
        // Inline blame after the primary line.
        if let Some(inline) = extras.inline
            && single_line
            && let Some((_, g)) = galleys.iter().find(|(l, _)| *l == pl)
        {
            let color = ui.visuals().weak_text_color().gamma_multiply(0.8);
            let p = Pos2::new(lay.text_x + g.size().x + 4.0 * char_w, lay.line_y(pl) + row_h / 2.0);
            painter.text(p, egui::Align2::LEFT_CENTER, inline, font.clone(), color);
        }
        // Carets (solid, no blinking: blinking would need a repaint twice a second).
        let caret_color = ui.visuals().text_cursor.stroke.color;
        for s in &st.sels {
            let (l, _) = ix.line_col(s.head);
            if let Some((_, g)) = galleys.iter().find(|(gl, _)| *gl == l) {
                let x = x_in(l, g, s.head);
                let y = lay.line_y(l);
                if focused || st.sels.len() > 1 {
                    painter.line_segment([Pos2::new(x, y), Pos2::new(x, y + row_h)], Stroke::new(2.0, caret_color));
                }
            }
        }
        // IME pre-edit text at the caret.
        let caret_x = lines.x_of(primary.head);
        let caret_rect = Rect::from_min_size(Pos2::new(lay.text_x + caret_x, lay.line_y(pl)), Vec2::new(2.0, row_h));
        if let Some(pre) = &st.preedit {
            let g = ui.fonts_mut(|f| f.layout_no_wrap(pre.clone(), font.clone(), text_color));
            let r = Rect::from_min_size(caret_rect.min, g.size());
            painter.rect_filled(r, 0.0, palette.color(Part::Background));
            painter.galley(r.min, g, text_color);
            painter.line_segment([r.left_bottom(), r.right_bottom()], Stroke::new(1.0, text_color));
        }
        out.caret_rect = Some(caret_rect);

        // Gutter (fixed while scrolling horizontally).
        let gx = rect.min.x + vp.min.x;
        let gutter_clip = Rect::from_min_max(Pos2::new(gx, clip.min.y), Pos2::new(gx + gutter_w, clip.max.y));
        let gp = ui.painter().with_clip_rect(gutter_clip);
        gp.rect_filled(gutter_clip, 0.0, palette.color(Part::Background));
        let weak = palette.color(Part::LineNumber);
        let strong = palette.active_line_number(ui.visuals());
        let gutter_hover = resp.hover_pos().is_some_and(|p| p.x < gx + gutter_w);
        for &l in &vis_lines {
            let y = lay.line_y(l);
            let folded = st.folded.contains(&l);
            if folded || (gutter_hover && st.regions.iter().any(|r| r.start == l)) {
                let icon = if folded {
                    crate::icons::CARET_RIGHT
                } else {
                    crate::icons::CARET_DOWN
                };
                gp.text(
                    Pos2::new(gx + 0.3 * char_w, y + row_h / 2.0),
                    egui::Align2::LEFT_CENTER,
                    icon,
                    font.clone(),
                    weak,
                );
            }
            let color = if l == pl { strong } else { weak };
            gp.text(
                Pos2::new(gx + gutter_w - 1.5 * char_w, y),
                egui::Align2::RIGHT_TOP,
                (l + 1).to_string(),
                font.clone(),
                color,
            );
        }
        for &(l, color, deleted) in extras.gutter {
            if !shown(l) && !(deleted && shown(l.saturating_sub(1))) {
                continue;
            }
            let x = gx + gutter_w - 0.9 * char_w;
            let y = lay.line_y(l);
            if deleted {
                let tri = vec![Pos2::new(x, y - 4.0), Pos2::new(x + 5.0, y), Pos2::new(x, y + 4.0)];
                gp.add(egui::Shape::convex_polygon(tri, color, Stroke::NONE));
            } else {
                gp.rect_filled(Rect::from_min_size(Pos2::new(x, y), Vec2::new(3.0, row_h)), 0.0, color);
            }
        }

        if let Some(overlay) = &sticky {
            overlay.paint(&lines, lay.text_x, gutter_w, char_w);
        }

        // Scroll requests.
        if let Some(center) = st.reveal.take() {
            let mut r = caret_rect.expand2(Vec2::new(4.0 * char_w, row_h));
            if !center && let Some(overlay) = &sticky {
                r.min.y -= overlay.height();
            }
            let r = r.translate(Vec2::new(-gutter_w, 0.0)).union(r); // keep the gutter from covering it
            ui.scroll_to_rect(r, if center { Some(egui::Align::Center) } else { None });
        }
        ptr
    });
    ui.allocate_rect(full, Sense::hover());
    if arrows {
        scroll_arrows(ui, id, st, &scroll, main_rect, arrow, row_h, palette);
    } else {
        st.arrow_repeat = None;
    }
    {
        let rows = scroll.state.clamped_offset().y / row_h;
        let row = (rows.floor() as usize).min(view.count().saturating_sub(1));
        out.top_line = view.line(row) as f32 + rows.fract();
    }
    if mini_w > 0.0 {
        minimap(ui, id, st, text, &ix, &view, spans, mini_rect, &scroll, row_h, font.size, palette);
    }

    // IME: report where the caret is, so the candidate window opens next to it.
    if focused && let Some(caret) = out.caret_rect {
        let to_global = ui.ctx().layer_transform_to_global(ui.layer_id()).unwrap_or_default();
        let rect = scroll.inner_rect;
        ui.output_mut(|o| {
            o.ime = Some(egui::output::IMEOutput {
                purpose: egui::IMEPurpose::Normal,
                rect: to_global * rect,
                cursor_rect: to_global * caret,
                should_interrupt_composition: false,
            })
        });
    }
    if let Some((b, pos)) = scroll.inner {
        // Hover: pointer resting on the same byte for 0.6 s.
        let now = ui.input(|i| i.time);
        match st.rest {
            Some((rb, since)) if rb == b => {
                if now - since > 0.6 {
                    out.hover = Some((b, pos));
                } else {
                    ui.ctx()
                        .request_repaint_after(std::time::Duration::from_secs_f64(0.6 - (now - since) + 0.01));
                }
            }
            _ => {
                st.rest = Some((b, now));
                ui.ctx().request_repaint_after(std::time::Duration::from_millis(610));
            }
        }
    } else {
        st.rest = None;
    }
    out
}

/// Up / down buttons at the ends of the vertical scroll bar: a press scrolls one line, holding repeats.
#[allow(clippy::too_many_arguments)]
fn scroll_arrows(
    ui: &egui::Ui,
    id: egui::Id,
    st: &mut State,
    scroll: &egui::scroll_area::ScrollAreaOutput<Option<(usize, Pos2)>>,
    rect: Rect,
    size: f32,
    row_h: f32,
    palette: &Palette,
) {
    let x = rect.max.x - size..=rect.max.x;
    let now = ui.input(|i| i.time);
    let mut held = false;
    for (up, y) in [(true, rect.min.y..=rect.min.y + size), (false, rect.max.y - size..=rect.max.y)] {
        let r = Rect::from_x_y_ranges(x.clone(), y);
        // Drags too: a press that moves a pixel is a drag, which would otherwise go to a scroll bar below.
        let resp = ui.interact(r, id.with(("scroll-arrow", up)), Sense::click_and_drag());
        let fg = palette.color(Part::Foreground);
        if resp.hovered() || resp.is_pointer_button_down_on() {
            ui.painter().rect_filled(r, 2.0, fg.gamma_multiply(0.12));
        }
        let (c, w) = (r.center(), size * 0.3);
        let h = if up { -w * 0.6 } else { w * 0.6 };
        let tri = vec![Pos2::new(c.x - w, c.y - h), Pos2::new(c.x + w, c.y - h), Pos2::new(c.x, c.y + h)];
        let color = fg.gamma_multiply(if resp.hovered() { 0.85 } else { 0.5 });
        ui.painter().add(egui::Shape::convex_polygon(tri, color, Stroke::NONE));
        if !resp.is_pointer_button_down_on() {
            continue;
        }
        held = true;
        let fire = match st.arrow_repeat {
            None => {
                st.arrow_repeat = Some(now + 0.4);
                true
            }
            Some(at) if now >= at => {
                st.arrow_repeat = Some(now + 0.05);
                true
            }
            Some(_) => false,
        };
        if fire {
            let max = (scroll.content_size.y - scroll.inner_rect.height()).max(0.0);
            let offset = scroll.state.clamped_offset().y + if up { -row_h } else { row_h };
            st.scroll_to = Some(offset.clamp(0.0, max));
            ui.ctx().request_repaint();
        }
        if let Some(at) = st.arrow_repeat {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_secs_f64((at - now).max(0.0)));
        }
    }
    if !held {
        st.arrow_repeat = None;
    }
}

/// Overview of the file on the right: 2 px per row, colored by syntax; drag to scroll.
#[allow(clippy::too_many_arguments)]
fn minimap(
    ui: &egui::Ui,
    id: egui::Id,
    st: &mut State,
    text: &str,
    ix: &LineIndex,
    view: &fold::View,
    spans: Option<&[highlight::Line]>,
    rect: Rect,
    scroll: &egui::scroll_area::ScrollAreaOutput<Option<(usize, Pos2)>>,
    row_h: f32,
    font_size: f32,
    palette: &Palette,
) {
    let char_width = font_size / f32::from(crate::settings::DEFAULT_FONT_SIZE);
    let mini_row_height = 2.0 * char_width;
    let rows = view.count();
    let capacity = ((rect.height() / mini_row_height) as usize).max(1);
    let vis_rows = scroll.inner_rect.height() / row_h;
    let offset_rows = scroll.state.clamped_offset().y / row_h;
    // Scroll the minimap proportionally when the file is longer than it can show.
    let max_top = rows.saturating_sub(capacity) as f32;
    let scrollable = (rows as f32 - vis_rows).max(1.0);
    let top = if rows <= capacity {
        0.0
    } else {
        (offset_rows / scrollable * max_top).clamp(0.0, max_top)
    };
    let top_row = top as usize;
    let painter = ui.painter().with_clip_rect(rect);
    let weak = palette.color(Part::Foreground).gamma_multiply(0.45);
    for (i, row) in (top_row..rows.min(top_row + capacity)).enumerate() {
        let l = view.line(row);
        let y = rect.min.y + i as f32 * mini_row_height;
        let lr = ix.line_range(text, l);
        let line = &text[lr];
        let draw = |range: Range<usize>, color: Color32| {
            let piece = &line[range.start.min(line.len())..range.end.min(line.len())];
            let lead = piece.len() - piece.trim_start().len();
            let body = piece.trim();
            if body.is_empty() {
                return;
            }
            let x = rect.min.x + 4.0 + (range.start + lead) as f32 * char_width;
            if x > rect.max.x {
                return;
            }
            let w = (body.len() as f32 * char_width).min(rect.max.x - x);
            painter.rect_filled(
                Rect::from_min_size(Pos2::new(x, y), Vec2::new(w, mini_row_height * 0.8)),
                0.0,
                color.gamma_multiply(0.7),
            );
        };
        match spans.and_then(|s| s.get(l)) {
            Some(hl) => {
                for sp in &hl.spans {
                    draw(sp.range.clone(), sp.color(palette));
                }
            }
            None => draw(0..line.len(), weak),
        }
    }
    // The visible part.
    let vy = rect.min.y + (offset_rows - top) * mini_row_height;
    let vr = Rect::from_min_size(Pos2::new(rect.min.x, vy), Vec2::new(rect.width(), vis_rows * mini_row_height));
    // The visible part of the file: a clear band with an outline, readable on both themes.
    let band = palette.color(Part::Foreground);
    painter.rect_filled(vr, 0.0, band.gamma_multiply(0.16));
    painter.rect_stroke(vr, 0.0, egui::Stroke::new(1.0, band.gamma_multiply(0.35)), egui::StrokeKind::Inside);

    let resp = ui.interact(rect, id.with("minimap"), Sense::click_and_drag());
    if resp.drag_started() || resp.clicked() {
        st.minimap_drag = true;
    }
    if ((st.minimap_drag && resp.dragged()) || resp.clicked())
        && let Some(p) = resp.interact_pointer_pos()
    {
        let row = top + (p.y - rect.min.y) / mini_row_height;
        st.scroll_to = Some((row - vis_rows / 2.0) * row_h);
        ui.ctx().request_repaint();
    }
    if resp.drag_stopped() {
        st.minimap_drag = false;
    }
}

fn squiggle(p: &egui::Painter, a: f32, b: f32, y: f32, color: Color32) {
    let mut pts = Vec::new();
    let mut x = a;
    let mut up = true;
    while x <= b {
        pts.push(Pos2::new(x, if up { y - 1.5 } else { y + 1.0 }));
        x += 2.5;
        up = !up;
    }
    if pts.len() > 1 {
        p.add(egui::Shape::line(pts, Stroke::new(1.0, color)));
    }
}

fn paint_whitespace(p: &egui::Painter, g: &egui::Galley, origin: Pos2, color: Color32) {
    let stroke = Stroke::new(1.0, color);
    for row in &g.rows {
        let y = origin.y + row.rect().center().y;
        for gl in &row.row.glyphs {
            if gl.chr != ' ' && gl.chr != '\t' {
                continue;
            }
            let x0 = origin.x + row.pos.x + gl.pos.x;
            let x1 = x0 + gl.advance_width;
            if gl.chr == ' ' {
                p.circle_filled(Pos2::new((x0 + x1) / 2.0, y), 1.0, color);
            } else {
                let (a, b) = (x0 + 2.0, (x1 - 2.0).max(x0 + 6.0));
                p.line_segment([Pos2::new(a, y), Pos2::new(b, y)], stroke);
                p.line_segment([Pos2::new(b - 3.0, y - 3.0), Pos2::new(b, y)], stroke);
                p.line_segment([Pos2::new(b - 3.0, y + 3.0), Pos2::new(b, y)], stroke);
            }
        }
    }
}

// ------------------------------------------------------------------ mouse

/// Handles clicks and drags; returns the byte under a resting pointer (for hover).
fn mouse(
    ui: &egui::Ui,
    st: &mut State,
    resp: &egui::Response,
    lines: &Lines,
    lay: &Layout,
    out: &mut Output,
    fold_x: f32,
) -> Option<(usize, Pos2)> {
    use egui::PointerButton::{Middle, Primary};
    let (pos, pressed, down, released, mid_pressed, mid_down, mods, time) = ui.input(|i| {
        (
            i.pointer.interact_pos(),
            i.pointer.button_pressed(Primary),
            i.pointer.button_down(Primary),
            i.pointer.button_released(Primary),
            i.pointer.button_pressed(Middle),
            i.pointer.button_down(Middle),
            i.modifiers,
            i.time,
        )
    });
    let ix = lines.ix;
    let text = lines.text;
    let to_line = |p: Pos2| lay.view.line(((p.y - lay.top) / lay.row_h).floor().max(0.0) as usize);
    let to_byte = |p: Pos2| lines.byte_at_x(to_line(p), p.x - lay.text_x);
    let hovered = resp.hovered();
    if hovered && !mods.command {
        ui.ctx().set_cursor_icon(egui::CursorIcon::Text);
    }

    // Cmd/Ctrl + hover: underline the symbol (the click goes to its definition).
    let mut hover_byte = None;
    if hovered && let Some(p) = resp.hover_pos() {
        let b = to_byte(p);
        hover_byte = Some((b, p));
        if mods.command {
            let l = to_line(p);
            let range = ix.line_range(text, l);
            if b < range.end {
                let w = buffer::word_at(text, range, b);
                if w.end > w.start && text[w.clone()].chars().all(|c| c.is_alphanumeric() || c == '_') {
                    let (a, z) = (lines.x_of(w.start), lines.x_of(w.end));
                    let y = lay.line_y(l) + lay.row_h - 1.0;
                    let color = ui.visuals().hyperlink_color;
                    ui.painter().line_segment(
                        [Pos2::new(lay.text_x + a, y), Pos2::new(lay.text_x + z, y)],
                        Stroke::new(1.0, color),
                    );
                    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                }
            }
        }
    }

    // Click on a fold arrow (left edge of the gutter): fold / unfold that region.
    if pressed
        && hovered
        && let Some(p) = pos
        && p.x < fold_x
    {
        let l = to_line(p);
        if st.regions.iter().any(|r| r.start == l) {
            if !st.folded.remove(&l) {
                st.folded.insert(l);
            }
            return None;
        }
    }
    if pressed
        && hovered
        && let Some(p) = pos
    {
        resp.request_focus();
        st.history.seal();
        st.preferred_x = None;
        let b = to_byte(p);
        // Click count (double/triple click), from our own press history.
        let count = match st.last_press {
            Some((t, lp, n)) if time - t < 0.4 && lp.distance(p) < 4.0 => (n % 3) + 1,
            _ => 1,
        };
        st.last_press = Some((time, p, count));
        let l = to_line(p);
        let line_range = ix.line_range(text, l);
        let full_line = ix.line_start(l)..ix.line_start((l + 1).min(ix.lines() - 1)).max(line_range.end);
        if mods.command {
            st.goto_press = Some(b);
            st.sels = vec![Sel::caret(b)];
        } else if mods.alt {
            // Alt+click adds a cursor.
            st.sels.push(Sel::caret(b));
        } else if mods.shift {
            let mut s = st.primary();
            s.head = b;
            st.sels = vec![s];
            st.drag = Some(Drag::Select {
                origin: s.anchor..s.anchor,
                unit: Unit::Char,
            });
        } else {
            let (origin, unit) = match count {
                2 => (buffer::word_at(text, line_range, b), Unit::Word),
                3 => (full_line, Unit::Line),
                _ => (b..b, Unit::Char),
            };
            st.sels = vec![Sel {
                anchor: origin.start,
                head: origin.end,
            }];
            st.drag = Some(Drag::Select { origin, unit });
        }
    } else if down && let (Some(p), Some(Drag::Select { origin, unit })) = (pos, &st.drag) {
        let b = to_byte(p);
        let l = to_line(p);
        let range = match unit {
            Unit::Char => b..b,
            Unit::Word => buffer::word_at(text, ix.line_range(text, l), b),
            Unit::Line => ix.line_start(l)..if l + 1 < ix.lines() { ix.line_start(l + 1) } else { text.len() },
        };
        let sel = if range.start < origin.start {
            Sel {
                anchor: origin.end,
                head: range.start,
            }
        } else {
            Sel {
                anchor: origin.start,
                head: range.end.max(origin.end),
            }
        };
        if let Some(last) = st.sels.last_mut() {
            *last = sel;
        }
        if b != origin.start {
            st.goto_press = None;
        }
        ui.scroll_to_rect(Rect::from_center_size(p, Vec2::splat(lay.row_h)), None);
    }
    if released {
        if let Some(b) = st.goto_press.take()
            && resp.clicked()
        {
            out.goto = Some(b);
        }
        st.drag = None;
    }

    // Middle-button drag: box selection, one cursor per line.
    if mid_pressed
        && hovered
        && let Some(p) = pos
    {
        resp.request_focus();
        st.history.seal();
        st.drag = Some(Drag::Column {
            line: to_line(p),
            x: p.x - lay.text_x,
        });
    }
    if let (Some(Drag::Column { line, x }), Some(p)) = (&st.drag, pos) {
        if mid_down {
            let (l1, x1) = (to_line(p), p.x - lay.text_x);
            let (lo, hi) = ((*line).min(l1), (*line).max(l1));
            let mut sels: Vec<Sel> = (lo..=hi)
                .map(|l| Sel {
                    anchor: lines.byte_at_x(l, *x),
                    head: lines.byte_at_x(l, x1),
                })
                .collect();
            if l1 < *line {
                sels.reverse(); // the primary is the line under the pointer
            }
            st.sels = sels;
            ui.scroll_to_rect(Rect::from_center_size(p, Vec2::splat(lay.row_h)), None);
        } else {
            st.drag = None;
        }
    }
    if down || mid_down { None } else { hover_byte }
}

// ------------------------------------------------------------------ keyboard

struct KeyCtx<'a> {
    font: &'a egui::FontId,
    is_mac: bool,
    viewport_rows: usize,
    opts: &'a Options<'a>,
}

fn closer(c: char) -> Option<char> {
    match c {
        '{' => Some('}'),
        '(' => Some(')'),
        '[' => Some(']'),
        _ => None,
    }
}

/// Returns true when the text changed.
fn handle_event(ui: &egui::Ui, st: &mut State, text: &mut String, version: &mut u64, k: &KeyCtx, ev: &Event) -> bool {
    if k.opts.read_only {
        let edits = match ev {
            Event::Text(_) | Event::Paste(_) | Event::Ime(egui::ImeEvent::Commit(_)) => true,
            Event::Key {
                key,
                pressed: true,
                modifiers: m,
                ..
            } => {
                matches!(key, Key::Backspace | Key::Delete | Key::Enter | Key::Tab)
                    || m.command && matches!(key, Key::Z | Key::Y | Key::Slash)
            }
            _ => false,
        };
        if edits {
            return false;
        }
        if matches!(ev, Event::Cut) {
            return handle_event(ui, st, text, version, k, &Event::Copy);
        }
    }
    let changed = match ev {
        Event::Text(t) if !t.is_empty() => {
            insert(st, text, t, Kind::Typing);
            true
        }
        Event::Ime(egui::ImeEvent::Preedit { text: pre, .. }) => {
            st.preedit = (!pre.is_empty()).then(|| pre.clone());
            false
        }
        Event::Ime(egui::ImeEvent::Commit(t)) => {
            st.preedit = None;
            if t.is_empty() {
                false
            } else {
                insert(st, text, t, Kind::Typing);
                true
            }
        }
        Event::Copy | Event::Cut => {
            let ix = st.index_arc(text, *version);
            let whole_lines = st.sels.iter().all(|s| s.is_empty());
            let ranges: Vec<Range<usize>> = if whole_lines {
                st.sels.iter().map(|s| full_line(&ix, text, ix.line_of_byte(s.head))).collect()
            } else {
                st.sels.iter().map(|s| s.range()).collect()
            };
            let mut sorted = ranges.clone();
            sorted.sort_by_key(|r| r.start);
            sorted.dedup();
            let copied: Vec<&str> = sorted.iter().map(|r| &text[r.clone()]).collect();
            let joined = if whole_lines { copied.concat() } else { copied.join("\n") };
            ui.ctx().copy_text(joined);
            if matches!(ev, Event::Cut) {
                let before = st.sels.clone();
                st.sels = buffer::edit(
                    text,
                    &mut st.history,
                    &before,
                    sorted.into_iter().map(|r| (r, String::new())).collect(),
                    Kind::Other,
                );
                true
            } else {
                false
            }
        }
        Event::Paste(p) => {
            let pieces: Vec<&str> = p.lines().collect();
            let before = st.sels.clone();
            if st.sels.len() > 1 && pieces.len() == st.sels.len() {
                let mut order: Vec<usize> = (0..before.len()).collect();
                order.sort_by_key(|&i| before[i].range().start);
                let edits = order
                    .iter()
                    .enumerate()
                    .map(|(n, &i)| (before[i].range(), pieces[n].to_string()))
                    .collect();
                st.sels = buffer::edit(text, &mut st.history, &before, edits, Kind::Other);
            } else {
                let p = p.replace("\r\n", "\n");
                st.sels = buffer::edit(
                    text,
                    &mut st.history,
                    &before,
                    before.iter().map(|s| (s.range(), p.clone())).collect(),
                    Kind::Other,
                );
            }
            true
        }
        Event::Key {
            key,
            pressed: true,
            modifiers: m,
            ..
        } => key_press(ui, st, text, version, k, *key, *m),
        _ => false,
    };
    if changed {
        *version += 1;
        st.reveal.get_or_insert(false);
        st.preferred_x = None;
    }
    changed
}

fn full_line(ix: &LineIndex, text: &str, l: usize) -> Range<usize> {
    let start = ix.line_start(l);
    let end = if l + 1 < ix.lines() { ix.line_start(l + 1) } else { text.len() };
    start..end
}

fn insert(st: &mut State, text: &mut String, t: &str, kind: Kind) {
    let before = st.sels.clone();
    st.sels = buffer::edit(
        text,
        &mut st.history,
        &before,
        before.iter().map(|s| (s.range(), t.to_string())).collect(),
        kind,
    );
}

fn key_press(ui: &egui::Ui, st: &mut State, text: &mut String, version: &mut u64, k: &KeyCtx, key: Key, m: egui::Modifiers) -> bool {
    let ix = st.index_arc(text, *version);
    let word = if k.is_mac { m.alt } else { m.ctrl };
    let line_mod = k.is_mac && m.mac_cmd;
    let shift = m.shift;
    let before = st.sels.clone();
    let view = st.view.clone();
    let lines = Lines {
        ui,
        text,
        ix: &ix,
        spans: None,
        semantic: &[],
        font: k.font.clone(),
        palette: k.opts.palette,
        depth: None,
    };
    // Move every caret with `f`; Shift extends.
    let moved = |st: &mut State, f: &dyn Fn(Sel) -> usize, keep_x: bool| {
        for s in &mut st.sels {
            let h = f(*s);
            *s = if shift { Sel { anchor: s.anchor, head: h } } else { Sel::caret(h) };
        }
        dedup(&mut st.sels);
        if !keep_x {
            st.preferred_x = None;
        }
        st.history.seal();
        st.reveal = Some(false);
        false
    };
    let line_home = |b: usize| {
        let l = ix.line_of_byte(b);
        let r = ix.line_range(text, l);
        let first = r.start + buffer::indent_of(&text[r.clone()]).len();
        if b == first { r.start } else { first }
    };
    let line_end = |b: usize| ix.line_range(text, ix.line_of_byte(b)).end;
    let vertical = |st: &mut State, rows: isize| {
        let px = st.preferred_x;
        let mut new_x = None;
        let mut f = |s: Sel| {
            let (l, _) = ix.line_col(s.head);
            let x = px.unwrap_or_else(|| lines.x_of(s.head));
            new_x = Some(x);
            let target = view.row(l) as isize + rows;
            if target < 0 {
                0
            } else if target as usize >= view.count() {
                text.len()
            } else {
                lines.byte_at_x(view.line(target as usize), x)
            }
        };
        let heads: Vec<usize> = st.sels.iter().map(|s| f(*s)).collect();
        for (s, h) in st.sels.iter_mut().zip(heads) {
            *s = if shift { Sel { anchor: s.anchor, head: h } } else { Sel::caret(h) };
        }
        dedup(&mut st.sels);
        st.preferred_x = px.or(new_x);
        st.history.seal();
        st.reveal = Some(false);
        false
    };
    match key {
        Key::ArrowLeft if line_mod => moved(st, &|s| line_home(s.head), false),
        Key::ArrowRight if line_mod => moved(st, &|s| line_end(s.head), false),
        Key::ArrowLeft if word => moved(st, &|s| buffer::prev_word(text, s.head), false),
        Key::ArrowRight if word => moved(st, &|s| buffer::next_word(text, s.head), false),
        Key::ArrowLeft => moved(
            st,
            &|s| {
                if !shift && !s.is_empty() {
                    s.range().start
                } else {
                    buffer::prev_char(text, s.head)
                }
            },
            false,
        ),
        Key::ArrowRight => moved(
            st,
            &|s| {
                if !shift && !s.is_empty() {
                    s.range().end
                } else {
                    buffer::next_char(text, s.head)
                }
            },
            false,
        ),
        Key::ArrowUp if line_mod => moved(st, &|_| 0, false),
        Key::ArrowDown if line_mod => moved(st, &|_| text.len(), false),
        Key::ArrowUp => vertical(st, -1),
        Key::ArrowDown => vertical(st, 1),
        Key::PageUp => vertical(st, -(k.viewport_rows as isize)),
        Key::PageDown => vertical(st, k.viewport_rows as isize),
        Key::Home if m.command => moved(st, &|_| 0, false),
        Key::End if m.command => moved(st, &|_| text.len(), false),
        Key::Home => moved(st, &|s| line_home(s.head), false),
        Key::End => moved(st, &|s| line_end(s.head), false),
        Key::OpenBracket if m.command && m.alt => {
            st.fold_at_caret(true);
            false
        }
        Key::CloseBracket if m.command && m.alt => {
            st.fold_at_caret(false);
            false
        }
        Key::A if m.command => {
            st.sels = vec![Sel {
                anchor: 0,
                head: text.len(),
            }];
            false
        }
        Key::Z if m.command && !shift => undo_redo(st, text, false),
        Key::Z if m.command && shift => undo_redo(st, text, true),
        Key::Y if m.command && !k.is_mac => undo_redo(st, text, true),
        Key::D if m.command => {
            add_next_occurrence(st, text, &ix);
            false
        }
        Key::Slash if m.command => match k.opts.comment {
            Some(token) => toggle_comment(st, text, &ix, token),
            None => false,
        },
        Key::Escape if st.sels.len() > 1 || !st.primary().is_empty() => {
            let p = st.primary();
            st.sels = vec![Sel::caret(p.head)];
            false
        }
        Key::Backspace | Key::Delete => {
            let back = key == Key::Backspace;
            let unit = k.opts.indent;
            let edits = before
                .iter()
                .map(|s| {
                    if !s.is_empty() {
                        return (s.range(), String::new());
                    }
                    let h = s.head;
                    let to = if back {
                        if line_mod {
                            ix.line_start(ix.line_of_byte(h))
                        } else if word {
                            buffer::prev_word(text, h)
                        } else {
                            soft_tab_back(text, &ix, h, unit).unwrap_or_else(|| buffer::prev_char(text, h))
                        }
                    } else if line_mod {
                        line_end(h)
                    } else if word {
                        buffer::next_word(text, h)
                    } else {
                        buffer::next_char(text, h)
                    };
                    (h.min(to)..h.max(to), String::new())
                })
                .collect();
            st.sels = buffer::edit(text, &mut st.history, &before, edits, Kind::Deleting);
            true
        }
        Key::Enter if !m.command => {
            let unit = k.opts.indent;
            let edits = before
                .iter()
                .map(|s| {
                    let r = s.range();
                    let l = ix.line_of_byte(r.start);
                    let lr = ix.line_range(text, l);
                    let indent = buffer::indent_of(&text[lr.clone()]).to_string();
                    let prev = text[lr.start..r.start].trim_end().chars().next_back();
                    let next = text[r.end..lr.end.max(r.end)].chars().next();
                    let opens = prev.is_some_and(|c| matches!(c, '{' | '(' | '[' | ':'));
                    if let (Some(p), Some(n)) = (prev, next)
                        && closer(p) == Some(n)
                    {
                        // Between brackets: put the closer on its own line.
                        let first = format!("\n{indent}{unit}");
                        let caret = first.len();
                        (r, format!("{first}\n{indent}"), caret)
                    } else {
                        let s = format!("\n{indent}{}", if opens { unit } else { "" });
                        let n = s.len();
                        (r, s, n)
                    }
                })
                .collect();
            st.sels = buffer::edit_carets(text, &mut st.history, &before, edits, Kind::Other);
            true
        }
        Key::Tab => {
            let multi_line = before.iter().any(|s| ix.line_of_byte(s.anchor) != ix.line_of_byte(s.head));
            if shift || multi_line {
                indent_lines(st, text, &ix, k.opts.indent, !shift)
            } else {
                insert(st, text, k.opts.indent, Kind::Typing);
                true
            }
        }
        _ => false,
    }
}

fn dedup(sels: &mut Vec<Sel>) {
    let mut seen = std::collections::HashSet::new();
    // Keep the last (primary) occurrence of each caret position.
    let mut out: Vec<Sel> = sels.iter().rev().filter(|s| seen.insert(s.head)).copied().collect();
    out.reverse();
    *sels = out;
}

/// Backspace inside leading spaces deletes back to the previous indent stop.
fn soft_tab_back(text: &str, ix: &LineIndex, h: usize, unit: &str) -> Option<usize> {
    if unit.contains('\t') || unit.is_empty() {
        return None;
    }
    let (l, col) = ix.line_col(h);
    let start = ix.line_start(l);
    if col == 0 || !text[start..h].bytes().all(|b| b == b' ') {
        return None;
    }
    let w = unit.len();
    Some(start + (col - 1) / w * w)
}

fn undo_redo(st: &mut State, text: &mut String, redo: bool) -> bool {
    let sels = if redo { st.history.redo(text) } else { st.history.undo(text) };
    match sels {
        Some(s) => {
            st.sels = s;
            true
        }
        None => false,
    }
}

fn add_next_occurrence(st: &mut State, text: &str, ix: &LineIndex) {
    let p = st.primary();
    if p.is_empty() {
        let l = ix.line_of_byte(p.head);
        let w = buffer::word_at(text, ix.line_range(text, l), p.head);
        if !w.is_empty() {
            *st.sels.last_mut().unwrap() = Sel {
                anchor: w.start,
                head: w.end,
            };
        }
        return;
    }
    let needle = &text[p.range()];
    let from = st.sels.iter().map(|s| s.range().end).max().unwrap_or(0);
    let found = text[from..].find(needle).map(|i| from + i).or_else(|| text.find(needle));
    if let Some(i) = found
        && !st.sels.iter().any(|s| s.range().start == i)
    {
        st.sels.push(Sel {
            anchor: i,
            head: i + needle.len(),
        });
        st.reveal = Some(false);
    }
}

fn selected_lines(st: &State, ix: &LineIndex) -> Vec<usize> {
    let mut lines: Vec<usize> = st
        .sels
        .iter()
        .flat_map(|s| {
            let r = s.range();
            let (a, b) = (ix.line_of_byte(r.start), ix.line_of_byte(r.end));
            // A selection ending at column 0 doesn't include that line.
            let b = if b > a && ix.line_start(b) == r.end { b - 1 } else { b };
            a..=b
        })
        .collect();
    lines.sort_unstable();
    lines.dedup();
    lines
}

fn indent_lines(st: &mut State, text: &mut String, ix: &LineIndex, unit: &str, indent: bool) -> bool {
    let edits: Vec<(Range<usize>, String)> = selected_lines(st, ix)
        .into_iter()
        .filter_map(|l| {
            let r = ix.line_range(text, l);
            let start = r.start;
            if indent {
                (!r.is_empty()).then(|| (start..start, unit.to_string()))
            } else {
                let ws = buffer::indent_of(&text[r]);
                let n = if ws.starts_with('\t') { 1 } else { ws.len().min(unit.len().max(1)) };
                (n > 0).then(|| (start..start + n, String::new()))
            }
        })
        .collect();
    if edits.is_empty() {
        return false;
    }
    let before = st.sels.clone();
    st.sels = buffer::edit_keep(text, &mut st.history, &before, edits);
    true
}

fn toggle_comment(st: &mut State, text: &mut String, ix: &LineIndex, token: &str) -> bool {
    let lines: Vec<usize> = selected_lines(st, ix)
        .into_iter()
        .filter(|&l| !text[ix.line_range(text, l)].trim().is_empty())
        .collect();
    if lines.is_empty() {
        return false;
    }
    let all_commented = lines.iter().all(|&l| text[ix.line_range(text, l)].trim_start().starts_with(token));
    let min_indent = lines
        .iter()
        .map(|&l| buffer::indent_of(&text[ix.line_range(text, l)]).len())
        .min()
        .unwrap_or(0);
    let edits = lines
        .iter()
        .map(|&l| {
            let r = ix.line_range(text, l);
            let line = &text[r.clone()];
            if all_commented {
                let at = r.start + buffer::indent_of(line).len();
                let mut end = at + token.len();
                if text[end..].starts_with(' ') {
                    end += 1;
                }
                (at..end, String::new())
            } else {
                (r.start + min_indent..r.start + min_indent, format!("{token} "))
            }
        })
        .collect();
    let before = st.sels.clone();
    st.sels = buffer::edit_keep(text, &mut st.history, &before, edits);
    true
}

/// Line comment token for a file extension.
pub fn comment_token(ext: &str) -> Option<&'static str> {
    Some(match ext {
        "rs" | "c" | "h" | "cc" | "cpp" | "cxx" | "hpp" | "hh" | "js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx" | "mts" | "go" | "java"
        | "cs" | "swift" | "kt" | "kts" | "scala" | "dart" | "zig" | "php" | "jsonc" | "proto" | "glsl" | "wgsl" => "//",
        "py" | "pyi" | "sh" | "bash" | "zsh" | "fish" | "rb" | "toml" | "yaml" | "yml" | "pl" | "r" | "cmake" | "mk" | "conf" | "ini"
        | "dockerfile" | "nix" | "ex" | "exs" | "jl" | "ps1" => "#",
        "lua" | "sql" | "hs" | "elm" | "ada" => "--",
        "lisp" | "clj" | "el" | "scm" => ";",
        "tex" | "erl" => "%",
        "vim" => "\"",
        _ => return None,
    })
}

/// Indent unit for a text: a tab if most indented lines start with a tab, otherwise
/// the most common small step of leading spaces (2 or 4; default 4).
/// The file's indentation unit; `default` spaces when the file doesn't show one.
pub fn detect_indent(text: &str, default: usize) -> String {
    let (mut tabs, mut spaces) = (0, 0);
    let mut steps = [0usize; 9];
    let mut prev = 0usize;
    for line in text.lines().take(2000) {
        if line.trim().is_empty() {
            continue;
        }
        if line.starts_with('\t') {
            tabs += 1;
            continue;
        }
        let n = line.len() - line.trim_start_matches(' ').len();
        if n > 0 {
            spaces += 1;
        }
        let d = n.abs_diff(prev);
        if (2..=8).contains(&d) {
            steps[d] += 1;
        }
        prev = n;
    }
    if tabs > spaces {
        return "\t".into();
    }
    if steps.iter().all(|&n| n == 0) {
        return " ".repeat(default);
    }
    let best = if steps[2] > steps[4] { 2 } else { 4 };
    " ".repeat(best)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs the real widget headlessly: one frame per batch of events.
    #[test]
    fn palette_changes_invalidate_only_affected_galleys() {
        let ctx = egui::Context::default();
        let text = "// sample\ncall(MAX);";
        let ix = LineIndex::new(text);
        let mut syntax = highlight::Cache::default();
        highlight::mark_syntax_warmed_for_test("rs");
        let spans = syntax.spans(&ctx, 1, text, "rs", true).unwrap();
        let tokens = [(0..4, highlight::SemanticRole::new(Part::Macro))];
        let mut palette = Palette::defaults(true);
        let mut output = ctx.run_ui(Default::default(), |ui| {
            let font = egui::TextStyle::Monospace.resolve(ui.style());
            let galley = |palette: &Palette, line, semantic| {
                let job = line_job(text, &ix, line, Some(spans), semantic, &font, palette, None);
                ui.fonts_mut(|fonts| fonts.layout_job(job))
            };
            let before = galley(&palette, 0, &[]);
            let semantic_before = galley(&palette, 1, &tokens);
            assert!(Arc::ptr_eq(&before, &galley(&palette, 0, &[])));
            palette.set(Part::Comment, Color32::RED);
            let after = galley(&palette, 0, &[]);
            assert!(!Arc::ptr_eq(&before, &after));
            assert_eq!(after.job.sections[0].format.color, Color32::RED);
            assert!(Arc::ptr_eq(&semantic_before, &galley(&palette, 1, &tokens)));
            palette.set(Part::Macro, Color32::GREEN);
            let semantic_after = galley(&palette, 1, &tokens);
            assert!(!Arc::ptr_eq(&semantic_before, &semantic_after));
            assert_eq!(semantic_after.job.sections[0].format.color, Color32::GREEN);
        });
        output.textures_delta.clear();
    }

    struct Harness {
        ctx: egui::Context,
        st: State,
        text: String,
        version: u64,
        hl: highlight::Cache,
        id: egui::Id,
        clipboard: Option<String>,
        read_only: bool,
        /// Input time, advanced 0.1 s per frame (None: no time, as before), for hover animations.
        clock: Option<f64>,
    }

    impl Harness {
        fn new(text: &str) -> Self {
            let mut h = Self {
                ctx: egui::Context::default(),
                st: State::default(),
                text: text.to_string(),
                version: 0,
                hl: Default::default(),
                id: egui::Id::NULL.with("test-editor"),
                clipboard: None,
                read_only: false,
                clock: None,
            };
            let id = h.id;
            h.ctx.memory_mut(|m| m.request_focus(id));
            h.frame(vec![]);
            h.frame(vec![]);
            h
        }

        fn frame(&mut self, events: Vec<Event>) -> Output {
            let input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(800.0, 600.0))),
                events,
                time: self.clock,
                ..Default::default()
            };
            if let Some(t) = &mut self.clock {
                *t += 0.1;
            }
            let mut out = Output::default();
            let (st, text, version, hl, id, read_only) = (
                &mut self.st,
                &mut self.text,
                &mut self.version,
                &mut self.hl,
                self.id,
                self.read_only,
            );
            let mut full = self.ctx.run_ui(input, |ui| {
                let opts = Options {
                    palette: &Palette::defaults(ui.visuals().dark_mode),
                    lang: "rs",
                    highlight: false,
                    whitespace: false,
                    current_line: true,
                    indent: "    ",
                    comment: Some("//"),
                    minimap: false,
                    bracket_colors: true,
                    sticky_scroll: true,
                    sticky_control_flow: true,
                    navigation_menu: false,
                    read_only,
                };
                out = show(ui, id, st, text, version, hl, &opts, &Extras::default());
            });
            full.textures_delta.clear(); // no renderer here (epaint asserts on dropped deltas in debug)
            for cmd in full.platform_output.commands {
                if let egui::OutputCommand::CopyText(t) = cmd {
                    self.clipboard = Some(t);
                }
            }
            out
        }

        fn key(&mut self, key: Key, modifiers: egui::Modifiers) {
            self.frame(vec![Event::Key {
                key,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers,
            }]);
        }

        fn typ(&mut self, t: &str) {
            self.frame(vec![Event::Text(t.into())]);
        }
    }

    const NONE: egui::Modifiers = egui::Modifiers::NONE;

    #[test]
    fn scroll_bar_arrows_scroll_one_line() {
        let mut h = Harness::new(&"line\n".repeat(200));
        h.st.scroll_to_line(20.0);
        assert!((h.frame(vec![]).top_line - 20.0).abs() < 0.1);
        let click = |h: &mut Harness, up: bool| {
            let pos = h
                .ctx
                .read_response(h.id.with(("scroll-arrow", up)))
                .expect("arrow shown")
                .rect
                .center();
            h.frame(vec![Event::PointerMoved(pos)]);
            for pressed in [true, false] {
                h.frame(vec![Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed,
                    modifiers: NONE,
                }]);
            }
            h.frame(vec![]).top_line
        };
        assert!((click(&mut h, false) - 21.0).abs() < 0.1);
        assert!((click(&mut h, true) - 20.0).abs() < 0.1);
        assert!((click(&mut h, true) - 19.0).abs() < 0.1);
    }

    #[test]
    fn the_down_arrow_wins_over_the_horizontal_scroll_bar() {
        let mut h = Harness::new(&format!("{}\n", "x".repeat(400)).repeat(200));
        h.clock = Some(1.0);
        h.st.scroll_to_line(20.0);
        h.frame(vec![]);
        let arrow = h.ctx.read_response(h.id.with(("scroll-arrow", false))).expect("arrow shown").rect;
        // Bottom edge of the arrow, where the horizontal bar runs.
        let pos = Pos2::new(arrow.center().x, arrow.bottom() - 2.0);
        // Hover long enough for the floating bars to expand, as a real pointer would.
        for _ in 0..10 {
            h.frame(vec![Event::PointerMoved(pos)]);
        }
        // A real click: the pointer moves a little while pressed, which makes it a drag.
        let moved = pos + Vec2::new(-12.0, 0.0);
        let press = |pos, pressed| Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: NONE,
        };
        h.frame(vec![press(pos, true)]);
        h.frame(vec![Event::PointerMoved(moved)]);
        // The arrow keeps the drag (the horizontal bar below would scroll sideways with it).
        let arrow_id = h.ctx.read_response(h.id.with(("scroll-arrow", false))).map(|r| r.id);
        assert_eq!(h.ctx.dragged_id(), arrow_id);
        h.frame(vec![press(moved, false)]);
        assert!((h.frame(vec![]).top_line - 21.0).abs() < 0.1);
    }

    #[test]
    fn clicking_a_sticky_header_scrolls_without_relocating_the_caret() {
        let text = format!("fn main() {{\n{}}}\n", "    body();\n".repeat(100));
        let mut h = Harness::new(&text);
        h.st.scroll_to_line(20.0);
        assert!((h.frame(vec![]).top_line - 20.0).abs() < 0.1);
        let header = h
            .ctx
            .read_response(h.id.with(("sticky_header", 0usize)))
            .expect("function header is pinned");
        let pos = header.rect.center();
        h.frame(vec![Event::PointerMoved(pos)]);
        for pressed in [true, false] {
            h.frame(vec![Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: NONE,
            }]);
        }
        let out = h.frame(vec![]);
        assert!(out.top_line < 0.1);
        assert!(out.focused);
        assert_eq!(h.st.primary().head, 0);
    }

    #[test]
    fn read_only_selects_and_copies_but_never_edits() {
        let mut h = Harness::new("base text\nline two\n");
        h.read_only = true;
        h.key(Key::A, egui::Modifiers::COMMAND);
        h.frame(vec![Event::Copy]);
        assert_eq!(h.clipboard.as_deref(), Some("base text\nline two\n"));
        h.typ("x");
        h.frame(vec![Event::Paste("pasted".into()), Event::Cut]);
        for key in [Key::Backspace, Key::Delete, Key::Enter, Key::Tab] {
            h.key(key, NONE);
        }
        h.key(Key::Slash, egui::Modifiers::COMMAND);
        h.key(Key::Z, egui::Modifiers::COMMAND);
        assert_eq!(h.text, "base text\nline two\n");
        assert_eq!(h.version, 0);
        // Cut copies instead.
        assert_eq!(h.clipboard.as_deref(), Some("base text\nline two\n"));
    }

    #[test]
    fn typing_navigation_and_undo() {
        let mut h = Harness::new("fn main() {}\nlet x = 1;");
        assert!(h.ctx.memory(|m| m.has_focus(h.id)));
        h.typ("// ");
        assert_eq!(h.text, "// fn main() {}\nlet x = 1;");
        h.key(Key::End, NONE);
        h.key(Key::ArrowLeft, NONE);
        h.key(Key::Enter, NONE);
        // Enter between braces: indented line, closer on its own line.
        assert_eq!(h.text, "// fn main() {\n    \n}\nlet x = 1;");
        h.typ("a");
        assert_eq!(h.text, "// fn main() {\n    a\n}\nlet x = 1;");
        h.key(Key::ArrowDown, NONE);
        h.key(Key::ArrowDown, NONE);
        h.key(Key::Home, NONE);
        h.key(Key::End, egui::Modifiers::SHIFT);
        h.frame(vec![Event::Copy]);
        assert_eq!(h.clipboard.as_deref(), Some("let x = 1;"));
        h.key(Key::Backspace, NONE);
        assert_eq!(h.text, "// fn main() {\n    a\n}\n");
        h.key(Key::Z, egui::Modifiers::COMMAND);
        assert_eq!(h.text, "// fn main() {\n    a\n}\nlet x = 1;");
        h.key(Key::Z, egui::Modifiers::COMMAND);
        h.key(Key::Z, egui::Modifiers::COMMAND);
        h.key(Key::Z, egui::Modifiers::COMMAND);
        assert_eq!(h.text, "fn main() {}\nlet x = 1;");
        assert!(h.version > 0);
    }

    #[test]
    fn multi_cursor_add_next_occurrence_and_paste() {
        let mut h = Harness::new("foo bar foo baz foo");
        h.key(Key::D, egui::Modifiers::COMMAND); // select word under caret
        h.key(Key::D, egui::Modifiers::COMMAND);
        h.key(Key::D, egui::Modifiers::COMMAND);
        assert_eq!(h.st.sels.len(), 3);
        h.typ("x");
        assert_eq!(h.text, "x bar x baz x");
        h.frame(vec![Event::Paste("1\n2\n3".into())]);
        assert_eq!(h.text, "x1 bar x2 baz x3");
        h.key(Key::Escape, NONE);
        assert_eq!(h.st.sels.len(), 1);
        h.key(Key::Slash, egui::Modifiers::COMMAND);
        assert_eq!(h.text, "// x1 bar x2 baz x3");
    }

    #[test]
    fn ime_commit_inserts_text_and_reports_caret() {
        let mut h = Harness::new("");
        h.frame(vec![Event::Ime(egui::ImeEvent::Preedit {
            text: "ni".into(),
            active_range_chars: None,
        })]);
        assert_eq!(h.st.preedit.as_deref(), Some("ni"));
        let out = h.frame(vec![Event::Ime(egui::ImeEvent::Commit("你好".into()))]);
        assert_eq!(h.text, "你好");
        assert!(h.st.preedit.is_none() && out.caret_rect.is_some());
        h.key(Key::ArrowLeft, NONE);
        h.key(Key::Backspace, NONE);
        assert_eq!(h.text, "好");
    }

    #[test]
    fn indent_detection_and_comment_tokens() {
        assert_eq!(detect_indent("fn a() {\n  b();\n  if x {\n    c();\n  }\n}", 8), "  ");
        assert_eq!(detect_indent("fn a() {\n    b();\n}", 8), "    ");
        assert_eq!(detect_indent("a:\n\tb\n\tc", 8), "\t");
        // No indentation to go by: the tab size setting.
        assert_eq!(detect_indent("a\nb\n", 2), "  ");
        assert_eq!(comment_token("rs"), Some("//"));
        assert_eq!(comment_token("py"), Some("#"));
        assert_eq!(comment_token("md"), None);
    }

    #[test]
    fn toggle_comment_and_indent_lines() {
        let mut text = String::from("fn a() {\n    b();\n    c();\n}");
        let mut st = State::default();
        let ix = LineIndex::new(&text);
        st.sels = vec![Sel { anchor: 13, head: 25 }];
        assert!(toggle_comment(&mut st, &mut text, &ix, "//"));
        assert_eq!(text, "fn a() {\n    // b();\n    // c();\n}");
        let ix = LineIndex::new(&text);
        assert!(toggle_comment(&mut st, &mut text, &ix, "//"));
        assert_eq!(text, "fn a() {\n    b();\n    c();\n}");
        let ix = LineIndex::new(&text);
        st.sels = vec![Sel {
            anchor: 0,
            head: text.len(),
        }];
        assert!(indent_lines(&mut st, &mut text, &ix, "  ", true));
        assert_eq!(text, "  fn a() {\n      b();\n      c();\n  }");
        let ix = LineIndex::new(&text);
        assert!(indent_lines(&mut st, &mut text, &ix, "  ", false));
        assert_eq!(text, "fn a() {\n    b();\n    c();\n}");
        assert_eq!(soft_tab_back("        x", &LineIndex::new("        x"), 6, "    "), Some(4));
    }
}
