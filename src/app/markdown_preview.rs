use super::*;
use crate::markdown::{self, Block, Content, Marker, Span};
use egui::text::LayoutJob;

#[derive(Default)]
pub(super) struct Preview {
    doc: u64,
    version: Option<u64>,
    rx: Option<Receiver<(u64, Vec<Block>)>>,
    blocks: Vec<Block>,
    /// Cumulative heights, with the last entry marking the bottom of the document.
    offsets: Vec<f32>,
    width: f32,
    last_scroll: f32,
    editor_top: Option<f32>,
    reveal: Option<f32>,
}

impl App {
    pub(super) fn can_preview(&self, i: usize) -> bool {
        matches!(self.tabs.get(i), Some(Tab::File(doc)) if matches!(doc.ext().as_str(), "md" | "markdown"))
    }

    pub(super) fn open_markdown_preview(&mut self, i: usize) {
        if !self.can_preview(i) {
            self.message = "Open a Markdown file to preview it".into();
            return;
        }
        let Tab::File(doc) = &self.tabs[i] else { return };
        self.active = i;
        self.split = None;
        if self.preview.as_ref().is_some_and(|preview| preview.doc == doc.id) {
            return;
        }
        self.preview = Some(Preview {
            doc: doc.id,
            ..Default::default()
        });
    }

    /// Returns whether the right pane was reserved for a preview.
    pub(super) fn markdown_pane(&mut self, ui: &mut egui::Ui) -> bool {
        let Some(preview) = self.preview.as_mut() else { return false };
        let Some((index, doc)) = self.tabs.iter_mut().enumerate().find_map(|(i, t)| match t {
            Tab::File(doc) if doc.id == preview.doc => Some((i, doc)),
            _ => None,
        }) else {
            self.preview = None;
            return false;
        };
        preview.update(doc, ui.ctx());
        let mut close = false;
        egui::Panel::right("split-pane")
            .resizable(true)
            .default_size(ui.available_width() / 2.0)
            .min_size(200.0)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.strong(format!("{} Preview", doc.name()));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        close = icon_button(ui, ic::X).on_hover_text("Close preview").clicked();
                    });
                });
                ui.separator();
                if let Some(line) = preview.show(ui, &doc.path)
                    && index == self.active
                {
                    doc.ed.scroll_to_line(line);
                    preview.editor_top = Some(line);
                }
            });
        if close {
            self.preview = None;
        }
        true
    }

    pub(super) fn sync_markdown_preview(&mut self, active: usize, out: &EditorOut) {
        let (Some(preview), Some(Tab::File(doc))) = (&mut self.preview, self.tabs.get(active)) else {
            return;
        };
        if doc.id != preview.doc {
            return;
        }
        if preview.editor_top.is_none_or(|line| (line - out.top_line).abs() >= 0.8) {
            preview.reveal = Some(out.top_line);
            self.ctx.request_repaint();
        }
        preview.editor_top = Some(out.top_line);
        if out.changed {
            preview.update(doc, &self.ctx);
        }
    }
}

impl Preview {
    #[cfg(feature = "tour")]
    pub(super) fn loading(&self) -> bool {
        self.rx.is_some()
    }

    fn update(&mut self, doc: &Doc, ctx: &egui::Context) {
        if let Some((version, blocks)) = receive(&mut self.rx)
            && version == doc.version
        {
            self.version = Some(version);
            self.blocks = blocks;
            self.offsets.clear();
            self.reveal = self.editor_top;
        }
        if self.version != Some(doc.version) && self.rx.is_none() {
            let version = doc.version;
            let text = doc.text.clone();
            self.rx = Some(background(ctx, move || (version, markdown::parse(&text))));
        }
    }

    fn show(&mut self, ui: &mut egui::Ui, path: &Path) -> Option<f32> {
        if self.version.is_none() {
            ui.weak("Rendering preview…");
            return None;
        }
        if self.blocks.is_empty() {
            return None;
        }
        let width = ui.available_width();
        if self.offsets.is_empty() || (self.width - width).abs() > 2.0 {
            self.width = width;
            let mut total = 0.0;
            self.offsets = std::iter::once(0.0)
                .chain(self.blocks.iter().map(|block| {
                    total += estimate(block, width);
                    total
                }))
                .collect();
        }
        let mut area = egui::ScrollArea::vertical()
            .id_salt(("markdown-preview", self.doc))
            .auto_shrink([false, false]);
        let linked = self.reveal.take().map(|line| {
            let i = self.blocks.partition_point(|b| b.line as f32 <= line).saturating_sub(1);
            let start = self.blocks[i].line as f32;
            let next = self.blocks.get(i + 1).map_or(start + 1.0, |b| b.line as f32);
            let fraction = ((line - start) / (next - start).max(1.0)).clamp(0.0, 1.0);
            egui::lerp(self.offsets[i]..=self.offsets[i + 1], fraction)
        });
        if let Some(offset) = linked {
            area = area.vertical_scroll_offset(offset);
        }
        let result = area.show_viewport(ui, |ui, viewport| {
            let first = self
                .offsets
                .partition_point(|&offset| offset <= viewport.min.y)
                .saturating_sub(1)
                .min(self.blocks.len() - 1);
            let end = self
                .offsets
                .partition_point(|&offset| offset < viewport.max.y + 72.0)
                .clamp(first + 1, self.blocks.len());
            let top = ui.cursor().min.y;
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 0.0;
            ui.add_space(self.offsets[first]);
            let mut delta = 0.0;
            for i in first..end {
                let saved = self.offsets[i + 1] - self.offsets[i];
                let y = ui.cursor().min.y;
                block_ui(ui, &self.blocks[i], path);
                let height = (ui.cursor().min.y - y).max(1.0);
                self.offsets[i] += delta;
                delta += height - saved;
            }
            if delta.abs() > f32::EPSILON {
                for offset in &mut self.offsets[end..] {
                    *offset += delta;
                }
            }
            ui.add_space((top + self.offsets.last().copied().unwrap_or_default() - ui.cursor().min.y).max(0.0));
        });
        let scroll = result.state.clamped_offset().y;
        let changed = (scroll - self.last_scroll).abs() > 0.5;
        self.last_scroll = scroll;
        if linked.is_some() || !changed || !ui.input(|input| input.pointer.hover_pos().is_some_and(|p| result.inner_rect.contains(p))) {
            return None;
        }
        let i = self
            .offsets
            .partition_point(|&offset| offset <= scroll)
            .saturating_sub(1)
            .min(self.blocks.len() - 1);
        let fraction = ((scroll - self.offsets[i]) / (self.offsets[i + 1] - self.offsets[i]).max(1.0)).clamp(0.0, 1.0);
        let start = self.blocks[i].line as f32;
        let end = self.blocks.get(i + 1).map_or(start + 1.0, |b| b.line as f32);
        Some(egui::lerp(start..=end, fraction))
    }
}

fn estimate(block: &Block, width: f32) -> f32 {
    let chars_per_line = (width / 7.0).max(20.0);
    match &block.content {
        Content::Text { heading, spans } => {
            let chars = spans.iter().map(|span| span.text.len()).sum::<usize>() as f32;
            let row = if *heading == 0 { 17.0 } else { 26.0 };
            (chars / chars_per_line).ceil().max(1.0) * row + 8.0
        }
        Content::Code { text, .. } => text.lines().count().max(1) as f32 * 15.0 + 24.0,
        Content::Table { .. } => 27.0,
        Content::Rule => 17.0,
    }
}

fn block_ui(ui: &mut egui::Ui, block: &Block, path: &Path) {
    let top = ui.cursor().min;
    let mut indent = (block.indent * 19 + block.quotes * 14) as f32;
    indent = indent.min(ui.available_width() / 2.0);
    ui.horizontal_top(|ui| {
        ui.add_space(indent);
        if let Some(marker) = &block.marker {
            let text = match marker {
                Marker::Bullet => ic::MINUS.to_owned(),
                Marker::Number(n) => format!("{n}."),
                Marker::Task(true) => ic::CHECK_SQUARE.to_owned(),
                Marker::Task(false) => ic::SQUARE.to_owned(),
            };
            ui.label(text);
            ui.add_space(3.0);
        }
        ui.vertical(|ui| {
            ui.set_width(ui.available_width());
            ui.add_space(4.0);
            match &block.content {
                Content::Text { heading, spans } => {
                    let size = [14.0, 28.0, 24.0, 20.0, 18.0, 16.0, 14.0][usize::from(*heading)];
                    text_ui(ui, path, spans, size, *heading != 0, egui::Align::LEFT);
                }
                Content::Code { language, text } => {
                    egui::Frame::new()
                        .fill(ui.visuals().faint_bg_color)
                        .inner_margin(8.0)
                        .show(ui, |ui| {
                            if !language.is_empty() {
                                ui.weak(language);
                            }
                            ui.add(egui::Label::new(RichText::new(text.trim_end_matches('\n')).monospace()).wrap());
                        });
                }
                Content::Table { align, header, cells } => {
                    if !cells.is_empty() {
                        let bg = ui.visuals().faint_bg_color;
                        egui::Frame::new()
                            .fill(if *header { bg } else { Color32::TRANSPARENT })
                            .inner_margin(4.0)
                            .show(ui, |ui| {
                                ui.columns(cells.len(), |columns| {
                                    for ((ui, cell), alignment) in columns.iter_mut().zip(cells).zip(align.iter()) {
                                        let alignment = match alignment {
                                            pulldown_cmark::Alignment::Center => egui::Align::Center,
                                            pulldown_cmark::Alignment::Right => egui::Align::RIGHT,
                                            _ => egui::Align::LEFT,
                                        };
                                        text_ui(ui, path, cell, 13.0, *header, alignment);
                                    }
                                });
                            });
                        ui.separator();
                    }
                }
                Content::Rule => {
                    ui.separator();
                }
            }
            ui.add_space(if block.indent != 0 || matches!(block.content, Content::Table { .. }) {
                2.0
            } else {
                7.0
            });
        });
    });
    for i in 0..block.quotes.min(5) {
        let x = top.x + (i * 14 + 4) as f32;
        ui.painter().vline(
            x,
            top.y..=ui.cursor().min.y,
            egui::Stroke::new(2.0, ui.visuals().widgets.noninteractive.bg_stroke.color),
        );
    }
}

fn text_ui(ui: &mut egui::Ui, path: &Path, spans: &[Span], size: f32, strong: bool, align: egui::Align) {
    let visuals = ui.visuals();
    let mut layout = LayoutJob::default();
    layout.wrap.max_width = ui.available_width().max(1.0);
    layout.halign = align;
    let mut links = Vec::new();
    let mut end = 0;
    for span in spans {
        let bold = strong || span.style.bold;
        let mut format = egui::TextFormat {
            font_id: if span.style.code {
                egui::FontId::monospace(size - 0.5)
            } else {
                egui::FontId::proportional(size + if bold && !strong { 0.5 } else { 0.0 })
            },
            color: if bold { visuals.strong_text_color() } else { visuals.text_color() },
            italics: span.style.italic,
            background: if span.style.code {
                visuals.code_bg_color
            } else {
                Color32::TRANSPARENT
            },
            ..Default::default()
        };
        if bold {
            format.coords.push(b"wght", 700.0);
        }
        if let Some(link) = &span.style.link {
            format.color = visuals.hyperlink_color;
            format.underline = egui::Stroke::new(1.0, visuals.hyperlink_color);
            links.push((end..end + span.text.chars().count(), link));
        }
        end += span.text.chars().count();
        layout.append(&span.text, 0.0, format);
    }
    let galley = ui.painter().layout_job(layout);
    let sense = if links.is_empty() {
        egui::Sense::hover()
    } else {
        egui::Sense::click()
    };
    let (rect, response) = ui.allocate_exact_size(egui::vec2(ui.available_width(), galley.size().y), sense);
    let pos = egui::pos2(
        align.align_size_within_range(galley.size().x, rect.x_range()).min - galley.rect.min.x,
        rect.top(),
    );
    if let Some(pointer) = response.hover_pos() {
        let cursor = galley.cursor_from_pos(pointer - pos);
        if let Some((_, link)) = links.iter().find(|(range, _)| range.contains(&cursor.index.0)) {
            let clicked = response.clicked();
            response
                .on_hover_cursor(egui::CursorIcon::PointingHand)
                .on_hover_text(link.as_str());
            if clicked && let Some(url) = preview_link(path, link) {
                ui.ctx().open_url(egui::OpenUrl::new_tab(url));
            }
        }
    }
    ui.painter().galley(pos, galley, ui.visuals().text_color());
}

fn preview_link(path: &Path, link: &str) -> Option<String> {
    let url = url::Url::parse(link)
        .ok()
        .or_else(|| url::Url::from_file_path(path).ok()?.join(link).ok())?;
    matches!(url.scheme(), "http" | "https" | "mailto" | "file").then(|| url.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_visible_blocks_after_a_source_position_jump() {
        let text = (0..500).map(|i| format!("Paragraph {i:03}\n\n")).collect::<String>();
        let position = 400.0;
        let mut preview = Preview {
            version: Some(0),
            blocks: markdown::parse(&text),
            reveal: Some(position),
            ..Default::default()
        };
        let ctx = egui::Context::default();
        let mut frame = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(420.0, 260.0))),
                ..Default::default()
            },
            |ui| {
                egui::CentralPanel::default().show(ui, |ui| {
                    assert!(preview.show(ui, Path::new("sample.md")).is_none());
                });
            },
        );
        let labels: Vec<_> = frame
            .shapes
            .iter()
            .filter_map(|paint| match &paint.shape {
                egui::Shape::Text(text) => Some(text.galley.text()),
                _ => None,
            })
            .collect();
        assert!(labels.contains(&"Paragraph 200"));
        assert!(!labels.contains(&"Paragraph 000"));
        assert!(labels.len() < 40);
        frame.textures_delta.clear();
    }
}
