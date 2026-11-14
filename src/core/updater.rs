//! Game updates from GitHub releases (`"update"` in game.json): read the
//! latest release, pick its file for the game's platform, and lay that
//! archive's files over the game folder. Like an installer run over an older
//! install, it only adds and replaces files: nothing is deleted, so saves,
//! settings, ROMs and mods the game keeps in its folder stay. `.cartridge/`
//! is never written by the archive.

use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

use super::app::GAME_DATA_DIR;
use super::models::Game;
use super::package::{self, Kind, Progress};
use super::{paths, platform, sgdb};

/// A release page answer is a few KB; anything this big is not one.
const MAX_RELEASE_BODY: u64 = 4 * 1024 * 1024;
/// Per read: a stalled download fails instead of hanging.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Asset {
    pub name: String,
    pub size: u64,
    pub url: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Release {
    pub tag: String,
    pub name: String,
    /// `YYYY-MM-DD`, empty when unknown.
    pub date: String,
    pub notes: String,
    pub assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct GithubAsset {
    name: String,
    size: u64,
    browser_download_url: String,
}

#[derive(Deserialize)]
struct GithubRelease {
    tag_name: String,
    name: Option<String>,
    published_at: Option<String>,
    body: Option<String>,
    #[serde(default)]
    assets: Vec<GithubAsset>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Archive {
    TarGz,
    Zip,
}

fn archive_of(name: &str) -> Option<Archive> {
    let name = name.to_lowercase();
    if name.ends_with(".tar.gz") || name.ends_with(".tgz") {
        Some(Archive::TarGz)
    } else if name.ends_with(".zip") {
        Some(Archive::Zip)
    } else {
        None
    }
}

// ── The release ──────────────────────────────────────────────────────────

/// The API address of the latest release behind a GitHub page
/// (`github.com/<owner>/<repo>`, any page under it).
pub fn api_url(page: &str) -> Result<String, String> {
    let rest = page
        .trim()
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_start_matches("www.");
    let mut parts = rest.split('/').filter(|p| !p.is_empty());
    let (host, owner, repo) = (parts.next(), parts.next(), parts.next());
    match (host, owner, repo) {
        (Some(host), Some(owner), Some(repo)) if host.eq_ignore_ascii_case("github.com") => Ok(
            format!("https://api.github.com/repos/{owner}/{repo}/releases/latest"),
        ),
        _ => Err(format!("Not a GitHub project page: {page}")),
    }
}

pub fn parse_release(json: &str) -> Result<Release, String> {
    let release: GithubRelease = serde_json::from_str(json).map_err(|e| e.to_string())?;
    Ok(Release {
        name: release
            .name
            .filter(|n| !n.trim().is_empty())
            .unwrap_or_else(|| release.tag_name.clone()),
        tag: release.tag_name,
        date: release
            .published_at
            .and_then(|d| d.get(..10).map(str::to_string))
            .unwrap_or_default(),
        notes: release.body.unwrap_or_default(),
        assets: release
            .assets
            .into_iter()
            .map(|a| Asset {
                name: a.name,
                size: a.size,
                url: a.browser_download_url,
            })
            .collect(),
    })
}

fn http_error(error: ureq::Error) -> String {
    match error {
        ureq::Error::Status(404, _) => "No release published yet".into(),
        ureq::Error::Status(403 | 429, _) => "GitHub's request limit is reached: try later".into(),
        other => other.to_string(),
    }
}

/// The latest release behind `page` (one request to GitHub).
pub fn check(page: &str) -> Result<Release, String> {
    let response = ureq::get(&api_url(page)?)
        .timeout(Duration::from_secs(15))
        .set("User-Agent", &sgdb::user_agent())
        .set("Accept", "application/vnd.github+json")
        .call()
        .map_err(http_error)?;
    let mut body = String::new();
    response
        .into_reader()
        .take(MAX_RELEASE_BODY + 1)
        .read_to_string(&mut body)
        .map_err(|e| e.to_string())?;
    if body.len() as u64 > MAX_RELEASE_BODY {
        return Err("GitHub's answer is too large for a release".into());
    }
    parse_release(&body)
}

// ── Which file ───────────────────────────────────────────────────────────

/// Words naming each OS / arch in release file names. Short ones must be a
/// whole word (`win` is in `darwin`); longer ones may sit anywhere.
const OS_WORDS: [(&str, &[&str]); 3] = [
    ("linux", &["linux"]),
    ("windows", &["windows", "win64", "win32", "win"]),
    ("macos", &["macos", "mac", "osx", "darwin"]),
];
const ARCH_WORDS: [(&str, &[&str]); 2] = [
    ("x64", &["x86_64", "x64", "amd64"]),
    ("arm64", &["arm64", "aarch64"]),
];
/// Builds that don't run from a plain folder.
const SKIPPED: [&str; 2] = ["flatpak", "snap"];

fn mentions(name: &str, words: &[&str]) -> bool {
    let tokens: Vec<&str> = name.split(|c: char| !c.is_ascii_alphanumeric()).collect();
    words
        .iter()
        .any(|w| (w.len() > 3 && name.contains(w)) || tokens.contains(w))
}

fn words_of<'a>(table: &[(&str, &'a [&'a str])], tag: &str) -> &'a [&'a str] {
    table
        .iter()
        .find(|(t, _)| *t == tag)
        .map_or(&[], |(_, words)| *words)
}

fn mentions_other_arch(name: &str, arch: &str) -> bool {
    ARCH_WORDS
        .iter()
        .filter(|(tag, _)| *tag != arch)
        .any(|(_, words)| mentions(name, words))
}

/// The release file to install: the first whose name contains `pattern`
/// when given, else the archive for `os`, preferring one that names `arch`
/// and never one that names another arch.
pub fn pick_asset<'a>(
    release: &'a Release,
    os: &str,
    arch: &str,
    pattern: &str,
) -> Option<&'a Asset> {
    let pattern = pattern.trim().to_lowercase();
    let usable = release.assets.iter().filter(|a| {
        let name = a.name.to_lowercase();
        archive_of(&name).is_some() && !SKIPPED.iter().any(|s| name.contains(s))
    });
    if !pattern.is_empty() {
        return usable
            .into_iter()
            .find(|a| a.name.to_lowercase().contains(&pattern));
    }
    let for_os: Vec<&Asset> = usable
        .filter(|a| {
            let name = a.name.to_lowercase();
            mentions(&name, words_of(&OS_WORDS, os)) && !mentions_other_arch(&name, arch)
        })
        .collect();
    let names_arch = |a: &&&Asset| mentions(&a.name.to_lowercase(), words_of(&ARCH_WORDS, arch));
    for_os
        .iter()
        .find(names_arch)
        .or_else(|| for_os.first())
        .copied()
}

/// The OS and arch the game runs as here: its target for this machine's
/// OS, else (a Windows game under Proton) its first target.
pub fn platform_of(game: &Game) -> (String, String) {
    let os = platform::os_tag();
    let target = game
        .targets
        .iter()
        .find(|t| t.os == os)
        .or_else(|| game.targets.first());
    let pick = |value: Option<&str>, fallback: &str| {
        value
            .filter(|v| !v.is_empty())
            .unwrap_or(fallback)
            .to_string()
    };
    (
        pick(target.map(|t| t.os.as_str()), os),
        pick(target.map(|t| t.arch.as_str()), platform::arch_tag()),
    )
}

/// The file of `release` to install for `game`.
pub fn asset_for<'a>(game: &Game, release: &'a Release) -> Option<&'a Asset> {
    let (os, arch) = platform_of(game);
    pick_asset(release, &os, &arch, &game.update.asset)
}

// ── Installing ───────────────────────────────────────────────────────────

/// Stream `asset` into `dest`, counting bytes in `progress`.
fn download(asset: &Asset, dest: &Path, progress: &Progress) -> Result<(), String> {
    progress.start(asset.size);
    let response = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        .timeout_read(DOWNLOAD_TIMEOUT)
        .build()
        .get(&asset.url)
        .set("User-Agent", &sgdb::user_agent())
        .call()
        .map_err(http_error)?;
    let mut reader = response.into_reader();
    let mut file =
        BufWriter::new(File::create(dest).map_err(|e| format!("{}: {e}", dest.display()))?);
    let mut buf = vec![0u8; 1 << 16];
    let mut written = 0u64;
    // A loop: each chunk checks for a cancel and reports progress.
    loop {
        if progress.is_cancelled() {
            return Err("cancelled".into());
        }
        let n = reader.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n]).map_err(|e| e.to_string())?;
        written += n as u64;
        progress.advance(n as u64);
    }
    file.flush().map_err(|e| e.to_string())?;
    if written != asset.size {
        return Err(format!(
            "{}: got {written} bytes of {}",
            asset.name, asset.size
        ));
    }
    Ok(())
}

/// Unpack `archive` into the empty folder `dest`. Both readers refuse
/// entries that would land outside it.
fn extract(kind: Archive, archive: &Path, dest: &Path) -> Result<(), String> {
    let file = BufReader::new(File::open(archive).map_err(|e| e.to_string())?);
    match kind {
        Archive::TarGz => {
            let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(file));
            tar.set_preserve_permissions(true);
            tar.unpack(dest).map_err(|e| e.to_string())
        }
        Archive::Zip => zip::ZipArchive::new(file)
            .and_then(|mut zip| zip.extract(dest))
            .map_err(|e| e.to_string()),
    }
}

/// Folders archivers add beside the content (macOS resource forks): never
/// part of the game.
const ARCHIVE_JUNK: [&str; 1] = ["__MACOSX"];

/// Paths of the archive that are not laid over the game.
fn is_skipped(rel: &Path) -> bool {
    rel.starts_with(GAME_DATA_DIR) || ARCHIVE_JUNK.iter().any(|junk| rel.starts_with(junk))
}

/// What an unpacked archive holds at its top, junk left out.
fn content_entries(dir: &Path) -> io::Result<Vec<fs::DirEntry>> {
    Ok(fs::read_dir(dir)?
        .collect::<io::Result<Vec<_>>>()?
        .into_iter()
        .filter(|e| !is_skipped(Path::new(&e.file_name())))
        .collect())
}

/// The archive `extracted` holds when it holds nothing else: zips that wrap
/// a `.tar.gz` so the exec bits survive.
fn inner_archive(extracted: &Path) -> io::Result<Option<(Archive, PathBuf)>> {
    let entries = content_entries(extracted)?;
    let [single] = entries.as_slice() else {
        return Ok(None);
    };
    if !single.file_type()?.is_file() {
        return Ok(None);
    }
    let kind = archive_of(&single.file_name().to_string_lossy());
    Ok(kind.map(|kind| (kind, single.path())))
}

/// An empty `dir`, whatever a crash left there.
fn fresh_dir(dir: &Path) -> Result<(), String> {
    let _ = fs::remove_dir_all(dir);
    fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))
}

/// Unpack `archive` into `outer`, and the archive it wraps, if any, into
/// `inner`. Returns the folder holding the files.
fn unpack(kind: Archive, archive: &Path, outer: &Path, inner: &Path) -> Result<PathBuf, String> {
    fresh_dir(outer)?;
    extract(kind, archive, outer)?;
    let Some((inner_kind, wrapped)) = inner_archive(outer).map_err(|e| e.to_string())? else {
        return Ok(outer.to_path_buf());
    };
    fresh_dir(inner)?;
    extract(inner_kind, &wrapped, inner)?;
    Ok(inner.to_path_buf())
}

/// Where the archive's game files start: inside its single top folder
/// (`Game-v2/...`) unless the game has a folder of that name itself.
fn content_root(extracted: &Path, game_dir: &Path) -> io::Result<PathBuf> {
    let entries = content_entries(extracted)?;
    let [single] = entries.as_slice() else {
        return Ok(extracted.to_path_buf());
    };
    let is_dir = single.file_type()?.is_dir();
    let name = single.file_name();
    Ok(if is_dir && !game_dir.join(&name).exists() {
        extracted.join(name)
    } else {
        extracted.to_path_buf()
    })
}

/// Why `item` can't be laid over the game, if it can't: it would go through
/// a link, or swap a file and a folder (the update deletes nothing).
fn conflict(item: &package::Item, game_dir: &Path) -> Option<String> {
    let dest = match package::safe_join(game_dir, &item.rel) {
        Ok(dest) => dest,
        Err(e) => return Some(e),
    };
    let existing_is_dir = fs::symlink_metadata(&dest).ok()?.is_dir();
    let why = match (&item.kind, existing_is_dir) {
        (Kind::Dir, false) => "the update has a folder where the game has a file or link",
        (Kind::File(_) | Kind::Symlink(_), true) => {
            "the update has a file where the game has a folder"
        }
        _ => return None,
    };
    Some(format!("{}: {why}", item.rel.display()))
}

/// Move `src` over `dest` (a file or a link), replacing it.
fn replace_file(src: &Path, dest: &Path) -> io::Result<()> {
    if fs::rename(src, dest).is_ok() {
        return Ok(());
    }
    // Another filesystem (the game folder is a link elsewhere): copy next
    // to `dest`, then swap, so a failed copy leaves the old file in place.
    let name = dest.file_name().unwrap_or_default().to_string_lossy();
    let temp = dest.with_file_name(package::staging_name(&name));
    let result = fs::copy(src, &temp).and_then(|_| fs::rename(&temp, dest));
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

/// Lay one item over the game (checked by `conflict` first).
fn place(item: &package::Item, src_root: &Path, game_dir: &Path) -> io::Result<()> {
    let (src, dest) = (src_root.join(&item.rel), game_dir.join(&item.rel));
    match &item.kind {
        Kind::Dir => fs::create_dir_all(&dest),
        Kind::File(_) => replace_file(&src, &dest),
        Kind::Symlink(target) => {
            if fs::symlink_metadata(&dest).is_ok() {
                fs::remove_file(&dest)?;
            }
            package::make_symlink(target, &dest)
        }
    }
}

/// Add or replace every file of `src_root` in `game_dir`; nothing in
/// `game_dir` is deleted. Every item is checked before the first is written,
/// so a refused update leaves the game as it was. Returns how many files
/// were written.
fn upsert(src_root: &Path, game_dir: &Path) -> Result<usize, String> {
    let items: Vec<package::Item> = package::walk(src_root)
        .map_err(|e| e.to_string())?
        .into_iter()
        .filter(|item| !is_skipped(&item.rel))
        .collect();
    if let Some(why) = items.iter().find_map(|item| conflict(item, game_dir)) {
        return Err(why);
    }
    items
        .iter()
        .map(|item| {
            place(item, src_root, game_dir)
                .map(|()| usize::from(item.kind != Kind::Dir))
                .map_err(|e| format!("{}: {e}", item.rel.display()))
        })
        .sum()
}

/// Rewrite the game's game.json as on disk now, changed by `change`.
fn rewrite_game_json(game: &Game, change: impl FnOnce(&mut Game)) -> Result<(), String> {
    let path = game.game_json_path();
    let text = fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut fresh = Game::from_json(&game.dir, &text).map_err(|e| e.to_string())?;
    change(&mut fresh);
    paths::write_atomic(&path, fresh.to_json()).map_err(|e| format!("{}: {e}", path.display()))
}

/// Remember `tag` as the installed release, without installing anything
/// (the game already is at that release).
pub fn mark_installed(game: &Game, tag: &str) -> Result<(), String> {
    rewrite_game_json(game, |g| g.update.installed = tag.to_string())
}

/// Download `asset` of release `tag` and lay its files over the game.
/// Downloads and unpacks under hidden names next to the game folder (the
/// scanner ignores them), removed whatever happens.
pub fn install(
    game: &Game,
    asset: &Asset,
    tag: &str,
    progress: &Progress,
) -> Result<PathBuf, String> {
    if !game.is_installed() {
        return Err("Unpack the game before updating it".into());
    }
    let kind = archive_of(&asset.name).ok_or_else(|| format!("{}: not an archive", asset.name))?;
    let games_dir = game
        .dir
        .parent()
        .ok_or_else(|| format!("{}: no parent folder", game.dir.display()))?;
    let name = &game.folder_name;
    let archive = games_dir.join(package::staging_name(&format!(
        "{name}.update-{}",
        asset.name
    )));
    let extracted = games_dir.join(package::staging_name(&format!("{name}.update")));
    let inner = games_dir.join(package::staging_name(&format!("{name}.update-inner")));
    let result = (|| {
        download(asset, &archive, progress)?;
        let unpacked = unpack(kind, &archive, &extracted, &inner)?;
        if progress.is_cancelled() {
            return Err("cancelled".into());
        }
        let root = content_root(&unpacked, &game.dir).map_err(|e| e.to_string())?;
        let files = upsert(&root, &game.dir)?;
        log::info!("{}: {files} files from {} ({tag})", game.title, asset.name);
        // The size changed: the run after the update measures it again.
        rewrite_game_json(game, |g| {
            g.update.installed = tag.to_string();
            g.size = None;
        })
    })();
    let _ = fs::remove_file(&archive);
    let _ = fs::remove_dir_all(&extracted);
    let _ = fs::remove_dir_all(&inner);
    result.map(|()| game.dir.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::models::Target;
    use crate::core::testutil::TempDir;

    fn banjo() -> Release {
        parse_release(include_str!("../../testdata/github_release.json")).unwrap()
    }

    fn asset(name: &str) -> Asset {
        Asset {
            name: name.into(),
            size: 1,
            url: String::new(),
        }
    }

    fn release(names: &[&str]) -> Release {
        Release {
            tag: "v1".into(),
            name: "v1".into(),
            date: String::new(),
            notes: String::new(),
            assets: names.iter().map(|n| asset(n)).collect(),
        }
    }

    fn picked(release: &Release, os: &str, arch: &str, pattern: &str) -> Option<String> {
        pick_asset(release, os, arch, pattern).map(|a| a.name.clone())
    }

    #[test]
    fn api_url_comes_from_any_page_of_the_project() {
        let api = "https://api.github.com/repos/BanjoRecomp/BanjoRecomp/releases/latest";
        for page in [
            "https://github.com/BanjoRecomp/BanjoRecomp/releases/latest",
            "github.com/BanjoRecomp/BanjoRecomp",
            "https://www.github.com/BanjoRecomp/BanjoRecomp/",
        ] {
            assert_eq!(api_url(page).as_deref(), Ok(api), "{page}");
        }
        assert!(api_url("https://gitlab.com/a/b").is_err());
        assert!(api_url("https://github.com/only-owner").is_err());
    }

    #[test]
    fn parses_a_github_release() {
        let release = banjo();
        assert_eq!(release.tag, "v1.0.2");
        assert_eq!(release.date, "2026-07-31");
        assert_eq!(release.assets.len(), 5);
        assert_eq!(release.assets[2].size, 8_380_240);
        assert!(release.assets[2].url.ends_with("Linux-X64.tar.gz"));
    }

    #[test]
    fn picks_the_archive_for_the_os_and_arch() {
        let release = banjo();
        let name = |os, arch| picked(&release, os, arch, "");
        assert_eq!(
            name("linux", "x64").unwrap(),
            "BanjoRecompiled-v1.0.2-Linux-X64.tar.gz"
        );
        assert_eq!(
            name("linux", "arm64").unwrap(),
            "BanjoRecompiled-v1.0.2-Linux-ARM64.tar.gz"
        );
        assert_eq!(
            name("windows", "x64").unwrap(),
            "BanjoRecompiled-v1.0.2-Windows.zip"
        );
        assert_eq!(
            name("macos", "arm64").unwrap(),
            "BanjoRecompiled-v1.0.2-macOS.zip"
        );
    }

    #[test]
    fn picks_the_linux_build_of_real_releases() {
        let starfox = release(&[
            "SF64Recomp-Dev-Cheats.zip",
            "Starfox64Recompiled-v1.0.3-Flatpak-X64-Release.zip",
            "Starfox64Recompiled-v1.0.3-Linux-ARM64-Release.zip",
            "Starfox64Recompiled-v1.0.3-Linux-X64-Release.zip",
            "Starfox64Recompiled-v1.0.3-Windows-RelWithDebInfo.zip",
        ]);
        assert_eq!(
            picked(&starfox, "linux", "x64", "").unwrap(),
            "Starfox64Recompiled-v1.0.3-Linux-X64-Release.zip"
        );
        let midas = release(&[
            "ProjectMidas-v1.0.2b-linux.zip",
            "ProjectMidas-v1.0.2b-steamdeck.zip",
            "ProjectMidas-v1.0.2b-windows.zip",
        ]);
        assert_eq!(
            picked(&midas, "linux", "x64", "").unwrap(),
            "ProjectMidas-v1.0.2b-linux.zip"
        );
    }

    #[test]
    fn a_pattern_picks_the_file_by_name() {
        let release = banjo();
        assert_eq!(
            picked(&release, "linux", "x64", "arm64").unwrap(),
            "BanjoRecompiled-v1.0.2-Linux-ARM64.tar.gz"
        );
        assert_eq!(picked(&release, "linux", "x64", "nothing like it"), None);
    }

    #[test]
    fn short_os_words_must_be_whole_words() {
        let release = release(&["Game-darwin.zip", "Game-win.zip"]);
        assert_eq!(
            picked(&release, "windows", "x64", "").unwrap(),
            "Game-win.zip"
        );
        assert_eq!(
            picked(&release, "macos", "x64", "").unwrap(),
            "Game-darwin.zip"
        );
    }

    #[test]
    fn skips_files_that_are_no_folder_archive() {
        let release = release(&[
            "Game-linux-x64.AppImage",
            "Game-linux-x64.tar.gz.sha256",
            "Game-linux-flatpak-x64.zip",
            "Game-linux-x86_64.tgz",
        ]);
        assert_eq!(
            picked(&release, "linux", "x64", "").unwrap(),
            "Game-linux-x86_64.tgz"
        );
        assert_eq!(picked(&release, "linux", "arm64", ""), None);
    }

    #[test]
    fn the_platform_is_the_target_for_this_os_else_the_first() {
        let target = |os: &str, arch: &str| Target {
            os: os.into(),
            arch: arch.into(),
            ..Default::default()
        };
        let mut game = Game::skeleton(Path::new("/g/G"));
        game.targets = vec![target("windows", "x64")];
        assert_eq!(platform_of(&game), ("windows".into(), "x64".into()));
        game.targets.push(target(platform::os_tag(), "arm64"));
        assert_eq!(
            platform_of(&game),
            (platform::os_tag().into(), "arm64".into())
        );
    }

    /// A game folder with user files next to the game's own.
    fn installed_game(dir: &TempDir) -> Game {
        dir.write("games/Banjo/BanjoRecompiled", b"old binary");
        dir.write("games/Banjo/assets/old.svg", b"old");
        dir.write("games/Banjo/saves/bk.bin", b"my save");
        dir.write("games/Banjo/rom.z64", b"rom");
        let mut game = Game::skeleton(&dir.path().join("games/Banjo"));
        game.size = Some(42);
        game.update.url = "https://github.com/BanjoRecomp/BanjoRecomp".into();
        crate::core::persister::write_game_json(&mut game).unwrap();
        game
    }

    #[test]
    fn upsert_replaces_and_adds_files_and_deletes_none() {
        let dir = TempDir::new();
        let game = installed_game(&dir);
        dir.write("new/BanjoRecompiled", b"new binary");
        dir.write("new/assets/new.svg", b"new");
        dir.write("new/.cartridge/game.json", b"{}");
        let written = upsert(&dir.path().join("new"), &game.dir).unwrap();
        assert_eq!(written, 2);
        let read = |rel: &str| fs::read(game.dir.join(rel)).unwrap();
        assert_eq!(read("BanjoRecompiled"), b"new binary");
        assert_eq!(read("assets/new.svg"), b"new");
        assert_eq!(read("assets/old.svg"), b"old");
        assert_eq!(read("saves/bk.bin"), b"my save");
        assert_eq!(read("rom.z64"), b"rom");
        assert_ne!(read(".cartridge/game.json"), b"{}");
    }

    #[test]
    fn upsert_refuses_a_file_over_a_folder_before_writing_anything() {
        let dir = TempDir::new();
        let game = installed_game(&dir);
        dir.write("new/BanjoRecompiled", b"new binary");
        dir.write("new/saves", b"not a folder");
        assert!(upsert(&dir.path().join("new"), &game.dir).is_err());
        assert_eq!(fs::read(game.dir.join("saves/bk.bin")).unwrap(), b"my save");
        assert_eq!(
            fs::read(game.dir.join("BanjoRecompiled")).unwrap(),
            b"old binary"
        );
    }

    #[test]
    fn upsert_refuses_a_folder_over_a_file() {
        let dir = TempDir::new();
        let game = installed_game(&dir);
        dir.write("new/rom.z64/part", b"x");
        assert!(upsert(&dir.path().join("new"), &game.dir).is_err());
        assert_eq!(fs::read(game.dir.join("rom.z64")).unwrap(), b"rom");
    }

    #[cfg(unix)]
    #[test]
    fn upsert_never_writes_through_a_link() {
        let dir = TempDir::new();
        let game = installed_game(&dir);
        let outside = dir.mkdir("outside");
        std::os::unix::fs::symlink(&outside, game.dir.join("data")).unwrap();
        dir.write("new/data/evil", b"x");
        assert!(upsert(&dir.path().join("new"), &game.dir).is_err());
        assert!(!outside.join("evil").exists());
    }

    #[test]
    fn macos_junk_is_neither_the_content_nor_installed() {
        let dir = TempDir::new();
        let game = installed_game(&dir);
        dir.write("x/Banjo-v2/BanjoRecompiled", b"new");
        dir.write("x/__MACOSX/Banjo-v2/._BanjoRecompiled", b"fork");
        let extracted = dir.path().join("x");
        assert_eq!(
            content_root(&extracted, &game.dir).unwrap(),
            extracted.join("Banjo-v2")
        );
        dir.write("y/BanjoRecompiled", b"new");
        dir.write("y/__MACOSX/._BanjoRecompiled", b"fork");
        upsert(&dir.path().join("y"), &game.dir).unwrap();
        assert!(!game.dir.join("__MACOSX").exists());
    }

    #[test]
    fn a_single_top_folder_is_the_content_unless_the_game_has_one_like_it() {
        let dir = TempDir::new();
        let game = installed_game(&dir);
        dir.write("x/Banjo-v2/BanjoRecompiled", b"new");
        let extracted = dir.path().join("x");
        assert_eq!(
            content_root(&extracted, &game.dir).unwrap(),
            extracted.join("Banjo-v2")
        );
        dir.write("y/assets/new.svg", b"new");
        let only_assets = dir.path().join("y");
        assert_eq!(content_root(&only_assets, &game.dir).unwrap(), only_assets);
    }

    fn tar_gz(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
            Vec::new(),
            flate2::Compression::fast(),
        ));
        for (path, data) in files {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            builder.append_data(&mut header, path, *data).unwrap();
        }
        builder.into_inner().unwrap().finish().unwrap()
    }

    #[test]
    fn extracts_a_tar_gz_with_its_exec_bits() {
        let dir = TempDir::new();
        let archive = dir.write("a.tar.gz", &tar_gz(&[("./Game", b"bin"), ("data/x", b"x")]));
        let dest = dir.mkdir("out");
        extract(Archive::TarGz, &archive, &dest).unwrap();
        assert_eq!(fs::read(dest.join("data/x")).unwrap(), b"x");
        assert!(platform::has_exec_permission(&dest.join("Game")));
    }

    #[test]
    fn a_zip_wrapping_a_tar_gz_unpacks_both_and_keeps_exec_bits() {
        let dir = TempDir::new();
        let mut zip = zip::ZipWriter::new(io::Cursor::new(Vec::new()));
        zip.start_file("Game.tar.gz", zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(&tar_gz(&[("Game", b"bin"), ("assets/a", b"a")]))
            .unwrap();
        let archive = dir.write("a.zip", &zip.finish().unwrap().into_inner());
        let (outer, inner) = (dir.path().join("outer"), dir.path().join("inner"));
        let unpacked = unpack(Archive::Zip, &archive, &outer, &inner).unwrap();
        assert_eq!(unpacked, inner);
        assert_eq!(fs::read(inner.join("assets/a")).unwrap(), b"a");
        assert!(platform::has_exec_permission(&inner.join("Game")));
    }

    #[test]
    fn an_archive_with_more_than_an_archive_is_not_unwrapped() {
        let dir = TempDir::new();
        let archive = dir.write(
            "a.tar.gz",
            &tar_gz(&[("data.zip", b"not unpacked"), ("Game", b"bin")]),
        );
        let (outer, inner) = (dir.path().join("outer"), dir.path().join("inner"));
        assert_eq!(
            unpack(Archive::TarGz, &archive, &outer, &inner).unwrap(),
            outer
        );
        assert!(!inner.exists());
    }

    #[test]
    fn extracts_a_zip() {
        let dir = TempDir::new();
        let mut zip = zip::ZipWriter::new(io::Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default();
        zip.start_file("Game/Game.exe", options).unwrap();
        zip.write_all(b"exe").unwrap();
        let bytes = zip.finish().unwrap().into_inner();
        let archive = dir.write("a.zip", &bytes);
        let dest = dir.mkdir("out");
        extract(Archive::Zip, &archive, &dest).unwrap();
        assert_eq!(fs::read(dest.join("Game/Game.exe")).unwrap(), b"exe");
    }

    #[test]
    fn marking_a_release_installed_keeps_the_rest_of_game_json() {
        let dir = TempDir::new();
        let game = installed_game(&dir);
        mark_installed(&game, "v1.0.2").unwrap();
        let text = fs::read_to_string(game.game_json_path()).unwrap();
        let back = Game::from_json(&game.dir, &text).unwrap();
        assert_eq!(back.update.installed, "v1.0.2");
        assert_eq!(back.update.url, game.update.url);
        assert_eq!(back.size, Some(42));
    }

    #[test]
    fn a_packaged_only_game_is_not_updated() {
        let mut game = Game::skeleton(Path::new("/g/G"));
        game.install = crate::core::models::Install::Packaged {
            package: "/g/G.cartridge.tar".into(),
        };
        let result = install(&game, &asset("G.zip"), "v1", &Progress::default());
        assert!(result.unwrap_err().contains("Unpack"));
    }
}
