//! Settings > APP: this app's own update, from the GitHub project it was
//! built for. Nothing is asked of GitHub until the user presses A, then at
//! most once every few minutes; an update is confirmed first and runs in
//! the background, and takes effect at the next launch.

use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver};
use std::sync::Arc;

use macroquad::prelude::*;

use super::dialog::{Dialog, Outcome};
use super::engine::theme;
use super::form::Field;
use super::{Hint, Ui};
use crate::core::app::DISPLAY_NAME;
use crate::core::package::Progress;
use crate::core::self_update::{self, Available, VERSION};
use crate::core::sizer;

/// The Settings field this screen owns.
pub const FIELD: &str = "APP_UPDATE";
/// Least time between two checks (GitHub limits anonymous requests).
const COOLDOWN_SECS: f64 = 300.0;

enum State {
    NotChecked,
    Checking(Receiver<Result<Option<Available>, String>>),
    UpToDate,
    Failed(String),
    Available(Available),
    Installing {
        tag: String,
        progress: Arc<Progress>,
        done: Receiver<Result<(), String>>,
    },
    /// Installed: runs once the app is started again.
    Installed(String),
}

pub struct AppUpdate {
    repo: &'static str,
    state: State,
    checked_at: Option<f64>,
    confirm: Option<Dialog>,
}

impl AppUpdate {
    /// `None` for a build that names no project to update from.
    pub fn new() -> Option<AppUpdate> {
        Some(AppUpdate {
            repo: self_update::repo()?,
            state: State::NotChecked,
            checked_at: None,
            confirm: None,
        })
    }

    pub fn has_modal(&self) -> bool {
        self.confirm.is_some()
    }

    /// Seconds until a new check is allowed (0: now).
    fn wait(&self) -> f64 {
        self.checked_at
            .map_or(0.0, |at| (at + COOLDOWN_SECS - get_time()).max(0.0))
    }

    /// The Settings row: what A does now, and the state on the right.
    pub fn field(&self) -> Field {
        let current = format!("v{VERSION}");
        let (label, note, color) = match &self.state {
            State::NotChecked => ("Check for updates".into(), current, theme::accent()),
            State::Checking(_) => ("Checking...".into(), current, theme::info()),
            State::UpToDate if self.wait() > 0.0 => ("Up to date".into(), current, theme::muted()),
            State::UpToDate => ("Up to date: check again".into(), current, theme::accent()),
            State::Failed(e) => ("Check again".into(), e.clone(), theme::danger()),
            State::Available(a) => (
                format!("Update to {}", a.release.tag),
                format!("{current} · {}", sizer::human(a.asset.size)),
                theme::accent(),
            ),
            State::Installing { tag, progress, .. } => (
                format!("Updating to {tag}..."),
                percent(progress),
                theme::info(),
            ),
            State::Installed(tag) => (
                format!("Restart {DISPLAY_NAME} to finish"),
                format!("{tag} installed"),
                theme::secondary(),
            ),
        };
        let help = format!(
            "Updates from github.com/{}. Only the app is replaced: settings, games and saves stay.",
            self.repo
        );
        Field::button(FIELD, &label, &note, color).with_help(&help)
    }

    /// A pressed: check, or ask to install what the check found.
    pub fn press(&mut self) {
        match &self.state {
            State::NotChecked | State::Failed(_) => self.check(),
            State::UpToDate if self.wait() == 0.0 => self.check(),
            State::Available(available) => self.confirm = Some(confirm_dialog(available)),
            _ => {}
        }
    }

    fn check(&mut self) {
        let (tx, rx) = channel();
        let repo = self.repo;
        std::thread::spawn(move || {
            let _ = tx.send(self_update::check(repo));
        });
        self.state = State::Checking(rx);
        self.checked_at = Some(get_time());
    }

    fn install(&mut self, target: PathBuf) {
        let State::Available(available) = &self.state else {
            return;
        };
        let (asset, tag) = (available.asset.clone(), available.release.tag.clone());
        let progress = Arc::new(Progress::default());
        let shared = Arc::clone(&progress);
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let _ = tx.send(self_update::install(&asset, &target, &shared));
        });
        self.state = State::Installing {
            tag,
            progress,
            done: rx,
        };
    }

    /// Collect a finished check or install.
    pub fn poll(&mut self) {
        let next = match &self.state {
            State::Checking(rx) => rx.try_recv().ok().map(|found| match found {
                Ok(Some(available)) => State::Available(available),
                Ok(None) => State::UpToDate,
                Err(e) => {
                    log::warn!("{DISPLAY_NAME} update check: {e}");
                    State::Failed(e)
                }
            }),
            State::Installing { tag, done, .. } => {
                done.try_recv().ok().map(|result| match result {
                    Ok(()) => {
                        log::info!("{DISPLAY_NAME} {tag} installed: runs at the next launch");
                        State::Installed(tag.clone())
                    }
                    Err(e) => {
                        log::error!("{DISPLAY_NAME} update failed: {e}");
                        State::Failed(e)
                    }
                })
            }
            _ => None,
        };
        if let Some(state) = next {
            // Nothing was learned (offline?): trying again costs GitHub nothing.
            if matches!(state, State::Failed(_)) {
                self.checked_at = None;
            }
            self.state = state;
        }
    }

    pub fn update_modal(&mut self, ui: &Ui, area: Rect) {
        let Some(dialog) = &mut self.confirm else {
            return;
        };
        match dialog.update(ui, area) {
            Outcome::Open => return,
            Outcome::Chosen(0) => match self_update::target() {
                Ok(target) => self.install(target),
                Err(e) => self.state = State::Failed(e),
            },
            Outcome::Chosen(_) | Outcome::Cancelled => {}
        }
        self.confirm = None;
    }

    pub fn draw_modal(&self, ui: &Ui, area: Rect) {
        if let Some(dialog) = &self.confirm {
            dialog.draw(ui, area);
        }
    }

    pub fn modal_hints(&self, ui: &Ui) -> Option<Vec<Hint>> {
        self.confirm.as_ref().map(|dialog| dialog.hints(ui))
    }
}

fn percent(progress: &Progress) -> String {
    match progress.total() {
        0 => "starting...".into(),
        total => format!("{:.0}%", progress.done() as f64 * 100.0 / total as f64),
    }
}

fn confirm_dialog(available: &Available) -> Dialog {
    let message = format!(
        "Update {DISPLAY_NAME} from v{VERSION} to {}?\n{} · {}\nThe new version runs at the next launch.",
        available.release.tag,
        available.asset.name,
        sizer::human(available.asset.size)
    );
    Dialog::new(
        "Update app",
        &message,
        &[("Update", theme::accent()), ("Cancel", theme::muted())],
    )
}
