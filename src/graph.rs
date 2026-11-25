//! Git graph: assigns each commit a lane and draws edges between rows.

use crate::git::{Commit, CommitRef};
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

pub(crate) fn secondary_text_color(vis: &egui::Visuals) -> Color32 {
    // Dark theme's normal text has less contrast to spare, especially on hovered rows.
    let weak_mix = if vis.dark_mode { 0.12 } else { 0.4 };
    vis.text_color().lerp_to_gamma(vis.weak_text_color(), weak_mix)
}

impl Graph {
    pub fn build(commits: Vec<Commit>) -> Self {
        use std::collections::HashMap;
        let index: HashMap<&str, usize> = commits.iter().enumerate().map(|(i, c)| (c.hash.as_str(), i)).collect();

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
            let parents: Vec<usize> = c.parents.iter().filter_map(|p| index.get(p.as_str()).copied()).collect();
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
            rows.push(Row {
                col,
                before,
                after: lanes.clone(),
            });
        }

        Graph {
            commits,
            rows,
            lanes: max_lanes,
        }
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
            let end = if target == r {
                Pos2::new(x(row.col), mid)
            } else {
                Pos2::new(x(lane), mid)
            };
            let color = lane_color(if target == r { row.col } else { lane });
            painter.line_segment([Pos2::new(x(lane), top), end], Stroke::new(w, color));
        }
        // Bottom half: lanes continuing to the row below.
        let parents = &self.commits[r].parents;
        for (lane, v) in row.after.iter().enumerate() {
            let Some(target) = *v else { continue };
            let passes = row.before.get(lane).copied().flatten() == Some(target) && lane != row.col;
            if passes {
                painter.line_segment([Pos2::new(x(lane), mid), Pos2::new(x(lane), bot)], Stroke::new(w, lane_color(lane)));
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
    pub fn ui(
        &self,
        ui: &mut egui::Ui,
        selected: Option<usize>,
        mut menu: impl FnMut(&mut egui::Ui, &Commit, Option<&CommitRef>),
    ) -> Option<usize> {
        let row_h = 22.0;
        let lane_w = 14.0;
        let graph_w = lane_w * self.lanes.min(24) as f32 + 6.0;
        let mut clicked = None;
        // Rows touch (no item spacing), so the lanes drawn in each row join into
        // continuous lines. show_rows reads the spacing from this ui as well.
        ui.spacing_mut().item_spacing.y = 0.0;
        egui::ScrollArea::both()
            .auto_shrink([false, false])
            .show_rows(ui, row_h, self.commits.len(), |ui, range| {
                for r in range {
                    let c = &self.commits[r];
                    let mono = egui::FontId::monospace(12.0);
                    // Branch labels take the color of their line in the graph.
                    let lane = lane_color(self.rows.get(r).map_or(0, |row| row.col));
                    let refs: Vec<_> = c
                        .refs
                        .iter()
                        .map(|reference| {
                            let (label, color) = ref_style(reference, lane);
                            (reference, ui.painter().layout_no_wrap(label, mono.clone(), Color32::WHITE), color)
                        })
                        .collect();
                    let refs_width: f32 = refs.iter().map(|(_, galley, _)| galley.size().x + 12.0).sum();
                    let width = ui.available_width().max(graph_w + refs_width + 506.0);
                    let (_, rect) = ui.allocate_space(Vec2::new(width, row_h));
                    let resp = ui.interact(rect, ui.make_persistent_id(&c.hash), Sense::click());
                    let painter = ui.painter_at(rect);
                    let vis = ui.visuals();
                    let is_selected = selected == Some(r);
                    if is_selected {
                        painter.rect_filled(rect, 0.0, vis.selection.bg_fill);
                    } else if resp.hovered() {
                        painter.rect_filled(rect, 0.0, vis.faint_bg_color);
                    }
                    let graph_rect = Rect::from_min_size(rect.min, Vec2::new(graph_w, row_h));
                    self.paint_row(&painter, graph_rect, r, lane_w);

                    let font = egui::FontId::proportional(13.0);
                    let (text_col, secondary) = if is_selected {
                        let text = vis.selection.stroke.color;
                        // The selection foreground has less contrast to spare on the blue fill.
                        (text, text.gamma_multiply(0.95))
                    } else {
                        (vis.text_color(), secondary_text_color(vis))
                    };
                    let mut px = rect.left() + graph_w + 4.0;
                    let cy = rect.center().y;
                    for (i, (reference, galley, color)) in refs.into_iter().enumerate() {
                        let bg = Rect::from_min_size(Pos2::new(px, cy - galley.size().y / 2.0 - 1.0), galley.size() + Vec2::new(8.0, 2.0));
                        painter.rect_filled(bg, 3.0, color);
                        painter.galley(bg.min + Vec2::new(4.0, 1.0), galley, label_text(color));
                        if matches!(reference, CommitRef::Local { .. } | CommitRef::Remote(_) | CommitRef::Tag(_)) {
                            let badge = ui.interact(bg, resp.id.with(i), Sense::click());
                            if badge.clicked() {
                                clicked = Some(r);
                            }
                            badge.context_menu(|ui| menu(ui, c, Some(reference)));
                        }
                        px = bg.right() + 4.0;
                    }
                    let right = rect.right();
                    painter.text(
                        Pos2::new(right - 8.0, cy),
                        egui::Align2::RIGHT_CENTER,
                        &c.short_hash,
                        mono.clone(),
                        secondary,
                    );
                    painter.text(
                        Pos2::new(right - 74.0, cy),
                        egui::Align2::RIGHT_CENTER,
                        &c.date,
                        font.clone(),
                        secondary,
                    );
                    painter.text(
                        Pos2::new(right - 210.0, cy),
                        egui::Align2::RIGHT_CENTER,
                        &c.author,
                        font.clone(),
                        secondary,
                    );
                    let subj_clip = Rect::from_x_y_ranges(px..=right - 330.0, rect.y_range());
                    ui.painter_at(subj_clip)
                        .text(Pos2::new(px, cy), egui::Align2::LEFT_CENTER, &c.subject, font, text_col);

                    if resp.clicked() {
                        clicked = Some(r);
                    }
                    resp.context_menu(|ui| menu(ui, c, None));
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

/// Label text and fill: branches (and HEAD) in their lane's color; remote branches a
/// little darker so local and remote copies of a branch can be told apart; tags amber.
fn ref_style(r: &CommitRef, lane: Color32) -> (String, Color32) {
    let darker = |c: Color32| Color32::from_rgb((c.r() as f32 * 0.7) as u8, (c.g() as f32 * 0.7) as u8, (c.b() as f32 * 0.7) as u8);
    match r {
        CommitRef::Local { name, head: true } => (format!("● {name}"), lane),
        CommitRef::Local { name, head: false } => (name.clone(), lane),
        CommitRef::Remote(name) | CommitRef::Other(name) => (name.clone(), darker(lane)),
        CommitRef::Tag(name) => (name.clone(), Color32::from_rgb(0x9a, 0x6d, 0x14)),
        CommitRef::Head => ("HEAD".into(), lane),
    }
}

/// Black or white, whichever reads better on `fill`.
fn label_text(fill: Color32) -> Color32 {
    let luminance = 0.299 * f32::from(fill.r()) + 0.587 * f32::from(fill.g()) + 0.114 * f32::from(fill.b());
    if luminance > 150.0 {
        Color32::from_rgb(0x1e, 0x1e, 0x1e)
    } else {
        Color32::WHITE
    }
}
