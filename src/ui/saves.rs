//! SAVES: each game's saves on this machine and in its backup, side by side
//! like two memory cards. A backs the focused game's saves up, X restores
//! its backup, both after a confirmation when they replace something.
//!
//! Under them, NETWORK: the same two cards for each other •cartridge
//! sharing this game. Right moves into them, the d-pad picks one; A gets it
//! as the saves here, X as the backup here. Only the focused game's folders
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
use crate::core::saver::{self, Report, Saves, Side, Stamp};
use crate::core::share::discovery::Peer;
use crate::core::share::{self, client, RemoteSaves};
use crate::core::sizer;

const ROW_H: f32 = 26.0;
const ROW_STEP: f32 = 30.0;
const GAP: f32 = 12.0;
const CARD_H: f32 = 66.0;
const CARD_GAP: f32 = 8.0;
/// The panel under the list when there is no room beside it.
const BELOW_H: f32 = 120.0;

#[derive(Clone)]
enum Direction {
    Backup,
    Restore,
    Get(Get),
}

/// A copy from another machine: which of its sides, into which side here.
#[derive(Clone)]
struct Get {
    peer: Peer,
    side: Side,
    into: Side,
    /// What its listing announced.
    stamp: Stamp,
}

/// The focused game's saves, as last read.
struct Shown {
    folder: String,
    title: String,
    saves: Saves,
    report: Report,
}

/// A copy of one game's saves.
#[derive(Clone)]
struct Copy {
    direction: Direction,
    saves: Saves,
    folder: String,
    title: String,
    /// Back the saves here up before the copy replaces them.
    backup_first: bool,
}

/// A copy waiting for the user's yes: one copy per option, Cancel last.
struct Confirm {
    dialog: Dialog,
    choices: Vec<Copy>,
}

/// Other machines found on the network.
#[derive(Default)]
struct Peers {
    /// Those sharing saves.
    sharing: Vec<Peer>,
    /// Why the others can't, one line each.
    outdated: Vec<String>,
}

/// One machine's saves of the focused game.
struct RemoteRow {
    peer: Peer,
    saves: RemoteSaves,
}

impl RemoteRow {
    fn stamp(&self, side: Side) -> Option<&Stamp> {
        match side {
            Side::Device => self.saves.device.as_ref(),
            Side::Backup => self.saves.backup.as_ref(),
        }
    }
}

/// The focused game's saves on other machines, as last read.
struct Remote {
    folder: String,
    rows: Vec<RemoteRow>,
    /// The screen's [`SavesScreen::generation`] when the read started.
    generation: u64,
}

impl Remote {
    /// Read for `folder` since the last search and tab opening.
    fn is_fresh(&self, folder: &str, generation: u64) -> bool {
        self.folder == folder && self.generation == generation
    }
}

/// Where the d-pad is: the game list, or one card of a machine's row.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Focus {
    #[default]
    List,
    Card {
        row: usize,
        side: Side,
    },
}

#[derive(Default)]
pub struct SavesScreen {
    list: List,
    shown: Option<Shown>,
    /// A read of one game's folders running in the background (one at a
    /// time: holding Down doesn't start one per game).
    reading: Option<Receiver<Shown>>,
    /// A backup, restore or copy running; its result message.
    copying: Option<Receiver<Result<String, String>>>,
    confirm: Option<Confirm>,
    toast: Toast,
    /// Searched once each time the tab is shown.
    searched: bool,
    searching: Option<Receiver<Peers>>,
    peers: Peers,
    remote: Option<Remote>,
    /// A read of the focused game's saves on every peer (one at a time too).
    reading_remote: Option<Receiver<Remote>>,
    /// Goes up when the tab is shown and when a search ends: reads made
    /// before are shown until a new one replaces them, never blanked.
    generation: u64,
    focus: Focus,
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

/// Find the other machines in the background.
fn search(cfg: &Config) -> Receiver<Peers> {
    let (tx, rx) = channel();
    let cfg = cfg.clone();
    std::thread::spawn(move || {
        let (sharing, older): (Vec<Peer>, Vec<Peer>) = by_name(client::find_peers(&cfg))
            .into_iter()
            .partition(|peer| peer.info.mismatch().is_none() && peer.info.has(share::SAVES));
        let outdated = older
            .iter()
            .map(|peer| {
                peer.info.mismatch().unwrap_or_else(|| {
                    format!(
                        "{} runs an older •cartridge: update it to copy saves",
                        peer.info.name
                    )
                })
            })
            .collect();
        let _ = tx.send(Peers { sharing, outdated });
    });
    rx
}

/// Machines by name, so their rows keep their places from one search to
/// the next (answers arrive in any order).
fn by_name(mut peers: Vec<Peer>) -> Vec<Peer> {
    peers.sort_by_key(|peer| (peer.info.name.to_lowercase(), peer.addr));
    peers
}

/// Read a game's saves on every peer in the background; peers without the
/// game have no row.
fn read_remote(folder: &str, peers: &[Peer], generation: u64) -> Receiver<Remote> {
    let (tx, rx) = channel();
    let (folder, peers) = (folder.to_string(), peers.to_vec());
    std::thread::spawn(move || {
        // One thread per peer: a machine gone since the search delays the
        // panel by its timeout once, not once more per machine after it.
        let rows = std::thread::scope(|scope| {
            let reads: Vec<_> = peers
                .into_iter()
                .map(|peer| scope.spawn(|| remote_row(peer, &folder)))
                .collect();
            reads
                .into_iter()
                .filter_map(|read| read.join().ok().flatten())
                .collect()
        });
        let _ = tx.send(Remote {
            folder,
            rows,
            generation,
        });
    });
    rx
}

/// `peer`'s saves of `folder`; none when it doesn't have the game or can't
/// be reached.
fn remote_row(peer: Peer, folder: &str) -> Option<RemoteRow> {
    match client::saves(peer.addr, folder) {
        Ok(saves) => Some(RemoteRow {
            peer,
            saves: saves?,
        }),
        Err(e) => {
            log::warn!("Saves of {folder} on {}: {e}", peer.info.name);
            None
        }
    }
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

/// `a` holds newer files than `b`, or `b` holds none. To the second: what
/// a copy between machines keeps.
fn newer_than(a: &Stamp, b: Option<&Stamp>) -> bool {
    b.is_none_or(|b| a.seconds() > b.seconds())
}

/// The side whose newest file is more recent, when they differ.
fn newer(report: &Report) -> Option<Side> {
    let (device, backup) = (report.device?, report.backup?);
    match device.seconds().cmp(&backup.seconds()) {
        std::cmp::Ordering::Greater => Some(Side::Device),
        std::cmp::Ordering::Less => Some(Side::Backup),
        std::cmp::Ordering::Equal => None,
    }
}

/// How a machine's two cards sit: the d-pad follows what the eye sees.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Arrangement {
    Beside,
    Stacked,
}

/// The date (to the second) of the newest card on the network, when it is
/// newer than the saves on this device: the cards to flag NEWER.
fn newest_remote(rows: &[RemoteRow], device: Option<&Stamp>) -> Option<u64> {
    rows.iter()
        .flat_map(|row| [row.saves.device, row.saves.backup])
        .flatten()
        .filter(|stamp| newer_than(stamp, device))
        .map(|stamp| stamp.seconds())
        .max()
}

/// The focus after a d-pad press, with `rows` machines listed: Right enters
/// the first machine's saves, Left from them goes back to the list.
fn step(focus: Focus, action: Action, rows: usize, arrangement: Arrangement) -> Focus {
    match arrangement {
        Arrangement::Beside => step_beside(focus, action, rows),
        Arrangement::Stacked => step_stacked(focus, action, rows),
    }
}

/// Left/Right between a machine's saves and backup, Up/Down between machines.
fn step_beside(focus: Focus, action: Action, rows: usize) -> Focus {
    let last = rows.saturating_sub(1);
    match (focus, action) {
        (Focus::List, Action::Right) if rows > 0 => Focus::Card {
            row: 0,
            side: Side::Device,
        },
        (
            Focus::Card {
                side: Side::Device, ..
            },
            Action::Left,
        ) => Focus::List,
        (Focus::Card { row, .. }, Action::Left) => Focus::Card {
            row,
            side: Side::Device,
        },
        (Focus::Card { row, .. }, Action::Right) => Focus::Card {
            row,
            side: Side::Backup,
        },
        (Focus::Card { row, side }, Action::Up) => Focus::Card {
            row: row.saturating_sub(1),
            side,
        },
        (Focus::Card { row, side }, Action::Down) => Focus::Card {
            row: (row + 1).min(last),
            side,
        },
        _ => focus,
    }
}

/// Up/Down through every card in turn (saves, backup, next machine...),
/// Left back to the list.
fn step_stacked(focus: Focus, action: Action, rows: usize) -> Focus {
    let card = |index: usize| Focus::Card {
        row: index / 2,
        side: if index.is_multiple_of(2) {
            Side::Device
        } else {
            Side::Backup
        },
    };
    let Focus::Card { row, side } = focus else {
        return step_beside(focus, action, rows);
    };
    let index = row * 2 + usize::from(side == Side::Backup);
    match action {
        Action::Left => Focus::List,
        Action::Up => card(index.saturating_sub(1)),
        Action::Down => card((index + 1).min(rows * 2 - 1)),
        _ => focus,
    }
}

/// The focus kept on a row that still exists, or back on the list.
fn clamp(focus: Focus, rows: usize) -> Focus {
    match focus {
        Focus::Card { .. } if rows == 0 => Focus::List,
        Focus::Card { row, side } => Focus::Card {
            row: row.min(rows - 1),
            side,
        },
        Focus::List => Focus::List,
    }
}

/// The rows shown when `fit` fit (at least the focused one), scrolled so
/// the focused row is one of them.
fn window(focused: usize, rows: usize, fit: usize) -> std::ops::Range<usize> {
    let fit = fit.max(1);
    let first = (focused + 1).saturating_sub(fit);
    first..rows.min(first + fit)
}

/// What a confirmation says before replacing one side with the other: the
/// question, both dates on their own lines, then what will be lost.
fn question(shown: &Shown, direction: &Direction) -> String {
    let report = &shown.report;
    let side = |stamp: Option<&Stamp>| stamp.map_or("none".into(), date);
    let title = &shown.title;
    let (ask, here, there, warning) = match direction {
        Direction::Backup => (
            format!("Replace the backup of {title} with the saves on this device?"),
            format!("Device: {}", side(report.device.as_ref())),
            format!("Backup: {}", side(report.backup.as_ref())),
            (newer(report) == Some(Side::Backup))
                .then_some("The backup is newer: it will be lost."),
        ),
        Direction::Restore => (
            format!("Replace the saves of {title} on this device with the backup?"),
            format!("Device: {}", side(report.device.as_ref())),
            format!("Backup: {}", side(report.backup.as_ref())),
            Some(if newer(report) == Some(Side::Device) {
                "The saves on this device are newer: they will be lost. Close the game first."
            } else {
                "Close the game first."
            }),
        ),
        Direction::Get(get) => {
            let name = &get.peer.info.name;
            let what = match get.side {
                Side::Device => "the saves",
                Side::Backup => "the backup",
            };
            let there = format!("{name}: {}", date(&get.stamp));
            match get.into {
                Side::Device => (
                    format!("Replace the saves of {title} on this device with {what} on {name}?"),
                    format!("This device: {}", side(report.device.as_ref())),
                    there,
                    Some(
                        if report
                            .device
                            .is_some_and(|d| newer_than(&d, Some(&get.stamp)))
                        {
                            "The saves on this device are newer: they will be lost. Close the game first."
                        } else {
                            "Close the game first."
                        },
                    ),
                ),
                Side::Backup => (
                    format!("Replace the backup of {title} with {what} on {name}?"),
                    format!("Backup: {}", side(report.backup.as_ref())),
                    there,
                    report
                        .backup
                        .is_some_and(|b| newer_than(&b, Some(&get.stamp)))
                        .then_some("The backup is newer: it will be lost."),
                ),
            }
        }
    };
    [ask, here, there]
        .into_iter()
        .chain(warning.map(String::from))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The side a copy writes holds nothing yet: no need to confirm.
fn replaces_nothing(report: &Report, direction: &Direction) -> bool {
    let into = match direction {
        Direction::Backup => Side::Backup,
        Direction::Restore => Side::Device,
        Direction::Get(get) => get.into,
    };
    match into {
        Side::Backup => report.backup.is_none(),
        Side::Device => report.device.is_none(),
    }
}

/// The saves here would be lost for good if replaced: no backup holds them.
fn unprotected(report: &Report) -> bool {
    report.cannot_back_up.is_none()
        && report
            .device
            .is_some_and(|d| newer_than(&d, report.backup.as_ref()))
}

/// Why nothing can be done, when the cards don't already say it.
fn note(shown: &Shown) -> Option<&str> {
    let report = &shown.report;
    if shown.saves.folders.is_empty() || report.none_found {
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

/// The note under this device's cards: on a network card, why a copy here
/// would be refused (the saves' reason covers the backup's); else why
/// nothing can be done here.
fn panel_note(shown: &Shown, focus: Focus) -> Option<&str> {
    match focus {
        Focus::Card { .. } => shown.report.cannot_receive(Side::Device),
        Focus::List => note(shown),
    }
}

/// A copy from another machine is done: from where, and what wasn't there.
fn got(title: &str, get: &Get, missing: &[String]) -> String {
    let name = &get.peer.info.name;
    let done = match get.into {
        Side::Device => format!("{title}: saves from {name}."),
        Side::Backup => format!("{title}: backup from {name}. X to restore it."),
    };
    if missing.is_empty() {
        return done;
    }
    format!("{done} Not on {name}: {}.", missing.join(", "))
}

fn run(copy: &Copy) -> Result<String, String> {
    let (saves, title) = (&copy.saves, &copy.title);
    match &copy.direction {
        Direction::Backup => saver::backup(saves).map(|()| format!("{title}: backed up.")),
        Direction::Restore => saver::restore(saves).map(|()| format!("{title}: backup restored.")),
        Direction::Get(get) => {
            if copy.backup_first {
                saver::backup(saves)?;
            }
            let (addr, bytes) = (get.peer.addr, get.stamp.bytes);
            client::copy_saves(addr, &copy.folder, get.side, saves, get.into, bytes)
                .map(|missing| got(title, get, &missing))
        }
    }
    .map_err(|e| format!("{title}: {e}"))
}

impl SavesScreen {
    /// A dialog or a copy is up: tabs and Back stay with this screen.
    pub fn has_modal(&self) -> bool {
        self.confirm.is_some() || self.copying.is_some()
    }

    /// Read the focused game and the network again when the tab is shown (a
    /// game may have saved since, a machine come or gone).
    pub fn forget(&mut self) {
        self.shown = None;
        self.searched = false;
        self.generation += 1;
        self.focus = Focus::List;
    }

    fn current<'a>(&'a self, games: &[Game]) -> Option<&'a Shown> {
        let focused = games.get(self.list.cursor)?;
        self.shown
            .as_ref()
            .filter(|s| s.folder == focused.folder_name)
    }

    /// The focused game's rows on other machines, once read.
    fn remote_rows(&self, games: &[Game]) -> &[RemoteRow] {
        let focused = games.get(self.list.cursor).map(|g| g.folder_name.as_str());
        self.remote
            .as_ref()
            .filter(|r| Some(r.folder.as_str()) == focused)
            .map_or(&[], |r| &r.rows)
    }

    /// Collect finished reads and copies; a copy is followed by a fresh read.
    pub fn poll(&mut self) {
        if let Some(shown) = self.reading.as_ref().and_then(|rx| rx.try_recv().ok()) {
            self.reading = None;
            self.shown = Some(shown);
        }
        if let Some(peers) = self.searching.as_ref().and_then(|rx| rx.try_recv().ok()) {
            self.searching = None;
            self.peers = peers;
            self.generation += 1;
        }
        if let Some(remote) = self
            .reading_remote
            .as_ref()
            .and_then(|rx| rx.try_recv().ok())
        {
            self.reading_remote = None;
            self.remote = Some(remote);
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
                Outcome::Chosen(i) => {
                    if let Some(copy) = self
                        .confirm
                        .take()
                        .and_then(|c| c.choices.into_iter().nth(i))
                    {
                        self.start_copy(copy);
                    }
                }
                Outcome::Cancelled => self.confirm = None,
            }
            return;
        }
        if !self.searched {
            self.searched = true;
            self.searching = Some(search(cfg));
        }
        // The library may have lost games since, a machine its saves.
        self.list.select(self.list.cursor, games.len(), |_| true);
        let rows = self.remote_rows(games).len();
        self.focus = clamp(self.focus, rows);
        let arrangement = Pair::at(ui, layout(ui, area).1, 0.0).arrangement();
        match self.focus {
            Focus::List => self.update_list(ui, area, games),
            Focus::Card { row, side } => self.update_cards(ui, games, row, side),
        }
        self.focus = [Action::Left, Action::Right, Action::Up, Action::Down]
            .into_iter()
            .filter(|action| ui.action(*action))
            .fold(self.focus, |focus, action| {
                step(focus, action, rows, arrangement)
            });
        self.read_focused(games, cfg);
    }

    fn update_list(&mut self, ui: &Ui, area: Rect, games: &[Game]) {
        let (list, _) = layout(ui, area);
        if self.list.update(ui, games.len(), list, ROW_STEP, |_| true) {
            self.ask(games, Direction::Backup);
        } else if ui.action(Action::Edit) {
            self.ask(games, Direction::Restore);
        }
    }

    fn update_cards(&mut self, ui: &Ui, games: &[Game], row: usize, side: Side) {
        if ui.action(Action::Confirm) {
            self.ask_get(games, row, side, Side::Device);
        } else if ui.action(Action::Edit) {
            self.ask_get(games, row, side, Side::Backup);
        }
    }

    fn read_focused(&mut self, games: &[Game], cfg: &Config) {
        let Some(game) = games.get(self.list.cursor) else {
            return;
        };
        if self.current(games).is_none() && self.reading.is_none() {
            self.reading = Some(read(game, cfg));
        }
        let folder = game.folder_name.as_str();
        let remote = self.remote.as_ref();
        let fresh = remote.is_some_and(|r| r.is_fresh(folder, self.generation));
        // Rows already shown for this game wait for the search to end: one
        // read with the new machines, not one before and one after.
        let shown = remote.is_some_and(|r| r.folder == folder);
        if fresh || self.reading_remote.is_some() || (shown && self.searching.is_some()) {
            return;
        }
        if self.peers.sharing.is_empty() {
            let (folder, rows, generation) = (folder.to_string(), Vec::new(), self.generation);
            self.remote = Some(Remote {
                folder,
                rows,
                generation,
            });
        } else {
            let read = read_remote(folder, &self.peers.sharing, self.generation);
            self.reading_remote = Some(read);
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
            // Copies from another machine ask in `ask_get`.
            Direction::Get(_) => return,
        };
        if let Some(why) = refusal.clone() {
            self.toast.show(&why, theme::secondary());
            return;
        }
        let next = confirm_or_start(
            shown,
            title,
            vec![(label, color, copy_of(shown, direction))],
        );
        self.start(next);
    }

    /// Get `side` of the machine on `row` into `into` here.
    fn ask_get(&mut self, games: &[Game], row: usize, side: Side, into: Side) {
        let (Some(shown), Some(remote)) = (self.current(games), self.remote_rows(games).get(row))
        else {
            return;
        };
        let Some(stamp) = remote.stamp(side).copied() else {
            let why = if side == Side::Device && remote.saves.playing {
                "The game is running there: close it first."
            } else {
                "Nothing to copy there."
            };
            self.toast.show(why, theme::secondary());
            return;
        };
        if let Some(why) = shown.report.cannot_receive(into).map(String::from) {
            self.toast.show(&why, theme::secondary());
            return;
        }
        let get = Get {
            peer: remote.peer.clone(),
            side,
            into,
            stamp,
        };
        let copy = copy_of(shown, Direction::Get(get));
        let (title, label) = match into {
            Side::Device => ("Get saves", "Replace"),
            Side::Backup => ("Get backup", "Replace"),
        };
        let mut choices = vec![(label, theme::danger(), copy.clone())];
        if into == Side::Device && unprotected(&shown.report) {
            let first = Copy {
                backup_first: true,
                ..copy
            };
            choices.insert(0, ("Back up mine, then get", theme::accent(), first));
        }
        let next = confirm_or_start(shown, title, choices);
        self.start(next);
    }

    fn start(&mut self, next: Next) {
        match next {
            Next::Copy(copy) => self.start_copy(*copy),
            Next::Confirm(confirm) => self.confirm = Some(confirm),
        }
    }

    fn start_copy(&mut self, copy: Copy) {
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let _ = tx.send(run(&copy));
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
            Some(shown) => draw_panel(ui, panel, shown, &self.network(games)),
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

    fn network<'a>(&'a self, games: &[Game]) -> Network<'a> {
        let busy = self.searching.is_some() || self.reading_remote.is_some();
        let rows = self.remote_rows(games);
        let device = self.current(games).and_then(|s| s.report.device.as_ref());
        Network {
            rows,
            newest: newest_remote(rows, device),
            focus: self.focus,
            busy,
            peers: self.peers.sharing.len(),
            outdated: &self.peers.outdated,
        }
    }

    fn draw_row(&self, ui: &Ui, game: &Game, i: usize, frame: Rect) {
        let focused = i == self.list.cursor;
        let stripe = if focused {
            theme::accent()
        } else {
            theme::secondary()
        };
        style::card(frame, stripe, focused && self.focus == Focus::List);
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
        if let Focus::Card { row, side } = self.focus {
            let offered = self
                .remote_rows(games)
                .get(row)
                .is_some_and(|r| r.stamp(side).is_some());
            let gets = [
                (
                    Side::Device,
                    Action::Confirm,
                    "Get as saves",
                    theme::danger(),
                ),
                (Side::Backup, Action::Edit, "Get as backup", theme::accent()),
            ];
            let back = ui.hint(Action::Left, "Games", theme::muted());
            return gets
                .into_iter()
                .filter(|(into, ..)| offered && shown.report.cannot_receive(*into).is_none())
                .map(|(_, action, label, color)| ui.hint(action, label, color))
                .chain([back])
                .collect();
        }
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
        let network = (!self.remote_rows(games).is_empty())
            .then(|| ui.hint(Action::Right, "Network", theme::muted()));
        backup.into_iter().chain(restore).chain(network).collect()
    }
}

fn copy_of(shown: &Shown, direction: Direction) -> Copy {
    Copy {
        direction,
        saves: shown.saves.clone(),
        folder: shown.folder.clone(),
        title: shown.title.clone(),
        backup_first: false,
    }
}

/// A copy to start, or the question to ask first.
enum Next {
    Copy(Box<Copy>),
    Confirm(Confirm),
}

/// Ask which of `choices` to run, unless the last one (the plain copy)
/// replaces nothing: it runs without a question then.
fn confirm_or_start(shown: &Shown, title: &str, mut choices: Vec<(&str, Color, Copy)>) -> Next {
    let replaces = choices
        .last()
        .is_some_and(|(_, _, plain)| !replaces_nothing(&shown.report, &plain.direction));
    if !replaces {
        if let Some((_, _, plain)) = choices.pop() {
            return Next::Copy(Box::new(plain));
        }
    }
    let message = choices
        .last()
        .map(|(_, _, plain)| question(shown, &plain.direction))
        .unwrap_or_default();
    let options: Vec<(&str, Color)> = choices
        .iter()
        .map(|(label, color, _)| (*label, *color))
        .chain([("Cancel", theme::muted())])
        .collect();
    Next::Confirm(Confirm {
        dialog: Dialog::new(title, &message, &options),
        choices: choices.into_iter().map(|(_, _, copy)| copy).collect(),
    })
}

/// What the panel shows of the other machines.
struct Network<'a> {
    rows: &'a [RemoteRow],
    /// The date of the cards flagged NEWER (see [`newest_remote`]).
    newest: Option<u64>,
    focus: Focus,
    /// Still searching or reading.
    busy: bool,
    /// Machines sharing saves.
    peers: usize,
    outdated: &'a [String],
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

/// A pair of cards (one machine's saves and backup) at `top`: side by side
/// when wide enough, else stacked.
struct Pair {
    left: Rect,
    right: Rect,
}

impl Pair {
    fn at(ui: &Ui, panel: Rect, top: f32) -> Pair {
        let card_w = ui.measure("0000-00-00 00:00:00") + 24.0;
        if panel.w >= 2.0 * card_w + GAP {
            let w = (panel.w - GAP) / 2.0;
            return Pair {
                left: Rect::new(panel.x, top, w, CARD_H),
                right: Rect::new(panel.x + w + GAP, top, w, CARD_H),
            };
        }
        Pair {
            left: Rect::new(panel.x, top, panel.w, CARD_H),
            right: Rect::new(panel.x, top + CARD_H + CARD_GAP, panel.w, CARD_H),
        }
    }

    fn bottom(&self) -> f32 {
        self.right.bottom()
    }

    fn arrangement(&self) -> Arrangement {
        if self.left.y == self.right.y {
            Arrangement::Beside
        } else {
            Arrangement::Stacked
        }
    }

    fn height(&self) -> f32 {
        self.right.bottom() - self.left.y
    }
}

/// The game's title, its two cards, the other machines' cards, then what
/// stops a copy and where the saves are. On a short panel focused on the
/// network, a machine's cards take the place of this device's.
fn draw_panel(ui: &Ui, panel: Rect, shown: &Shown, network: &Network) {
    ui.title(
        &ui.truncate(&shown.title, panel.w),
        panel.x,
        panel.y + 20.0,
        theme::text(),
    );
    let top = panel.y + 32.0;
    let local = Pair::at(ui, panel, top);
    let notes: Vec<String> = panel_note(shown, network.focus)
        .map(|note| {
            // A long path doesn't wrap: cut at the panel's edge.
            let lines = ui.wrap(note, panel.w).into_iter().take(2);
            lines.map(|line| ui.truncate(&line, panel.w)).collect()
        })
        .unwrap_or_default();
    let header_y = local.bottom() + 20.0 + 20.0 * notes.len() as f32;
    let rows_top = header_y + 8.0 + 16.0 * network.outdated.len() as f32;
    let step = local.height() + CARD_GAP;
    let fit = ((panel.bottom() - rows_top + CARD_GAP) / step).max(0.0) as usize;
    let rows = network.rows.len();
    let focused = match network.focus {
        Focus::Card { row, .. } => Some(row),
        Focus::List => None,
    };
    if let (Some(row), 0) = (focused, fit) {
        let y = draw_remote_rows(ui, panel, top, network, window(row, rows, 1));
        draw_notes(ui, panel, y + 8.0, &notes);
        return;
    }
    draw_local(ui, &local, shown);
    draw_notes(ui, panel, local.bottom() + 20.0, &notes);
    draw_network_header(ui, panel, header_y, network, fit);
    let mut y = match fit {
        0 => rows_top + 12.0,
        _ => draw_remote_rows(
            ui,
            panel,
            rows_top,
            network,
            window(focused.unwrap_or(0), rows, fit),
        ),
    };
    for folder in &shown.saves.folders {
        let color = if shown.report.missing.contains(&folder.device) {
            theme::danger()
        } else {
            theme::muted()
        };
        let path = folder.device.display().to_string();
        for line in ui.wrap_path_small(&path, panel.w) {
            if y > panel.bottom() {
                return;
            }
            ui.small_text(&line, panel.x, y, color);
            y += 16.0;
        }
    }
}

/// The note's lines from baseline `y`, as many as the panel holds.
fn draw_notes(ui: &Ui, panel: Rect, y: f32, lines: &[String]) {
    let baselines = (0..).map(|i| y + 20.0 * i as f32);
    for (line, y) in lines.iter().zip(baselines) {
        if y > panel.bottom() {
            return;
        }
        ui.text(line, panel.x, y, theme::secondary());
    }
}

fn draw_local(ui: &Ui, pair: &Pair, shown: &Shown) {
    let newer = newer(&shown.report);
    let device_label = if shown.saves.in_game {
        "IN GAME"
    } else {
        "THIS DEVICE"
    };
    let no_path = shown.saves.folders.is_empty();
    let device_empty = if no_path {
        "NO SAVE PATH"
    } else if shown.report.none_found {
        "NOT FOUND"
    } else {
        "NO SAVE"
    };
    style::well(pair.left);
    draw_card(
        ui,
        pair.left,
        device_label,
        shown.report.device.as_ref(),
        device_empty,
        newer == Some(Side::Device),
    );
    let backup_empty = if no_path { "NO SAVE PATH" } else { "NO BACKUP" };
    style::well(pair.right);
    draw_card(
        ui,
        pair.right,
        "BACKUP",
        shown.report.backup.as_ref(),
        backup_empty,
        newer == Some(Side::Backup),
    );
}

/// "NETWORK", what is going on, then one line per machine that can't share
/// saves.
fn draw_network_header(ui: &Ui, panel: Rect, y: f32, network: &Network, fit: usize) {
    let rows = network.rows.len();
    if network.busy {
        draw_spinner(panel.right() - 8.0, y - 5.0, 5.0, theme::accent());
    }
    let status = if rows > fit {
        let machines = if rows == 1 { "machine" } else { "machines" };
        format!("{rows} {machines}: > to see")
    } else if network.busy || rows > 0 {
        String::new()
    } else if network.peers == 0 {
        "No other •cartridge sharing".into()
    } else {
        "Not on the other machines".into()
    };
    ui.small_text("NETWORK", panel.x, y, theme::muted());
    let x = panel.x + ui.measure_small("NETWORK  ");
    ui.small_text(
        &ui.truncate_small(&status, panel.right() - x),
        x,
        y,
        theme::muted(),
    );
    let lines = network.outdated.iter().zip(1..);
    for (line, i) in lines.take_while(|(_, i)| y + 16.0 * *i as f32 <= panel.bottom()) {
        let line = ui.truncate_small(line, panel.w);
        ui.small_text(&line, panel.x, y + 16.0 * i as f32, theme::secondary());
    }
}

/// The cards of the machines in `shown`, from `top`; the baseline after them.
fn draw_remote_rows(
    ui: &Ui,
    panel: Rect,
    top: f32,
    network: &Network,
    shown: std::ops::Range<usize>,
) -> f32 {
    let mut y = top;
    for row in shown {
        let pair = Pair::at(ui, panel, y);
        draw_remote(ui, &pair, &network.rows[row], row, network);
        y = pair.bottom() + CARD_GAP;
    }
    y + 12.0
}

fn draw_remote(ui: &Ui, pair: &Pair, remote: &RemoteRow, row: usize, network: &Network) {
    let name = remote.peer.info.name.to_uppercase();
    let count = network.rows.len();
    let place = if count > 1 {
        format!(" {}/{count}", row + 1)
    } else {
        String::new()
    };
    let cards = [
        (pair.left, Side::Device, format!("{name}{place}"), "NO SAVE"),
        (
            pair.right,
            Side::Backup,
            format!("{name} BACKUP"),
            "NO BACKUP",
        ),
    ];
    for (r, side, label, empty) in cards {
        let focused = network.focus == Focus::Card { row, side };
        let stripe = if focused {
            theme::accent()
        } else {
            theme::secondary()
        };
        style::card(r, stripe, focused);
        let empty = if side == Side::Device && remote.saves.playing {
            "PLAYING"
        } else {
            empty
        };
        let stamp = remote.stamp(side);
        let newer = stamp.is_some_and(|s| Some(s.seconds()) == network.newest);
        draw_card(ui, r, &label, stamp, empty, newer);
    }
}

/// A card's label, NEWER chip, then its date and contents (or `empty`).
fn draw_card(ui: &Ui, r: Rect, label: &str, stamp: Option<&Stamp>, empty: &str, newer: bool) {
    let x = r.x + 10.0;
    let chip_w = if newer {
        style::chip_width(ui, "NEWER") + 10.0
    } else {
        0.0
    };
    ui.small_text(
        &ui.truncate_small(label, r.w - 20.0 - chip_w),
        x,
        r.y + 16.0,
        theme::muted(),
    );
    if newer {
        let chip_x = r.right() - chip_w;
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
    use crate::core::share::ServerInfo;
    use std::time::{Duration, SystemTime};

    fn at(secs: u64) -> Stamp {
        Stamp {
            modified: SystemTime::UNIX_EPOCH + Duration::from_secs(secs),
            files: 1,
            bytes: 1,
        }
    }

    fn stamp(secs: u64) -> Option<Stamp> {
        Some(at(secs))
    }

    fn report(device: Option<Stamp>, backup: Option<Stamp>) -> Report {
        Report {
            device,
            backup,
            cannot_back_up: None,
            cannot_restore: None,
            missing: Vec::new(),
            none_found: false,
            cannot_receive_saves: None,
            cannot_receive_backup: None,
        }
    }

    fn shown(report: Report) -> Shown {
        let dir = crate::core::testutil::TempDir::new();
        let game = Game::skeleton(&dir.path().join("games/Game"));
        Shown {
            folder: "Game".into(),
            title: "SomeGame".into(),
            saves: saver::saves_of(&game, &dir.path().join("saves-copies")),
            report,
        }
    }

    fn get(into: Side, secs: u64) -> Direction {
        Direction::Get(Get {
            peer: Peer {
                info: ServerInfo::new("deck".into(), "id".into(), 1),
                addr: ([127, 0, 0, 1], 1).into(),
            },
            side: Side::Device,
            into,
            stamp: at(secs),
        })
    }

    #[test]
    fn the_newer_side_is_the_one_with_the_most_recent_file() {
        assert_eq!(newer(&report(stamp(2), stamp(1))), Some(Side::Device));
        assert_eq!(newer(&report(stamp(1), stamp(2))), Some(Side::Backup));
        assert_eq!(newer(&report(stamp(1), stamp(1))), None);
        assert_eq!(newer(&report(stamp(1), None)), None);
    }

    #[test]
    fn sides_within_the_same_second_are_as_new() {
        let copied = Stamp {
            modified: at(5).modified + Duration::from_millis(900),
            ..at(5)
        };
        assert_eq!(newer(&report(Some(copied), stamp(5))), None);
        assert!(!newer_than(&copied, Some(&at(5))));
        assert!(newer_than(&at(5), None));
    }

    #[test]
    fn restoring_over_newer_saves_says_they_will_be_lost() {
        let shown = shown(report(stamp(2), stamp(1)));
        let restore = question(&shown, &Direction::Restore);
        assert_eq!(restore.lines().count(), 4);
        assert!(restore.ends_with("they will be lost. Close the game first."));
        assert_eq!(question(&shown, &Direction::Backup).lines().count(), 3);
    }

    #[test]
    fn getting_saves_names_the_machine_and_what_is_lost() {
        let shown = shown(report(stamp(9), stamp(1)));
        let into_saves = question(&shown, &get(Side::Device, 5));
        assert!(into_saves
            .starts_with("Replace the saves of SomeGame on this device with the saves on deck?"));
        assert!(into_saves.contains("\ndeck: "));
        assert!(into_saves.ends_with("they will be lost. Close the game first."));
        let into_backup = question(&shown, &get(Side::Backup, 5));
        assert!(into_backup.starts_with("Replace the backup of SomeGame with the saves on deck?"));
        assert_eq!(
            into_backup.lines().count(),
            3,
            "the backup is older: nothing lost"
        );
    }

    #[test]
    fn only_a_copy_over_something_needs_a_confirmation() {
        let first_backup = report(stamp(1), None);
        assert!(replaces_nothing(&first_backup, &Direction::Backup));
        assert!(replaces_nothing(&first_backup, &get(Side::Backup, 1)));
        let fresh_install = report(None, stamp(1));
        assert!(replaces_nothing(&fresh_install, &Direction::Restore));
        assert!(replaces_nothing(&fresh_install, &get(Side::Device, 1)));
        let both = report(stamp(1), stamp(1));
        assert!(!replaces_nothing(&both, &Direction::Backup));
        assert!(!replaces_nothing(&both, &Direction::Restore));
        assert!(!replaces_nothing(&both, &get(Side::Device, 1)));
    }

    #[test]
    fn saves_newer_than_their_backup_are_offered_a_backup_first() {
        assert!(unprotected(&report(stamp(2), stamp(1))));
        assert!(unprotected(&report(stamp(2), None)));
        assert!(!unprotected(&report(stamp(1), stamp(1))));
        assert!(!unprotected(&report(None, stamp(1))));
        let refused = Report {
            cannot_back_up: Some("no".into()),
            ..report(stamp(2), None)
        };
        assert!(!unprotected(&refused));
    }

    #[test]
    fn on_a_network_card_the_note_says_why_a_copy_here_is_refused() {
        let prefix = "Launch the game once first.";
        let refused = shown(Report {
            cannot_receive_saves: Some(prefix.into()),
            ..report(stamp(1), None)
        });
        let card = Focus::Card {
            row: 0,
            side: Side::Device,
        };
        assert_eq!(panel_note(&refused, card), Some(prefix));
        assert_eq!(panel_note(&refused, Focus::List), None, "the cards say it");
        assert_eq!(panel_note(&shown(report(stamp(1), None)), card), None);
    }

    #[test]
    fn the_dpad_moves_between_the_list_and_each_machines_cards() {
        let card = |row, side| Focus::Card { row, side };
        let step = |focus, action, rows| step(focus, action, rows, Arrangement::Beside);
        assert_eq!(step(Focus::List, Action::Right, 0), Focus::List);
        assert_eq!(step(Focus::List, Action::Right, 2), card(0, Side::Device));
        assert_eq!(
            step(card(0, Side::Device), Action::Right, 2),
            card(0, Side::Backup)
        );
        assert_eq!(
            step(card(0, Side::Backup), Action::Right, 2),
            card(0, Side::Backup)
        );
        assert_eq!(
            step(card(0, Side::Backup), Action::Down, 2),
            card(1, Side::Backup)
        );
        assert_eq!(
            step(card(1, Side::Backup), Action::Down, 2),
            card(1, Side::Backup)
        );
        assert_eq!(
            step(card(1, Side::Backup), Action::Up, 2),
            card(0, Side::Backup)
        );
        assert_eq!(
            step(card(0, Side::Backup), Action::Left, 2),
            card(0, Side::Device)
        );
        assert_eq!(step(card(0, Side::Device), Action::Left, 2), Focus::List);
    }

    #[test]
    fn stacked_cards_are_walked_top_to_bottom() {
        let card = |row, side| Focus::Card { row, side };
        let step = |focus, action| step(focus, action, 2, Arrangement::Stacked);
        assert_eq!(step(Focus::List, Action::Right), card(0, Side::Device));
        assert_eq!(
            step(card(0, Side::Device), Action::Down),
            card(0, Side::Backup)
        );
        assert_eq!(
            step(card(0, Side::Backup), Action::Down),
            card(1, Side::Device)
        );
        assert_eq!(
            step(card(1, Side::Backup), Action::Down),
            card(1, Side::Backup)
        );
        assert_eq!(
            step(card(1, Side::Device), Action::Up),
            card(0, Side::Backup)
        );
        assert_eq!(
            step(card(0, Side::Device), Action::Up),
            card(0, Side::Device)
        );
        assert_eq!(
            step(card(1, Side::Backup), Action::Right),
            card(1, Side::Backup)
        );
        assert_eq!(step(card(1, Side::Backup), Action::Left), Focus::List);
    }

    #[test]
    fn the_focus_follows_machines_that_go_away() {
        let card = |row| Focus::Card {
            row,
            side: Side::Backup,
        };
        assert_eq!(clamp(card(2), 2), card(1));
        assert_eq!(clamp(card(0), 0), Focus::List);
        assert_eq!(clamp(Focus::List, 0), Focus::List);
    }

    fn row(name: &str, device: Option<Stamp>, backup: Option<Stamp>) -> RemoteRow {
        RemoteRow {
            peer: Peer {
                info: ServerInfo::new(name.into(), name.into(), 1),
                addr: ([127, 0, 0, 1], 1).into(),
            },
            saves: RemoteSaves {
                device,
                backup,
                playing: false,
            },
        }
    }

    #[test]
    fn the_newest_card_on_the_network_is_flagged_when_newer_than_this_device() {
        let rows = [
            row("deck", stamp(5), stamp(9)),
            row("desktop", stamp(9), None),
            row("laptop", stamp(7), None),
        ];
        assert_eq!(newest_remote(&rows, Some(&at(3))), Some(9), "both 9s");
        assert_eq!(newest_remote(&rows, None), Some(9), "no saves here");
        assert_eq!(newest_remote(&rows, Some(&at(9))), None, "as new here");
        assert_eq!(newest_remote(&[], Some(&at(1))), None);
    }

    #[test]
    fn machines_are_listed_by_name() {
        let peers = by_name(
            ["steamdeck", "Desktop", "box"]
                .map(|name| row(name, None, None).peer)
                .to_vec(),
        );
        let names: Vec<&str> = peers.iter().map(|p| p.info.name.as_str()).collect();
        assert_eq!(names, ["box", "Desktop", "steamdeck"]);
    }

    #[test]
    fn rows_read_before_the_tab_was_shown_again_are_stale() {
        let remote = Remote {
            folder: "Game".into(),
            rows: Vec::new(),
            generation: 3,
        };
        assert!(remote.is_fresh("Game", 3));
        assert!(
            !remote.is_fresh("Game", 4),
            "shown again, or a search ended"
        );
        assert!(!remote.is_fresh("Other", 3));
    }

    #[test]
    fn a_short_panel_shows_the_focused_machine() {
        assert_eq!(window(0, 3, 0), 0..1, "no room: still the focused one");
        assert_eq!(window(2, 3, 1), 2..3);
        assert_eq!(window(2, 3, 2), 1..3);
        assert_eq!(window(0, 3, 5), 0..3);
    }

    #[test]
    fn a_copy_from_a_machine_says_what_was_not_there() {
        let Direction::Get(from) = get(Side::Backup, 1) else {
            unreachable!()
        };
        assert_eq!(
            got("SomeGame", &from, &[]),
            "SomeGame: backup from deck. X to restore it."
        );
        let partial = got("SomeGame", &from, &["Saves_2".into()]);
        assert!(partial.ends_with("Not on deck: Saves_2."));
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
