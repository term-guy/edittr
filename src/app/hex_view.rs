use super::*;
use crate::hex::{self, Pair, ROW_BYTES, Source, Summary};
use std::{fmt::Write as _, io, ops::Range};

const ROW_SIZE: u64 = ROW_BYTES as u64;
// A page keeps egui's f32 scroll coordinates precise even for multi-gigabyte files.
const PAGE_ROWS: u64 = 65_536;
const WINDOW_ROWS: u64 = 128;

struct Window {
    rows: Range<u64>,
    left: Vec<u8>,
    right: Vec<u8>,
}

impl Window {
    fn row(&self, row: u64) -> Option<(&[u8], &[u8])> {
        if !self.rows.contains(&row) {
            return None;
        }
        let i = ((row - self.rows.start) * ROW_SIZE) as usize;
        let part = |len: usize| i.min(len)..(i + ROW_BYTES).min(len);
        Some((&self.left[part(self.left.len())], &self.right[part(self.right.len())]))
    }
}

pub(super) struct Viewer {
    pub path: PathBuf,
    pub title: String,
    pair: Pair,
    labels: Option<(String, String)>,
    summary: Option<Summary>,
    summary_rx: Option<Receiver<io::Result<Summary>>>,
    nav_rx: Option<Receiver<io::Result<Option<u64>>>>,
    window_rx: Option<Receiver<io::Result<Window>>>,
    copy_rx: Option<Receiver<io::Result<String>>>,
    cancel: Arc<AtomicBool>,
    window: Option<Window>,
    error: Option<String>,
    page: u64,
    top: u64,
    jump: Option<u64>,
    cursor: Option<u64>,
    selection: Option<(u64, u64, bool)>,
    dragging: bool,
}

impl Drop for Viewer {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

impl Viewer {
    pub(super) fn file(path: PathBuf, source: Source) -> Self {
        let title = format!("{} (Hex)", dir_name(&path));
        Self::new(
            path,
            title,
            Pair {
                left: source,
                right: Source::empty(),
            },
            None,
        )
    }

    pub(super) fn comparison(path: PathBuf, title: String, pair: Pair, labels: (String, String), ctx: &egui::Context) -> Self {
        let mut view = Self::new(path, title, pair, Some(labels));
        let (pair, cancel) = (view.pair.clone(), view.cancel.clone());
        view.summary_rx = Some(background(ctx, move || pair.summarize(&cancel)));
        view
    }

    fn new(path: PathBuf, title: String, pair: Pair, labels: Option<(String, String)>) -> Self {
        Self {
            path,
            title,
            pair,
            labels,
            summary: None,
            summary_rx: None,
            nav_rx: None,
            window_rx: None,
            copy_rx: None,
            cancel: Default::default(),
            window: None,
            error: None,
            page: 0,
            top: 0,
            jump: None,
            cursor: None,
            selection: None,
            dragging: false,
        }
    }

    #[cfg(feature = "tour")]
    pub(super) fn loading(&self) -> bool {
        self.summary_rx.is_some() || self.window_rx.is_some()
    }

    pub(super) fn comparing(&self) -> bool {
        self.labels.is_some()
    }

    pub(super) fn is_file(&self, path: &Path) -> bool {
        self.labels.is_none() && self.path == path
    }

    pub(super) fn size(&self) -> u64 {
        self.pair.size()
    }

    pub(super) fn go_to(&mut self, offset: u64) {
        if offset < self.size() {
            self.page = offset / ROW_SIZE / PAGE_ROWS;
            self.jump = Some(offset / ROW_SIZE);
            self.cursor = Some(offset);
        }
    }

    fn poll(&mut self, ctx: &egui::Context) {
        match receive(&mut self.summary_rx) {
            Some(Ok(summary)) => {
                self.summary = Some(summary);
                if self.cursor.is_none()
                    && let Some(first) = summary.first
                {
                    self.go_to(first);
                }
            }
            Some(Err(err)) => self.error = Some(err.to_string()),
            None => {}
        }
        match receive(&mut self.window_rx) {
            Some(Ok(window)) => self.window = Some(window),
            Some(Err(err)) => self.error = Some(err.to_string()),
            None => {}
        }
        match receive(&mut self.nav_rx) {
            Some(Ok(Some(offset))) => self.go_to(offset),
            Some(Err(err)) => self.error = Some(err.to_string()),
            _ => {}
        }
        match receive(&mut self.copy_rx) {
            Some(Ok(text)) => ctx.copy_text(text),
            Some(Err(err)) => self.error = Some(err.to_string()),
            None => {}
        }
    }

    fn request(&mut self, rows: Range<u64>, ctx: &egui::Context) {
        if rows.is_empty()
            || self.error.is_some()
            || self.window_rx.is_some()
            || self
                .window
                .as_ref()
                .is_some_and(|w| w.rows.start <= rows.start && w.rows.end >= rows.end)
        {
            return;
        }
        let rows = rows.start.saturating_sub(WINDOW_ROWS)..(rows.end + WINDOW_ROWS).min(self.size().div_ceil(ROW_SIZE));
        let pair = self.pair.clone();
        self.window_rx = Some(background(ctx, move || {
            let range = rows.start * ROW_SIZE..rows.end.saturating_mul(ROW_SIZE);
            Ok(Window {
                rows,
                left: pair.left.read(range.clone())?,
                right: pair.right.read(range)?,
            })
        }));
    }

    fn navigate(&mut self, backwards: bool, ctx: &egui::Context) {
        let Some(summary) = self.summary else { return };
        let from = self.cursor.unwrap_or(self.top.saturating_mul(ROW_SIZE));
        if !backwards && let Some(offset) = summary.first.filter(|&first| from < first) {
            self.go_to(offset);
            return;
        }
        let (pair, cancel) = (self.pair.clone(), self.cancel.clone());
        self.nav_rx = Some(background(ctx, move || pair.navigate(from, backwards, &cancel)));
    }

    fn copy(&mut self, hex: bool, ctx: &egui::Context) {
        let Some((a, b, right)) = self.selection else { return };
        let source = if right { &self.pair.right } else { &self.pair.left }.clone();
        self.copy_rx = Some(background(ctx, move || {
            let bytes = source.read(a.min(b)..a.max(b).saturating_add(1))?;
            Ok(if hex {
                hex::hex_string(&bytes)
            } else {
                String::from_utf8_lossy(&bytes).into_owned()
            })
        }));
    }

    fn page_by(&mut self, direction: i64) {
        self.page = self.page.saturating_add_signed(direction);
        self.jump = Some(self.page * PAGE_ROWS);
        self.cursor = None;
    }

    pub(super) fn show(&mut self, ui: &mut egui::Ui) {
        let total_rows = self.size().div_ceil(ROW_SIZE);
        let pages = total_rows.div_ceil(PAGE_ROWS);
        ui.horizontal_wrapped(|ui| {
            if self.labels.is_some() {
                match self.summary {
                    Some(Summary {
                        count, first: Some(first), ..
                    }) => ui.label(format!("{count} bytes differ, first difference at 0x{first:X}")),
                    Some(_) => ui.label("0 bytes differ"),
                    None => ui.weak("Scanning byte differences…"),
                };
                let from = self.cursor.unwrap_or(self.top.saturating_mul(ROW_SIZE));
                let enabled = self.nav_rx.is_none();
                let prev = enabled && self.summary.and_then(|s| s.first).is_some_and(|i| i < from);
                let next = enabled && self.summary.and_then(|s| s.last).is_some_and(|i| i > from);
                if ui
                    .add_enabled(prev, egui::Button::new(ic::ARROW_UP))
                    .on_hover_text("Previous difference")
                    .clicked()
                {
                    self.navigate(true, ui.ctx());
                }
                if ui
                    .add_enabled(next, egui::Button::new(ic::ARROW_DOWN))
                    .on_hover_text("Next difference")
                    .clicked()
                {
                    self.navigate(false, ui.ctx());
                }
            } else {
                ui.label(format!("{} bytes", self.size()));
            }
            ui.weak("Ctrl+G to go to offset");
            if pages > 1 {
                ui.separator();
                if ui
                    .add_enabled(self.page > 0, egui::Button::new(ic::CARET_LEFT))
                    .on_hover_text("Previous page")
                    .clicked()
                {
                    self.page_by(-1);
                }
                ui.label(format!("Page {} / {pages}", self.page + 1));
                if ui
                    .add_enabled(self.page + 1 < pages, egui::Button::new(ic::CARET_RIGHT))
                    .on_hover_text("Next page")
                    .clicked()
                {
                    self.page_by(1);
                }
            }
        });
        if let Some(error) = &self.error {
            ui.colored_label(ui.visuals().error_fg_color, error);
        }
        if let Some((a, b, _)) = self.selection {
            ui.horizontal_wrapped(|ui| {
                ui.label(format!("{} bytes selected", a.abs_diff(b) + 1));
                let ready = self.copy_rx.is_none();
                if ui.add_enabled(ready, egui::Button::new("Copy as hex")).clicked() {
                    self.copy(true, ui.ctx());
                }
                if ui.add_enabled(ready, egui::Button::new("Copy as text")).clicked() {
                    self.copy(false, ui.ctx());
                }
            });
        }
        ui.separator();

        let page_start = self.page * PAGE_ROWS;
        let row_count = total_rows.saturating_sub(page_start).min(PAGE_ROWS) as usize;
        let height = 18.0;
        let header = self.labels.as_ref().map(|_| {
            ui.allocate_exact_size(egui::vec2(ui.available_width(), height), egui::Sense::hover())
                .0
        });
        let mut area = egui::ScrollArea::both()
            .id_salt(("hex", &self.path, &self.title))
            .auto_shrink([false, false]);
        if let Some(row) = self.jump.take() {
            area = area.vertical_scroll_offset(row.saturating_sub(page_start) as f32 * (height + ui.spacing().item_spacing.y));
        }
        let font = egui::FontId::monospace(12.0);
        let char_width = ui.fonts_mut(|fonts| fonts.glyph_width(&font, '0'));
        let gap = if self.labels.is_some() { SIDE_GAP } else { 0 };
        let side_width = (OFFSET_WIDTH + ROW_BYTES * 4 + 3 + gap) as f32 * char_width;
        let width = side_width * if self.labels.is_some() { 2.0 } else { 1.0 };
        let (dragging, pos) = ui.input(|i| (i.pointer.primary_down(), i.pointer.interact_pos()));
        if !dragging {
            self.dragging = false;
        }
        let output = area.show_rows(ui, height, row_count, |ui, range| {
            let rows = page_start + range.start as u64..page_start + range.end as u64;
            for row in rows.clone() {
                let (rect, response) = ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::click_and_drag());
                let Some((left, right)) = self.window.as_ref().and_then(|w| w.row(row)) else {
                    ui.painter().text(
                        rect.left_center(),
                        egui::Align2::LEFT_CENTER,
                        format!("{:0OFFSET_WIDTH$X}", row * ROW_SIZE),
                        font.clone(),
                        ui.visuals().weak_text_color(),
                    );
                    continue;
                };
                let offset = row * ROW_SIZE;
                let compare = self.labels.is_some();
                let mut job = egui::text::LayoutJob::default();
                let mut diff = [false; ROW_BYTES];
                if compare {
                    for range in hex::diff_ranges(left, right) {
                        diff[range].fill(true);
                    }
                }
                for (right_side, bytes) in [(false, left), (true, right)].into_iter().take(if compare { 2 } else { 1 }) {
                    self.append_row(ui, &mut job, &font, offset, right_side, bytes, &diff);
                    if compare && !right_side {
                        job.append(
                            &" ".repeat(SIDE_GAP),
                            0.0,
                            egui::TextFormat::simple(font.clone(), Color32::TRANSPARENT),
                        );
                    }
                }
                let galley = ui.painter().layout_job(job);
                ui.painter().galley(
                    egui::pos2(rect.left(), rect.center().y - galley.size().y / 2.0),
                    galley,
                    ui.visuals().text_color(),
                );
                let can_track = response.clicked() || response.drag_started() || self.dragging && dragging;
                if can_track && let Some(pos) = pos.filter(|p| rect.contains(*p) && ui.clip_rect().contains(*p)) {
                    let x = pos.x - rect.left();
                    let right_side = compare && x >= side_width;
                    let x = if right_side { x - side_width } else { x };
                    let col = (x / char_width) as usize;
                    let ascii_start = OFFSET_WIDTH + ROW_BYTES * 3 + 3;
                    let byte = if col >= ascii_start {
                        Some(col - ascii_start)
                    } else if col >= OFFSET_WIDTH + 2 {
                        Some((col - OFFSET_WIDTH - 2) / 3)
                    } else {
                        None
                    };
                    let len = if right_side { right.len() } else { left.len() };
                    if let Some(i) = byte.filter(|&i| i < len) {
                        let byte = offset + i as u64;
                        let extend = self.dragging || ui.input(|i| i.modifiers.shift);
                        let anchor = match self.selection {
                            Some((anchor, _, side)) if extend && side == right_side => anchor,
                            _ => byte,
                        };
                        let selection = Some((anchor, byte, right_side));
                        if self.selection != selection {
                            self.selection = selection;
                            ui.ctx().request_repaint();
                        }
                        self.cursor = Some(byte);
                        self.dragging |= response.drag_started();
                    }
                }
                response.context_menu(|ui| {
                    if self.selection.is_some() && ui.button("Copy as hex").clicked() {
                        self.copy(true, ui.ctx());
                        ui.close();
                    }
                    if self.selection.is_some() && ui.button("Copy as text").clicked() {
                        self.copy(false, ui.ctx());
                        ui.close();
                    }
                });
            }
            rows
        });
        if let (Some(header), Some((left, right))) = (header, &self.labels) {
            let left_x = output.inner_rect.left() - output.state.clamped_offset().x;
            let painter = ui.painter_at(header);
            for (i, (name, size)) in [(left, self.pair.left.size), (right, self.pair.right.size)].into_iter().enumerate() {
                painter.text(
                    egui::pos2(left_x + i as f32 * side_width, header.center().y),
                    egui::Align2::LEFT_CENTER,
                    format!("{name} ({size} bytes)"),
                    font.clone(),
                    ui.visuals().strong_text_color(),
                );
            }
        }
        self.top = output.inner.start;
        self.request(output.inner, ui.ctx());
    }

    #[allow(clippy::too_many_arguments)]
    fn append_row(
        &self,
        ui: &egui::Ui,
        job: &mut egui::text::LayoutJob,
        font: &egui::FontId,
        offset: u64,
        side: bool,
        bytes: &[u8],
        diff: &[bool; ROW_BYTES],
    ) {
        let visuals = ui.visuals();
        let format = |color, background| egui::TextFormat {
            font_id: font.clone(),
            color,
            background,
            ..Default::default()
        };
        let normal = format(visuals.text_color(), Color32::TRANSPARENT);
        let weak = format(visuals.weak_text_color(), Color32::TRANSPARENT);
        job.append(&format!("{offset:0OFFSET_WIDTH$X}  "), 0.0, weak.clone());
        let mut styles = Vec::with_capacity(ROW_BYTES);
        let ascii = hex::ascii(bytes);
        for (i, different) in diff.iter().copied().enumerate() {
            let at = offset + i as u64;
            let selected = self
                .selection
                .is_some_and(|(a, b, right)| side == right && (a.min(b)..=a.max(b)).contains(&at));
            let focused = self.cursor == Some(at);
            let style = if selected || focused {
                format(visuals.selection.stroke.color, visuals.selection.bg_fill)
            } else if different {
                format(visuals.text_color(), visuals.warn_fg_color.gamma_multiply(0.29))
            } else {
                normal.clone()
            };
            if let Some(byte) = bytes.get(i) {
                let mut value = String::new();
                let _ = write!(value, "{byte:02X}");
                job.append(&value, 0.0, style.clone());
            } else {
                job.append("  ", 0.0, style.clone());
            }
            job.append(" ", 0.0, weak.clone());
            styles.push(style);
        }
        job.append(" ", 0.0, weak.clone());
        for (i, style) in styles.into_iter().enumerate() {
            job.append(ascii.get(i..i + 1).unwrap_or(" "), 0.0, style);
        }
    }
}

/// Hex digits of the offset column (10: files up to 1 TB line up).
const OFFSET_WIDTH: usize = 10;
/// Blank columns between the two sides of a comparison.
const SIDE_GAP: usize = 4;

pub(super) fn binary_patch(patch: &str) -> bool {
    patch.lines().any(|line| {
        (line.starts_with("Binary files ") && line.ends_with(" differ"))
            || line == "GIT binary patch"
            || line.starts_with("Cannot display: file marked as a binary type")
    })
}

pub(super) fn vcs_comparison(
    path: PathBuf,
    title: String,
    vcs: Vcs,
    repo: &Path,
    change: &FileChange,
    staged: bool,
    ctx: &egui::Context,
) -> Result<(Tab, bool), String> {
    let pair = vcs.binary_sides(repo, change, staged)?;
    let (left, right) = match (vcs, staged) {
        (Vcs::Git, true) => ("HEAD", "Index"),
        (Vcs::Git, false) => ("Index", "Working tree"),
        (Vcs::Svn, _) => ("BASE", "Working copy"),
    };
    let hex = Viewer::comparison(path, title, pair, (left.into(), right.into()), ctx);
    Ok((Tab::Hex(Box::new(hex)), false))
}

impl App {
    pub(super) fn poll_hex_tabs(&mut self, ctx: &egui::Context) {
        for tab in &mut self.tabs {
            if let Tab::Hex(viewer) = tab {
                viewer.poll(ctx);
            }
        }
    }

    pub(super) fn poll_loaded_diff(&mut self) {
        let Some(result) = receive(&mut self.diff_rx) else { return };
        let (tab, refresh) = match result {
            Ok(value) => value,
            Err(error) => {
                self.message = format!("Could not load comparison: {error}");
                return;
            }
        };
        let Tab::Diff(mut tab) = tab else {
            self.show_tab(tab);
            return;
        };
        let current = self.tabs.iter_mut().find_map(|t| match t {
            Tab::Diff(d) => Some(d),
            _ => None,
        });
        if refresh && current.is_none() {
            return;
        }
        if tab.ignore_eol != self.settings.diff_ignore_eol && !matches!(tab.source, DiffSource::Commit) {
            self.load_diff(tab.title, tab.path, tab.source, refresh);
        } else {
            if let Some(current) = current {
                tab.inline = current.inline;
                if refresh {
                    tab.live = current.live.take();
                    *current = tab;
                    return;
                }
            }
            self.show_tab(Tab::Diff(tab));
        }
    }

    pub(super) fn select_for_compare(&mut self, path: PathBuf) {
        self.message = format!("Selected {} for comparison", dir_name(&path));
        self.compare_path = Some(path);
    }

    pub(super) fn compare_with_selected(&mut self, right: PathBuf) {
        let Some(left) = self.compare_path.clone() else {
            self.message = "Select a file for comparison first".into();
            return;
        };
        self.compare_files(left, right);
    }

    pub(super) fn compare_files(&mut self, left: PathBuf, right: PathBuf) {
        let left = workspace::plain_path(&left);
        let right = workspace::plain_path(&right);
        let ctx = self.ctx.clone();
        let ignore_eol = self.settings.diff_ignore_eol;
        self.diff_rx = Some(background(&self.ctx, move || {
            let load = |p: &Path| Source::open(p).map_err(|err| format!("{}: {err}", p.display()));
            let pair = Pair {
                left: load(&left)?,
                right: load(&right)?,
            };
            let left_name = dir_name(&left);
            let right_name = dir_name(&right);
            let title = format!("{left_name} / {right_name}");
            if pair.left.is_binary().map_err(|err| err.to_string())? || pair.right.is_binary().map_err(|err| err.to_string())? {
                let hex = Viewer::comparison(right, title, pair, (left_name, right_name), &ctx);
                Ok((Tab::Hex(Box::new(hex)), false))
            } else {
                let read = |source: &Source| source.read(0..source.size).map_err(|err| err.to_string());
                let (a, b) = (read(&pair.left)?, read(&pair.right)?);
                let (a, b) = (String::from_utf8_lossy(&a), String::from_utf8_lossy(&b));
                let hunks = crate::diff::unified_hunks(&a, &b, ignore_eol, 3, str::to_owned);
                let only_eol = hunks.is_empty() && a != b;
                let patch = if hunks.is_empty() {
                    hunks
                } else {
                    format!("--- {left_name}\n+++ {right_name}\n{hunks}")
                };
                let diff = DiffTab::new(title, right, &patch, DiffSource::Commit, ignore_eol, only_eol);
                Ok((Tab::Diff(Box::new(diff)), false))
            }
        }));
    }
}
