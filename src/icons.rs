//! Phosphor icon font (MIT, https://phosphoricons.com), registered as a fallback
//! so icons can be mixed into any label text. The bundled cmap also aliases Phosphor's
//! private-use codepoints at their original value + 0xF2000, in Supplementary Private
//! Use Area-B. These aliases avoid the BMP assignments in Nerd Fonts.

use eframe::egui::{FontData, FontDefinitions, FontFamily};
use std::sync::Arc;

pub const ARROW_DOWN: &str = "\u{10003E}";
pub const ARROW_SQUARE_OUT: &str = "\u{1005DE}";
pub const ARROW_U_UP_LEFT: &str = "\u{10008A}";
pub const ARROW_UP: &str = "\u{10008E}";
pub const ARROWS_CLOCKWISE: &str = "\u{100094}";
pub const CARET_DOWN: &str = "\u{100136}";
pub const CARET_LEFT: &str = "\u{100138}";
pub const CARET_RIGHT: &str = "\u{10013A}";
pub const CHECK_SQUARE: &str = "\u{100186}";
pub const EYE: &str = "\u{100220}";
pub const SQUARE: &str = "\u{10045E}";
pub const CHECK: &str = "\u{100182}";
pub const CLOUD_ARROW_DOWN: &str = "\u{1001AC}";
pub const FILE: &str = "\u{100230}";
pub const FILES: &str = "\u{100710}";
pub const FILE_SEARCH: &str = "\u{100238}";
pub const FOLDER: &str = "\u{10024A}";
pub const FOLDER_OPEN: &str = "\u{100256}";
pub const GEAR: &str = "\u{100270}";
pub const APP_WINDOW: &str = "\u{1005DA}";
pub const GIT_BRANCH: &str = "\u{100278}";
pub const GIT_DIFF: &str = "\u{10027C}";
pub const MAGNIFYING_GLASS: &str = "\u{10030C}";
pub const MINUS: &str = "\u{10032A}";
pub const MOON: &str = "\u{100330}";
pub const PLUS: &str = "\u{1003D4}";
pub const SUN: &str = "\u{100472}";
pub const TREE_STRUCTURE: &str = "\u{10067C}";
pub const TREE_VIEW: &str = "\u{100E48}";
pub const LIST: &str = "\u{1002F0}";
pub const INFO: &str = "\u{1002CE}";
pub const LIGHTBULB: &str = "\u{1002DC}";
pub const PENCIL_SIMPLE: &str = "\u{1003B4}";
pub const WRENCH: &str = "\u{1005D4}";
pub const WARNING: &str = "\u{1004E0}";
pub const X_CIRCLE: &str = "\u{1004F8}";
pub const X: &str = "\u{1004F6}";
pub const COLUMNS: &str = "\u{1001E2}";
pub const ROWS: &str = "\u{1005A2}";

/// Preserve egui's bundled fallbacks and its native system font provider.
pub(crate) fn definitions(tab_size: usize, editor: Option<&FontData>, ui: Option<&FontData>) -> FontDefinitions {
    let mut fonts = FontDefinitions::default();
    fonts.font_data.insert(
        "phosphor".into(),
        Arc::new(FontData::from_static(include_bytes!("../assets/Phosphor.ttf"))),
    );
    for family in [FontFamily::Proportional, FontFamily::Monospace] {
        fonts.families.entry(family).or_default().push("phosphor".into());
    }
    for (font, name, family, base) in [
        (
            editor,
            "editor-font",
            FontFamily::Name(crate::fonts::EDITOR_FAMILY.into()),
            FontFamily::Monospace,
        ),
        (ui, "ui-font", FontFamily::Proportional, FontFamily::Proportional),
    ] {
        let mut chain = fonts.families[&base].clone();
        if let Some(font) = font {
            fonts.font_data.insert(name.into(), Arc::new(font.clone()));
            chain.insert(0, name.into());
        }
        fonts.families.insert(family, chain);
    }
    for data in fonts.font_data.values_mut() {
        Arc::make_mut(data).tweak.tab_size = tab_size as f32;
    }
    fonts
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui::{Context, FontId};

    #[test]
    fn custom_fonts_precede_all_bundled_fallbacks() {
        let default = FontDefinitions::default();
        let editor = default.font_data["Hack"].as_ref();
        let ui = default.font_data["Ubuntu-Light"].as_ref();
        let fonts = definitions(8, Some(editor), Some(ui));
        for (family, base, chosen) in [
            (
                FontFamily::Name(crate::fonts::EDITOR_FAMILY.into()),
                FontFamily::Monospace,
                "editor-font",
            ),
            (FontFamily::Proportional, FontFamily::Proportional, "ui-font"),
        ] {
            let chain = &fonts.families[&family];
            assert_eq!(chain.first().unwrap(), chosen);
            assert_eq!(&chain[1..chain.len() - 1], &default.families[&base]);
            assert_eq!(chain.last().unwrap(), "phosphor");
            assert_eq!(fonts.font_data[chosen].tweak.tab_size, 8.0);
        }
        assert!(default.font_data.keys().all(|name| fonts.font_data.contains_key(name)));
        assert_eq!(
            &fonts.families[&FontFamily::Monospace][..default.families[&FontFamily::Monospace].len()],
            &default.families[&FontFamily::Monospace]
        );
        assert!(fonts.font_data.contains_key("phosphor"));
    }

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
                for icon in [INFO, LIGHTBULB, WARNING, X_CIRCLE, PENCIL_SIMPLE, WRENCH] {
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
            ctx.set_fonts(definitions(n, None, None));
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
