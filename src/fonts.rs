//! Font discovery and file I/O live on a worker, with a cached catalog for both pickers.

use crate::settings::{DEFAULT_FONT_SIZE, FontChoice, FontFile, Settings};
use eframe::egui::{self, FontData, FontFamily, FontId, Style, TextStyle};
use fontique::{Collection, CollectionOptions, FontInfo, SourceKind};
use skrifa::{FontRef, MetadataProvider};
use std::sync::{
    Arc,
    mpsc::{Receiver, Sender, channel},
};

pub(crate) const EDITOR_FAMILY: &str = "Editor";
/// Text style of the breadcrumbs (file path) above the editor.
pub(crate) const BREADCRUMB_STYLE: &str = "Breadcrumbs";

#[derive(Clone, Copy, Debug, Hash)]
pub(crate) enum Target {
    Editor,
    Ui,
}

impl Target {
    pub(crate) fn choice(self, settings: &mut Settings) -> &mut Option<FontChoice> {
        match self {
            Self::Editor => &mut settings.editor_font,
            Self::Ui => &mut settings.ui_font,
        }
    }

    pub(crate) fn size(self, settings: &mut Settings) -> &mut u8 {
        match self {
            Self::Editor => &mut settings.editor_font_size,
            Self::Ui => &mut settings.ui_font_size,
        }
    }
}

pub(crate) struct Family {
    pub(crate) name: String,
    pub(crate) file: Option<FontFile>,
    pub(crate) monospace: bool,
    pub(crate) search_name: String,
}

struct Loaded {
    data: FontData,
    file: Option<FontFile>,
}

enum Request {
    List,
    Load(Target, FontChoice),
}

enum Response {
    List(Vec<Family>),
    Load(Target, FontChoice, Option<Loaded>),
}

#[derive(Default)]
pub(crate) struct Fonts {
    worker: Option<(Sender<Request>, Receiver<Response>)>,
    list_requested: bool,
    pub(crate) families: Option<Vec<Family>>,
    faces: [Option<FontData>; 2],
    /// The platform's UI font, used while no UI font is chosen (None: the bundled one).
    default_ui: Option<(&'static str, FontData)>,
}

impl Fonts {
    pub(crate) fn start(&mut self, settings: &Settings, ctx: &egui::Context) {
        self.default_ui = platform_ui_font();
        self.install(settings, ctx);
        self.apply_sizes(settings, ctx);
        for (target, choice) in [(Target::Editor, &settings.editor_font), (Target::Ui, &settings.ui_font)] {
            if let Some(choice) = choice {
                self.send(Request::Load(target, choice.clone()), ctx);
            }
        }
    }

    pub(crate) fn install(&self, settings: &Settings, ctx: &egui::Context) {
        let ui = self.faces[1].as_ref().or(self.default_ui.as_ref().map(|(_, data)| data));
        ctx.set_fonts(crate::icons::definitions(settings.tab_size, self.faces[0].as_ref(), ui));
    }

    /// Name of the font "Default" stands for in the UI font picker.
    pub(crate) fn default_ui_name(&self) -> Option<&'static str> {
        self.default_ui.as_ref().map(|(name, _)| *name)
    }

    pub(crate) fn apply_sizes(&self, settings: &Settings, ctx: &egui::Context) {
        let mut styles = egui::style::default_text_styles();
        let scale = f32::from(settings.ui_font_size) / f32::from(DEFAULT_FONT_SIZE);
        for font in styles.values_mut() {
            font.size *= scale;
        }
        let family = if self.faces[0].is_some() {
            FontFamily::Name(EDITOR_FAMILY.into())
        } else {
            FontFamily::Monospace
        };
        styles.insert(
            TextStyle::Name(EDITOR_FAMILY.into()),
            FontId::new(settings.editor_font_size.into(), family),
        );
        styles.insert(
            TextStyle::Name(BREADCRUMB_STYLE.into()),
            FontId::new(settings.breadcrumb_font_size.into(), FontFamily::Proportional),
        );
        let min_row_height = egui::style::Spacing::default().interact_size.y;
        ctx.all_styles_mut(|style| {
            style.text_styles.extend(styles.clone());
            // Several virtual lists use the minimum widget height as their row height.
            style.spacing.interact_size.y = min_row_height * scale.max(1.0);
        });
        ctx.request_repaint();
    }

    pub(crate) fn request_list(&mut self, ctx: &egui::Context) {
        if !self.list_requested {
            self.list_requested = true;
            self.send(Request::List, ctx);
        }
    }

    pub(crate) fn select(&mut self, target: Target, choice: Option<FontChoice>, ctx: &egui::Context) {
        if let Some(choice) = choice {
            self.send(Request::Load(target, choice), ctx);
        } else {
            self.faces[target as usize] = None;
        }
    }

    /// Returns whether a successfully re-resolved path should be saved.
    pub(crate) fn poll(&mut self, settings: &mut Settings, ctx: &egui::Context) -> bool {
        let Some((_, rx)) = &self.worker else { return false };
        let (mut reinstall, mut save) = (false, false);
        for response in rx.try_iter() {
            match response {
                Response::List(families) => self.families = Some(families),
                Response::Load(target, requested, loaded) => {
                    let Some(choice) = target.choice(settings).as_mut().filter(|choice| **choice == requested) else {
                        continue;
                    };
                    if let Some(loaded) = &loaded
                        && choice.file != loaded.file
                    {
                        choice.file.clone_from(&loaded.file);
                        save = true;
                    }
                    self.faces[target as usize] = loaded.map(|loaded| loaded.data);
                    reinstall = true;
                }
            }
        }
        if reinstall {
            self.install(settings, ctx);
            self.apply_sizes(settings, ctx);
        }
        save
    }

    fn send(&mut self, request: Request, ctx: &egui::Context) {
        let (tx, _) = self.worker.get_or_insert_with(|| {
            let (tx, requests) = channel();
            let (reply, rx) = channel();
            let ctx = ctx.clone();
            std::thread::spawn(move || {
                let mut catalog = None;
                for request in requests {
                    let response = match request {
                        Request::List => Response::List(list(catalog.get_or_insert_with(system_fonts))),
                        Request::Load(target, choice) => {
                            let loaded = load_saved(&choice).or_else(|| {
                                let family = catalog.get_or_insert_with(system_fonts).family_by_name(&choice.family)?;
                                load(family.default_font()?)
                            });
                            Response::Load(target, choice, loaded)
                        }
                    };
                    if reply.send(response).is_err() {
                        break;
                    }
                    ctx.request_repaint();
                }
            });
            (tx, rx)
        });
        let _ = tx.send(request);
    }
}

/// Where the platform's UI font usually is, best first: (name, file, face index).
fn platform_ui_candidates() -> Vec<(&'static str, std::path::PathBuf, u32)> {
    use std::path::PathBuf;
    if cfg!(windows) {
        let windows = std::env::var_os("WINDIR").map_or_else(|| PathBuf::from(r"C:\Windows"), PathBuf::from);
        vec![("Segoe UI", windows.join(r"Fonts\segoeui.ttf"), 0)]
    } else if cfg!(target_os = "macos") {
        vec![
            ("San Francisco", PathBuf::from("/System/Library/Fonts/SFNS.ttf"), 0),
            ("Helvetica Neue", PathBuf::from("/System/Library/Fonts/HelveticaNeue.ttc"), 0),
        ]
    } else {
        // The default sans-serif fonts of the common desktops (Ubuntu, GNOME, Fedora, KDE, others),
        // at their usual paths on Debian / Ubuntu, Fedora and Arch.
        [
            ("Ubuntu", "truetype/ubuntu/Ubuntu-R.ttf"),
            ("Ubuntu", "truetype/ubuntu/Ubuntu[wdth,wght].ttf"),
            ("Adwaita Sans", "adwaita-sans-fonts/AdwaitaSans-Regular.ttf"),
            ("Adwaita Sans", "Adwaita/AdwaitaSans-Regular.ttf"),
            ("Cantarell", "cantarell/Cantarell-VF.otf"),
            ("Cantarell", "abattis-cantarell-vf-fonts/Cantarell-VF.otf"),
            ("Cantarell", "opentype/cantarell/Cantarell-VF.otf"),
            ("Noto Sans", "truetype/noto/NotoSans-Regular.ttf"),
            ("Noto Sans", "google-noto/NotoSans-Regular.ttf"),
            ("Noto Sans", "noto/NotoSans-Regular.ttf"),
            ("DejaVu Sans", "truetype/dejavu/DejaVuSans.ttf"),
            ("DejaVu Sans", "dejavu-sans-fonts/DejaVuSans.ttf"),
            ("DejaVu Sans", "TTF/DejaVuSans.ttf"),
            ("Liberation Sans", "truetype/liberation/LiberationSans-Regular.ttf"),
            ("Liberation Sans", "liberation-sans/LiberationSans-Regular.ttf"),
        ]
        .into_iter()
        .map(|(name, path)| (name, PathBuf::from("/usr/share/fonts").join(path), 0))
        .collect()
    }
}

/// The platform's UI font: Segoe UI on Windows, San Francisco on macOS, the desktop's usual
/// sans-serif on Linux. Read from known paths: no font catalog scan at startup.
fn platform_ui_font() -> Option<(&'static str, FontData)> {
    platform_ui_candidates().into_iter().find_map(|(name, path, index)| {
        let data = FontData::from_blob(Arc::new(std::fs::read(&path).ok()?), index);
        valid_face(&data)?;
        Some((name, data))
    })
}

fn system_fonts() -> Collection {
    Collection::new(CollectionOptions {
        shared: false,
        system_fonts: true,
    })
}

fn list(catalog: &mut Collection) -> Vec<Family> {
    let names: Vec<_> = catalog.family_names().map(str::to_owned).collect();
    let mut families: Vec<_> = names
        .into_iter()
        .filter_map(|name| {
            let family = catalog.family_by_name(&name)?;
            let font = family.default_font()?;
            let blob = font.load(None)?;
            let face = FontRef::from_index(blob.as_ref(), font.index()).ok()?;
            use skrifa::raw::TableProvider as _;
            let monospace = face.post().is_ok_and(|post| post.is_fixed_pitch() != 0);
            Some(Family {
                search_name: name.to_lowercase(),
                name,
                file: font_file(font),
                monospace,
            })
        })
        .collect();
    families.sort_unstable_by(|a, b| a.search_name.cmp(&b.search_name).then_with(|| a.name.cmp(&b.name)));
    families
}

fn load_saved(choice: &FontChoice) -> Option<Loaded> {
    use skrifa::string::StringId;

    let file = choice.file.as_ref()?;
    let data = FontData::from_blob(Arc::new(std::fs::read(&file.path).ok()?), file.index);
    let font = valid_face(&data)?;
    // A font update can replace a file or change the order of faces in a collection.
    let name_matches = [StringId::FAMILY_NAME, StringId::TYPOGRAPHIC_FAMILY_NAME, StringId::WWS_FAMILY_NAME]
        .into_iter()
        .any(|id| font.localized_strings(id).any(|name| name.to_string() == choice.family));
    name_matches.then(|| Loaded {
        data,
        file: Some(file.clone()),
    })
}

fn font_file(info: &FontInfo) -> Option<FontFile> {
    match &info.source().kind {
        SourceKind::Path(path) => path.to_str().map(|path| FontFile {
            path: path.into(),
            index: info.index(),
        }),
        SourceKind::Memory(_) => None,
    }
}

fn load(info: &FontInfo) -> Option<Loaded> {
    let (blob, _) = info.load(None)?.into_raw_parts();
    let data = FontData::from_blob(blob, info.index());
    valid_face(&data)?;
    Some(Loaded {
        data,
        file: font_file(info),
    })
}

fn valid_face(data: &FontData) -> Option<FontRef<'_>> {
    let face = FontRef::from_index(data.bytes(), data.index).ok()?;
    face.charmap().has_map().then_some(face)
}

pub(crate) fn editor_font(style: &Style) -> FontId {
    style
        .text_styles
        .iter()
        .find_map(|(key, font)| match key {
            TextStyle::Name(name) if name.as_ref() == EDITOR_FAMILY => Some(font.clone()),
            _ => None,
        })
        .unwrap_or_else(|| TextStyle::Monospace.resolve(style))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_platform_ui_font_is_the_default_ui_font() {
        let started = std::time::Instant::now();
        let found = platform_ui_font();
        eprintln!(
            "platform UI font: {:?} in {:?}",
            found.as_ref().map(|(name, _)| *name),
            started.elapsed()
        );
        if cfg!(any(windows, target_os = "macos")) {
            assert!(found.is_some(), "Segoe UI / San Francisco ship with the OS");
        }
        let ctx = egui::Context::default();
        let mut fonts = Fonts::default();
        fonts.start(&Settings::default(), &ctx);
        assert_eq!(fonts.default_ui_name(), found.as_ref().map(|(name, _)| *name));
        let mut out = ctx.run_ui(Default::default(), |ui| {
            ui.label("Text in the UI font");
        });
        out.textures_delta.clear();
        if found.is_some() {
            let families = ctx.fonts(|f| f.definitions().families[&FontFamily::Proportional].clone());
            assert_eq!(families[0], "ui-font");
        }
    }

    #[test]
    fn defaults_do_not_start_the_font_worker_and_sizes_are_independent() {
        let ctx = egui::Context::default();
        let mut fonts = Fonts::default();
        let mut settings = Settings::default();
        fonts.start(&settings, &ctx);
        assert!(fonts.worker.is_none());
        let default_styles = egui::style::default_text_styles();
        for theme in [egui::Theme::Dark, egui::Theme::Light] {
            let style = ctx.style_of(theme);
            assert_eq!(editor_font(&style), default_styles[&TextStyle::Monospace]);
            for (kind, font) in &default_styles {
                assert_eq!(*font, kind.resolve(&style));
            }
        }
        settings.editor_font_size = 21;
        settings.ui_font_size = 26;
        fonts.apply_sizes(&settings, &ctx);
        for theme in [egui::Theme::Dark, egui::Theme::Light] {
            let style = ctx.style_of(theme);
            for (kind, font) in &default_styles {
                assert_eq!(kind.resolve(&style).size, font.size * 2.0);
            }
            assert_eq!(editor_font(&style), FontId::monospace(21.0));
        }
    }
}
