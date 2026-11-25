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

pub(crate) static PRESETS: [Preset; 8] = [
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
        Self::new(if dark { 0 } else { 6 })
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
