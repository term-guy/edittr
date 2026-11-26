//! Editor palettes share color roles across syntax grammars, language servers and the widget.

use eframe::egui::{Color32, Theme, ThemePreference};
use std::fmt::Write as _;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub(crate) enum Part {
    Background,
    Foreground,
    Selection,
    CurrentLine,
    LineNumber,
    Comment,
    Keyword,
    String,
    Number,
    Constant,
    Macro,
    Type,
    Function,
    Variable,
    Operator,
    Disabled,
    Bracket1,
    Bracket2,
    Bracket3,
}

use Part::*;

impl Part {
    pub(crate) const ALL: [(Self, &'static str, &'static str); COUNT] = [
        (Background, "background", "Background"),
        (Foreground, "foreground", "Foreground"),
        (Selection, "selection", "Selection"),
        (CurrentLine, "current_line", "Current line"),
        (LineNumber, "line_number", "Line numbers"),
        (Comment, "comment", "Comments"),
        (Keyword, "keyword", "Keywords"),
        (String, "string", "Strings"),
        (Number, "number", "Numbers"),
        (Constant, "constant", "Constants and enum members"),
        (Macro, "macro", "Macros and preprocessor"),
        (Type, "type", "Types and namespaces"),
        (Function, "function", "Functions"),
        (Variable, "variable", "Variables and parameters"),
        (Operator, "operator", "Operators and punctuation"),
        (Disabled, "disabled", "Disabled code"),
        (Bracket1, "bracket_1", "Bracket depth 1"),
        (Bracket2, "bracket_2", "Bracket depth 2"),
        (Bracket3, "bracket_3", "Bracket depth 3"),
    ];
}

const COUNT: usize = Bracket3 as usize + 1;

pub(crate) struct Preset {
    pub(crate) id: &'static str,
    pub(crate) name: &'static str,
    pub(crate) base: Theme,
    colors: [Color32; COUNT],
    legacy: bool,
}

const fn preset(id: &'static str, name: &'static str, base: Theme, legacy: bool, rgb: [u32; COUNT]) -> Preset {
    let mut colors = [Color32::TRANSPARENT; COUNT];
    let mut i = 0;
    while i < COUNT {
        let n = rgb[i];
        colors[i] = Color32::from_rgb((n >> 16) as u8, (n >> 8) as u8, n as u8);
        i += 1;
    }
    Preset {
        id,
        name,
        base,
        colors,
        legacy,
    }
}

const fn legacy(mut preset: Preset, foreground: u8, selection: [u8; 4], current_line: [u8; 4], line_number: [u8; 4]) -> Preset {
    preset.colors[Foreground as usize] = Color32::from_gray(foreground);
    preset.colors[Selection as usize] = Color32::from_rgba_premultiplied(selection[0], selection[1], selection[2], selection[3]);
    preset.colors[CurrentLine as usize] =
        Color32::from_rgba_premultiplied(current_line[0], current_line[1], current_line[2], current_line[3]);
    preset.colors[LineNumber as usize] = Color32::from_rgba_premultiplied(line_number[0], line_number[1], line_number[2], line_number[3]);
    preset
}

pub(crate) static PRESETS: [Preset; 18] = [
    // Background, foreground, selection, current line, line numbers, then the syntax and bracket colors.
    legacy(
        preset(
            "edittr-dark",
            "edittr Dark",
            Theme::Dark,
            true,
            [
                0x1b1b1b, 0, 0, 0, 0, 0x7e705a, 0xa89bb9, 0xbeb55b, 0xd28b71, 0xf7a8d0, 0xf7a8d0, 0xf4bc87, 0x8ab3b5, 0xcb6077, 0xd0c8c6,
                0x7e705a, 0xffd700, 0xda70d6, 0x179fff,
            ],
        ),
        140,
        [0, 92, 128, 255],
        [8, 8, 8, 15],
        [84, 84, 84, 153],
    ),
    preset(
        "tidal-glass",
        "Tidal Glass",
        Theme::Dark,
        false,
        [
            0x111d25, 0xd0e1e7, 0x365d73, 0x1b2e3b, 0x819ba8, 0x819ca6, 0x9abaf2, 0x8dd0b5, 0xf1bd84, 0xf2a7bc, 0xebb489, 0x80d4dc,
            0xb1d3ff, 0xd0e1e7, 0x9db7c4, 0x748791, 0xe2c472, 0xc3a9ef, 0x75cdd7,
        ],
    ),
    preset(
        "kiln-ash",
        "Kiln Ash",
        Theme::Dark,
        false,
        [
            0x211918, 0xeadbd0, 0x69483c, 0x312422, 0xa29082, 0xae9988, 0xf0a58d, 0xc2d198, 0xf5c182, 0xebaece, 0xf19c73, 0x9ccec6,
            0xf2d3a2, 0xe8d4bf, 0xc5ac9e, 0x9a857e, 0xf2c269, 0xed9ba5, 0x91c9b8,
        ],
    ),
    preset(
        "plum-circuit",
        "Plum Circuit",
        Theme::Dark,
        false,
        [
            0x21182b, 0xe9dcef, 0x56436d, 0x30243d, 0xa194b0, 0xa995b0, 0xd6aafa, 0xa9d7b0, 0xf4b5ac, 0xf1a1cb, 0xf4cb8d, 0x9bd8e3,
            0xb9baff, 0xe8c8ec, 0xc9aece, 0x97879f, 0xf4c479, 0xe1a6ee, 0x89d1df,
        ],
    ),
    preset(
        "fern-relay",
        "Fern Relay",
        Theme::Dark,
        false,
        [
            0x17221c, 0xdae4d7, 0x435f4d, 0x223127, 0x93a395, 0x92a78f, 0xc3b1ed, 0xb9d587, 0xecc28b, 0xedaeab, 0xe2c68f, 0x8cd9c6,
            0xbce0a8, 0xd9ddc1, 0xabbfae, 0x85958a, 0xebc471, 0xd9a6df, 0x81ccbc,
        ],
    ),
    preset(
        "graphite-signal",
        "Graphite Signal",
        Theme::Dark,
        false,
        [
            0x181a1e, 0xe6e7ea, 0x344c68, 0x262930, 0x969da9, 0x9a9fab, 0x9ec8ff, 0xc5d890, 0xf2c17f, 0xefa7c9, 0xffb08d, 0x9fddd8,
            0xc6bcff, 0xdde2ef, 0xb8c2d2, 0x868f9e, 0xffce70, 0xd7a6ff, 0x80cfff,
        ],
    ),
    preset(
        "harbor-night",
        "Harbor Night",
        Theme::Dark,
        false,
        [
            0x0f1724, 0xd6e2f0, 0x1f4470, 0x172338, 0x6f84a0, 0x6f8aa8, 0x6fb3ff, 0x9ad6a6, 0xf0b77a, 0xe7a3d0, 0xc9a6ff, 0x7fd8e6,
            0x8fc4ff, 0xd6e2f0, 0x9fb6cf, 0x5d6f86, 0x5aa9ff, 0xc39bff, 0x6fe0d0,
        ],
    ),
    preset(
        "cobalt-deep",
        "Cobalt Deep",
        Theme::Dark,
        false,
        [
            0x101a3a, 0xe2e8ff, 0x2a4598, 0x18234a, 0x7d8bbd, 0x7f8fc4, 0x5fc2ff, 0xb5e08f, 0xffc078, 0xff9ec7, 0xffd479, 0x7ee2f2,
            0x9fb8ff, 0xe2e8ff, 0xa9b6e8, 0x5c6a9c, 0xffd479, 0x5fc2ff, 0xff9ec7,
        ],
    ),
    preset(
        "steel-blue",
        "Steel Blue",
        Theme::Dark,
        false,
        [
            0x1a1c20, 0xdfe3ea, 0x264f78, 0x23262c, 0x7d8590, 0x7b8594, 0x5aa2ff, 0x9cc98a, 0xd9a46f, 0x7fb8ff, 0xb59cff, 0x56c8d8,
            0x8ab8ff, 0xdfe3ea, 0xa8b0bd, 0x646b76, 0x5aa2ff, 0xd9a46f, 0x56c8d8,
        ],
    ),
    preset(
        "glacier-night",
        "Glacier Night",
        Theme::Dark,
        false,
        [
            0x0d1e26, 0xd3e8ee, 0x1d4a5e, 0x142a34, 0x6c8e99, 0x6f929d, 0x4fb8e8, 0xa3d9b1, 0xf2c38b, 0xb7c7ff, 0xe7a8c9, 0x6ee0d6,
            0x7cc4f5, 0xd3e8ee, 0x9bbcc6, 0x587680, 0x4fb8e8, 0xf2c38b, 0xb7c7ff,
        ],
    ),
    preset(
        "crimson-slate",
        "Crimson Slate",
        Theme::Dark,
        false,
        [
            0x1c1719, 0xece0e2, 0x5c2a35, 0x2a2124, 0x9a858a, 0x968187, 0xff7a8a, 0xc9d98f, 0xffb27a, 0xf5a3c7, 0xffc46b, 0x8fd3c9,
            0xf7b2a8, 0xece0e2, 0xc4aeb3, 0x7a676c, 0xffc46b, 0xff7a8a, 0x8fd3c9,
        ],
    ),
    preset(
        "ink-mono",
        "Ink Mono",
        Theme::Dark,
        false,
        [
            0x141414, 0xdedede, 0x3d3d3d, 0x1e1e1e, 0x7a7a7a, 0x7c7c7c, 0xf5f5f5, 0xb8c7a8, 0xd4c3a3, 0xd4c3a3, 0xc9b8d6, 0xc6d4e0,
            0xe8e8e8, 0xcfcfcf, 0x9e9e9e, 0x5e5e5e, 0xd8d8d8, 0xa8b8c8, 0xc8b8a8,
        ],
    ),
    legacy(
        preset(
            "edittr-light",
            "edittr Light",
            Theme::Light,
            true,
            [
                0xf8f8f8, 0, 0, 0, 0, 0x93a1a1, 0x859900, 0x2aa198, 0x6c71c4, 0x902052, 0x902052, 0xb58900, 0xb58900, 0x268bd2, 0x859900,
                0x93a1a1, 0x0431fa, 0x319331, 0x7b3814,
            ],
        ),
        80,
        [144, 209, 255, 255],
        [5, 5, 5, 15],
        [48, 48, 48, 153],
    ),
    preset(
        "oat-paper",
        "Oat Paper",
        Theme::Light,
        false,
        [
            0xf8f3e9, 0x302923, 0xd8e2f2, 0xeee6d8, 0x746960, 0x706c60, 0x743d83, 0x306746, 0x874a23, 0x913c62, 0x993e29, 0x266672,
            0x354e92, 0x523c34, 0x564f4c, 0x756a62, 0x725200, 0x8a4587, 0x235b87,
        ],
    ),
    preset(
        "harbor-day",
        "Harbor Day",
        Theme::Light,
        false,
        [
            0xf6f9fc, 0x1f2a37, 0xc8dcf5, 0xe9f0f8, 0x6b7a8c, 0x6a7a8a, 0x0b5cc4, 0x2a7a3b, 0xa4501a, 0x8a3fa0, 0x8a5a00, 0x00728a,
            0x1d4fb8, 0x1f2a37, 0x4a5868, 0x8a96a3, 0x0b5cc4, 0x8a3fa0, 0x2a7a3b,
        ],
    ),
    preset(
        "mint-sheet",
        "Mint Sheet",
        Theme::Light,
        false,
        [
            0xf3f8f4, 0x1f2b24, 0xcfe6d6, 0xe6f0e8, 0x67786c, 0x66786b, 0x1f6b52, 0x8a4d14, 0x9c3a4a, 0x7a3e9d, 0x8a5a00, 0x1d5f8a,
            0x2f6b2f, 0x1f2b24, 0x4c5c52, 0x8a978e, 0x1f6b52, 0x7a3e9d, 0x8a4d14,
        ],
    ),
    preset(
        "rose-paper",
        "Rose Paper",
        Theme::Light,
        false,
        [
            0xfcf5f5, 0x33262a, 0xf2d3da, 0xf6e8ea, 0x7d6a70, 0x7d6a70, 0xa3264b, 0x3d7040, 0x9a4f16, 0x6b46a8, 0x8a5a00, 0x1f6a7a,
            0x8a2f6b, 0x33262a, 0x5c4a50, 0x9a8a8f, 0xa3264b, 0x6b46a8, 0x1f6a7a,
        ],
    ),
    preset(
        "clear-white",
        "Clear White",
        Theme::Light,
        false,
        [
            0xffffff, 0x1a1a1a, 0xcce0ff, 0xf2f5f9, 0x6e6e6e, 0x6a737d, 0x0033b3, 0x067d17, 0xa33a00, 0x871094, 0x7a5c00, 0x00627a,
            0x00438a, 0x1a1a1a, 0x444444, 0x8c8c8c, 0x0033b3, 0x871094, 0x067d17,
        ],
    ),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Palette {
    preset: usize,
    overrides: [Option<Color32>; COUNT],
}

impl Palette {
    pub(crate) const fn new(preset: usize) -> Self {
        Self {
            preset,
            overrides: [None; COUNT],
        }
    }

    pub(crate) fn preset(&self) -> &'static Preset {
        &PRESETS[self.preset]
    }

    pub(crate) fn reset(&mut self) {
        self.overrides = [None; COUNT];
    }

    pub(crate) fn is_custom(&self) -> bool {
        self.overrides.iter().any(Option::is_some)
    }

    pub(crate) fn color(&self, part: Part) -> Color32 {
        self.overrides[part as usize].unwrap_or(self.preset().colors[part as usize])
    }

    pub(crate) fn set(&mut self, part: Part, color: Color32) {
        self.overrides[part as usize] = (color != self.preset().colors[part as usize]).then_some(color);
    }

    pub(crate) fn active_line_number(&self, visuals: &eframe::egui::Visuals) -> Color32 {
        if self.preset().legacy && self.overrides[LineNumber as usize].is_none() {
            visuals.strong_text_color()
        } else {
            self.color(LineNumber)
        }
    }

    pub(crate) fn uses_legacy_color(&self, part: Part) -> bool {
        self.preset().legacy && self.overrides[part as usize].is_none()
    }

    pub(crate) fn defaults(dark: bool) -> Self {
        let id = if dark { "edittr-dark" } else { "edittr-light" };
        Self::new(PRESETS.iter().position(|preset| preset.id == id).unwrap_or(0))
    }

    pub(crate) fn parse(value: &str, base: Theme) -> Option<Self> {
        let mut fields = value.split(',');
        let id = fields.next()?;
        let preset = PRESETS.iter().position(|p| p.id == id && p.base == base)?;
        let mut palette = Self::new(preset);
        for (key, value) in fields.filter_map(|field| field.split_once(':')) {
            if let Some(&(part, _, _)) = Part::ALL.iter().find(|(_, name, _)| *name == key)
                && value.len() == 8
                && let Ok(rgba) = u32::from_str_radix(value, 16)
            {
                let [r, g, b, a] = rgba.to_be_bytes();
                if r <= a && g <= a && b <= a {
                    palette.set(part, Color32::from_rgba_premultiplied(r, g, b, a));
                }
            }
        }
        Some(palette)
    }

    pub(crate) fn write(&self, text: &mut std::string::String) {
        text.push_str(self.preset().id);
        for (part, key, _) in Part::ALL {
            if let Some(color) = self.overrides[part as usize] {
                let rgba = u32::from_be_bytes(color.to_array());
                let _ = write!(text, ",{key}:{rgba:08x}");
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Themes {
    pub(crate) preference: ThemePreference,
    pub(crate) dark: Palette,
    pub(crate) light: Palette,
    pub(crate) custom_dark: Option<Palette>,
    pub(crate) custom_light: Option<Palette>,
}

impl Default for Themes {
    fn default() -> Self {
        Self {
            preference: ThemePreference::System,
            dark: Palette::defaults(true),
            light: Palette::defaults(false),
            custom_dark: None,
            custom_light: None,
        }
    }
}

impl Themes {
    pub(crate) fn active(&self, dark: bool) -> &Palette {
        if dark { &self.dark } else { &self.light }
    }

    pub(crate) fn set_custom(&mut self, base: Theme, palette: Option<Palette>) {
        match base {
            Theme::Dark => self.custom_dark = palette,
            Theme::Light => self.custom_light = palette,
        }
    }

    pub(crate) fn set_palette(&mut self, palette: Palette) {
        match palette.preset().base {
            Theme::Dark => self.dark = palette,
            Theme::Light => self.light = palette,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn contrast(a: Color32, b: Color32) -> f32 {
        let luminance = |c: Color32| {
            let linear = |v: u8| {
                let v = f32::from(v) / 255.0;
                if v <= 0.039_28 {
                    v / 12.92
                } else {
                    ((v + 0.055) / 1.055).powf(2.4)
                }
            };
            0.2126 * linear(c.r()) + 0.7152 * linear(c.g()) + 0.0722 * linear(c.b())
        };
        let (a, b) = (luminance(a), luminance(b));
        (a.max(b) + 0.05) / (a.min(b) + 0.05)
    }

    #[test]
    fn presets_have_unique_ids_and_readable_colors() {
        for (i, preset) in PRESETS.iter().enumerate() {
            assert!(PRESETS[..i].iter().all(|other| other.id != preset.id), "duplicate id {}", preset.id);
        }
        assert_eq!(Palette::defaults(true).preset().id, "edittr-dark");
        assert_eq!(Palette::defaults(false).preset().id, "edittr-light");
        for preset in PRESETS.iter().filter(|preset| !preset.legacy) {
            let palette = Palette::new(PRESETS.iter().position(|p| p.id == preset.id).unwrap());
            for (part, _, label) in Part::ALL {
                let minimum = match part {
                    Background | Selection | CurrentLine => continue,
                    Foreground => 7.0,
                    Comment | LineNumber => 3.0,
                    Disabled => 2.5,
                    _ => 4.5,
                };
                for behind in [Background, CurrentLine] {
                    let ratio = contrast(palette.color(part), palette.color(behind));
                    assert!(ratio >= minimum, "{}: {label} on {behind:?} is {ratio:.2}:1", preset.name);
                }
            }
            let ratio = contrast(palette.color(Foreground), palette.color(Selection));
            assert!(ratio >= 4.5, "{}: text on the selection is {ratio:.2}:1", preset.name);
        }
    }
}
