//! Settings: every `config.txt` key, grouped. Each change is written straight
//! to `config.txt`, touching only that line (comments and order are kept).

use std::collections::BTreeMap;

use macroquad::prelude::*;

use super::app_update::{self, AppUpdate};
use super::engine::theme::{self, Tone};
use super::form::{Event as FormEvent, Field, Form};
use super::kiosk;
use super::{Hint, Ui};
use crate::core::app::debug_mode;
use crate::core::config::{self, Config};
use crate::core::steam;

/// Settings that mean nothing without Steam.
const STEAM_KEYS: [&str; 3] = [SELF_SHORTCUT, "STEAM_USERID", "PROTON_KEEP_DOWNLOADS"];
/// Labelled with the cartridge folder's name, as it appears in Steam.
const SELF_SHORTCUT: &str = "STEAM_ADD_SELF";
const WIDESCREEN: &[&str] = &["auto", "on", "off"];
const WINDOW_MODES: &[&str] = &["borderless", "window"];

enum Kind {
    Text,
    Secret,
    Flag(bool),
    /// One of these values; the first is the default.
    Choice(&'static [&'static str]),
    /// 0 to 100 %, with this default.
    Percent(u32),
    /// A Steam account on this machine; empty means all of them.
    SteamUser,
    /// One of the UI themes; empty means the default.
    Theme,
}

struct Setting {
    key: &'static str,
    label: &'static str,
    kind: Kind,
    help: &'static str,
}

const fn s(key: &'static str, label: &'static str, kind: Kind, help: &'static str) -> Setting {
    Setting {
        key,
        label,
        kind,
        help,
    }
}

/// Shown only in debug mode (`core::app::debug_mode`).
const DEBUG_SECTION: &str = "DEBUG";

const SCHEMA: [(&str, &[Setting]); 5] = [
    ("GENERAL", &[
        s(SELF_SHORTCUT, "Add {} to Steam", Kind::Flag(false), "Add this cartridge to Steam under its folder name, with retro artwork (or your cover.png, hero.png... in system/). Applied right away."),
        s("MUSIC_ENABLED", "Background music", Kind::Flag(true), "Shuffle the audio files in system/ (mp3, flac, ogg, wav, m4a); fades out while a game runs. SELECT (M) turns it on and off anywhere."),
        s("MUSIC_VOLUME", "Music volume", Kind::Percent(50), "How loud the background music plays. Applied right away."),
        s("KIOSK_MODE", "Kiosk mode", Kind::Choice(kiosk::LEVELS), "Children-proof. on: the Library only, to launch games or quit. strict: launching only. To leave it, hold L1 + R1 + X (Q + E + X) for 5 seconds, or set KIOSK_MODE=off in system/config.txt."),
    ]),
    ("DISPLAY", &[
        s("THEME", "Theme", Kind::Theme, "Colors, font and shapes of the interface. The layout stays the same."),
        s("WINDOW_MODE", "Window mode", Kind::Choice(WINDOW_MODES), "borderless: fullscreen at the screen's native resolution. window: a window you can resize (fullscreen anyway under gamescope). F toggles."),
        s("WIDESCREEN", "Widescreen", Kind::Choice(WIDESCREEN), "auto: side panels on screens 3:2 or wider, 4:3 otherwise (taller on portrait screens). on / off: always / never."),
    ]),
    ("STEAM & ARTWORK", &[
        s("STEAM_USERID", "Steam user", Kind::SteamUser, "Only update this Steam user. Default: all users."),
        s("PROTON_KEEP_DOWNLOADS", "Keep Proton downloads", Kind::Flag(false), "Keep what winetricks downloads for Windows games in system/winetricks, so this cartridge brings it to other machines."),
        s("STEAMGRIDDB_API_KEY", "SteamGridDB API key", Kind::Secret, "For automatic artwork: steamgriddb.com/profile/preferences/api"),
        s("STEAMGRIDDB_NSFW", "Artwork: NSFW", Kind::Flag(false), "Include adult content in artwork results."),
        s("STEAMGRIDDB_HUMOR", "Artwork: humor", Kind::Flag(false), "Include humor content in artwork results."),
        s("STEAMGRIDDB_EPILEPSY", "Artwork: epilepsy", Kind::Flag(false), "Include epilepsy-triggering content in artwork results."),
        s("MANIFEST_EXPORT", "Export manifests", Kind::Flag(true), "Write manifests.json for Steam ROM Manager."),
    ]),
    ("SHARING", &[
        s("SHARE_ENABLED", "Share games on the LAN", Kind::Flag(false), "Let other •cartridge machines on the network see and copy your games."),
        s("SHARE_PORT", "Sharing port", Kind::Text, "TCP + UDP port (default 47800). Open it in the firewall if peers can't see this machine."),
        s("SHARE_PEERS", "Extra peers", Kind::Text, "host[:port], comma-separated: machines to list even when broadcast can't reach them."),
    ]),
    (DEBUG_SECTION, &[
        s("DEBUG_NO_STEAM", "Pretend Steam is missing", Kind::Flag(false), "Hide the Steam buttons and dim the Steam settings, as on a machine without Steam. Interface only."),
        s("DEBUG_FAKE_PROGRESS", "Fake update progress", Kind::Flag(false), "Loop a fake library update in the Library's progress card, to see it. Interface only: nothing runs."),
    ]),
];

fn theme_options() -> Vec<(String, String)> {
    theme::all()
        .iter()
        .map(|t| (t.id.clone(), t.name.clone()))
        .collect()
}

fn steam_user_options() -> Vec<(String, String)> {
    std::iter::once((String::new(), "All users".to_string()))
        .chain(steam::users())
        .collect()
}

fn fields(cfg: &Config) -> Vec<Field> {
    let raw = cfg.raw();
    let value = |key: &str| raw.get(key).map_or("", String::as_str);
    let steam_found = steam::detected(cfg);
    SCHEMA
        .iter()
        .filter(|(category, _)| *category != DEBUG_SECTION || debug_mode())
        .flat_map(|(category, settings)| {
            let fields = settings.iter().map(|s| {
                let v = value(s.key);
                let label = if s.key == SELF_SHORTCUT {
                    s.label.replace("{}", &cfg.label())
                } else {
                    s.label.to_string()
                };
                let label = label.as_str();
                let field = match s.kind {
                    Kind::Text => Field::text(s.key, label, v),
                    Kind::Secret => Field::secret(s.key, label, v),
                    Kind::Flag(default) => Field::flag(s.key, label, v, default),
                    Kind::Percent(default) => Field::percent(s.key, label, v, default, 5),
                    Kind::Choice(options) => {
                        let v = if v.is_empty() { options[0] } else { v };
                        Field::choice(s.key, label, v, options)
                    }
                    Kind::SteamUser => {
                        Field::choice_labelled(s.key, label, v, steam_user_options())
                    }
                    Kind::Theme => {
                        let v = theme::find(Some(v)).id.clone();
                        Field::choice_labelled(s.key, label, &v, theme_options())
                    }
                };
                field
                    .with_help(s.help)
                    .disabled_if(!steam_found && STEAM_KEYS.contains(&s.key))
            });
            std::iter::once(Field::section(category)).chain(fields)
        })
        .collect()
}

pub struct Settings {
    form: Form,
    message: Option<(String, Tone)>,
    /// The APP section, for builds that name a project to update from.
    app_update: Option<AppUpdate>,
}

/// The config fields, then the APP section when there is one.
fn all_fields(cfg: &Config, app_update: Option<&AppUpdate>) -> Vec<Field> {
    let app = app_update.map(|update| [Field::section("APP"), update.field()]);
    fields(cfg)
        .into_iter()
        .chain(app.into_iter().flatten())
        .collect()
}

impl Settings {
    pub fn new(cfg: &Config) -> Settings {
        let app_update = AppUpdate::new();
        Settings {
            form: Form::new(all_fields(cfg, app_update.as_ref())),
            message: None,
            app_update,
        }
    }

    /// Rebuild from a reloaded config, keeping the cursor.
    pub fn refresh(&mut self, cfg: &Config) {
        self.form
            .set_fields(all_fields(cfg, self.app_update.as_ref()));
    }

    pub fn has_modal(&self) -> bool {
        self.form.has_modal() || self.app_update.as_ref().is_some_and(AppUpdate::has_modal)
    }

    /// Show the APP row as its update stands now.
    fn sync_app_update(&mut self) {
        let Some(update) = &mut self.app_update else {
            return;
        };
        update.poll();
        let field = update.field();
        if let Some(row) = self
            .form
            .fields
            .iter_mut()
            .find(|f| f.id == app_update::FIELD)
        {
            *row = field;
        }
    }

    fn form_area(area: Rect) -> Rect {
        Rect::new(area.x, area.y, area.w, area.h - 22.0)
    }

    /// Show a message under the form (e.g. what applying a setting did).
    pub fn note(&mut self, message: &str, tone: Tone) {
        self.message = Some((message.to_string(), tone));
    }

    /// The key saved to `config.txt`, if any (the app reloads it).
    pub fn update(&mut self, ui: &mut Ui, area: Rect, cfg: &Config) -> Option<String> {
        self.sync_app_update();
        if let Some(update) = self.app_update.as_mut().filter(|u| u.has_modal()) {
            update.update_modal(ui, area);
            return None;
        }
        let id = match self.form.update(ui, Settings::form_area(area))? {
            FormEvent::Changed(id) => id,
            FormEvent::Pressed(id) => {
                if let Some(update) = self.app_update.as_mut().filter(|_| id == app_update::FIELD) {
                    update.press();
                }
                return None;
            }
        };
        let value = self.form.value(&id).to_string();
        match config::update_file(&cfg.path, &BTreeMap::from([(id.clone(), value)])) {
            Ok(()) => {
                self.message = Some((format!("Saved {id}."), Tone::Accent));
                Some(id)
            }
            Err(e) => {
                self.message = Some((format!("Could not save: {e}"), Tone::Danger));
                None
            }
        }
    }

    pub fn draw(&self, ui: &Ui, area: Rect) {
        if let Some((message, tone)) = &self.message {
            ui.text(
                &ui.truncate(message, area.w),
                area.x,
                area.bottom() - 4.0,
                tone.color(),
            );
        }
        self.form.draw(ui, Settings::form_area(area));
        if let Some(update) = &self.app_update {
            update.draw_modal(ui, area);
        }
    }

    pub fn hints(&self, ui: &Ui) -> Vec<Hint> {
        self.app_update
            .as_ref()
            .and_then(|update| update.modal_hints(ui))
            .unwrap_or_else(|| self.form.hints(ui))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_setting_is_documented_in_the_default_config() {
        let keys = SCHEMA.iter().flat_map(|(_, settings)| settings.iter());
        for setting in keys {
            let line = format!("{}=", setting.key);
            assert!(config::DEFAULT_CONFIG.contains(&line), "{}", setting.key);
        }
    }
}
