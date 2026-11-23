//! Git graph: assigns each commit a lane and draws edges between rows.

use crate::git::Commit;
use eframe::egui::{self, Color32, Pos2, Rect, Sense, Stroke, Vec2};

pub struct Row {
    pub col: usize,
    /// Lane -> commit index expected in that lane, before / after this row.
    before: Vec<Option<usize>>,
    after: Vec<Option<usize>>,
}

pub struct Graph {
    pub commits: Vec<Commit>,
    pub rows: Vec<Row>,
    pub lanes: usize,
}

const PALETTE: [Color32; 8] = [
    Color32::from_rgb(0x4e, 0x9a, 0xf0),
    Color32::from_rgb(0xe0, 0x6c, 0x75),
    Color32::from_rgb(0x98, 0xc3, 0x79),
    Color32::from_rgb(0xe5, 0xc0, 0x7b),
    Color32::from_rgb(0xc6, 0x78, 0xdd),
    Color32::from_rgb(0x56, 0xb6, 0xc2),
    Color32::from_rgb(0xd1, 0x9a, 0x66),
    Color32::from_rgb(0xab, 0xb2, 0xbf),
];

pub fn lane_color(i: usize) -> Color32 {
    PALETTE[i % PALETTE.len()]
}

impl Graph {
    pub fn build(commits: Vec<Commit>) -> Self {
        use std::collections::HashMap;
        let index: HashMap<&str, usize> = commits
            .iter()
            .enumerate()
            .map(|(i, c)| (c.hash.as_str(), i))
            .collect();

        let mut lanes: Vec<Option<usize>> = Vec::new();
        let mut rows = Vec::with_capacity(commits.len());
        let mut max_lanes = 1;

        for (i, c) in commits.iter().enumerate() {
            let before = lanes.clone();
            let col = match lanes.iter().position(|l| *l == Some(i)) {
                Some(p) => p,
                None => free_slot(&mut lanes),
            };
            // Any other lanes waiting for this commit converge here.
            for l in lanes.iter_mut() {
                if *l == Some(i) {
                    *l = None;
                }
            }
            lanes[col] = None;

            // Parents outside the loaded window are simply dropped.
            let parents: Vec<usize> = c
                .parents
                .iter()
                .filter_map(|p| index.get(p.as_str()).copied())
                .collect();
            for (n, &p) in parents.iter().enumerate() {
                if lanes.contains(&Some(p)) {
                    continue; // already tracked: we'll draw an edge into that lane
                }
                let slot = if n == 0 { col } else { free_slot(&mut lanes) };
                lanes[slot] = Some(p);
            }
            while lanes.last() == Some(&None) {
                lanes.pop();
            }
            max_lanes = max_lanes.max(lanes.len()).max(col + 1);
            rows.push(Row { col, before, after: lanes.clone() });
        }

        Graph { commits, rows, lanes: max_lanes }
    }

    /// Paint the graph cell for row `r` into `rect`.
    pub fn paint_row(&self, painter: &egui::Painter, rect: Rect, r: usize, lane_w: f32) {
        let row = &self.rows[r];
        let x = |lane: usize| rect.left() + lane_w * (lane as f32 + 0.5);
        let (top, mid, bot) = (rect.top(), rect.center().y, rect.bottom());
        let w = 1.6;

        // Top half: lanes coming in from the row above.
        for (lane, v) in row.before.iter().enumerate() {
            let Some(target) = *v else { continue };
            let end = if target == r { Pos2::new(x(row.col), mid) } else { Pos2::new(x(lane), mid) };
            let color = lane_color(if target == r { row.col } else { lane });
            painter.line_segment([Pos2::new(x(lane), top), end], Stroke::new(w, color));
        }
        // Bottom half: lanes continuing to the row below.
        let parents = &self.commits[r].parents;
        for (lane, v) in row.after.iter().enumerate() {
            let Some(target) = *v else { continue };
            let passes = row.before.get(lane).copied().flatten() == Some(target) && lane != row.col;
            if passes {
                painter.line_segment(
                    [Pos2::new(x(lane), mid), Pos2::new(x(lane), bot)],
                    Stroke::new(w, lane_color(lane)),
                );
            }
            if parents.iter().any(|p| *p == self.commits[target].hash) {
                painter.line_segment(
                    [Pos2::new(x(row.col), mid), Pos2::new(x(lane), bot)],
                    Stroke::new(w, lane_color(lane.max(row.col))),
                );
            }
        }

        let c = Pos2::new(x(row.col), mid);
        let color = lane_color(row.col);
        if parents.len() > 1 {
            painter.circle(c, 4.0, painter.ctx().global_style().visuals.panel_fill, Stroke::new(2.0, color));
        } else {
            painter.circle_filled(c, 4.0, color);
        }
    }

    /// Show the whole graph as a virtualised list. Returns the clicked row.
    pub fn ui(&self, ui: &mut egui::Ui, selected: Option<usize>) -> Option<usize> {
        let row_h = 22.0;
        let lane_w = 14.0;
        let graph_w = lane_w * self.lanes.min(24) as f32 + 6.0;
        let mut clicked = None;
        egui::ScrollArea::both()
            .auto_shrink([false, false])
            .show_rows(ui, row_h, self.commits.len(), |ui, range| {
                for r in range {
                    let c = &self.commits[r];
                    let width = ui.available_width().max(600.0);
                    let (rect, resp) = ui.allocate_exact_size(Vec2::new(width, row_h), Sense::click());
                    let painter = ui.painter_at(rect);
                    let vis = ui.visuals();
                    if selected == Some(r) {
                        painter.rect_filled(rect, 0.0, vis.selection.bg_fill);
                    } else if resp.hovered() {
                        painter.rect_filled(rect, 0.0, vis.faint_bg_color);
                    }
                    let graph_rect = Rect::from_min_size(rect.min, Vec2::new(graph_w, row_h));
                    self.paint_row(&painter, graph_rect, r, lane_w);

                    let font = egui::FontId::proportional(13.0);
                    let mono = egui::FontId::monospace(12.0);
                    let text_col = vis.text_color();
                    let weak = vis.weak_text_color();
                    let mut px = rect.left() + graph_w + 4.0;
                    let cy = rect.center().y;
                    for rf in &c.refs {
                        let (label, color) = ref_style(rf);
                        let galley = painter.layout_no_wrap(label, mono.clone(), Color32::WHITE);
                        let bg = Rect::from_min_size(
                            Pos2::new(px, cy - galley.size().y / 2.0 - 1.0),
                            galley.size() + Vec2::new(8.0, 2.0),
                        );
                        painter.rect_filled(bg, 3.0, color);
                        painter.galley(bg.min + Vec2::new(4.0, 1.0), galley, Color32::WHITE);
                        px = bg.right() + 4.0;
                    }
                    let right = rect.right();
                    painter.text(Pos2::new(right - 8.0, cy), egui::Align2::RIGHT_CENTER, &c.hash[..7.min(c.hash.len())], mono.clone(), weak);
                    painter.text(Pos2::new(right - 74.0, cy), egui::Align2::RIGHT_CENTER, &c.date, font.clone(), weak);
                    painter.text(Pos2::new(right - 210.0, cy), egui::Align2::RIGHT_CENTER, &c.author, font.clone(), weak);
                    let subj_clip = Rect::from_x_y_ranges(px..=right - 330.0, rect.y_range());
                    ui.painter_at(subj_clip).text(Pos2::new(px, cy), egui::Align2::LEFT_CENTER, &c.subject, font, text_col);

                    if resp.clicked() {
                        clicked = Some(r);
                    }
                }
            });
        clicked
    }
}

fn free_slot(lanes: &mut Vec<Option<usize>>) -> usize {
    match lanes.iter().position(Option::is_none) {
        Some(p) => p,
        None => {
            lanes.push(None);
            lanes.len() - 1
        }
    }
}

fn ref_style(r: &str) -> (String, Color32) {
    if let Some(b) = r.strip_prefix("HEAD -> ") {
        (format!("● {b}"), Color32::from_rgb(0x2f, 0x81, 0x3a))
    } else if let Some(t) = r.strip_prefix("tag: ") {
        (t.to_string(), Color32::from_rgb(0x9a, 0x6d, 0x14))
    } else if r == "HEAD" {
        ("HEAD".into(), Color32::from_rgb(0x2f, 0x81, 0x3a))
    } else if r.contains('/') {
        (r.to_string(), Color32::from_rgb(0x6b, 0x4f, 0x9e))
    } else {
        (r.to_string(), Color32::from_rgb(0x2b, 0x6c, 0xb0))
    }
}
