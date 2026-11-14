//! SAVES: each game's saves on this machine and in its backup, side by side
//! like two memory cards. A backs the focused game's saves up, X restores
//! its backup, both after a confirmation. Only the focused game's folders
//! are read, in the background; copies run in the background too, behind a
//! modal that keeps the user (and a game launch) out until they end.

use std::sync::mpsc::{channel, Receiver};

use macroquad::prelude::*;

use super::dialog::{Dialog, Outcome};
use super::engine::input::Action;
use super::engine::theme;
use super::list::List;
use super::style::{self, Toast};
use super::{centered, draw_spinner, Hint, Ui};
use crate::core::config::Config;
use crate::core::models::Game;
use crate::core::saver::{self, Report, Saves, Stamp};
use crate::core::sizer;

const ROW_H: f32 = 26.0;
const ROW_STEP: f32 = 30.0;
const GAP: f32 = 12.0;
const CARD_H: f32 = 66.0;
/// The panel under the list when there is no room beside it.
const BELOW_H: f32 = 120.0;

#[derive(Clone, Copy)]
enum Direction {
    Backup,
    Restore,
}

#[derive(Debug, PartialEq)]
enum Side {
    Device,
    Backup,
}

/// The focused game's saves, as last read.
struct Shown {
    folder: String,
    title: String,
    saves: Saves,
    report: Report,
}

/// A copy waiting for the user's yes, with the saves it was asked for.
struct Confirm {
    dialog: Dialog,
    direction: Direction,
    saves: Saves,
    title: String,
}

#[derive(Default)]
pub struct SavesScreen {
    list: List,
    shown: Option<Shown>,
    /// A read of one game's folders running in the background (one at a
    /// time: holding Down doesn't start one per game).
    reading: Option<Receiver<Shown>>,
    /// A backup or restore running; its result message.
    copying: Option<Receiver<Result<String, String>>>,
    confirm: Option<Confirm>,
    toast: Toast,
}

/// Read a game's save folders and backup in the background.
fn read(game: &Game, cfg: &Config) -> Receiver<Shown> {
    let (tx, rx) = channel();
    let (game, saves_root) = (game.clone(), cfg.saves_copies_dir());
    std::thread::spawn(move || {
        let saves = saver::saves_of(&game, &saves_root);
        if let Err(e) = saver::recover(&saves) {
            log::error!("{}: {e}", game.title);
        }
        let report = saver::report(&saves);
        let (folder, title) = (game.folder_name, game.title);
        let _ = tx.send(Shown {
            folder,
            title,
            saves,
            report,
        });
    });
    rx
}

fn date(stamp: &Stamp) -> String {
    chrono::DateTime::<chrono::Local>::from(stamp.modified)
        .format("%Y-%m-%d %H:%M:%S")
        .to_string()
}

fn contents(stamp: &Stamp) -> String {
    let files = match stamp.files {
        1 => "1 file".to_string(),
        n => format!("{n} files"),
    };
    format!("{files} · {}", sizer::human(stamp.bytes))
}

/// The side whose newest file is more recent, when they differ.
fn newer(report: &Report) -> Option<Side> {
    let (device, backup) = (report.device?.modified, report.backup?.modified);
    match device.cmp(&backup) {
        std::cmp::Ordering::Greater => Some(Side::Device),
        std::cmp::Ordering::Less => Some(Side::Backup),
        std::cmp::Ordering::Equal => None,
    }
}

/// What a confirmation says before replacing one side with the other: the
/// question, both dates on their own lines, then what will be lost.
fn question(shown: &Shown, direction: Direction) -> String {
    let report = &shown.report;
    let newer = newer(report);
    let (ask, warning) = match direction {
        Direction::Backup => (
            format!(
                "Replace the backup of {} with the saves on this device?",
                shown.title
            ),
            (newer == Some(Side::Backup)).then_some("The backup is newer: it will be lost."),
        ),
        Direction::Restore => (
            format!(
                "Replace the saves of {} on this device with the backup?",
                shown.title
            ),
            Some(if newer == Some(Side::Device) {
                "The saves on this device are newer: they will be lost. Close the game first."
            } else {
                "Close the game first."
            }),
        ),
    };
    let side = |stamp: Option<Stamp>| stamp.as_ref().map_or("none".into(), date);
    [
        ask,
        format!("Device: {}", side(report.device)),
        format!("Backup: {}", side(report.backup)),
    ]
    .into_iter()
    .chain(warning.map(String::from))
    .collect::<Vec<_>>()
    .join("\n")
}

/// Why nothing can be done, when the cards don't already say it.
fn note(shown: &Shown) -> Option<&str> {
    let report = &shown.report;
    if shown.saves.folders.is_empty() {
        return report.cannot_back_up.as_deref();
    }
    let backup = report
        .cannot_back_up
        .as_deref()
        .filter(|_| report.device.is_some());
    let restore = report
        .cannot_restore
        .as_deref()
        .filter(|_| report.backup.is_some());
    backup.or(restore)
}

impl SavesScreen {
    /// A dialog or a copy is up: tabs and Back stay with this screen.
    pub fn has_modal(&self) -> bool {
        self.confirm.is_some() || self.copying.is_some()
    }

    /// Read the focused game again when the tab is shown (a game may have
    /// saved since).
    pub fn forget(&mut self) {
        self.shown = None;
    }

    fn current<'a>(&'a self, games: &[Game]) -> Option<&'a Shown> {
        let focused = games.get(self.list.cursor)?;
        self.shown
            .as_ref()
            .filter(|s| s.folder == focused.folder_name)
    }

    /// Collect a finished read or copy; a copy is followed by a fresh read.
    pub fn poll(&mut self) {
        if let Some(shown) = self.reading.as_ref().and_then(|rx| rx.try_recv().ok()) {
            self.reading = None;
            self.shown = Some(shown);
        }
        let Some(result) = self.copying.as_ref().and_then(|rx| rx.try_recv().ok()) else {
            return;
        };
        self.copying = None;
        self.shown = None;
        match result {
            Ok(message) => self.toast.show(&message, theme::accent()),
            Err(message) => self.toast.show(&message, theme::danger()),
        }
    }

    pub fn update(&mut self, ui: &Ui, area: Rect, games: &[Game], cfg: &Config) {
        if self.copying.is_some() {
            return;
        }
        if let Some(confirm) = &mut self.confirm {
            match confirm.dialog.update(ui, area) {
                Outcome::Open => {}
                Outcome::Chosen(0) => {
                    if let Some(confirm) = self.confirm.take() {
                        self.start_copy(confirm);
                    }
                }
                Outcome::Chosen(_) | Outcome::Cancelled => self.confirm = None,
            }
            return;
        }
        // The library may have lost games since.
        self.list.select(self.list.cursor, games.len(), |_| true);
        let (list, _) = layout(ui, area);
        let backup = self.list.update(ui, games.len(), list, ROW_STEP, |_| true);
        self.read_focused(games, cfg);
        if backup {
            self.ask(games, Direction::Backup);
        } else if ui.action(Action::Edit) {
            self.ask(games, Direction::Restore);
        }
    }

    fn read_focused(&mut self, games: &[Game], cfg: &Config) {
        let Some(game) = games.get(self.list.cursor) else {
            return;
        };
        if self.current(games).is_none() && self.reading.is_none() {
            self.reading = Some(read(game, cfg));
        }
    }

    fn ask(&mut self, games: &[Game], direction: Direction) {
        let Some(shown) = self.current(games) else {
            return;
        };
        let (refusal, title, label, color) = match direction {
            Direction::Backup => (
                &shown.report.cannot_back_up,
                "Back up saves",
                "Back up",
                theme::accent(),
            ),
            Direction::Restore => (
                &shown.report.cannot_restore,
                "Restore saves",
                "Restore",
                theme::danger(),
            ),
        };
        if let Some(why) = refusal.clone() {
            self.toast.show(&why, theme::secondary());
            return;
        }
        self.confirm = Some(Confirm {
            dialog: Dialog::new(
                title,
                &question(shown, direction),
                &[(label, color), ("Cancel", theme::muted())],
            ),
            direction,
            saves: shown.saves.clone(),
            title: shown.title.clone(),
        });
    }

    fn start_copy(&mut self, confirm: Confirm) {
        let Confirm {
            direction,
            saves,
            title,
            ..
        } = confirm;
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let result = match direction {
                Direction::Backup => saver::backup(&saves).map(|()| format!("{title}: backed up.")),
                Direction::Restore => {
                    saver::restore(&saves).map(|()| format!("{title}: backup restored."))
                }
            };
            let _ = tx.send(result.map_err(|e| format!("{title}: {e}")));
        });
        self.copying = Some(rx);
    }

    // --- Drawing -------------------------------------------------------------

    pub fn draw(&self, ui: &Ui, area: Rect, games: &[Game]) {
        if games.is_empty() {
            ui.text("No games.", area.x, area.y + 30.0, theme::text());
            return;
        }
        let (list, panel) = layout(ui, area);
        for (i, r) in self.list.visible(games.len(), list, ROW_STEP) {
            self.draw_row(ui, &games[i], i, Rect::new(r.x, r.y, r.w, ROW_H));
        }
        match self.current(games) {
            Some(shown) => draw_panel(ui, panel, shown),
            None => {
                draw_spinner(panel.x + 6.0, panel.y + 24.0, 6.0, theme::accent());
                ui.text(
                    "Reading saves...",
                    panel.x + 20.0,
                    panel.y + 30.0,
                    theme::muted(),
                );
            }
        }
        self.toast.draw(ui, list);
        if let Some(confirm) = &self.confirm {
            confirm.dialog.draw(ui, area);
        }
        if self.copying.is_some() {
            draw_copying(ui, area);
        }
    }

    fn draw_row(&self, ui: &Ui, game: &Game, i: usize, frame: Rect) {
        let focused = i == self.list.cursor;
        let stripe = if focused {
            theme::accent()
        } else {
            theme::secondary()
        };
        style::card(frame, stripe, focused);
        let color = if focused {
            theme::text()
        } else {
            theme::muted()
        };
        ui.text(
            &ui.truncate(&game.title, frame.w - 18.0),
            frame.x + 12.0,
            frame.y + 18.0,
            color,
        );
    }

    pub fn hints(&self, ui: &Ui, games: &[Game]) -> Vec<Hint> {
        if let Some(confirm) = &self.confirm {
            return confirm.dialog.hints(ui);
        }
        if self.copying.is_some() {
            return Vec::new();
        }
        let Some(shown) = self.current(games) else {
            return Vec::new();
        };
        let backup = shown
            .report
            .cannot_back_up
            .is_none()
            .then(|| ui.hint(Action::Confirm, "Back up", theme::accent()));
        let restore = shown
            .report
            .cannot_restore
            .is_none()
            .then(|| ui.hint(Action::Edit, "Restore", theme::danger()));
        backup.into_iter().chain(restore).collect()
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

/// The game's title, its two cards (stacked, or side by side when wide
/// enough), then what stops a copy and where the saves are.
fn draw_panel(ui: &Ui, panel: Rect, shown: &Shown) {
    ui.title(
        &ui.truncate(&shown.title, panel.w),
        panel.x,
        panel.y + 20.0,
        theme::text(),
    );
    let top = panel.y + 32.0;
    let card_w = ui.measure("0000-00-00 00:00:00") + 24.0;
    let beside = panel.w >= 2.0 * card_w + GAP;
    let (device, backup) = if beside {
        let w = (panel.w - GAP) / 2.0;
        (
            Rect::new(panel.x, top, w, CARD_H),
            Rect::new(panel.x + w + GAP, top, w, CARD_H),
        )
    } else {
        (
            Rect::new(panel.x, top, panel.w, CARD_H),
            Rect::new(panel.x, top + CARD_H + 8.0, panel.w, CARD_H),
        )
    };
    let newer = newer(&shown.report);
    let device_label = if shown.saves.in_game {
        "IN GAME"
    } else {
        "THIS DEVICE"
    };
    let no_path = shown.saves.folders.is_empty();
    let device_empty = if no_path { "NO SAVE PATH" } else { "NO SAVE" };
    draw_card(
        ui,
        device,
        device_label,
        shown.report.device.as_ref(),
        device_empty,
        newer == Some(Side::Device),
    );
    let backup_empty = if no_path { "NO SAVE PATH" } else { "NO BACKUP" };
    draw_card(
        ui,
        backup,
        "BACKUP",
        shown.report.backup.as_ref(),
        backup_empty,
        newer == Some(Side::Backup),
    );
    let mut y = backup.bottom() + 20.0;
    if let Some(note) = note(shown) {
        for line in ui.wrap(note, panel.w).into_iter().take(2) {
            ui.text(&line, panel.x, y, theme::secondary());
            y += 20.0;
        }
    }
    for folder in &shown.saves.folders {
        if y > panel.bottom() {
            break;
        }
        let path = folder.device.display().to_string();
        ui.small_text(
            &ui.truncate_start(&path, panel.w),
            panel.x,
            y,
            theme::muted(),
        );
        y += 16.0;
    }
}

fn draw_card(ui: &Ui, r: Rect, label: &str, stamp: Option<&Stamp>, empty: &str, newer: bool) {
    style::well(r);
    let x = r.x + 10.0;
    ui.small_text(label, x, r.y + 16.0, theme::muted());
    if newer {
        let chip_x = r.right() - 10.0 - style::chip_width(ui, "NEWER");
        style::chip(ui, "NEWER", chip_x, r.y + 16.0, theme::accent());
    }
    match stamp {
        Some(stamp) => {
            ui.text(&date(stamp), x, r.y + 38.0, theme::text());
            ui.small_text(&contents(stamp), x, r.y + 56.0, theme::muted());
        }
        None => ui.text(empty, x, r.y + 38.0, theme::secondary()),
    }
}

fn draw_copying(ui: &Ui, area: Rect) {
    let frame = centered(area, 220.0, 56.0);
    style::panel(ui, frame, theme::secondary());
    draw_spinner(frame.x + 22.0, frame.center().y - 5.0, 6.0, theme::accent());
    ui.text(
        "Copying...",
        frame.x + 40.0,
        frame.center().y + 1.0,
        theme::text(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};

    fn stamp(secs: u64) -> Option<Stamp> {
        Some(Stamp {
            modified: SystemTime::UNIX_EPOCH + Duration::from_secs(secs),
            files: 1,
            bytes: 1,
        })
    }

    fn report(device: Option<Stamp>, backup: Option<Stamp>) -> Report {
        Report {
            device,
            backup,
            cannot_back_up: None,
            cannot_restore: None,
        }
    }

    #[test]
    fn the_newer_side_is_the_one_with_the_most_recent_file() {
        assert_eq!(newer(&report(stamp(2), stamp(1))), Some(Side::Device));
        assert_eq!(newer(&report(stamp(1), stamp(2))), Some(Side::Backup));
        assert_eq!(newer(&report(stamp(1), stamp(1))), None);
        assert_eq!(newer(&report(stamp(1), None)), None);
    }

    #[test]
    fn restoring_over_newer_saves_says_they_will_be_lost() {
        let dir = crate::core::testutil::TempDir::new();
        let game = Game::skeleton(&dir.path().join("games/Game"));
        let shown = Shown {
            folder: "Game".into(),
            title: "Celeste".into(),
            saves: saver::saves_of(&game, &dir.path().join("saves-copies")),
            report: report(stamp(2), stamp(1)),
        };
        let restore = question(&shown, Direction::Restore);
        assert_eq!(restore.lines().count(), 4);
        assert!(restore.ends_with("they will be lost. Close the game first."));
        assert_eq!(question(&shown, Direction::Backup).lines().count(), 3);
    }

    #[test]
    fn contents_count_files_and_size() {
        let one = Stamp {
            modified: SystemTime::UNIX_EPOCH,
            files: 1,
            bytes: 10,
        };
        assert_eq!(contents(&one), "1 file · 10 B");
        assert_eq!(contents(&Stamp { files: 3, ..one }), "3 files · 10 B");
    }
}
