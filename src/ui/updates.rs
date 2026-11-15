//! UPDATES: the games whose game.json names a release page. Nothing is asked
//! of GitHub until the user presses X; then one request per game, at most
//! once every few minutes (GitHub limits anonymous requests). A installs the
//! focused game's new release over its files (after a confirmation), Y only
//! records it as installed (the game already is at that release). The shell
//! does both, once the autopilot is not writing game.json files.

use std::collections::BTreeMap;
use std::sync::mpsc::{channel, Receiver, TryRecvError};

use macroquad::prelude::*;

use super::dialog::{Dialog, Outcome};
use super::engine::input::Action;
use super::engine::theme;
use super::jobs::Jobs;
use super::list::List;
use super::style::{self, Toast};
use super::{draw_spinner, Hint, Ui};
use crate::core::models::Game;
use crate::core::sizer;
use crate::core::updater::{self, Asset, Platform, Release};

const ROW_H: f32 = 26.0;
const ROW_STEP: f32 = 30.0;
const GAP: f32 = 12.0;
/// The panel under the list when there is no room beside it.
const BELOW_H: f32 = 120.0;
/// Least time between two checks.
const COOLDOWN_SECS: f64 = 300.0;

pub enum Event {
    /// Download and lay this release file over the game.
    Install {
        game: Box<Game>,
        asset: Asset,
        tag: String,
    },
    /// Record this release as installed, without installing anything.
    MarkInstalled {
        game: Box<Game>,
        tag: String,
        asset: Option<Asset>,
    },
}

/// A focused game's state, as the row chip and the panel show it.
enum Status<'a> {
    NotChecked,
    Checking,
    Failed(&'a str),
    NoFile(&'a Release),
    UpToDate(&'a Release),
    Available(&'a Release, &'a Asset),
}

impl Status<'_> {
    fn chip(&self) -> (&'static str, Color) {
        match self {
            Status::NotChecked => ("NOT CHECKED", theme::muted()),
            Status::Checking => ("CHECKING", theme::info()),
            Status::Failed(_) => ("ERROR", theme::danger()),
            Status::NoFile(_) => ("NO FILE", theme::secondary()),
            Status::UpToDate(_) => ("UP TO DATE", theme::muted()),
            Status::Available(..) => ("UPDATE", theme::accent()),
        }
    }
}

/// A confirmed install waits for the user's yes.
struct Confirm {
    dialog: Dialog,
    game: Game,
    asset: Asset,
    tag: String,
}

/// The release file picked for a game, and what it was picked for: picked
/// again only when the game's platform or `asset` pattern changes.
struct Pick {
    platform: Platform,
    pattern: String,
    asset: Option<Asset>,
}

#[derive(Default)]
pub struct UpdatesScreen {
    list: List,
    /// The latest release of each game (by folder name), as last checked.
    releases: BTreeMap<String, Result<Release, String>>,
    /// The file of each checked release to install (by folder name).
    picks: BTreeMap<String, Pick>,
    /// The check running in the background, one answer per game.
    checking: Option<Receiver<(String, Result<Release, String>)>>,
    checked_at: Option<f64>,
    confirm: Option<Confirm>,
    pub toast: Toast,
}

/// The games with a release page this app may write, in library order
/// (packaged-only games and those of a newer •cartridge are read-only).
fn sources(games: &[Game]) -> Vec<&Game> {
    games
        .iter()
        .filter(|g| g.has_update_url() && g.is_writable())
        .collect()
}

/// `Checked just now` / `Checked 3 min ago`.
fn checked_ago(secs: f64) -> String {
    match (secs / 60.0) as u64 {
        0 => "Checked just now".into(),
        n => format!("Checked {n} min ago"),
    }
}

/// GitHub's alert markers (`> [!NOTE]`): no text of their own.
fn is_alert_marker(line: &str) -> bool {
    line.starts_with("[!") && line.ends_with(']')
}

fn minutes(secs: f64) -> String {
    match (secs / 60.0).ceil() as u64 {
        0 | 1 => "1 min".into(),
        n => format!("{n} min"),
    }
}

impl UpdatesScreen {
    pub fn has_modal(&self) -> bool {
        self.confirm.is_some()
    }

    /// A checked game has a release to install (the tab's icon shows a dot).
    pub fn has_available(&self, games: &[Game]) -> bool {
        sources(games)
            .iter()
            .any(|g| matches!(self.status(g), Status::Available(..)))
    }

    fn status<'a>(&'a self, game: &Game) -> Status<'a> {
        match self.releases.get(&game.folder_name) {
            None if self.checking.is_some() => Status::Checking,
            None => Status::NotChecked,
            Some(Err(e)) => Status::Failed(e),
            Some(Ok(release)) => {
                let asset = self
                    .picks
                    .get(&game.folder_name)
                    .and_then(|p| p.asset.as_ref());
                match asset {
                    _ if updater::is_current(&game.update, release, asset) => {
                        Status::UpToDate(release)
                    }
                    Some(asset) => Status::Available(release, asset),
                    None => Status::NoFile(release),
                }
            }
        }
    }

    /// Seconds until a new check is allowed (0: now).
    fn wait(&self) -> f64 {
        self.checked_at
            .map_or(0.0, |at| (at + COOLDOWN_SECS - get_time()).max(0.0))
    }

    fn can_check(&self) -> bool {
        self.checking.is_none() && self.wait() == 0.0
    }

    fn check(&mut self, games: &[Game]) {
        if !self.can_check() {
            let why = format!("Checked recently: again in {}", minutes(self.wait()));
            self.toast.show(&why, theme::secondary());
            return;
        }
        if sources(games).is_empty() {
            return;
        }
        let pages: Vec<(String, String)> = sources(games)
            .iter()
            .map(|g| (g.folder_name.clone(), g.update.url.clone()))
            .collect();
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            for (folder, page) in pages {
                if tx.send((folder, updater::check(&page))).is_err() {
                    return;
                }
            }
        });
        self.releases.clear();
        self.picks.clear();
        self.checking = Some(rx);
        self.checked_at = Some(get_time());
    }

    /// Collect the answers that arrived, and pick the file of each release
    /// for its game (every frame, whatever the tab: the tab icon shows a dot
    /// for an available update).
    pub fn poll(&mut self, games: &[Game]) {
        self.receive();
        self.pick(games);
    }

    /// Pick again the file of the releases whose game changed platform or
    /// pattern, or that have none picked yet.
    fn pick(&mut self, games: &[Game]) {
        let picked: Vec<(String, Pick)> = sources(games)
            .into_iter()
            .filter_map(|game| {
                let Some(Ok(release)) = self.releases.get(&game.folder_name) else {
                    return None;
                };
                let platform = updater::platform_of(game);
                let pattern = &game.update.asset;
                let fresh = self
                    .picks
                    .get(&game.folder_name)
                    .is_some_and(|p| p.platform == platform && p.pattern == *pattern);
                if fresh {
                    return None;
                }
                let pick = Pick {
                    asset: updater::pick_asset(release, &platform, pattern).cloned(),
                    platform,
                    pattern: pattern.clone(),
                };
                Some((game.folder_name.clone(), pick))
            })
            .collect();
        self.picks.extend(picked);
    }

    fn receive(&mut self) {
        let Some(rx) = &self.checking else { return };
        loop {
            match rx.try_recv() {
                Ok((folder, release)) => {
                    if let Err(e) = &release {
                        log::warn!("Update check of {folder}: {e}");
                    }
                    self.releases.insert(folder, release);
                }
                Err(TryRecvError::Empty) => return,
                Err(TryRecvError::Disconnected) => {
                    self.checking = None;
                    // Nothing was learned (offline?): trying again costs GitHub nothing.
                    if self.releases.values().all(Result::is_err) {
                        self.checked_at = None;
                    }
                    return;
                }
            }
        }
    }

    pub fn update(&mut self, ui: &Ui, area: Rect, games: &[Game], jobs: &Jobs) -> Option<Event> {
        if let Some(confirm) = &mut self.confirm {
            return match confirm.dialog.update(ui, area) {
                Outcome::Open => None,
                Outcome::Chosen(0) => self.confirm.take().map(|c| Event::Install {
                    game: Box::new(c.game),
                    asset: c.asset,
                    tag: c.tag,
                }),
                Outcome::Chosen(_) | Outcome::Cancelled => {
                    self.confirm = None;
                    None
                }
            };
        }
        let shown = sources(games);
        self.list.select(self.list.cursor, shown.len(), |_| true);
        let (list, _) = layout(ui, area);
        let activated = self.list.update(ui, shown.len(), list, ROW_STEP, |_| true);
        if ui.action(Action::Edit) {
            self.check(games);
            return None;
        }
        let game = *shown.get(self.list.cursor)?;
        if jobs.for_game(&game.folder_name).is_some() {
            return None;
        }
        if activated {
            self.ask(game);
            return None;
        }
        if ui.action(Action::Alt) {
            return self.mark_installed(game);
        }
        None
    }

    fn ask(&mut self, game: &Game) {
        let Status::Available(release, asset) = self.status(game) else {
            return;
        };
        let message = format!(
            "Update {} to {}?\n{} · {}\nIts files replace the game's; saves, settings and other files stay.",
            game.title,
            release.tag,
            asset.name,
            sizer::human(asset.size)
        );
        let confirm = Confirm {
            dialog: Dialog::new(
                "Update game",
                &message,
                &[("Update", theme::accent()), ("Cancel", theme::muted())],
            ),
            game: game.clone(),
            asset: asset.clone(),
            tag: release.tag.clone(),
        };
        self.confirm = Some(confirm);
    }

    fn mark_installed(&self, game: &Game) -> Option<Event> {
        let (release, asset) = match self.status(game) {
            Status::Available(release, asset) => (release, Some(asset.clone())),
            Status::NoFile(release) => (release, None),
            _ => return None,
        };
        Some(Event::MarkInstalled {
            game: Box::new(game.clone()),
            tag: release.tag.clone(),
            asset,
        })
    }

    // --- Drawing -------------------------------------------------------------

    pub fn draw(&self, ui: &Ui, area: Rect, games: &[Game], jobs: &Jobs) {
        let shown = sources(games);
        if shown.is_empty() {
            draw_empty(ui, area);
            return;
        }
        let (list, panel) = layout(ui, area);
        for (i, r) in self.list.visible(shown.len(), list, ROW_STEP) {
            self.draw_row(ui, shown[i], i, Rect::new(r.x, r.y, r.w, ROW_H), jobs);
        }
        if let Some(game) = shown.get(self.list.cursor) {
            self.draw_panel(ui, panel, game, jobs);
        }
        self.toast.draw(ui, list);
        if let Some(confirm) = &self.confirm {
            confirm.dialog.draw(ui, area);
        }
    }

    fn draw_row(&self, ui: &Ui, game: &Game, i: usize, frame: Rect, jobs: &Jobs) {
        let focused = i == self.list.cursor;
        let status = self.status(game);
        let (label, color) = match jobs.for_game(&game.folder_name) {
            Some(job) => (
                format!("{:.0}%", job.fraction() * 100.0),
                theme::secondary(),
            ),
            None => {
                let (label, color) = status.chip();
                (label.to_string(), color)
            }
        };
        style::card(frame, color, focused);
        let label_w = style::chip_width(ui, &label) + 12.0;
        style::chip(
            ui,
            &label,
            frame.right() - label_w + 6.0,
            frame.y + 18.0,
            color,
        );
        if matches!(status, Status::Checking) {
            draw_spinner(frame.right() - label_w - 6.0, frame.y + 13.0, 5.0, color);
        }
        let title_color = if focused {
            theme::text()
        } else {
            theme::muted()
        };
        ui.text(
            &ui.truncate(&game.title, frame.w - 36.0 - label_w),
            frame.x + 12.0,
            frame.y + 18.0,
            title_color,
        );
    }

    /// The focused game: its installed and latest releases, the file to
    /// install and the release notes, or what to do first.
    fn draw_panel(&self, ui: &Ui, panel: Rect, game: &Game, jobs: &Jobs) {
        ui.title(
            &ui.truncate(&game.title, panel.w),
            panel.x,
            panel.y + 20.0,
            theme::text(),
        );
        let mut lines = Lines::new(ui, panel, panel.y + 46.0);
        let installed = Some(game.update.installed.as_str())
            .filter(|t| !t.is_empty())
            .unwrap_or("unknown");
        lines.text(&format!("Installed: {installed}"), theme::muted());
        if let Some(job) = jobs.for_game(&game.folder_name) {
            lines.text(
                &format!("{} · {}", job.kind.verb(), job.summary()),
                theme::secondary(),
            );
            return;
        }
        match self.status(game) {
            Status::NotChecked => {
                let wait = self.wait();
                let prompt = if wait > 0.0 {
                    format!("Next check in {}.", minutes(wait))
                } else {
                    format!(
                        "Press {} to check GitHub for new releases.",
                        ui.key(Action::Edit)
                    )
                };
                lines.wrapped(&prompt, theme::text(), 3);
            }
            Status::Checking => lines.text("Asking GitHub...", theme::info()),
            Status::Failed(e) => lines.wrapped(e, theme::danger(), 3),
            Status::NoFile(release) => {
                lines.release(release);
                let platform = updater::platform_of(game);
                let why = format!(
                    "No {} {} build in this release: set \"asset\" in the game's update block to part of the file name.",
                    platform.os, platform.arch
                );
                lines.wrapped(&why, theme::secondary(), 3);
            }
            Status::UpToDate(release) => {
                lines.release(release);
                lines.text("Up to date.", theme::accent());
                lines.notes(release);
            }
            Status::Available(release, asset) => {
                lines.release(release);
                let file = format!("{} · {}", asset.name, sizer::human(asset.size));
                lines.small(&file, theme::muted());
                lines.notes(release);
            }
        }
        let checked = self
            .checked_at
            .filter(|_| self.checking.is_none())
            .map(|at| checked_ago(get_time() - at));
        if let Some(checked) = checked {
            ui.small_text(&checked, panel.x, panel.bottom() - 4.0, theme::muted());
        }
    }

    pub fn hints(&self, ui: &Ui, games: &[Game], jobs: &Jobs) -> Vec<Hint> {
        if let Some(confirm) = &self.confirm {
            return confirm.dialog.hints(ui);
        }
        let shown = sources(games);
        let check = (self.can_check() && !shown.is_empty())
            .then(|| ui.hint(Action::Edit, "Check for updates", theme::info()));
        let Some(game) = shown
            .get(self.list.cursor)
            .filter(|g| jobs.for_game(&g.folder_name).is_none())
        else {
            return check.into_iter().collect();
        };
        let status = self.status(game);
        let install = matches!(status, Status::Available(..))
            .then(|| ui.hint(Action::Confirm, "Update", theme::accent()));
        let mark = matches!(status, Status::Available(..) | Status::NoFile(_))
            .then(|| ui.hint(Action::Alt, "Mark installed", theme::muted()));
        install.into_iter().chain(check).chain(mark).collect()
    }
}

/// Text lines down the panel, stopping at its bottom (kept clear for the
/// "checked" line).
struct Lines<'a> {
    ui: &'a Ui,
    panel: Rect,
    y: f32,
}

impl<'a> Lines<'a> {
    fn new(ui: &'a Ui, panel: Rect, y: f32) -> Self {
        Lines { ui, panel, y }
    }

    fn room(&self) -> bool {
        self.y < self.panel.bottom() - 20.0
    }

    fn text(&mut self, s: &str, color: Color) {
        if self.room() {
            let text = self.ui.truncate(s, self.panel.w);
            self.ui.text(&text, self.panel.x, self.y, color);
        }
        self.y += 20.0;
    }

    fn small(&mut self, s: &str, color: Color) {
        if self.room() {
            let text = self.ui.truncate_small(s, self.panel.w);
            self.ui.small_text(&text, self.panel.x, self.y, color);
        }
        self.y += 18.0;
    }

    fn wrapped(&mut self, s: &str, color: Color, max_lines: usize) {
        for line in self.ui.wrap(s, self.panel.w).into_iter().take(max_lines) {
            self.text(&line, color);
        }
    }

    fn release(&mut self, release: &Release) {
        let date = Some(release.date.as_str())
            .filter(|d| !d.is_empty())
            .map_or(String::new(), |d| format!(" · {d}"));
        self.text(&format!("Latest: {}{date}", release.tag), theme::text());
    }

    /// The release notes, as many lines as fit (Markdown marks dropped).
    fn notes(&mut self, release: &Release) {
        self.y += 6.0;
        let text = release
            .notes
            .lines()
            .map(|l| l.trim().trim_start_matches(['#', '*', '-', '>', ' ']))
            .filter(|l| !l.is_empty() && !is_alert_marker(l))
            .collect::<Vec<_>>()
            .join("\n");
        for paragraph in text.lines() {
            for line in self.ui.wrap(paragraph, self.panel.w) {
                if !self.room() {
                    return;
                }
                self.small(&line, theme::muted());
            }
        }
    }
}

/// (list, panel): the panel beside the list, or under it on narrow screens.
fn layout(ui: &Ui, area: Rect) -> (Rect, Rect) {
    match ui.split(area, 0.5, GAP) {
        (list, Some(panel)) => (list, panel),
        (_, None) => (
            Rect::new(area.x, area.y, area.w, area.h - BELOW_H - GAP),
            Rect::new(area.x, area.bottom() - BELOW_H, area.w, BELOW_H),
        ),
    }
}

fn draw_empty(ui: &Ui, area: Rect) {
    let y = area.y + 30.0;
    ui.text("No game has a release page.", area.x, y, theme::text());
    let help = [
        "In the game editor, set Release page to the game's",
        "GitHub page (github.com/<owner>/<repo>): its new",
        "releases are installed over the game's files.",
    ];
    for (i, line) in help.iter().enumerate() {
        ui.text(line, area.x, y + 30.0 + i as f32 * 20.0, theme::muted());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minutes_round_up_and_never_say_zero() {
        assert_eq!(minutes(0.0), "1 min");
        assert_eq!(minutes(61.0), "2 min");
        assert_eq!(minutes(300.0), "5 min");
    }

    #[test]
    fn a_check_under_a_minute_old_was_just_now() {
        assert_eq!(checked_ago(30.0), "Checked just now");
        assert_eq!(checked_ago(150.0), "Checked 2 min ago");
    }

    #[test]
    fn github_alert_markers_are_not_shown() {
        assert!(is_alert_marker("[!NOTE]"));
        assert!(!is_alert_marker("[link](x) text"));
    }
}
