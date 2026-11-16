//! Themes: palette, typeface and shape style. Every theme dresses the same
//! layout; only colors, fonts, corners and glows change.
//!
//! Two themes are built in; more come from folders (`system/themes/<name>/
//! theme.json`, read by `ui::themes`), parsed here by `from_json`. A theme
//! may give each tab its own accent: the theme shown is the chosen one with
//! the current tab's accent swapped in, so every screen follows.
//!
//! The current theme is global to the UI thread (screens read colors while
//! drawing, far from any shared state), picked from `THEME` in `config.txt`.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ops::RangeInclusive;
use std::path::Path;
use std::sync::{Arc, LazyLock};

use macroquad::prelude::Color;
use serde::Deserialize;

use super::text::Typeface;

#[derive(Clone, Debug)]
pub struct Theme {
    /// Value in `config.txt`: a built-in id, or a theme folder's name.
    pub id: String,
    /// Shown in Settings.
    pub name: String,
    pub typeface: Typeface,
    /// Corner radius of cards and buttons, in design pixels; 0 is square.
    pub radius: f32,
    /// Soft halo around the focused card.
    pub glow: bool,
    /// Outline width of the focused card and of panels.
    pub focus_width: f32,
    /// Outline unfocused cards too (otherwise only their fill shows).
    pub card_outline: bool,
    /// Background gradient, top to bottom.
    pub bg_top: Color,
    pub bg_bottom: Color,
    /// Cards and panels.
    pub surface: Color,
    /// The focused card or row.
    pub surface_hi: Color,
    pub border: Color,
    /// Behind panels.
    pub shadow: Color,
    pub text: Color,
    pub muted: Color,
    pub disabled: Color,
    /// Focus, confirm, success.
    pub accent: Color,
    /// Titles, warnings, going back.
    pub secondary: Color,
    /// Information, Steam, folders.
    pub info: Color,
    /// Errors and destructive actions.
    pub danger: Color,
    /// The accent of each tab while it is shown, by tab id (`library`,
    /// `settings`...); tabs left out keep `accent`.
    pub tabs: HashMap<String, Color>,
}

const fn rgb(hex: u32) -> Color {
    Color::new(
        ((hex >> 16) & 0xff) as f32 / 255.0,
        ((hex >> 8) & 0xff) as f32 / 255.0,
        (hex & 0xff) as f32 / 255.0,
        1.0,
    )
}

/// The default theme, and the base of theme files (keys they leave out).
fn midnight() -> Theme {
    Theme {
        id: "midnight".into(),
        name: "Midnight".into(),
        typeface: Typeface::Barlow,
        radius: 6.0,
        glow: true,
        focus_width: 1.5,
        card_outline: false,
        bg_top: rgb(0x141b2e),
        bg_bottom: rgb(0x070a12),
        surface: rgb(0x151c2d),
        surface_hi: rgb(0x212b45),
        border: rgb(0x273250),
        shadow: Color::new(0.0, 0.0, 0.0, 0.55),
        text: rgb(0xe9edf6),
        muted: rgb(0x8b94ad),
        disabled: rgb(0x4b5370),
        accent: rgb(0x4fe0b0),
        secondary: rgb(0xf4b860),
        info: rgb(0x6ea8ff),
        danger: rgb(0xff6b6b),
        tabs: HashMap::new(),
    }
}

/// The original look: pure black, the pixel font, square frames.
fn retro() -> Theme {
    Theme {
        id: "retro".into(),
        name: "Retro".into(),
        typeface: Typeface::Pixel,
        radius: 0.0,
        glow: false,
        focus_width: 2.0,
        card_outline: true,
        bg_top: rgb(0x000000),
        bg_bottom: rgb(0x000000),
        surface: rgb(0x000000),
        surface_hi: rgb(0x0e0b08),
        border: rgb(0x41301e),
        shadow: Color::new(0.0, 0.0, 0.0, 0.0),
        text: rgb(0xe6ebf2),
        muted: rgb(0x6e7682),
        disabled: rgb(0x3d424a),
        accent: rgb(0x26a86c),
        secondary: rgb(0xa05210),
        info: rgb(0x569cd6),
        danger: rgb(0xd9554d),
        tabs: HashMap::new(),
    }
}

/// The first one is the default.
static BUILT_IN: LazyLock<[Arc<Theme>; 2]> =
    LazyLock::new(|| [Arc::new(midnight()), Arc::new(retro())]);

struct State {
    /// Themes read from `system/themes/`, after the built-in ones.
    from_files: Vec<Arc<Theme>>,
    chosen: Arc<Theme>,
    tab: String,
    /// `chosen` with the accent of `tab`.
    shown: Arc<Theme>,
}

// Only the UI thread draws: a thread-local keeps the many color lookups of a
// frame free of locks.
thread_local! {
    static STATE: RefCell<State> = RefCell::new(State {
        from_files: Vec::new(),
        chosen: BUILT_IN[0].clone(),
        tab: String::new(),
        shown: BUILT_IN[0].clone(),
    });
}

/// A value of the theme shown, without cloning it.
fn read<T>(value: impl FnOnce(&Theme) -> T) -> T {
    STATE.with(|state| value(&state.borrow().shown))
}

/// Whether `id` names a built-in theme (theme folders can't take it).
pub fn is_built_in(id: &str) -> bool {
    BUILT_IN
        .iter()
        .any(|t| t.id.eq_ignore_ascii_case(id.trim()))
}

/// Replace the themes read from files.
pub fn install(themes: Vec<Theme>) {
    STATE.with(|state| state.borrow_mut().from_files = themes.into_iter().map(Arc::new).collect());
}

/// Every theme, built-in ones first.
pub fn all() -> Vec<Arc<Theme>> {
    STATE.with(|state| {
        BUILT_IN
            .iter()
            .chain(&state.borrow().from_files)
            .cloned()
            .collect()
    })
}

/// The theme with this id (any case); unset or unknown means the default.
pub fn find(id: Option<&str>) -> Arc<Theme> {
    id.and_then(|id| {
        all()
            .into_iter()
            .find(|t| t.id.eq_ignore_ascii_case(id.trim()))
    })
    .unwrap_or_else(|| BUILT_IN[0].clone())
}

/// `theme` with the accent of `tab`, if it gives that tab one.
fn shown(theme: &Arc<Theme>, tab: &str) -> Arc<Theme> {
    match theme.tabs.get(tab) {
        Some(&accent) => Arc::new(Theme {
            accent,
            ..Theme::clone(theme)
        }),
        None => theme.clone(),
    }
}

/// Switch every screen to `theme`.
pub fn set(theme: Arc<Theme>) {
    STATE.with(|state| {
        let mut state = state.borrow_mut();
        state.shown = shown(&theme, &state.tab);
        state.chosen = theme;
    });
}

/// The tab on screen (its id, e.g. `library`), for its accent.
pub fn set_tab(tab: &str) {
    STATE.with(|state| {
        let mut state = state.borrow_mut();
        if state.tab != tab {
            state.tab = tab.to_string();
            state.shown = shown(&state.chosen, tab);
        }
    });
}

/// The accent the chosen theme gives `tab`, if any (for the tab icons).
pub fn tab_color(tab: &str) -> Option<Color> {
    STATE.with(|state| state.borrow().chosen.tabs.get(tab).copied())
}

/// The theme shown (for drawing code that reads several of its values).
pub fn current() -> Arc<Theme> {
    STATE.with(|state| state.borrow().shown.clone())
}

pub fn bg() -> Color {
    read(|t| t.bg_bottom)
}
pub fn surface_hi() -> Color {
    read(|t| t.surface_hi)
}
pub fn border() -> Color {
    read(|t| t.border)
}
pub fn text() -> Color {
    read(|t| t.text)
}
pub fn muted() -> Color {
    read(|t| t.muted)
}
pub fn disabled() -> Color {
    read(|t| t.disabled)
}
pub fn accent() -> Color {
    read(|t| t.accent)
}
pub fn secondary() -> Color {
    read(|t| t.secondary)
}
pub fn info() -> Color {
    read(|t| t.info)
}
pub fn danger() -> Color {
    read(|t| t.danger)
}

/// A color by its role, looked up in the current theme when drawn: text kept
/// on screen across a theme change follows it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Tone {
    #[default]
    Muted,
    Accent,
    Secondary,
    Danger,
}

impl Tone {
    fn of(self, theme: &Theme) -> Color {
        match self {
            Tone::Muted => theme.muted,
            Tone::Accent => theme.accent,
            Tone::Secondary => theme.secondary,
            Tone::Danger => theme.danger,
        }
    }

    pub fn color(self) -> Color {
        read(|t| self.of(t))
    }
}

/// `color` at `alpha` opacity.
pub fn fade(color: Color, alpha: f32) -> Color {
    Color { a: alpha, ..color }
}

/// A theme's `theme.json`. Every key is optional: what it leaves out comes
/// from Midnight, and keys it doesn't know are ignored (a theme made for a
/// newer app still loads).
#[derive(Deserialize, Default)]
#[serde(default)]
struct ThemeFile {
    font: Option<String>,
    font_size: Option<f32>,
    radius: Option<f32>,
    glow: Option<bool>,
    focus_width: Option<f32>,
    card_outline: Option<bool>,
    colors: HashMap<String, String>,
    tabs: HashMap<String, String>,
}

/// Body text size of the built-in smooth face, in design pixels.
pub const DEFAULT_FONT_SIZE: f32 = 18.0;

/// The theme `id` from the `theme.json` text of the folder `dir` (where its
/// font file sits).
pub fn from_json(id: &str, dir: &Path, json: &str) -> Result<Theme, String> {
    let file: ThemeFile = serde_json::from_str(json).map_err(|e| e.to_string())?;
    let named = Theme {
        id: id.to_string(),
        name: id.to_string(),
        ..midnight()
    };
    colored(shaped(named, &file, dir)?, &file)
}

/// `base` with the typeface and shapes of `file`.
fn shaped(base: Theme, file: &ThemeFile, dir: &Path) -> Result<Theme, String> {
    Ok(Theme {
        typeface: match &file.font {
            Some(font) => typeface(font, file.font_size, dir)?,
            None => base.typeface.clone(),
        },
        radius: within("radius", file.radius, 0.0..=24.0)?.unwrap_or(base.radius),
        glow: file.glow.unwrap_or(base.glow),
        focus_width: within("focus_width", file.focus_width, 0.0..=8.0)?
            .unwrap_or(base.focus_width),
        card_outline: file.card_outline.unwrap_or(base.card_outline),
        ..base
    })
}

/// `theme` with the colors and tab colors of `file`.
fn colored(mut theme: Theme, file: &ThemeFile) -> Result<Theme, String> {
    theme.tabs = file
        .tabs
        .iter()
        .map(|(tab, color)| Ok((tab.to_lowercase(), parse_color(color)?)))
        .collect::<Result<_, String>>()?;
    file.colors.iter().try_for_each(|(role, value)| {
        if let Some(slot) = color_slot(&mut theme, role) {
            *slot = parse_color(value)?;
        }
        Ok::<_, String>(())
    })?;
    Ok(theme)
}

/// `value`, refused outside `range` (a size that would hide the interface).
fn within(
    key: &str,
    value: Option<f32>,
    range: RangeInclusive<f32>,
) -> Result<Option<f32>, String> {
    match value {
        Some(v) if !range.contains(&v) => Err(format!(
            "{key} {v} is outside {}-{}",
            range.start(),
            range.end()
        )),
        v => Ok(v),
    }
}

/// `font`: a built-in face (`barlow`, `pixel`) or a font file in `dir`.
fn typeface(font: &str, size: Option<f32>, dir: &Path) -> Result<Typeface, String> {
    match font.trim().to_lowercase().as_str() {
        "barlow" => Ok(Typeface::Barlow),
        "pixel" => Ok(Typeface::Pixel),
        _ => {
            let path = dir.join(font.trim());
            if !path.is_file() {
                return Err(format!("font {font} not found next to theme.json"));
            }
            Ok(Typeface::File {
                path,
                size: within("font_size", size, 8.0..=48.0)?.unwrap_or(DEFAULT_FONT_SIZE),
            })
        }
    }
}

fn color_slot<'a>(theme: &'a mut Theme, role: &str) -> Option<&'a mut Color> {
    Some(match role {
        "bg_top" => &mut theme.bg_top,
        "bg_bottom" => &mut theme.bg_bottom,
        "surface" => &mut theme.surface,
        "surface_hi" => &mut theme.surface_hi,
        "border" => &mut theme.border,
        "shadow" => &mut theme.shadow,
        "text" => &mut theme.text,
        "muted" => &mut theme.muted,
        "disabled" => &mut theme.disabled,
        "accent" => &mut theme.accent,
        "secondary" => &mut theme.secondary,
        "info" => &mut theme.info,
        "danger" => &mut theme.danger,
        _ => return None,
    })
}

/// `#rrggbb`, or `#rrggbbaa` with opacity.
fn parse_color(text: &str) -> Result<Color, String> {
    let hex = text.trim().trim_start_matches('#');
    let valid = matches!(hex.len(), 6 | 8) && hex.chars().all(|c| c.is_ascii_hexdigit());
    if !valid {
        return Err(format!("{text:?} is not a #rrggbb color"));
    }
    let byte = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).unwrap() as f32 / 255.0;
    let alpha = if hex.len() == 8 { byte(6) } else { 1.0 };
    Ok(Color::new(byte(0), byte(2), byte(4), alpha))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::testutil::TempDir;

    #[test]
    fn find_reads_any_case_and_defaults_to_the_first() {
        assert_eq!(find(Some("RETRO")).id, "retro");
        assert_eq!(find(Some("nope")).id, BUILT_IN[0].id);
        assert_eq!(find(None).id, BUILT_IN[0].id);
    }

    #[test]
    fn tones_take_their_color_from_the_theme() {
        let retro = find(Some("retro"));
        assert_eq!(Tone::Muted.of(&retro), retro.muted);
        assert_eq!(Tone::Danger.of(&BUILT_IN[0]), BUILT_IN[0].danger);
    }

    #[test]
    fn built_in_ids_are_unique() {
        let mut ids: Vec<&str> = BUILT_IN.iter().map(|t| t.id.as_str()).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), BUILT_IN.len());
    }

    #[test]
    fn a_tab_with_its_own_color_swaps_the_accent() {
        let theme = Arc::new(Theme {
            tabs: HashMap::from([("mods".to_string(), rgb(0xff0000))]),
            ..midnight()
        });
        assert_eq!(shown(&theme, "mods").accent, rgb(0xff0000));
        assert_eq!(shown(&theme, "library").accent, theme.accent);
    }

    #[test]
    fn an_empty_theme_file_is_midnight_under_the_folder_name() {
        let theme = from_json("Mine", Path::new("."), "{}").unwrap();
        assert_eq!((theme.id.as_str(), theme.name.as_str()), ("Mine", "Mine"));
        assert_eq!(theme.accent, midnight().accent);
        assert_eq!(theme.typeface, Typeface::Barlow);
    }

    #[test]
    fn a_theme_file_sets_colors_shapes_and_tabs() {
        let json = r##"{"radius": 0, "colors": {"bg_top": "#102030", "shadow": "#00000080"},
                       "tabs": {"Library": "#ffd400"}, "future_key": 1}"##;
        let theme = from_json("T", Path::new("."), json).unwrap();
        assert_eq!(theme.radius, 0.0);
        assert_eq!(theme.bg_top, rgb(0x102030));
        assert!((theme.shadow.a - 128.0 / 255.0).abs() < 1e-6);
        assert_eq!(theme.tabs["library"], rgb(0xffd400));
    }

    #[test]
    fn the_shown_theme_follows_both_the_theme_and_the_tab() {
        let red = rgb(0xff0000);
        let theme = Arc::new(Theme {
            tabs: HashMap::from([("mods".to_string(), red)]),
            ..midnight()
        });
        set_tab("mods");
        set(theme.clone());
        assert_eq!(accent(), red);
        set_tab("library");
        assert_eq!(accent(), theme.accent);
        set(find(Some("retro")));
        set_tab("mods");
        assert_eq!(accent(), find(Some("retro")).accent);
    }

    #[test]
    fn sizes_that_would_hide_the_interface_are_refused() {
        let dir = TempDir::new();
        dir.write("Fun.ttf", b"");
        let error =
            from_json("T", dir.path(), r#"{"font": "Fun.ttf", "font_size": 0}"#).unwrap_err();
        assert!(error.contains("font_size"), "{error}");
        assert!(from_json("T", dir.path(), r#"{"radius": -1}"#).is_err());
        assert!(from_json("T", dir.path(), r#"{"focus_width": 99}"#).is_err());
    }

    #[test]
    fn a_bad_color_names_the_value() {
        let error = from_json("T", Path::new("."), r#"{"colors": {"text": "red"}}"#).unwrap_err();
        assert!(error.contains("\"red\""), "{error}");
    }

    #[test]
    fn a_font_file_is_found_next_to_theme_json() {
        let dir = TempDir::new();
        dir.write("Fun.ttf", b"");
        let theme = from_json("T", dir.path(), r#"{"font": "Fun.ttf", "font_size": 20}"#).unwrap();
        assert_eq!(
            theme.typeface,
            Typeface::File {
                path: dir.path().join("Fun.ttf"),
                size: 20.0
            }
        );
        assert!(from_json("T", dir.path(), r#"{"font": "Missing.ttf"}"#).is_err());
        assert_eq!(
            from_json("T", dir.path(), r#"{"font": "Pixel"}"#)
                .unwrap()
                .typeface,
            Typeface::Pixel
        );
    }
}
