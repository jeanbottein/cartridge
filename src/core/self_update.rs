//! Self-update from the GitHub project this binary was built for. The
//! release workflow bakes its repository in (`CARTRIDGE_REPO`), so a fork's
//! builds update from the fork's releases; a build without one (local,
//! `cargo run`) never replaces itself.
//!
//! A release ships ready cartridges (`Cartridge/<app>` plus READMEs, see
//! package.sh): only the app is taken from the archive and put in place of
//! the running one, which is moved aside first and removed at the next
//! launch. The download is checked against the sha256 GitHub publishes for
//! it (see `updater::download`).

use std::fs;
use std::path::{Path, PathBuf};

use super::app::{self, DISPLAY_NAME};
use super::package::{self, Progress};
use super::updater::{self, Asset, Platform, Release};
use super::{paths, platform};

/// This build's version, compared with release tags. Release builds take it
/// from their tag (`CARTRIDGE_VERSION`, set by release.yml), so Cargo.toml's
/// version is only what local builds report.
pub const VERSION: &str = match option_env!("CARTRIDGE_VERSION") {
    Some(version) if !version.is_empty() => version,
    _ => env!("CARGO_PKG_VERSION"),
};

/// `owner/name` of the GitHub project this build updates from.
pub fn repo() -> Option<&'static str> {
    option_env!("CARTRIDGE_REPO").filter(|repo| !repo.trim().is_empty())
}

pub fn page(repo: &str) -> String {
    format!("https://github.com/{repo}")
}

/// The numbers of a version or tag: `v1.2.3-rc1` → `[1, 2, 3]`.
fn numbers(version: &str) -> Vec<u64> {
    version
        .trim()
        .trim_start_matches(['v', 'V'])
        .split('.')
        .map(|part| {
            let digits: String = part.chars().take_while(char::is_ascii_digit).collect();
            digits.parse().unwrap_or(0)
        })
        .collect()
}

/// `tag` names a later version than `current` (`v0.10.0` is after `0.9.1`).
fn is_newer(tag: &str, current: &str) -> bool {
    numbers(tag) > numbers(current)
}

/// The app's name inside release archives (`app_name` in release.yml).
fn shipped_name() -> &'static str {
    match platform::os_tag() {
        "macos" => "Cartridge.app",
        "windows" => "cartridge.exe",
        _ => "Cartridge.AppImage",
    }
}

fn this_machine() -> Platform {
    Platform {
        os: platform::os_tag().into(),
        arch: platform::arch_tag().into(),
        target: shipped_name().into(),
    }
}

/// A release newer than this build, and its file for this machine.
#[derive(Clone, Debug)]
pub struct Available {
    pub release: Release,
    pub asset: Asset,
}

/// What the latest release of `repo` offers: `None` when this build is
/// as new (one request to GitHub).
pub fn check(repo: &str) -> Result<Option<Available>, String> {
    let release = updater::check(&page(repo))?;
    if !is_newer(&release.tag, VERSION) {
        return Ok(None);
    }
    let asset = updater::pick_asset(&release, &this_machine(), "")
        .cloned()
        .ok_or_else(|| format!("{} has no file for this machine", release.tag))?;
    Ok(Some(Available { release, asset }))
}

/// The file to replace: the AppImage, `.app` or binary that was launched.
pub fn target() -> Result<PathBuf, String> {
    let file = app::launched_file().ok_or("Cannot tell where this app is")?;
    if app::is_translocated(&file) {
        return Err(format!(
            "macOS runs {DISPLAY_NAME} from a hidden copy: move it out of Downloads first"
        ));
    }
    Ok(file)
}

/// Where the replaced app waits for the next launch to remove it.
fn previous_of(target: &Path) -> PathBuf {
    let name = target.file_name().unwrap_or_default().to_string_lossy();
    target.with_file_name(format!(".{name}.previous"))
}

fn remove_any(path: &Path) {
    let _ = fs::remove_dir_all(path).or_else(|_| fs::remove_file(path));
}

/// Remove the app an update replaced (it ran until the restart).
pub fn remove_previous(target: &Path) {
    let previous = previous_of(target);
    if fs::symlink_metadata(&previous).is_ok() {
        remove_any(&previous);
        log::info!("Removed the previous app: {}", previous.display());
    }
}

/// The app in an unpacked release: at its top or in its top folder.
fn find_app(root: &Path) -> Result<PathBuf, String> {
    let name = shipped_name();
    std::iter::once(root.to_path_buf())
        .chain(paths::visible_subdirs(root))
        .map(|dir| dir.join(name))
        .find(|path| path.exists())
        .ok_or_else(|| format!("The release archive holds no {name}"))
}

/// Put `new` in place of the running `target`: the old one is moved aside
/// first (Windows renames a running exe but won't replace it; an `.app` is
/// a folder), and put back if the new one can't take its place.
fn swap(new: &Path, target: &Path) -> Result<(), String> {
    let fail = |e: std::io::Error| format!("{}: {e}", target.display());
    if new.is_file() {
        platform::make_executable(new).map_err(fail)?;
    }
    let previous = previous_of(target);
    remove_any(&previous);
    fs::rename(target, &previous).map_err(fail)?;
    fs::rename(new, target).map_err(|e| {
        let _ = fs::rename(&previous, target);
        fail(e)
    })
}

/// Download `asset` and put the app it holds in place of `target`.
/// Everything is staged under hidden names next to `target` (same
/// filesystem: the swap is two renames), removed whatever happens.
pub fn install(asset: &Asset, target: &Path, progress: &Progress) -> Result<(), String> {
    let dir = target.parent().ok_or("The app has no folder")?;
    let staged = |what: &str| dir.join(package::staging_name(&format!("{}.{what}", app::APP_NAME)));
    let (download, unpacked, inner) = (staged("download"), staged("update"), staged("inner"));
    let result = (|| {
        updater::download(asset, &download, progress)?;
        let app = match updater::archive_of(&asset.name) {
            Some(kind) => find_app(&updater::unpack(kind, &download, &unpacked, &inner)?)?,
            None => download.clone(),
        };
        if progress.is_cancelled() {
            return Err("cancelled".into());
        }
        swap(&app, target)
    })();
    [&download, &unpacked, &inner]
        .into_iter()
        .for_each(|p| remove_any(p));
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::testutil::TempDir;

    #[test]
    fn versions_compare_by_number_not_text() {
        assert!(is_newer("v0.10.0", "0.9.1"));
        assert!(is_newer("v0.2.1", "0.2.0"));
        assert!(is_newer("1.0", "0.99.99"));
        assert!(!is_newer("v0.2.0", "0.2.0"));
        assert!(!is_newer("0.1.9", "0.2.0"));
        assert!(!is_newer("nightly", "0.2.0"));
    }

    #[test]
    fn picks_this_machines_cartridge_archive() {
        let asset = |name: &str| Asset {
            name: name.into(),
            size: 1,
            url: String::new(),
            build: String::new(),
        };
        let release = Release {
            tag: "v9.0.0".into(),
            name: String::new(),
            date: String::new(),
            notes: String::new(),
            assets: [
                "SHA256SUMS",
                "cartridge-linux-aarch64.tar.gz",
                "cartridge-linux-x86_64.tar.gz",
                "cartridge-macos-arm64.zip",
                "cartridge-windows-x86_64.zip",
            ]
            .map(asset)
            .to_vec(),
        };
        let expected = match (platform::os_tag(), platform::arch_tag()) {
            ("linux", "x64") => "cartridge-linux-x86_64.tar.gz",
            ("linux", "arm64") => "cartridge-linux-aarch64.tar.gz",
            ("macos", "arm64") => "cartridge-macos-arm64.zip",
            ("windows", "x64") => "cartridge-windows-x86_64.zip",
            _ => return,
        };
        let picked = updater::pick_asset(&release, &this_machine(), "").unwrap();
        assert_eq!(picked.name, expected);
    }

    #[test]
    fn finds_the_app_in_the_cartridge_folder() {
        let dir = TempDir::new();
        dir.write("Cartridge/README.txt", b"");
        dir.write(&format!("Cartridge/{}", shipped_name()), b"new");
        let found = find_app(dir.path()).unwrap();
        assert_eq!(found, dir.path().join("Cartridge").join(shipped_name()));
        assert!(find_app(&dir.mkdir("empty")).is_err());
    }

    #[test]
    fn swap_moves_the_old_app_aside_until_the_next_launch() {
        let dir = TempDir::new();
        dir.write("cartridge", b"old");
        dir.write("new/cartridge", b"new");
        let target = dir.path().join("cartridge");
        swap(&dir.path().join("new/cartridge"), &target).unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"new");
        assert_eq!(fs::read(previous_of(&target)).unwrap(), b"old");
        remove_previous(&target);
        assert!(!previous_of(&target).exists() && target.exists());
    }

    #[test]
    fn a_failed_swap_puts_the_old_app_back() {
        let dir = TempDir::new();
        dir.write("cartridge", b"old");
        let target = dir.path().join("cartridge");
        assert!(swap(&dir.path().join("missing"), &target).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"old");
    }

    #[test]
    fn swaps_an_app_bundle_folder() {
        let dir = TempDir::new();
        dir.write("Cartridge.app/Contents/old", b"");
        dir.write("new/Cartridge.app/Contents/new", b"");
        let target = dir.path().join("Cartridge.app");
        swap(&dir.path().join("new/Cartridge.app"), &target).unwrap();
        assert!(target.join("Contents/new").exists() && !target.join("Contents/old").exists());
    }
}
