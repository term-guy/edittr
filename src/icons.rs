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
pub const CARET_RIGHT: &str = "\u{E13A}";
pub const CHECK: &str = "\u{E182}";
pub const CLOUD_ARROW_DOWN: &str = "\u{E1AC}";
pub const FILE: &str = "\u{E230}";
pub const FILES: &str = "\u{E710}";
pub const FILE_SEARCH: &str = "\u{E238}";
pub const FOLDER: &str = "\u{E24A}";
pub const FOLDER_OPEN: &str = "\u{E256}";
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
pub const X: &str = "\u{E4F6}";
pub const COLUMNS: &str = "\u{E1E2}";
pub const ROWS: &str = "\u{E5A2}";

pub fn install(ctx: &eframe::egui::Context) {
    let mut fonts = FontDefinitions::default();
    fonts.font_data.insert(
        "phosphor".into(),
        Arc::new(FontData::from_static(include_bytes!("../assets/Phosphor.ttf"))),
    );
    for family in [FontFamily::Proportional, FontFamily::Monospace] {
        fonts.families.entry(family).or_default().push("phosphor".into());
    }
    ctx.set_fonts(fonts);
}
