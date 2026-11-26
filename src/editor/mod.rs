//! The code editor widget. Unlike `egui::TextEdit` it lays out and paints only the
//! visible lines (each line is its own small galley, cached by egui), so frame cost
//! doesn't grow with file size, and it supports multiple cursors natively.
//!
//! The text stays a plain `String` owned by the caller (the rest of the app reads it);
//! positions are byte offsets (see [`buffer`]).

pub mod buffer;
pub mod conflicts;
mod edit_ops;
pub mod fold;
pub mod highlights;
mod navigation_menu;
pub(crate) mod sticky;

use crate::highlight;
use crate::theme::{Palette, Part};
use buffer::{History, Kind, LineIndex, Sel};
use eframe::egui::{self, Color32, Event, Key, Pos2, Rect, Sense, Stroke, Vec2, text::CCursor, text::LayoutJob};
use std::collections::BTreeSet;
use std::ops::Range;
use std::sync::Arc;

pub use edit_ops::affix_lines;

/// Editing and navigation commands that also have editor shortcuts (the app runs them
/// from the command palette with [`State::run`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Duplicate,
    MoveLinesUp,
    MoveLinesDown,
    CopyLinesUp,
    CopyLinesDown,
    DeleteLines,
    JoinLines,
    AddNextOccurrence,
    RemoveLastOccurrence,
    ToggleBookmark,
    NextBookmark,
    PreviousBookmark,
    ClearBookmarks,
    NextConflict,
    PreviousConflict,
    AcceptCurrent,
    AcceptIncoming,
    AcceptBoth,
}

impl Action {
    /// Changes the text (not available in read-only editors).
    fn edits(self) -> bool {
        use Action::*;
        matches!(
            self,
            Duplicate
                | MoveLinesUp
                | MoveLinesDown
                | CopyLinesUp
                | CopyLinesDown
                | DeleteLines
                | JoinLines
                | AcceptCurrent
                | AcceptIncoming
                | AcceptBoth
        )
    }
}

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
    /// The rows shown: `base_view` (folds) with word wrap applied when it is on.
    view: Arc<fold::View>,
    base_view: Arc<fold::View>,
    wrap: WrapCache,
    sticky: sticky::Cache,
    /// (version, folded) the regions/view were computed for.
    view_key: Option<(u64, Vec<usize>)>,
    /// Vertical scroll offset to apply next frame (minimap clicks).
    scroll_to: Option<f32>,
    /// Line (fractional) to put at the top next frame (`scroll_to_line`).
    scroll_line: Option<f32>,
    minimap_drag: bool,
    /// Lines (trimmed text) the user hid from sticky scroll in this file.
    sticky_hidden: BTreeSet<String>,
    /// A scroll bar arrow is held: when it scrolls another line.
    arrow_repeat: Option<f64>,
    /// Bracket nesting depth at the start of each line, for a version of the text.
    depth: (Option<u64>, Arc<Vec<u32>>),
    /// Bookmarked lines, as the byte offsets of their starts (kept on their text through edits).
    bookmarks: Vec<usize>,
    /// Merge conflicts, for a version of the text.
    conflicts: (Option<u64>, Arc<Vec<conflicts::Conflict>>),
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
        self.base_view = Arc::new(fold::View::new(ix.lines(), &self.regions, &self.folded));
        self.view = self.base_view.clone();
        self.wrap.applied = None;
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

    /// Run an editing or navigation action; bumps `version` when the text changed.
    pub fn run(&mut self, text: &mut String, version: &mut u64, action: Action) -> bool {
        let changed = run_action(self, text, *version, action);
        if changed {
            *version += 1;
        }
        self.reveal.get_or_insert(false);
        self.preferred_x = None;
        changed
    }

    /// Lines the selections touch (what line commands act on).
    pub fn selected_line_count(&mut self, text: &str, version: u64) -> usize {
        let ix = self.index_arc(text, version);
        selected_lines(self, &ix).len()
    }

    /// Bookmarked lines (zero-based), in order.
    pub fn bookmark_lines(&mut self, text: &str, version: u64) -> Vec<usize> {
        self.sync_bookmarks(text, version);
        let ix = self.index_arc(text, version);
        self.bookmarks.iter().map(|&b| ix.line_of_byte(b)).collect()
    }

    /// Move the bookmarks through the edits made since the last call, onto line starts.
    fn sync_bookmarks(&mut self, text: &str, version: u64) {
        let (log, _) = self.history.take_log();
        if self.bookmarks.is_empty() {
            return;
        }
        for b in &mut self.bookmarks {
            *b = buffer::map_through(&log, *b).min(text.len());
        }
        let ix = self.index_arc(text, version);
        for b in &mut self.bookmarks {
            *b = ix.line_start(ix.line_of_byte(*b));
        }
        self.bookmarks.sort_unstable();
        self.bookmarks.dedup();
    }

    fn conflicts(&mut self, text: &str, version: u64) -> Arc<Vec<conflicts::Conflict>> {
        if self.conflicts.0 != Some(version) {
            let ix = self.index_arc(text, version);
            self.conflicts = (Some(version), Arc::new(conflicts::find(text, &ix)));
        }
        self.conflicts.1.clone()
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

/// Word wrap: display rows per line for one wrap width and font, and the line texts
/// already measured (so an edit only lays out the lines it changed).
#[derive(Default)]
struct WrapCache {
    /// (base view, wrap width, font) the current `State::view` was wrapped for.
    applied: Option<(usize, u32, u64)>,
    /// Wrap width and font `rows` were measured with.
    measured: Option<(u32, u64)>,
    /// Rows by hash of a line's text.
    rows: std::collections::HashMap<u64, u32>,
    /// Wrap width used for the last layout (keyboard moves use it before the next layout).
    width: Option<f32>,
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
    /// A divider line below the pinned sticky scroll lines.
    pub sticky_line: bool,
    /// Kinds of lines sticky scroll may pin (control flow ones only with `sticky_control_flow`).
    pub sticky_shown: sticky::Kinds,
    pub navigation_menu: bool,
    /// Wrap long lines at the editor's width (no horizontal scrolling).
    pub wrap: bool,
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
    /// Icon and tooltip when each marker is an action button.
    pub gutter_button: Option<(&'static str, &'static str)>,
    /// Full-width line backgrounds (e.g. changed lines in a diff).
    pub line_bg: &'a [(usize, Color32)],
    /// Other uses of the symbol under the caret (byte ranges; `true`: written there).
    pub symbol_marks: &'a [(Range<usize>, bool)],
    /// Sorted ranges and color roles from the language server, per line.
    pub semantic: &'a [highlight::TokenLine],
    /// Terms the user highlighted (Ctrl/Cmd+Shift+H), each occurrence in its own color.
    pub highlights: &'a [highlights::Highlight],
}

#[derive(Default)]
pub struct Output {
    pub changed: bool,
    pub gutter_click: Option<(usize, bool, Rect)>,
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

    /// Height of line `l` (several rows when it wraps).
    fn line_h(&self, l: usize) -> f32 {
        self.view.rows_of(l) as f32 * self.row_h
    }

    /// The line under screen position `p`.
    fn line_at(&self, p: Pos2) -> usize {
        self.view.line(((p.y - self.top) / self.row_h).floor().max(0.0) as usize)
    }

    /// The byte offset closest to screen position `p`.
    fn byte_at(&self, lines: &Lines, p: Pos2) -> usize {
        let l = self.line_at(p);
        lines.byte_at(l, Vec2::new(p.x - self.text_x, p.y - self.line_y(l)))
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
    /// Word wrap width (infinite: no wrap).
    wrap: f32,
}

impl Lines<'_> {
    fn galley(&self, l: usize) -> Arc<egui::Galley> {
        self.galley_wrapped(l, self.wrap)
    }

    /// Line `l` on one row, even with word wrap (sticky scroll headers).
    fn galley_unwrapped(&self, l: usize) -> Arc<egui::Galley> {
        self.galley_wrapped(l, f32::INFINITY)
    }

    fn galley_wrapped(&self, l: usize, wrap: f32) -> Arc<egui::Galley> {
        let brackets = match (self.depth, self.spans.and_then(|s| s.get(l))) {
            (Some((depth, colors)), Some(hl)) if !hl.brackets.is_empty() => Some(Brackets {
                depth: depth.get(l).copied().unwrap_or(0),
                marks: &hl.brackets,
                colors,
            }),
            _ => None,
        };
        let semantic = self.semantic.get(l).map_or(&[][..], Vec::as_slice);
        let mut job = line_job(self.text, self.ix, l, self.spans, semantic, &self.font, self.palette, brackets);
        job.wrap.max_width = wrap;
        // The layout cache hashes TextFormat, so palette changes invalidate affected galleys
        // while syntax and semantic token ranges remain reusable.
        self.ui.fonts_mut(|f| f.layout_job(job))
    }

    /// Position of byte offset `b` relative to the start of its line: x, and the top of its row.
    fn pos_of(&self, b: usize) -> Vec2 {
        let (l, col) = self.ix.line_col(b);
        let range = self.ix.line_range(self.text, l);
        let col = col.min(range.len());
        let chars = self.text[range.start..range.start + col].chars().count();
        self.galley(l).pos_from_cursor(CCursor::new(chars)).min.to_vec2()
    }

    /// Byte offset on line `l` closest to `pos` (relative to the line's top left).
    fn byte_at(&self, l: usize, pos: Vec2) -> usize {
        let range = self.ix.line_range(self.text, l);
        let g = self.galley(l);
        let c = g.cursor_from_pos(pos).index.0;
        let line = &self.text[range.clone()];
        range.start + line.char_indices().nth(c).map_or(line.len(), |(i, _)| i)
    }

    /// Byte offset on the first row of line `l` closest to x (relative to text start).
    fn byte_at_x(&self, l: usize, x: f32) -> usize {
        self.byte_at(l, Vec2::new(x, 0.0))
    }
}

/// End of a line's text in its galley: x after the last char, and the top of the last row.
fn line_end_pos(g: &egui::Galley) -> Vec2 {
    g.rows.last().map_or(Vec2::ZERO, |row| Vec2::new(row.pos.x + row.size.x, row.pos.y))
}

/// Display rows per line for word wrap at `width`, applied to `st.base_view` as `st.view`.
/// Only lines that could reach the width are laid out, and each line text once per width.
fn wrap_view(st: &mut State, ui: &egui::Ui, text: &str, ix: &LineIndex, font: &egui::FontId, width: f32, char_w: f32) {
    use std::hash::{Hash, Hasher};
    let font_key = {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        font.hash(&mut h);
        char_w.to_bits().hash(&mut h);
        h.finish()
    };
    let key = (Arc::as_ptr(&st.base_view) as usize, width.to_bits(), font_key);
    if st.wrap.applied == Some(key) {
        return;
    }
    if st.wrap.measured != Some((width.to_bits(), font_key)) || st.wrap.rows.len() > 200_000 {
        st.wrap.rows.clear();
        st.wrap.measured = Some((width.to_bits(), font_key));
    }
    let tab_w = ui.fonts_mut(|f| f.glyph_width(font, '\t')).max(char_w);
    let palette = Palette::defaults(true);
    let lines = Lines {
        ui,
        text,
        ix,
        spans: None,
        semantic: &[],
        font: font.clone(),
        palette: &palette,
        depth: None,
        wrap: width,
    };
    let mut cache = std::mem::take(&mut st.wrap.rows);
    let rows_of = |l: usize| {
        let line = &text[ix.line_range(text, l)];
        // Monospace: an ASCII line's width is known without laying it out; other chars are at most two columns.
        let fits = if line.is_ascii() {
            let tabs = line.bytes().filter(|&b| b == b'\t').count();
            (line.len() - tabs) as f32 * char_w + tabs as f32 * tab_w.max(char_w) <= width
        } else {
            line.chars().count() as f32 * 2.0 * char_w.max(tab_w) <= width
        };
        if fits {
            return 1;
        }
        let mut h = std::collections::hash_map::DefaultHasher::new();
        line.hash(&mut h);
        *cache.entry(h.finish()).or_insert_with(|| lines.galley(l).rows.len().max(1) as u32) as usize
    };
    let view = st.base_view.wrapped(rows_of);
    st.wrap.rows = cache;
    st.view = Arc::new(view);
    st.wrap.applied = Some(key);
}

/// Rectangles (relative to the line's top left: x0, x1, row top) covering chars `a..b` of a
/// line's galley, one per wrapped row they touch.
fn char_spans(g: &egui::Galley, a: usize, b: usize) -> Vec<(f32, f32, f32)> {
    let mut out = Vec::new();
    let mut start = 0;
    let last = g.rows.len().saturating_sub(1);
    for (i, row) in g.rows.iter().enumerate() {
        let n = row.char_count_excluding_newline().0;
        let end = start + n;
        // An empty range at a wrap point goes on the next row (the last one at the line's end).
        let overlaps = a < end && b > start;
        let empty_here = a == b && (start..=end).contains(&a) && (a < end || i == last);
        if overlaps || empty_here {
            let x0 = row.pos.x + row.x_offset(egui::text::CharIndex(a.saturating_sub(start).min(n)));
            let x1 = row.pos.x + row.x_offset(egui::text::CharIndex(b.saturating_sub(start).min(n)));
            out.push((x0, x1, row.pos.y));
        }
        start = end + usize::from(row.ends_with_newline);
    }
    out
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
    let band_dy = band_offset(ui, &font, row_h);
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
            row_h,
            is_mac,
            viewport_rows,
            opts,
        };
        // Option+J types "∆" on macOS right after the key event: skip that text.
        let mut skip_text = false;
        for ev in events {
            if std::mem::take(&mut skip_text) && matches!(ev, Event::Text(_)) {
                continue;
            }
            if let Event::Key {
                key,
                pressed: true,
                modifiers,
                ..
            } = &ev
            {
                skip_text = modifiers.alt && !modifiers.command && shortcut_action(*key, *modifiers).is_some();
            }
            if handle_event(ui, st, text, version, &ctx, &ev) {
                out.changed = true;
            }
        }
    }

    // ---- layout
    st.index(text, *version);
    st.sync_bookmarks(text, *version);
    st.update_view(text, *version);
    st.unfold_carets();
    st.update_view(text, *version);
    let ix = st.index_arc(text, *version);
    if opts.sticky_scroll {
        let kinds = sticky::Kinds::pinned(opts.sticky_shown, opts.sticky_control_flow);
        st.sticky.update(*version, text, &ix, &st.regions, kinds);
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
    let full = ui.available_rect_before_wrap();
    let mini_w = if opts.minimap && full.width() > 400.0 { 80.0 } else { 0.0 };
    // Word wrap: lines break before the vertical scroll bar (at least 20 columns wide).
    let wrap = opts.wrap.then(|| {
        let bar = ui.spacing().scroll.bar_width.max(14.0) + ui.spacing().scroll.bar_outer_margin;
        (full.width() - mini_w - gutter_w - bar - char_w).max(20.0 * char_w)
    });
    st.wrap.width = wrap;
    match wrap {
        Some(width) => wrap_view(st, ui, text, &ix, &font, width, char_w),
        None => st.view = st.base_view.clone(),
    }
    let view = st.view.clone();
    let found_conflicts = st.conflicts(text, *version);
    let bookmarked: BTreeSet<usize> = st.bookmarks.iter().map(|&b| ix.line_of_byte(b)).collect();
    let mut resolve: Option<(conflicts::Conflict, conflicts::Choice)> = None;
    let content = Vec2::new(
        match wrap {
            Some(width) => gutter_w + width,
            None => gutter_w + (ix.max_line_bytes as f32 + 8.0) * line_char_w + 200.0,
        },
        // Let the last line scroll up to the middle of the view.
        (view.count() as f32) * row_h + ui.available_height() * 0.5,
    );
    let text_color = palette.color(Part::Foreground);

    // The minimap takes a strip on the right.
    ui.painter().rect_filled(full, 0.0, palette.color(Part::Background));
    let main_rect = Rect::from_min_max(full.min, Pos2::new(full.max.x - mini_w, full.max.y));
    let mini_rect = Rect::from_min_max(Pos2::new(full.max.x - mini_w, full.min.y), full.max);
    let mut main_ui = ui.new_child(egui::UiBuilder::new().max_rect(main_rect));
    let mut area = egui::ScrollArea::new([wrap.is_none(), true])
        .id_salt(id.with("scroll"))
        .auto_shrink([false, false]);
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
            wrap: wrap.unwrap_or(f32::INFINITY),
        };
        let visible_rows = (vp.min.y / row_h).floor().max(0.0) as usize..((vp.max.y / row_h).ceil() as usize + 1).min(view.count());
        let mut vis_lines: Vec<usize> = visible_rows.clone().map(|r| view.line(r)).collect();
        vis_lines.dedup(); // a wrapped line spans several rows
        // On screen: a row of the line is in the visible rows.
        let shown = |l: usize| !view.is_hidden(l) && view.row(l) < visible_rows.end && view.row(l) + view.rows_of(l) > visible_rows.start;
        let clip = Rect::from_min_max(
            Pos2::new(rect.min.x + vp.min.x + gutter_w, rect.min.y + vp.min.y),
            rect.min + vp.max.to_vec2(),
        );

        let sticky = opts.sticky_scroll.then(|| {
            let viewport = vp.translate(rect.min.to_vec2()).intersect(ui.clip_rect());
            let hidden = &st.sticky_hidden;
            let is_hidden = |line: usize| !hidden.is_empty() && hidden.contains(text[ix.line_range(text, line)].trim());
            sticky::Overlay::new(&st.sticky, &view, viewport, row_h, visible_rows.start, is_hidden)
        });
        if let Some(overlay) = &sticky {
            let action = overlay.interact(ui, id, &view, opts.sticky_shown, opts.sticky_control_flow, st.sticky_hidden.len());
            if let Some(offset) = action.scroll {
                st.scroll_to = Some(offset);
                resp.request_focus();
                ui.ctx().request_repaint();
            }
            if let Some(line) = action.hide {
                st.sticky_hidden.insert(text[ix.line_range(text, line)].trim().to_string());
                ui.ctx().request_repaint();
            }
            if action.show_hidden {
                st.sticky_hidden.clear();
                ui.ctx().request_repaint();
            }
            out.context_command = out.context_command.or(action.command);
        }
        let over_sticky = sticky.as_ref().is_some_and(|overlay| overlay.contains_pointer(ui));
        if opts.navigation_menu && (!over_sticky || resp.context_menu_opened()) {
            // Don't drop a command the pinned lines' menu gave this frame (its menu can open below them).
            if let Some(command) = navigation_menu::show(&resp, st, &lines, &lay, *version, extras.highlights) {
                out.context_command = Some(command);
            }
        }

        let marker_x = clip.min.x - 0.9 * char_w;
        let button = extras.gutter_button;
        let marker_hit = |line: usize, deleted: bool| {
            let y = lay.line_y(if button.is_some() { line.min(ix.lines() - 1) } else { line });
            let (top, bottom) = if deleted && button.is_none() {
                (y - 4.0, y + 4.0)
            } else {
                (y, y + row_h)
            };
            Rect::from_min_max(Pos2::new(marker_x - 3.0, top), Pos2::new(clip.min.x, bottom)).intersect(ui.clip_rect())
        };
        let marker_visible = |line: usize, deleted: bool| shown(line) || (deleted && line == ix.lines() && shown(line.saturating_sub(1)));
        let mut over_marker = false;
        let mut hovered_marker = None;
        let first = vis_lines.first().copied().unwrap_or(0);
        let end = vis_lines.last().copied().unwrap_or(0).saturating_add(1);
        let markers = if out.changed {
            &[][..]
        } else {
            &extras.gutter[extras.gutter.partition_point(|m| m.0 < first)..extras.gutter.partition_point(|m| m.0 <= end)]
        };
        if !over_sticky && !opts.read_only {
            for &(line, _, deleted) in markers {
                if !marker_visible(line, deleted) {
                    continue;
                }
                let hit = marker_hit(line, deleted);
                if !hit.is_positive() {
                    continue;
                }
                let marker = ui
                    .interact(hit, id.with(("change-marker", line, deleted)), Sense::CLICK)
                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                    .on_hover_text(button.map_or("Show change", |(_, label)| label));
                if marker.contains_pointer() {
                    over_marker = true;
                    hovered_marker = Some(line);
                }
                if marker.clicked() {
                    resp.request_focus();
                    out.gutter_click = Some((line, deleted, hit));
                }
            }
        }
        // A drag started in the text can still finish over the gutter or a pinned header.
        let ptr = if (over_sticky || over_marker) && st.drag.is_none() && st.goto_press.is_none() {
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
                let y = lay.line_y(l) + band_dy;
                let band = Rect::from_min_max(Pos2::new(clip.min.x - gutter_w, y), Pos2::new(clip.max.x, y + lay.line_h(l)));
                ui.painter().with_clip_rect(ui.clip_rect()).rect_filled(band, 0.0, color);
            }
        }
        // Merge conflicts: ours in green, theirs in blue, the marker lines stronger.
        if !found_conflicts.is_empty() {
            let (green, blue) = (Color32::from_rgb(0x2e, 0xa0, 0x43), Color32::from_rgb(0x3d, 0x8b, 0xfd));
            let gray = ui.visuals().weak_text_color();
            let (strong, faint) = if ui.visuals().dark_mode { (0.32, 0.14) } else { (0.26, 0.10) };
            let first = vis_lines.first().copied().unwrap_or(0);
            let last = vis_lines.last().copied().unwrap_or(0);
            for c in found_conflicts.iter().filter(|c| c.end >= first && c.start <= last) {
                for &l in vis_lines.iter().filter(|&&l| c.contains(l)) {
                    let color = if l == c.start {
                        green.gamma_multiply(strong)
                    } else if l == c.end {
                        blue.gamma_multiply(strong)
                    } else if l == c.mid || Some(l) == c.base {
                        gray.gamma_multiply(faint)
                    } else if l < c.base.unwrap_or(c.mid) {
                        green.gamma_multiply(faint)
                    } else if l < c.mid {
                        gray.gamma_multiply(faint * 0.7)
                    } else {
                        blue.gamma_multiply(faint)
                    };
                    let y = lay.line_y(l) + band_dy;
                    let band = Rect::from_min_max(Pos2::new(clip.min.x - gutter_w, y), Pos2::new(clip.max.x, y + lay.line_h(l)));
                    ui.painter().with_clip_rect(ui.clip_rect()).rect_filled(band, 0.0, color);
                }
            }
        }
        if opts.current_line && single_line && st.sels.len() == 1 && shown(pl) {
            let y = lay.line_y(pl) + band_dy;
            let band = Rect::from_min_max(Pos2::new(clip.min.x - gutter_w, y), Pos2::new(clip.max.x, y + lay.line_h(pl)));
            ui.painter()
                .with_clip_rect(ui.clip_rect())
                .rect_filled(band, 0.0, palette.color(Part::CurrentLine));
        }
        // Char index in line `l` of byte `b` (clamped to the line).
        let chars_in = |l: usize, b: usize| {
            let range = ix.line_range(text, l);
            let b = b.clamp(range.start, range.end);
            text[range.start..b].chars().count()
        };
        // Screen rects (x0, x1, row top) of the part of byte range `r` on line `l`, one per wrapped row.
        let spans_in = |l: usize, g: &egui::Galley, r: &Range<usize>| {
            let lr = ix.line_range(text, l);
            let (a, b) = (chars_in(l, r.start.max(lr.start)), chars_in(l, r.end.min(lr.end)));
            let top = lay.line_y(l);
            char_spans(g, a, b)
                .into_iter()
                .map(move |(x0, x1, y)| (lay.text_x + x0, lay.text_x + x1, top + y))
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
                    let past_eol = extend_eol && r.end > ix.line_range(text, *l).end;
                    let spans: Vec<_> = spans_in(*l, g, &r).collect();
                    let last = spans.len().saturating_sub(1);
                    for (i, (a, mut b, y)) in spans.into_iter().enumerate() {
                        if past_eol && i == last {
                            b += char_w * 0.6;
                        }
                        let y = y + band_dy;
                        painter.rect_filled(Rect::from_min_max(Pos2::new(a, y), Pos2::new(b, y + row_h)), 2.0, color);
                    }
                }
            }
        };
        let mark = ui.visuals().warn_fg_color.gamma_multiply(0.25);
        // Search-in-files results in blue, under the file's own find matches (amber).
        let global = Color32::from_rgb(0x3d, 0x8b, 0xfd).gamma_multiply(if ui.visuals().dark_mode { 0.35 } else { 0.28 });
        // Other uses of the symbol under the caret: neutral, a bit stronger where written.
        let uses = ui.visuals().text_color();
        let (read, write) = if ui.visuals().dark_mode { (0.13, 0.22) } else { (0.10, 0.17) };
        // User highlights (Ctrl/Cmd+Shift+H), searched in the visible lines only.
        for h in extras.highlights {
            let mut found = galleys.iter().flat_map(|&(l, _)| {
                let lr = ix.line_range(text, l);
                highlights::matches(&text[lr.clone()], &h.term).map(move |r| lr.start + r.start..lr.start + r.end)
            });
            paint_ranges(&mut found, highlights::color(h.color, ui.visuals().dark_mode), false);
        }
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
                let end = line_end_pos(g);
                let x = lay.text_x + end.x + char_w;
                let r = Rect::from_min_size(Pos2::new(x, lay.line_y(*l) + end.y + 2.0), Vec2::new(3.0 * char_w, row_h - 4.0));
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
                for (a, b, y) in spans_in(*l, g, r) {
                    squiggle(&painter, a, b.max(a + char_w), y + row_h - 1.5, *color);
                }
            }
        }
        // Inline blame after the primary line.
        if let Some(inline) = extras.inline
            && single_line
            && let Some((_, g)) = galleys.iter().find(|(l, _)| *l == pl)
        {
            let color = ui.visuals().weak_text_color().gamma_multiply(0.8);
            let end = line_end_pos(g);
            let p = Pos2::new(lay.text_x + end.x + 4.0 * char_w, lay.line_y(pl) + end.y + row_h / 2.0);
            painter.text(p, egui::Align2::LEFT_CENTER, inline, font.clone(), color);
        }
        // "Accept current | incoming | both" after each visible conflict's first marker.
        if !opts.read_only {
            for c in found_conflicts.iter() {
                let Some((_, g)) = galleys.iter().find(|(l, _)| *l == c.start) else {
                    continue;
                };
                let end = line_end_pos(g);
                let mut x = lay.text_x + end.x + 3.0 * char_w;
                let y = lay.line_y(c.start) + end.y;
                for (n, (label, choice)) in [
                    ("Accept current", conflicts::Choice::Current),
                    ("Accept incoming", conflicts::Choice::Incoming),
                    ("Accept both", conflicts::Choice::Both),
                ]
                .into_iter()
                .enumerate()
                {
                    let galley = ui.fonts_mut(|f| f.layout_no_wrap(label.into(), font.clone(), text_color));
                    let r = Rect::from_min_size(Pos2::new(x, y), Vec2::new(galley.size().x, row_h));
                    let button = ui.interact(r, id.with(("conflict", c.start, n)), Sense::click());
                    let color = if button.hovered() {
                        ui.visuals().hyperlink_color
                    } else {
                        ui.visuals().weak_text_color()
                    };
                    painter.galley(r.min, galley, color);
                    if button.hovered() {
                        painter.hline(r.x_range(), r.bottom() - 1.0, Stroke::new(1.0, color));
                        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                    }
                    if button.clicked() {
                        resolve = Some((*c, choice));
                        // Keep typing (and Ctrl+Z) in the editor after the click.
                        ui.memory_mut(|m| m.request_focus(id));
                    }
                    x = r.right() + 2.0 * char_w;
                }
            }
        }
        // Carets (solid, no blinking: blinking would need a repaint twice a second).
        let caret_color = ui.visuals().text_cursor.stroke.color;
        for s in &st.sels {
            let (l, _) = ix.line_col(s.head);
            if let Some((_, g)) = galleys.iter().find(|(gl, _)| *gl == l) {
                let at = g.pos_from_cursor(CCursor::new(chars_in(l, s.head))).min;
                let x = lay.text_x + at.x;
                let y = lay.line_y(l) + at.y + band_dy;
                if focused || st.sels.len() > 1 {
                    painter.line_segment([Pos2::new(x, y), Pos2::new(x, y + row_h)], Stroke::new(2.0, caret_color));
                }
            }
        }
        // IME pre-edit text at the caret.
        let caret_at = lines.pos_of(primary.head);
        let caret_rect = Rect::from_min_size(
            Pos2::new(lay.text_x + caret_at.x, lay.line_y(pl) + caret_at.y),
            Vec2::new(2.0, row_h),
        );
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
            if bookmarked.contains(&l) {
                // Bookmark: a pill behind the line number.
                let r = Rect::from_min_max(
                    Pos2::new(gx + fold_w, y + band_dy + 1.0),
                    Pos2::new(gx + gutter_w - 1.1 * char_w, y + band_dy + row_h - 1.0),
                );
                gp.rect_filled(r, row_h / 2.0, palette.color(Part::Selection));
            }
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
        for &(l, color, deleted) in markers {
            if !marker_visible(l, deleted) {
                continue;
            }
            let x = gx + gutter_w - 0.9 * char_w;
            let y = lay.line_y(l);
            if let Some((icon, _)) = button {
                let hit = marker_hit(l, deleted);
                if hovered_marker == Some(l) {
                    gp.rect_filled(hit, 2.0, ui.visuals().widgets.hovered.bg_fill);
                }
                gp.text(
                    hit.center(),
                    egui::Align2::CENTER_CENTER,
                    icon,
                    egui::FontId::proportional(12.0),
                    color,
                );
            } else if deleted {
                let tri = vec![Pos2::new(x, y - 4.0), Pos2::new(x + 5.0, y), Pos2::new(x, y + 4.0)];
                gp.add(egui::Shape::convex_polygon(tri, color, Stroke::NONE));
            } else {
                gp.rect_filled(Rect::from_min_size(Pos2::new(x, y), Vec2::new(3.0, lay.line_h(l))), 0.0, color);
            }
        }

        if let Some(overlay) = &sticky {
            overlay.paint(&lines, lay.text_x, gutter_w, char_w, opts.sticky_line);
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
    if let Some((conflict, choice)) = resolve {
        let ix = st.index_arc(text, *version);
        let (range, with) = conflict.resolve(text, &ix, choice);
        st.replace(text, range.clone(), &with);
        st.sels = vec![Sel::caret(range.start)];
        st.history.seal();
        *version += 1;
        out.changed = true;
    }
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
        if view.row(l) != row {
            continue; // a wrapped line's later rows
        }
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

/// How far to move row-high backgrounds (selection, marks, current line, caret) so they
/// are centered on capital letters: fonts with a large descent (e.g. Consolas) sit high
/// in egui's row, so a band covering the whole row looks shifted down. Measured from the
/// rendered glyph, so it follows the chosen font and size.
fn band_offset(ui: &egui::Ui, font: &egui::FontId, row_h: f32) -> f32 {
    let g = ui.fonts_mut(|f| f.layout_no_wrap("H".into(), font.clone(), Color32::WHITE));
    let (mut top, mut bottom) = (f32::INFINITY, f32::NEG_INFINITY);
    for placed in &g.rows {
        for glyph in &placed.row.glyphs {
            if glyph.uv_rect.size.y > 0.0 {
                let y = placed.pos.y + glyph.pos.y + glyph.uv_rect.offset.y;
                top = top.min(y);
                bottom = bottom.max(y + glyph.uv_rect.size.y);
            }
        }
    }
    if !top.is_finite() {
        return 0.0;
    }
    let dy = (top + bottom) / 2.0 - row_h / 2.0;
    // Never more than the gap that causes it.
    let ppp = ui.ctx().pixels_per_point();
    (dy.clamp(-row_h * 0.25, row_h * 0.25) * ppp).round() / ppp
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
                paint_tab_arrow(p, x0, x1, y, stroke);
            }
        }
    }
}

/// The "show whitespace" mark of a tab spanning `x0..x1`: an arrow on the row's middle `y`.
pub(crate) fn paint_tab_arrow(p: &egui::Painter, x0: f32, x1: f32, y: f32, stroke: Stroke) {
    let (a, b) = (x0 + 2.0, (x1 - 2.0).max(x0 + 6.0));
    p.line_segment([Pos2::new(a, y), Pos2::new(b, y)], stroke);
    p.line_segment([Pos2::new(b - 3.0, y - 3.0), Pos2::new(b, y)], stroke);
    p.line_segment([Pos2::new(b - 3.0, y + 3.0), Pos2::new(b, y)], stroke);
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
    let to_line = |p: Pos2| lay.line_at(p);
    let to_byte = |p: Pos2| lay.byte_at(lines, p);
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
                    let (a, z) = (lines.pos_of(w.start), lines.pos_of(w.end.max(w.start)));
                    let (a, z) = (a.x, if z.y == a.y { z.x } else { a.x + lay.row_h });
                    let y = lay.line_y(l) + lines.pos_of(w.start).y + lay.row_h - 1.0;
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
    row_h: f32,
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
    if let Some(action) = shortcut_action(key, m) {
        if k.opts.read_only && action.edits() {
            return false;
        }
        return run_action(st, text, *version, action);
    }
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
        // The width of the last layout (wrapped rows are what Up / Down move between).
        wrap: st.wrap.width.filter(|_| k.opts.wrap).unwrap_or(f32::INFINITY),
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
            let at = lines.pos_of(s.head);
            let x = px.unwrap_or(at.x);
            new_x = Some(x);
            // Display rows: with word wrap a line can take several.
            let sub_row = (at.y / k.row_h).round() as isize;
            let target = view.row(l) as isize + sub_row + rows;
            if target < 0 {
                0
            } else if target as usize >= view.count() {
                text.len()
            } else {
                let line = view.line(target as usize);
                let y = (target as usize - view.row(line)) as f32 * k.row_h + k.row_h / 2.0;
                lines.byte_at(line, Vec2::new(x, y))
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

/// Returns true when the text changed (the caller bumps the version).
fn run_action(st: &mut State, text: &mut String, version: u64, action: Action) -> bool {
    use conflicts::Choice;
    let ix = st.index_arc(text, version);
    let caret_line = ix.line_of_byte(st.primary().head);
    let go_to = |st: &mut State, at: usize| {
        st.sels = vec![Sel::caret(at)];
        st.reveal = Some(true);
        st.history.seal();
    };
    match action {
        Action::Duplicate => edit_ops::duplicate(st, text, &ix),
        Action::MoveLinesUp => edit_ops::move_lines(st, text, &ix, false),
        Action::MoveLinesDown => edit_ops::move_lines(st, text, &ix, true),
        Action::CopyLinesUp => edit_ops::copy_lines(st, text, &ix, false),
        Action::CopyLinesDown => edit_ops::copy_lines(st, text, &ix, true),
        Action::DeleteLines => edit_ops::delete_lines(st, text, &ix),
        Action::JoinLines => edit_ops::join_lines(st, text, &ix),
        Action::AddNextOccurrence => {
            add_next_occurrence(st, text, &ix);
            false
        }
        Action::RemoveLastOccurrence => {
            edit_ops::remove_last_occurrence(st);
            false
        }
        Action::ToggleBookmark => {
            st.sync_bookmarks(text, version);
            let mut starts: Vec<usize> = st.sels.iter().map(|s| ix.line_start(ix.line_of_byte(s.head))).collect();
            starts.sort_unstable();
            starts.dedup();
            // Toggle as a group: if any line has no bookmark, bookmark them all.
            if starts.iter().all(|b| st.bookmarks.contains(b)) {
                st.bookmarks.retain(|b| !starts.contains(b));
            } else {
                st.bookmarks.extend(starts);
                st.bookmarks.sort_unstable();
                st.bookmarks.dedup();
            }
            false
        }
        Action::NextBookmark | Action::PreviousBookmark => {
            st.sync_bookmarks(text, version);
            let here = ix.line_start(caret_line);
            let next = if action == Action::NextBookmark {
                st.bookmarks.iter().find(|&&b| b > here).or(st.bookmarks.first())
            } else {
                st.bookmarks.iter().rev().find(|&&b| b < here).or(st.bookmarks.last())
            };
            if let Some(&b) = next {
                go_to(st, b);
            }
            false
        }
        Action::ClearBookmarks => {
            st.bookmarks.clear();
            false
        }
        Action::NextConflict | Action::PreviousConflict => {
            let found = st.conflicts(text, version);
            let next = if action == Action::NextConflict {
                found.iter().find(|c| c.start > caret_line).or(found.first())
            } else {
                found.iter().rev().find(|c| c.start < caret_line).or(found.last())
            };
            if let Some(c) = next {
                go_to(st, ix.line_start(c.start));
            }
            false
        }
        Action::AcceptCurrent | Action::AcceptIncoming | Action::AcceptBoth => {
            let choice = match action {
                Action::AcceptCurrent => Choice::Current,
                Action::AcceptIncoming => Choice::Incoming,
                _ => Choice::Both,
            };
            let found = st.conflicts(text, version);
            let Some(c) = found.iter().find(|c| c.contains(caret_line)) else {
                return false;
            };
            let (range, with) = c.resolve(text, &ix, choice);
            st.replace(text, range.clone(), &with);
            st.sels = vec![Sel::caret(range.start)];
            st.history.seal();
            true
        }
    }
}

/// The action an editor shortcut runs, if any.
fn shortcut_action(key: Key, m: egui::Modifiers) -> Option<Action> {
    let (shift, alt, command) = (m.shift, m.alt, m.command);
    Some(match key {
        Key::D if command && !shift && !alt => Action::Duplicate,
        Key::J if alt && !command => {
            if shift {
                Action::RemoveLastOccurrence
            } else {
                Action::AddNextOccurrence
            }
        }
        Key::ArrowUp if alt && !command => {
            if shift {
                Action::CopyLinesUp
            } else {
                Action::MoveLinesUp
            }
        }
        Key::ArrowDown if alt && !command => {
            if shift {
                Action::CopyLinesDown
            } else {
                Action::MoveLinesDown
            }
        }
        Key::K if command && shift && !alt => Action::DeleteLines,
        Key::J if command && !shift && !alt => Action::JoinLines,
        Key::F2 if command && !shift && !alt => Action::ToggleBookmark,
        Key::PageDown if alt && !command && !shift => Action::NextBookmark,
        Key::PageUp if alt && !command && !shift => Action::PreviousBookmark,
        _ => return None,
    })
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
        gutter: Vec<(usize, Color32, bool)>,
        gutter_button: Option<(&'static str, &'static str)>,
        /// Input time, advanced 0.1 s per frame (None: no time, as before), for hover animations.
        clock: Option<f64>,
        wrap: bool,
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
                gutter: Vec::new(),
                gutter_button: None,
                clock: None,
                wrap: false,
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
            let (st, text, version, hl, id, read_only, wrap) = (
                &mut self.st,
                &mut self.text,
                &mut self.version,
                &mut self.hl,
                self.id,
                self.read_only,
                self.wrap,
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
                    sticky_line: true,
                    wrap,
                    sticky_shown: sticky::Kinds::ALL,
                    navigation_menu: false,
                    read_only,
                };
                let extras = Extras {
                    gutter: &self.gutter,
                    gutter_button: self.gutter_button,
                    ..Default::default()
                };
                out = show(ui, id, st, text, version, hl, &opts, &extras);
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
    fn clicks_gutter_changes_without_moving_the_caret() {
        for button in [
            None,
            Some((crate::icons::PLUS, "Stage hunk")),
            Some((crate::icons::ARROW_U_UP_LEFT, "Revert hunk")),
        ] {
            let mut h = Harness::new("one\ntwo\nthree");
            h.gutter = vec![(0, Color32::GREEN, false), (2, Color32::RED, true)];
            h.gutter_button = button;
            h.st.select(5..5);
            h.frame(vec![]);
            for (line, deleted) in [(0, false), (2, true)] {
                let marker = h.ctx.read_response(h.id.with(("change-marker", line, deleted))).unwrap();
                let pos = marker.rect.center();
                h.frame(vec![Event::PointerMoved(pos)]);
                for pressed in [true, false] {
                    let out = h.frame(vec![Event::PointerButton {
                        pos,
                        button: egui::PointerButton::Primary,
                        pressed,
                        modifiers: NONE,
                    }]);
                    assert_eq!(out.gutter_click.map(|(l, d, _)| (l, d)), (!pressed).then_some((line, deleted)));
                    assert_eq!(h.st.primary().head, 5);
                }
            }
        }
    }

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
        h.key(Key::J, egui::Modifiers::ALT); // select word under caret
        h.key(Key::J, egui::Modifiers::ALT);
        // macOS types "∆" with Option+J right after the key: it must not reach the text.
        h.frame(vec![
            Event::Key {
                key: Key::J,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::ALT,
            },
            Event::Text("∆".into()),
        ]);
        assert_eq!(h.st.sels.len(), 3);
        assert_eq!(h.text, "foo bar foo baz foo");
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
    fn line_editing_shortcuts() {
        let cmd = egui::Modifiers::COMMAND;
        let alt = egui::Modifiers::ALT;
        let alt_shift = alt.plus(egui::Modifiers::SHIFT);
        let mut h = Harness::new("one\ntwo\nthree");
        // Duplicate the line: the caret moves to the copy, same column.
        h.key(Key::ArrowRight, NONE);
        h.key(Key::D, cmd);
        assert_eq!(h.text, "one\none\ntwo\nthree");
        assert_eq!(h.st.primary(), Sel::caret(5));
        // Duplicate a selection: the copy gets selected.
        h.key(Key::ArrowRight, egui::Modifiers::SHIFT);
        h.key(Key::D, cmd);
        assert_eq!(h.text, "one\nonne\ntwo\nthree");
        assert_eq!(h.st.primary().range(), 6..7);
        h.key(Key::Z, cmd);
        h.key(Key::Z, cmd);
        assert_eq!(h.text, "one\ntwo\nthree");
        // Move a line down twice, then back up; the caret stays on it.
        h.st.sels = vec![Sel::caret(1)];
        h.key(Key::ArrowDown, alt);
        h.key(Key::ArrowDown, alt);
        assert_eq!(h.text, "two\nthree\none");
        assert_eq!(h.st.primary(), Sel::caret(11));
        h.key(Key::ArrowDown, alt); // already last: nothing
        assert_eq!(h.text, "two\nthree\none");
        h.key(Key::ArrowUp, alt);
        assert_eq!(h.text, "two\none\nthree");
        // Copy lines up / down.
        h.key(Key::ArrowDown, alt_shift);
        assert_eq!(h.text, "two\none\none\nthree");
        assert_eq!(h.st.primary(), Sel::caret(9));
        h.key(Key::ArrowUp, alt_shift);
        assert_eq!(h.text, "two\none\none\none\nthree");
        assert_eq!(h.st.primary(), Sel::caret(9));
        // Delete lines (the last one takes the newline before it).
        h.key(Key::K, cmd.plus(egui::Modifiers::SHIFT));
        h.key(Key::K, cmd.plus(egui::Modifiers::SHIFT));
        assert_eq!(h.text, "two\none\nthree");
        h.st.sels = vec![Sel::caret(h.text.len())];
        h.key(Key::K, cmd.plus(egui::Modifiers::SHIFT));
        assert_eq!(h.text, "two\none");
        // Join lines.
        h.st.sels = vec![Sel::caret(0)];
        h.key(Key::J, cmd);
        assert_eq!(h.text, "two one");
        // Prepend / append on the selected lines.
        h.text = "a\n\nb".into();
        h.version += 1;
        h.st.sels = vec![Sel { anchor: 0, head: 4 }];
        assert!(affix_lines(&mut h.st, &mut h.text, h.version, "- ", ";", true));
        assert_eq!(h.text, "- a;\n\n- b;");
    }

    #[test]
    fn bookmarks_follow_their_lines() {
        let mut h = Harness::new("a\nb\nc\nd");
        h.st.sels = vec![Sel::caret(4)]; // line "c"
        h.key(Key::F2, egui::Modifiers::COMMAND);
        assert_eq!(h.st.bookmark_lines(&h.text, h.version), [2]);
        // Lines inserted above move the bookmark down with its text.
        h.st.sels = vec![Sel::caret(0)];
        h.typ("x\ny\n");
        assert_eq!(h.st.bookmark_lines(&h.text, h.version), [4]);
        // Next bookmark wraps around and goes to it.
        h.st.sels = vec![Sel::caret(h.text.len())];
        h.key(Key::PageDown, egui::Modifiers::ALT);
        assert_eq!(h.st.primary(), Sel::caret(8));
        // Undo puts it back.
        h.key(Key::Z, egui::Modifiers::COMMAND);
        h.frame(vec![]);
        assert_eq!(h.st.bookmark_lines(&h.text, h.version), [2]);
        h.st.sels = vec![Sel::caret(4)];
        h.key(Key::F2, egui::Modifiers::COMMAND);
        assert!(h.st.bookmark_lines(&h.text, h.version).is_empty());
    }

    #[test]
    fn merge_conflict_buttons_and_commands_resolve() {
        let mut h = Harness::new("a\n<<<<<<< HEAD\nours\n=======\ntheirs\n>>>>>>> topic\nz\n");
        // The "Accept incoming" button on the marker line.
        let pos = h
            .ctx
            .read_response(h.id.with(("conflict", 1usize, 1usize)))
            .expect("conflict buttons shown")
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
        assert_eq!(h.text, "a\ntheirs\nz\n");
        h.key(Key::Z, egui::Modifiers::COMMAND);
        h.st.sels = vec![Sel::caret(0)];
        let (mut text, mut version) = (h.text.clone(), h.version);
        h.st.run(&mut text, &mut version, Action::NextConflict);
        assert_eq!(h.st.primary(), Sel::caret(2));
        h.st.run(&mut text, &mut version, Action::AcceptBoth);
        assert_eq!(text, "a\nours\ntheirs\nz\n");
    }

    #[test]
    fn word_wrap_breaks_long_lines_into_rows_and_moves_by_row() {
        let long = "word ".repeat(60);
        let mut h = Harness::new(&format!("{long}\nshort"));
        h.wrap = true;
        h.frame(vec![]);
        let rows = h.st.view.rows_of(0);
        assert!(rows > 1, "the long line wraps ({rows} rows)");
        assert_eq!(h.st.view.row(1), rows);
        // Down moves to the next row of the same line, not to the next line.
        h.st.sels = vec![Sel::caret(0)];
        h.key(Key::ArrowDown, NONE);
        let head = h.st.primary().head;
        assert!(head > 0 && head < long.len(), "{head}");
        // Turning wrap off goes back to one row per line.
        h.wrap = false;
        h.frame(vec![]);
        assert_eq!(h.st.view.rows_of(0), 1);
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
