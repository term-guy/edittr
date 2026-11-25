//! Phosphor icon font (MIT, https://phosphoricons.com), registered as a fallback
//! so icons can be mixed into any label text.

use eframe::egui::{FontData, FontDefinitions, FontFamily};
use std::sync::Arc;

pub const ARROW_DOWN: &str = "\u{E03E}";
pub const ARROW_SQUARE_OUT: &str = "\u{E5DE}";
pub const ARROW_U_UP_LEFT: &str = "\u{E08A}";
pub const ARROW_UP: &str = "\u{E08E}";
pub const ARROWS_CLOCKWISE: &str = "\u{E094}";
pub const CARET_DOWN: &str = "\u{E136}";
pub const CARET_LEFT: &str = "\u{E138}";
pub const CARET_RIGHT: &str = "\u{E13A}";
pub const CHECK_SQUARE: &str = "\u{E18C}";
pub const EYE: &str = "\u{E220}";
pub const SQUARE: &str = "\u{E440}";
pub const CHECK: &str = "\u{E182}";
pub const CLOUD_ARROW_DOWN: &str = "\u{E1AC}";
pub const FILE: &str = "\u{E230}";
pub const FILES: &str = "\u{E710}";
pub const FILE_SEARCH: &str = "\u{E238}";
pub const FOLDER: &str = "\u{E24A}";
pub const FOLDER_OPEN: &str = "\u{E256}";
pub const GEAR: &str = "\u{E270}";
pub const GIT_BRANCH: &str = "\u{E278}";
pub const GIT_DIFF: &str = "\u{E27C}";
pub const MAGNIFYING_GLASS: &str = "\u{E30C}";
pub const MINUS: &str = "\u{E32A}";
pub const MOON: &str = "\u{E330}";
pub const PLUS: &str = "\u{E3D4}";
pub const SUN: &str = "\u{E472}";
pub const TREE_STRUCTURE: &str = "\u{E67C}";
pub const TREE_VIEW: &str = "\u{EE48}";
pub const LIST: &str = "\u{E2F0}";
pub const INFO: &str = "\u{E2CE}";
pub const LIGHTBULB: &str = "\u{E2DC}";
pub const WARNING: &str = "\u{E4E0}";
pub const X_CIRCLE: &str = "\u{E4F8}";
pub const X: &str = "\u{E4F6}";
pub const COLUMNS: &str = "\u{E1E2}";
pub const ROWS: &str = "\u{E5A2}";

/// Fonts with the Phosphor icons; `tab_size` is how many spaces wide a tab is drawn.
pub fn install(ctx: &eframe::egui::Context, tab_size: usize) {
    let mut fonts = FontDefinitions::default();
    for data in fonts.font_data.values_mut() {
        Arc::make_mut(data).tweak.tab_size = tab_size as f32;
    }
    fonts.font_data.insert(
        "phosphor".into(),
        Arc::new(FontData::from_static(include_bytes!("../assets/Phosphor.ttf"))),
    );
    for family in [FontFamily::Proportional, FontFamily::Monospace] {
        fonts.families.entry(family).or_default().push("phosphor".into());
    }
    ctx.set_fonts(fonts);
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui::{Context, FontId};

    #[test]
    fn phosphor_contains_problem_icons() {
        let ctx = Context::default();
        let mut fonts = FontDefinitions::empty();
        fonts.font_data.insert(
            "phosphor".into(),
            Arc::new(FontData::from_static(include_bytes!("../assets/Phosphor.ttf"))),
        );
        fonts.families.insert(FontFamily::Proportional, vec!["phosphor".into()]);
        ctx.set_fonts(fonts);
        let mut full = ctx.run_ui(Default::default(), |ui| {
            ui.fonts_mut(|fonts| {
                for icon in [INFO, LIGHTBULB, WARNING, X_CIRCLE] {
                    assert!(fonts.has_glyphs(&FontId::proportional(14.0), icon), "missing glyph {icon:?}");
                }
            });
        });
        full.textures_delta.clear(); // no renderer here (epaint asserts on dropped deltas in debug)
    }

    #[test]
    fn tab_size_sets_the_drawn_width_of_a_tab() {
        for n in [2usize, 8] {
            let ctx = Context::default();
            install(&ctx, n);
            let mut full = ctx.run_ui(Default::default(), |ui| {
                let width = |text: &str| {
                    ui.fonts_mut(|f| f.layout_no_wrap(text.into(), FontId::monospace(14.0), eframe::egui::Color32::WHITE))
                        .size()
                        .x
                };
                let (tab, spaces) = (width("\tx"), width(&format!("{}x", " ".repeat(n))));
                assert!((tab - spaces).abs() < 0.5, "tab size {n}: tab {tab} vs spaces {spaces}");
            });
            full.textures_delta.clear();
        }
    }
}
