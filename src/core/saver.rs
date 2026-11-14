//! SAVES tab: each game's save folders and their backup, backed up or
//! restored one game at a time, by hand.
//!
//! A game's backup is `saves-copies/<game folder>/<save folder>/` (folder
//! names don't change when a game is renamed). A backup is written under a
//! hidden name and only takes the real one once complete, so a failed copy
//! never passes for a backup. A restore empties the save folders and copies
//! the backup in: the user chose to replace their saves.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use super::models::{Game, Install};
use super::{package, paths, steam};

const WINDOWS_RESERVED: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// A folder name safe on every OS.
pub fn sanitize_title(title: &str) -> String {
    let replaced: String = title
        .trim()
        .chars()
        .map(|c| {
            if "<>:\"/\\|?*".contains(c) || (c as u32) < 0x20 {
                '_'
            } else {
                c
            }
        })
        .collect();
    let collapsed = replaced.split_whitespace().collect::<Vec<_>>().join("_");
    let trimmed = collapsed.trim_end_matches(['.', ' ']);
    if trimmed.is_empty() {
        return "game".into();
    }
    let name = if WINDOWS_RESERVED.contains(&trimmed.to_uppercase().as_str()) {
        format!("{trimmed}_game")
    } else {
        trimmed.to_string()
    };
    let short: String = name.chars().take(100).collect();
    let short = short.trim_end_matches(['.', ' ']);
    if short.is_empty() {
        "game".into()
    } else {
        short.to_string()
    }
}

/// Hands out folder names, adding `_2`, `_3`… to a name already taken, so
/// two save folders never share one backup folder. Case-insensitive:
/// Windows and macOS folders are.
#[derive(Default)]
struct FolderNames(HashSet<String>);

impl FolderNames {
    fn claim(&mut self, wanted: &str) -> String {
        let name = std::iter::once(wanted.to_string())
            .chain((2..).map(|n| format!("{wanted}_{n}")))
            .find(|name| !self.0.contains(&name.to_lowercase()))
            .unwrap_or_default();
        self.0.insert(name.to_lowercase());
        name
    }
}

/// Every file under `root` by relative path. Links are skipped: a save
/// backup copies the saves, never what a link points to.
fn file_map(root: &Path) -> BTreeMap<PathBuf, PathBuf> {
    fn walk(dir: &Path, root: &Path, out: &mut BTreeMap<PathBuf, PathBuf>) {
        for path in entries(dir) {
            if path.is_dir() {
                walk(&path, root, out);
            } else {
                out.insert(path.strip_prefix(root).unwrap_or(&path).to_path_buf(), path);
            }
        }
    }
    let mut out = BTreeMap::new();
    if root.is_dir() {
        walk(root, root, &mut out);
    }
    out
}

/// What `dir` holds, links left out (copies and restores never touch them).
fn entries(dir: &Path) -> Vec<PathBuf> {
    paths::sorted_entries(dir)
        .into_iter()
        .map(|name| dir.join(name))
        .filter(|path| !paths::is_symlink(path))
        .collect()
}

// ── A game's saves ───────────────────────────────────────────────────────

/// One save folder of a game, and the name of its copy in the backup.
#[derive(Clone, Debug)]
pub struct SaveFolder {
    pub device: PathBuf,
    pub name: String,
}

/// A game's save folders on this machine and where they are backed up.
#[derive(Clone, Debug)]
pub struct Saves {
    /// `saves-copies/<game folder>`.
    pub backup: PathBuf,
    pub folders: Vec<SaveFolder>,
    /// The saves live in the game's own folder (portable saves).
    pub in_game: bool,
    /// Only the package exists: its save paths aren't read.
    packaged: bool,
    /// The Proton prefix (`drive_c`) holding save folders, if any.
    prefix: Option<PathBuf>,
    /// A save path a restore must not empty (see `is_too_wide`).
    too_wide: Option<PathBuf>,
}

/// Folders many apps share, under the home folder (or a Proton prefix's
/// user folder): a save path may be inside one, never one of them.
const SHARED: [&str; 18] = [
    ".config",
    ".local",
    ".local/share",
    ".local/state",
    ".cache",
    ".var",
    ".var/app",
    ".steam",
    "Desktop",
    "Documents",
    "Documents/My Games",
    "Downloads",
    "Saved Games",
    "AppData",
    "AppData/Local",
    "AppData/LocalLow",
    "AppData/Roaming",
    "Library/Application Support",
];

pub fn saves_of(game: &Game, saves_root: &Path) -> Saves {
    let mut names = FolderNames::default();
    let folders: Vec<SaveFolder> = game
        .resolved_save_paths
        .iter()
        .map(|device| SaveFolder {
            name: names.claim(&sanitize_title(
                &device.file_name().unwrap_or_default().to_string_lossy(),
            )),
            device: device.clone(),
        })
        .collect();
    let prefix = steam::appid_of(game)
        .map(|appid| paths::expand(&steam::proton_prefix(appid)))
        .filter(|prefix| folders.iter().any(|f| f.device.starts_with(prefix)));
    let kept = kept_folders(&game.dir, saves_root, prefix.as_deref());
    Saves {
        backup: saves_root.join(&game.folder_name),
        in_game: folders.iter().any(|f| f.device.starts_with(&game.dir)),
        packaged: matches!(game.install, Install::Packaged { .. }),
        too_wide: folders
            .iter()
            .map(|f| f.device.clone())
            .find(|device| is_too_wide(device, &kept, saves_root)),
        prefix,
        folders,
    }
}

/// Folders a save path must not be, nor hold: the home folder, the game,
/// the backups, the folders apps share (also in the game's Proton prefix).
fn kept_folders(game_dir: &Path, saves_root: &Path, prefix: Option<&Path>) -> Vec<PathBuf> {
    let home = paths::home_dir();
    let users = [
        Some(home.clone()),
        prefix.map(|p| p.join("users/steamuser")),
    ];
    let shared = users
        .into_iter()
        .flatten()
        .flat_map(|user| SHARED.iter().map(move |s| user.join(s)));
    [home, game_dir.to_path_buf(), saves_root.to_path_buf()]
        .into_iter()
        .chain(prefix.map(Path::to_path_buf))
        .chain(shared)
        .map(|p| real_path(&p))
        .collect()
}

/// `path` with its links resolved, also when its end doesn't exist yet (a
/// save folder the game hasn't made): `/home` is a link on some systems.
fn real_path(path: &Path) -> PathBuf {
    let path = paths::abspath(path);
    path.ancestors()
        .find_map(|a| {
            Some(
                std::fs::canonicalize(a)
                    .ok()?
                    .join(path.strip_prefix(a).ok()?),
            )
        })
        .unwrap_or(path)
}

/// A save path a restore must not empty: a disk (root or mount point), a
/// kept folder or one holding it, or a folder inside the backups.
fn is_too_wide(device: &Path, kept: &[PathBuf], saves_root: &Path) -> bool {
    let device = real_path(device);
    device.parent().is_none()
        || is_mount_point(&device)
        || device.starts_with(real_path(saves_root))
        || kept.iter().any(|k| k.starts_with(&device))
}

/// A whole disk mounted there (an SD card, a USB drive).
#[cfg(unix)]
fn is_mount_point(dir: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let device_of = |p: &Path| std::fs::metadata(p).map(|m| m.dev()).ok();
    match (device_of(dir), dir.parent().and_then(device_of)) {
        (Some(own), Some(parent)) => own != parent,
        _ => false,
    }
}

#[cfg(not(unix))]
fn is_mount_point(_dir: &Path) -> bool {
    false
}

// ── Status ───────────────────────────────────────────────────────────────

/// The files of a set of folders: newest modification, count, total size.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Stamp {
    pub modified: SystemTime,
    pub files: usize,
    pub bytes: u64,
}

fn stamp(dirs: impl IntoIterator<Item = PathBuf>) -> Option<Stamp> {
    let files: Vec<std::fs::Metadata> = dirs
        .into_iter()
        .flat_map(|dir| file_map(&dir).into_values())
        .filter_map(|path| std::fs::metadata(path).ok())
        .collect();
    let modified = files.iter().filter_map(|m| m.modified().ok()).max()?;
    Some(Stamp {
        modified,
        files: files.len(),
        bytes: files.iter().map(std::fs::Metadata::len).sum(),
    })
}

/// What the SAVES tab shows for a game: both sides, and why a copy can't
/// be made, if it can't.
#[derive(Clone, Debug)]
pub struct Report {
    pub device: Option<Stamp>,
    pub backup: Option<Stamp>,
    pub cannot_back_up: Option<String>,
    pub cannot_restore: Option<String>,
    /// Save folders not on disk: the game hasn't saved yet, or the path is
    /// wrong (an absolute path from another machine).
    pub missing: Vec<PathBuf>,
    /// The game has save folders and none of them exist.
    pub none_found: bool,
}

const NO_SAVES: &str = "No saves to back up yet.";
const NOT_FOUND: &str =
    "Save folder not found: the game hasn't saved yet, or its save path is wrong (Library, X).";
const NO_BACKUP: &str = "No backup yet.";

pub fn report(saves: &Saves) -> Report {
    let device = stamp(saves.folders.iter().map(|f| f.device.clone()));
    let backup = stamp(saves.folders.iter().map(|f| saves.backup.join(&f.name)));
    let missing: Vec<PathBuf> = saves
        .folders
        .iter()
        .map(|f| f.device.clone())
        .filter(|device| !device.is_dir())
        .collect();
    let none_found = !saves.folders.is_empty() && missing.len() == saves.folders.len();
    // A Windows game whose prefix isn't made yet: that is why, not the path.
    let empty = if none_found {
        missing_prefix(saves).unwrap_or_else(|| NOT_FOUND.into())
    } else {
        NO_SAVES.into()
    };
    let refused = refusal(saves);
    Report {
        missing,
        none_found,
        cannot_back_up: refused
            .clone()
            .or_else(|| device.is_none().then_some(empty)),
        cannot_restore: refused
            .or_else(|| backup.is_none().then(|| NO_BACKUP.into()))
            .or_else(|| missing_prefix(saves)),
        device,
        backup,
    }
}

/// Why neither a backup nor a restore can be made, if so.
fn refusal(saves: &Saves) -> Option<String> {
    if saves.packaged {
        return Some("Packaged game: unpack it to back up or restore its saves.".into());
    }
    if saves.folders.is_empty() {
        return Some("No save path: add one in the game's editor (Library, X).".into());
    }
    saves.too_wide.as_ref().map(|path| {
        format!(
            "Unsafe save path {}: a restore would empty a folder other apps or games use.",
            path.display()
        )
    })
}

fn missing_prefix(saves: &Saves) -> Option<String> {
    saves
        .prefix
        .as_ref()
        .filter(|p| !p.is_dir())
        .map(|_| "Launch the game once first: its Proton prefix doesn't exist yet.".into())
}

// ── Copies ───────────────────────────────────────────────────────────────

fn io_error(path: &Path) -> impl Fn(std::io::Error) -> String + '_ {
    move |e| format!("{}: {e}", path.display())
}

/// Copy a file, on disk before this returns: the backup it belongs to may
/// replace the previous one right after.
fn copy_file(src: &Path, dst: &Path) -> Result<(), String> {
    paths::copy_with_mtime(src, dst).map_err(io_error(src))?;
    // Windows flushes through a handle with write access only.
    std::fs::File::options()
        .read(true)
        .write(cfg!(windows))
        .open(dst)
        .and_then(|f| f.sync_all())
        .map_err(io_error(dst))
}

/// Copy `src` into `dst`, empty folders included; the number of files.
fn copy_tree(src: &Path, dst: &Path) -> Result<usize, String> {
    std::fs::create_dir_all(dst).map_err(io_error(dst))?;
    entries(src)
        .into_iter()
        .map(|from| {
            let to = dst.join(from.file_name().unwrap_or_default());
            if from.is_dir() {
                copy_tree(&from, &to)
            } else {
                copy_file(&from, &to).map(|()| 1)
            }
        })
        .sum()
}

fn remove_dir_if_exists(dir: &Path) -> Result<(), String> {
    match std::fs::remove_dir_all(dir) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(io_error(dir)(e)),
        _ => Ok(()),
    }
}

/// `dir` emptied (created when missing), links left in place. `dir` itself
/// stays, so a save folder linked elsewhere still is.
fn empty_dir(dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(io_error(dir))?;
    entries(dir).iter().try_for_each(|path| {
        if path.is_dir() {
            std::fs::remove_dir_all(path)
        } else {
            std::fs::remove_file(path)
        }
        .map_err(io_error(path))
    })
}

/// Where a backup is written before it is complete (hidden).
fn part_of(backup: &Path) -> PathBuf {
    let name = backup.file_name().unwrap_or_default().to_string_lossy();
    backup.with_file_name(package::staging_name(&name))
}

/// Where the previous backup goes while the new one takes its name.
fn old_of(backup: &Path) -> PathBuf {
    let name = backup.file_name().unwrap_or_default().to_string_lossy();
    backup.with_file_name(format!(".{name}.old"))
}

fn rename(from: &Path, to: &Path) -> Result<(), String> {
    std::fs::rename(from, to).map_err(|e| format!("{} -> {}: {e}", from.display(), to.display()))
}

/// Finish a backup interrupted between its two renames (crash, power
/// loss): the previous backup, put aside, is the last complete one.
pub fn recover(saves: &Saves) -> Result<(), String> {
    let old = old_of(&saves.backup);
    if saves.backup.exists() || !old.is_dir() {
        return Ok(());
    }
    rename(&old, &saves.backup)
}

/// Copy the save folders into `saves.backup`. The copy is made under a
/// hidden name and replaces the previous backup only once complete: when it
/// fails, the previous backup stays as it was.
pub fn backup(saves: &Saves) -> Result<(), String> {
    if let Some(why) = refusal(saves) {
        return Err(why);
    }
    recover(saves)?;
    let (part, old) = (part_of(&saves.backup), old_of(&saves.backup));
    remove_dir_if_exists(&part)?;
    remove_dir_if_exists(&old)?;
    match copy_saves(saves, &part) {
        Ok(0) => {
            remove_dir_if_exists(&part)?;
            Err(NO_SAVES.into())
        }
        Ok(_) => swap_in(&part, &saves.backup, &old),
        Err(e) => {
            let _ = std::fs::remove_dir_all(&part);
            Err(format!("Backup failed, the previous one is kept: {e}"))
        }
    }
}

/// Copy the save folders that exist into `into`; the number of files.
fn copy_saves(saves: &Saves, into: &Path) -> Result<usize, String> {
    saves
        .folders
        .iter()
        .filter(|f| f.device.is_dir())
        .map(|f| copy_tree(&f.device, &into.join(&f.name)))
        .sum()
}

/// Put the finished copy `part` in place of `target`, by renames (no rename
/// replaces a folder on Windows): `target` → `old`, `part` → `target`.
fn swap_in(part: &Path, target: &Path, old: &Path) -> Result<(), String> {
    if target.exists() {
        rename(target, old)?;
    }
    rename(part, target)?;
    // Hidden, and removed by the next backup anyway.
    let _ = std::fs::remove_dir_all(old);
    Ok(())
}

/// Replace the save folders' contents with the backup. Folders with no copy
/// in the backup are left alone.
pub fn restore(saves: &Saves) -> Result<(), String> {
    if let Some(why) = refusal(saves).or_else(|| missing_prefix(saves)) {
        return Err(why);
    }
    recover(saves)?;
    let sources: Vec<(&SaveFolder, PathBuf)> = saves
        .folders
        .iter()
        .map(|f| (f, saves.backup.join(&f.name)))
        .filter(|(_, copy)| copy.is_dir())
        .collect();
    if sources.is_empty() {
        return Err(NO_BACKUP.into());
    }
    sources.iter().try_for_each(|(folder, copy)| {
        empty_dir(&folder.device)?;
        copy_tree(copy, &folder.device).map(|_| ())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::testutil::TempDir;

    #[test]
    fn sanitize_title_is_filesystem_safe() {
        assert_eq!(
            sanitize_title("Hollow Knight: Silksong?"),
            "Hollow_Knight__Silksong_"
        );
        assert_eq!(sanitize_title("  CON "), "CON_game");
        assert_eq!(sanitize_title("..."), "game");
        assert_eq!(sanitize_title("a.  "), "a");
    }

    #[test]
    fn folder_names_add_a_number_to_a_taken_name_ignoring_case() {
        let mut names = FolderNames::default();
        assert_eq!(names.claim("Saves"), "Saves");
        assert_eq!(names.claim("saves"), "saves_2");
        assert_eq!(names.claim("Saves"), "Saves_3");
    }

    /// A game in `<tmp>/games/Game` with these save folders.
    fn game(dir: &TempDir, saves: &[&str]) -> Game {
        let mut game = Game::skeleton(&dir.path().join("games/Game"));
        game.title = "Some Title".into();
        game.resolved_save_paths = saves.iter().map(|s| dir.path().join(s)).collect();
        game
    }

    fn saves(dir: &TempDir, game: &Game) -> Saves {
        saves_of(game, &dir.path().join("saves-copies"))
    }

    fn read(path: PathBuf) -> String {
        std::fs::read_to_string(path).unwrap()
    }

    #[test]
    fn a_backup_is_named_after_the_game_folder_with_one_folder_per_save_path() {
        let dir = TempDir::new();
        let game = game(&dir, &["local/Studio/Saves", "documents/Studio/Saves"]);
        let saves = saves(&dir, &game);
        assert_eq!(saves.backup, dir.path().join("saves-copies/Game"));
        let names: Vec<&str> = saves.folders.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["Saves", "Saves_2"]);
    }

    #[test]
    fn saves_in_the_game_folder_are_in_game() {
        let dir = TempDir::new();
        assert!(saves(&dir, &game(&dir, &["games/Game/saves"])).in_game);
        assert!(!saves(&dir, &game(&dir, &["home/.local/Game"])).in_game);
    }

    #[test]
    fn a_save_path_must_not_be_or_hold_a_folder_others_use() {
        let dir = TempDir::new();
        let game_dir = dir.mkdir("games/Game");
        let saves_root = dir.mkdir("saves-copies");
        let prefix = dir.path().join("pfx/drive_c");
        let kept = kept_folders(&game_dir, &saves_root, Some(&prefix));
        let home = paths::home_dir();
        let user = prefix.join("users/steamuser");
        let too_wide = |p: &Path| is_too_wide(p, &kept, &saves_root);
        assert!(too_wide(Path::new("/")));
        assert!(too_wide(&home));
        assert!(too_wide(&home.join(".local/share")));
        assert!(too_wide(&home.join("Documents")));
        assert!(too_wide(&game_dir));
        assert!(too_wide(&dir.path().join("games")));
        assert!(too_wide(&saves_root.join("Other")));
        assert!(too_wide(&user.join("AppData/Roaming")));
        assert!(!too_wide(&home.join(".local/share/Game")));
        assert!(!too_wide(&game_dir.join("saves")));
        assert!(!too_wide(&user.join("AppData/Roaming/Studio/Game")));
    }

    #[test]
    fn the_report_shows_both_sides_and_why_a_copy_cannot_be_made() {
        let dir = TempDir::new();
        dir.write("live/a.sav", b"12345");
        dir.write("live/sub/b.sav", b"123");
        let saves = saves(&dir, &game(&dir, &["live"]));

        let report = report(&saves);
        let device = report.device.unwrap();
        assert_eq!((device.files, device.bytes), (2, 8));
        assert_eq!(report.backup, None);
        assert_eq!(report.cannot_back_up, None);
        assert_eq!(report.cannot_restore.as_deref(), Some("No backup yet."));
    }

    #[test]
    fn a_save_folder_that_is_not_on_disk_is_reported_missing() {
        let dir = TempDir::new();
        dir.mkdir("empty");
        let saves = saves(&dir, &game(&dir, &["gone", "empty"]));

        let report = report(&saves);
        assert_eq!(report.missing, vec![dir.path().join("gone")]);
        assert_eq!(report.cannot_back_up.as_deref(), Some(NO_SAVES));
    }

    #[test]
    fn no_save_folder_on_disk_says_the_save_path_may_be_wrong() {
        let dir = TempDir::new();
        let report = report(&saves(&dir, &game(&dir, &["gone"])));
        assert!(report
            .cannot_back_up
            .unwrap()
            .starts_with("Save folder not found"));
    }

    #[test]
    fn nothing_can_be_copied_without_a_save_path() {
        let dir = TempDir::new();
        let report = report(&saves(&dir, &game(&dir, &[])));
        assert!(report.cannot_back_up.unwrap().starts_with("No save path"));
        assert!(report.cannot_restore.unwrap().starts_with("No save path"));
    }

    #[test]
    fn a_backup_then_a_restore_bring_the_saves_back_as_they_were() {
        let dir = TempDir::new();
        dir.write("one/slot1.sav", b"one");
        dir.write("two/Saves/slot2.sav", b"two");
        let saves = saves(&dir, &game(&dir, &["one", "two/Saves"]));

        backup(&saves).unwrap();
        assert_eq!(read(saves.backup.join("one/slot1.sav")), "one");
        assert_eq!(read(saves.backup.join("Saves/slot2.sav")), "two");
        let report = report(&saves);
        assert_eq!(report.device, report.backup, "same files, same dates");

        dir.write("one/slot1.sav", b"overwritten");
        dir.write("one/extra.sav", b"new");
        restore(&saves).unwrap();
        assert_eq!(read(dir.path().join("one/slot1.sav")), "one");
        assert!(!dir.path().join("one/extra.sav").exists());
        assert_eq!(read(dir.path().join("two/Saves/slot2.sav")), "two");
    }

    #[test]
    fn a_new_backup_replaces_the_previous_one_and_leaves_nothing_hidden() {
        let dir = TempDir::new();
        dir.write("live/new.sav", b"new");
        dir.write("saves-copies/Game/live/old.sav", b"old");
        dir.write("saves-copies/.Game.part/live/half.sav", b"interrupted");
        let saves = saves(&dir, &game(&dir, &["live"]));

        backup(&saves).unwrap();
        assert_eq!(read(saves.backup.join("live/new.sav")), "new");
        assert!(!saves.backup.join("live/old.sav").exists());
        assert_eq!(
            paths::sorted_entries(&dir.path().join("saves-copies")),
            ["Game"]
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_failed_backup_keeps_the_previous_one() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new();
        dir.write("live/a.sav", b"a");
        let unreadable = dir.path().join("live/b.sav");
        dir.write("live/b.sav", b"b");
        std::fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read(&unreadable).is_ok() {
            return; // running as root: every file is readable
        }
        dir.write("saves-copies/Game/live/a.sav", b"previous");
        let saves = saves(&dir, &game(&dir, &["live"]));

        let error = backup(&saves).unwrap_err();
        assert!(error.contains("previous one is kept"), "{error}");
        assert_eq!(read(saves.backup.join("live/a.sav")), "previous");
        assert_eq!(
            paths::sorted_entries(&dir.path().join("saves-copies")),
            ["Game"]
        );
    }

    #[test]
    fn backing_up_no_saves_is_refused_and_keeps_the_previous_backup() {
        let dir = TempDir::new();
        dir.mkdir("live");
        dir.write("saves-copies/Game/live/a.sav", b"previous");
        let saves = saves(&dir, &game(&dir, &["live"]));

        assert_eq!(backup(&saves).unwrap_err(), "No saves to back up yet.");
        assert_eq!(read(saves.backup.join("live/a.sav")), "previous");
        assert_eq!(
            paths::sorted_entries(&dir.path().join("saves-copies")),
            ["Game"]
        );
    }

    #[test]
    fn a_restore_creates_a_missing_save_folder() {
        let dir = TempDir::new();
        dir.write("saves-copies/Game/Saves/a.sav", b"a");
        let saves = saves(&dir, &game(&dir, &["home/Studio/Saves"]));

        restore(&saves).unwrap();
        assert_eq!(read(dir.path().join("home/Studio/Saves/a.sav")), "a");
    }

    #[test]
    fn a_restore_leaves_save_folders_without_a_backup_alone() {
        let dir = TempDir::new();
        dir.write("saves-copies/Game/one/a.sav", b"a");
        dir.write("two/keep.sav", b"keep");
        let saves = saves(&dir, &game(&dir, &["one", "two"]));

        restore(&saves).unwrap();
        assert_eq!(read(dir.path().join("one/a.sav")), "a");
        assert_eq!(read(dir.path().join("two/keep.sav")), "keep");
    }

    #[cfg(unix)]
    #[test]
    fn a_restore_keeps_a_save_folder_that_is_a_link() {
        let dir = TempDir::new();
        let elsewhere = dir.mkdir("sdcard/Saves");
        dir.write("sdcard/Saves/old.sav", b"old");
        std::os::unix::fs::symlink(&elsewhere, dir.path().join("Saves")).unwrap();
        dir.write("saves-copies/Game/Saves/a.sav", b"a");
        let saves = saves(&dir, &game(&dir, &["Saves"]));

        restore(&saves).unwrap();
        assert!(paths::is_symlink(&dir.path().join("Saves")));
        assert_eq!(read(elsewhere.join("a.sav")), "a");
        assert!(!elsewhere.join("old.sav").exists());
    }

    #[test]
    fn packaged_games_neither_back_up_nor_restore() {
        let dir = TempDir::new();
        dir.write("saves-copies/Game/saves/a.sav", b"a");
        let mut game = game(&dir, &["games/Game/saves"]);
        game.install = Install::Packaged {
            package: dir.path().join("games/Game.cartridge.tar"),
        };
        let saves = saves(&dir, &game);

        assert!(restore(&saves).unwrap_err().starts_with("Packaged game"));
        assert!(backup(&saves).unwrap_err().starts_with("Packaged game"));
        assert!(!dir.path().join("games/Game").exists());
    }

    #[test]
    fn an_interrupted_backup_gets_its_previous_backup_back() {
        let dir = TempDir::new();
        dir.write("saves-copies/.Game.old/live/a.sav", b"previous");
        dir.write("saves-copies/.Game.part/live/a.sav", b"half");
        let saves = saves(&dir, &game(&dir, &["live"]));

        recover(&saves).unwrap();
        assert_eq!(read(saves.backup.join("live/a.sav")), "previous");
        assert_eq!(report(&saves).backup.map(|s| s.files), Some(1));
    }

    #[cfg(unix)]
    #[test]
    fn copies_keep_empty_folders_and_leave_links_alone() {
        let dir = TempDir::new();
        dir.write("live/a.sav", b"a");
        dir.mkdir("live/empty");
        std::os::unix::fs::symlink(dir.mkdir("elsewhere"), dir.path().join("live/cloud")).unwrap();
        std::os::unix::fs::symlink("/nowhere", dir.path().join("live/broken")).unwrap();
        let saves = saves(&dir, &game(&dir, &["live"]));

        backup(&saves).unwrap();
        assert!(saves.backup.join("live/empty").is_dir());
        assert!(!saves.backup.join("live/cloud").exists());
        std::fs::remove_dir(dir.path().join("live/empty")).unwrap();
        restore(&saves).unwrap();
        assert!(dir.path().join("live/empty").is_dir());
        assert!(paths::is_symlink(&dir.path().join("live/cloud")));
        assert!(paths::is_symlink(&dir.path().join("live/broken")));
    }

    #[test]
    fn proton_saves_wait_for_the_prefix() {
        let dir = TempDir::new();
        dir.write("saves-copies/Game/Saves/a.sav", b"a");
        let prefix = dir.path().join("compatdata/1/pfx/drive_c");
        let saves = Saves {
            prefix: Some(prefix.clone()),
            ..saves(&dir, &game(&dir, &["compatdata/1/pfx/drive_c/Saves"]))
        };

        assert!(restore(&saves)
            .unwrap_err()
            .starts_with("Launch the game once"));
        std::fs::create_dir_all(&prefix).unwrap();
        restore(&saves).unwrap();
        assert_eq!(read(prefix.join("Saves/a.sav")), "a");
    }

    #[test]
    fn saves_not_found_before_the_prefix_exists_ask_to_launch_the_game() {
        let dir = TempDir::new();
        let saves = Saves {
            prefix: Some(dir.path().join("compatdata/1/pfx/drive_c")),
            ..saves(&dir, &game(&dir, &["compatdata/1/pfx/drive_c/Saves"]))
        };
        let report = report(&saves);
        assert!(report.none_found);
        assert!(report
            .cannot_back_up
            .unwrap()
            .starts_with("Launch the game once"));
    }
}
