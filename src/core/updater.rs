//! Game updates from GitHub releases (`"update"` in game.json): read the
//! latest release, pick its file for the game's platform, and lay that
//! archive's files over the game folder (a release file that is the program
//! itself, like an AppImage, replaces the game's executable). Like an
//! installer run over an older install, it only adds and replaces files:
//! nothing is deleted, so saves, settings, ROMs and mods the game keeps in
//! its folder stay. `.cartridge/` is never written by the archive.

use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use sha2::{Digest, Sha256};
use zip::read::{ExtractOptions, SymlinkExtractAction};

use super::app::GAME_DATA_DIR;
use super::links::{points_inside, Links};
use super::models::{Game, Update, NEWER_APP};
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
    /// Tells this very file from another build under the same name.
    pub build: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Release {
    pub tag: String,
    pub name: String,
    /// `YYYY-MM-DD` of its latest build, empty when unknown.
    pub date: String,
    pub notes: String,
    pub assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct GithubAsset {
    name: String,
    size: u64,
    browser_download_url: String,
    /// `sha256:…`, for files uploaded since 2025.
    digest: Option<String>,
    updated_at: Option<String>,
}

#[derive(Deserialize)]
struct GithubRelease {
    tag_name: String,
    name: Option<String>,
    published_at: Option<String>,
    /// Moves when CI recreates a rolling release; `published_at` stays.
    created_at: Option<String>,
    body: Option<String>,
    #[serde(default)]
    assets: Vec<GithubAsset>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Archive {
    TarGz,
    Zip,
}

/// A Linux program in one file.
fn is_appimage(name: &str) -> bool {
    name.to_lowercase().ends_with(".appimage")
}

pub(super) fn archive_of(name: &str) -> Option<Archive> {
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
        // ISO dates sort as text.
        date: release
            .published_at
            .max(release.created_at)
            .and_then(|d| d.get(..10).map(str::to_string))
            .unwrap_or_default(),
        notes: release.body.unwrap_or_default(),
        assets: release
            .assets
            .into_iter()
            .map(|a| Asset {
                build: a.digest.or(a.updated_at).unwrap_or_default(),
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
/// The 32-bit rows are never a game's arch: they only reject those builds.
/// No bare `x86`: `x86_64` splits into the tokens `x86` and `64`.
const ARCH_WORDS: [(&str, &[&str]); 4] = [
    ("x64", &["x86_64", "x64", "amd64"]),
    ("arm64", &["arm64", "aarch64"]),
    ("x86", &["i386", "i586", "i686"]),
    ("arm32", &["armhf", "armv7", "armel"]),
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

/// What a game runs as on this machine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Platform {
    pub os: String,
    pub arch: String,
    /// The game's executable, relative to its folder (empty when unknown).
    pub target: String,
}

/// The file is a build for `platform`: it names its OS, or is an AppImage
/// for Linux (taken as x64 unless it names an arch); never another arch.
fn is_for(name: &str, platform: &Platform) -> bool {
    let name = name.to_lowercase();
    if mentions_other_arch(&name, &platform.arch) {
        return false;
    }
    let names_arch = mentions(&name, words_of(&ARCH_WORDS, &platform.arch));
    let linux_appimage =
        platform.os == "linux" && is_appimage(&name) && (names_arch || platform.arch == "x64");
    mentions(&name, words_of(&OS_WORDS, &platform.os)) || linux_appimage
}

/// The release file to install: the first whose name contains `pattern`
/// when given, else a build for `platform`, preferring the same kind as the
/// game's executable (an AppImage, or else an archive), then one that names
/// its arch.
pub fn pick_asset<'a>(
    release: &'a Release,
    platform: &Platform,
    pattern: &str,
) -> Option<&'a Asset> {
    let pattern = pattern.trim().to_lowercase();
    let usable = release.assets.iter().filter(|a| {
        let name = a.name.to_lowercase();
        let installable = archive_of(&name).is_some() || is_appimage(&name);
        installable && !SKIPPED.iter().any(|s| name.contains(s))
    });
    if !pattern.is_empty() {
        return usable
            .into_iter()
            .find(|a| a.name.to_lowercase().contains(&pattern));
    }
    let wants_appimage = is_appimage(&platform.target);
    let arch_words = words_of(&ARCH_WORDS, &platform.arch);
    usable
        .filter(|a| is_for(&a.name, platform))
        .enumerate()
        .min_by_key(|(i, a)| {
            let other_kind = is_appimage(&a.name) != wants_appimage;
            let no_arch = !mentions(&a.name.to_lowercase(), arch_words);
            (other_kind, no_arch, *i)
        })
        .map(|(_, a)| a)
}

/// What the game runs as here: its target for this machine's OS, else (a
/// Windows game under Proton) its first target.
pub fn platform_of(game: &Game) -> Platform {
    let os = platform::os_tag();
    let target = game
        .targets
        .iter()
        .find(|t| t.os == os)
        .or_else(|| game.targets.first());
    // `any` (like empty) means it runs as whatever this machine is.
    let pick = |value: Option<&str>, fallback: &str| {
        value
            .filter(|v| !v.is_empty() && !v.eq_ignore_ascii_case("any"))
            .unwrap_or(fallback)
            .to_string()
    };
    Platform {
        os: pick(target.map(|t| t.os.as_str()), os),
        arch: pick(target.map(|t| t.arch.as_str()), platform::arch_tag()),
        target: pick(target.map(|t| t.target.as_str()), ""),
    }
}

/// The game already runs `asset` of `release`: same tag, and the same file
/// when the installed one was fingerprinted.
pub fn is_current(update: &Update, release: &Release, asset: Option<&Asset>) -> bool {
    let same_file = update.build.is_empty() || asset.is_some_and(|a| a.build == update.build);
    update.installed == release.tag && same_file
}

/// Record `asset` of release `tag` as what the game runs.
fn record(update: &mut Update, tag: &str, asset: Option<&Asset>) {
    update.installed = tag.to_string();
    update.build = asset.map(|a| a.build.clone()).unwrap_or_default();
}

// ── Installing ───────────────────────────────────────────────────────────

/// Lowercase hex of `bytes`.
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The download matches the sha256 GitHub computed at upload (`build` is
/// `sha256:…`); files without one (uploaded before 2025) are taken as is.
fn check_digest(asset: &Asset, sha256: &str) -> Result<(), String> {
    match asset.build.strip_prefix("sha256:") {
        Some(expected) if !expected.eq_ignore_ascii_case(sha256) => Err(format!(
            "{}: the download is damaged (checksum differs), try again",
            asset.name
        )),
        _ => Ok(()),
    }
}

/// Stream `asset` into `dest`, counting bytes in `progress`, and check it
/// against its published checksum.
pub(super) fn download(asset: &Asset, dest: &Path, progress: &Progress) -> Result<(), String> {
    progress.start(asset.size);
    let response = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        .timeout_read(DOWNLOAD_TIMEOUT)
        .build()
        .get(&asset.url)
        .set("User-Agent", &sgdb::user_agent())
        .call()
        .map_err(http_error)?;
    // One byte past the size is enough to tell a wrong answer: never more.
    let mut reader = response.into_reader().take(asset.size + 1);
    let mut file =
        BufWriter::new(File::create(dest).map_err(|e| format!("{}: {e}", dest.display()))?);
    let mut buf = vec![0u8; 1 << 16];
    let mut written = 0u64;
    let mut hasher = Sha256::new();
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
        hasher.update(&buf[..n]);
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
    check_digest(asset, &hex(&hasher.finalize()))
}

/// Unpack `archive` into the empty folder `dest`. Both readers refuse
/// entries that would land outside it.
fn extract(kind: Archive, archive: &Path, dest: &Path) -> Result<(), String> {
    let file = BufReader::new(File::open(archive).map_err(|e| e.to_string())?);
    match kind {
        Archive::TarGz => extract_tar_gz(file, dest).map_err(|e| e.to_string()),
        Archive::Zip => extract_zip(file, dest).map_err(|e| e.to_string()),
    }
}

/// Links go through `Links`: copies where the filesystem has none.
fn extract_tar_gz(file: impl Read, dest: &Path) -> io::Result<()> {
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(file));
    tar.set_preserve_permissions(true);
    let mut links = Links::default();
    for entry in tar.entries()? {
        let mut entry = entry?;
        if !entry.header().entry_type().is_symlink() {
            entry.unpack_in(dest)?;
            continue;
        }
        let rel: PathBuf = entry
            .path()?
            .components()
            .filter(|c| !matches!(c, Component::CurDir))
            .collect();
        let target = entry.link_name()?.unwrap_or_default().into_owned();
        let link = package::safe_join(dest, &rel).map_err(io::Error::other)?;
        links.add(&target, &link)?;
    }
    links.copy_refused(dest)
}

/// The zip reader makes the files; its links, those that stay inside
/// `dest`, go through `Links`.
fn extract_zip(file: impl Read + io::Seek, dest: &Path) -> zip::result::ZipResult<()> {
    let mut zip = zip::ZipArchive::new(file)?;
    let options = ExtractOptions::default().symlink_action(SymlinkExtractAction::NoExtract);
    zip.extract_with_options(dest, options)?;
    let mut links = Links::default();
    for (rel, target) in zip_links(&mut zip)? {
        let link = dest.join(rel);
        if points_inside(dest, &link, &target) {
            links.add(&target, &link)?;
        }
    }
    Ok(links.copy_refused(dest)?)
}

/// `(path, target)` of each link in `zip`.
fn zip_links(
    zip: &mut zip::ZipArchive<impl Read + io::Seek>,
) -> zip::result::ZipResult<Vec<(PathBuf, PathBuf)>> {
    let mut links = Vec::new();
    // A loop: each entry borrows the archive.
    for index in 0..zip.len() {
        let mut entry = zip.by_index(index)?;
        let Some(rel) = entry.enclosed_name().filter(|_| entry.is_symlink()) else {
            continue;
        };
        let mut target = String::new();
        entry.read_to_string(&mut target)?;
        links.push((rel, PathBuf::from(target)));
    }
    Ok(links)
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

/// The file `extracted` holds when it holds nothing else.
fn single_file(extracted: &Path) -> io::Result<Option<PathBuf>> {
    let entries = content_entries(extracted)?;
    let [single] = entries.as_slice() else {
        return Ok(None);
    };
    Ok(single.file_type()?.is_file().then(|| single.path()))
}

/// An empty `dir`, whatever a crash left there.
fn fresh_dir(dir: &Path) -> Result<(), String> {
    let _ = fs::remove_dir_all(dir);
    fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))
}

/// Unpack `archive` into `outer`, and the archive it wraps, if any, into
/// `inner` (zips that wrap a `.tar.gz` so the exec bits survive). Returns
/// the folder holding the files. Each archive is deleted once unpacked: an
/// update needs its size on disk once, not three times.
pub(super) fn unpack(
    kind: Archive,
    archive: &Path,
    outer: &Path,
    inner: &Path,
) -> Result<PathBuf, String> {
    fresh_dir(outer)?;
    extract(kind, archive, outer)?;
    let _ = fs::remove_file(archive);
    let wrapped = single_file(outer)
        .map_err(|e| e.to_string())?
        .and_then(|file| Some((archive_of(&file.file_name()?.to_string_lossy())?, file)));
    let Some((inner_kind, file)) = wrapped else {
        return Ok(outer.to_path_buf());
    };
    fresh_dir(inner)?;
    extract(inner_kind, &file, inner)?;
    let _ = fs::remove_file(&file);
    Ok(inner.to_path_buf())
}

/// The lone AppImage `root` holds, when the game's `target` is one too.
/// Another target, like a launcher script, may run the AppImage by its
/// name: then it is laid over the folder as it is.
fn lone_appimage(root: &Path, target: &str) -> io::Result<Option<PathBuf>> {
    if !is_appimage(target) {
        return Ok(None);
    }
    let file = single_file(root)?;
    Ok(file.filter(|f| is_appimage(&f.to_string_lossy())))
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
fn place(
    item: &package::Item,
    src_root: &Path,
    game_dir: &Path,
    links: &mut Links,
) -> io::Result<()> {
    let (src, dest) = (src_root.join(&item.rel), game_dir.join(&item.rel));
    match &item.kind {
        Kind::Dir => fs::create_dir_all(&dest),
        Kind::File(_) => replace_file(&src, &dest),
        Kind::Symlink(target) => {
            if fs::symlink_metadata(&dest).is_ok() {
                fs::remove_file(&dest)?;
            }
            links.add(target, &dest)
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
    let mut links = Links::default();
    let files = items
        .iter()
        .map(|item| {
            place(item, src_root, game_dir, &mut links)
                .map(|()| usize::from(item.kind != Kind::Dir))
                .map_err(|e| format!("{}: {e}", item.rel.display()))
        })
        .sum::<Result<usize, String>>()?;
    links.copy_refused(game_dir).map_err(|e| e.to_string())?;
    Ok(files)
}

/// Put the downloaded program `file` in place of the game's executable
/// `target` (relative to `game_dir`), runnable.
fn replace_program(file: &Path, game_dir: &Path, target: &str) -> Result<(), String> {
    let rel: PathBuf = Path::new(target)
        .components()
        .filter(|c| !matches!(c, std::path::Component::CurDir))
        .collect();
    if rel.as_os_str().is_empty() {
        return Err("The game has no executable for this file to replace".into());
    }
    let dest = package::safe_join(game_dir, &rel)?;
    if dest.is_dir() {
        return Err(format!("{target}: a folder, not a program"));
    }
    platform::make_executable(file)
        .and_then(|()| replace_file(file, &dest))
        .map_err(|e| format!("{target}: {e}"))
}

/// Unpack `archive` and lay its files over the game, or put the AppImage it
/// wraps in place of the executable `target` when that is an AppImage too
/// (another target, like a launcher script, may run the AppImage by its
/// name: it is laid over the folder as it is). Returns how many files were
/// written.
fn lay_archive(
    kind: Archive,
    archive: &Path,
    (game_dir, target): (&Path, &str),
    (extracted, inner): (&Path, &Path),
    progress: &Progress,
) -> Result<usize, String> {
    let unpacked = unpack(kind, archive, extracted, inner)?;
    if progress.is_cancelled() {
        return Err("cancelled".into());
    }
    let root = content_root(&unpacked, game_dir).map_err(|e| e.to_string())?;
    match lone_appimage(&root, target).map_err(|e| e.to_string())? {
        Some(file) => replace_program(&file, game_dir, target).map(|()| 1),
        None => upsert(&root, game_dir),
    }
}

/// Rewrite the game's game.json as on disk now, changed by `change`.
fn rewrite_game_json(game: &Game, change: impl FnOnce(&mut Game)) -> Result<(), String> {
    let path = game.game_json_path();
    let text = fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut fresh = Game::from_json(&game.dir, &text).map_err(|e| e.to_string())?;
    if fresh.is_from_newer_app() {
        return Err(NEWER_APP.into());
    }
    change(&mut fresh);
    paths::write_atomic(&path, fresh.to_json()).map_err(|e| format!("{}: {e}", path.display()))
}

/// Remember `asset` of release `tag` as installed, without installing
/// anything (the game already is at that release).
pub fn mark_installed(game: &Game, tag: &str, asset: Option<&Asset>) -> Result<(), String> {
    rewrite_game_json(game, |g| record(&mut g.update, tag, asset))
}

/// Download `asset` of release `tag` and lay its files over the game (or,
/// when it is a program in one file, put it in place of the executable).
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
    if game.is_from_newer_app() {
        return Err(NEWER_APP.into());
    }
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
    let target = platform_of(game).target;
    let result = (|| {
        download(asset, &archive, progress)?;
        let files = match archive_of(&asset.name) {
            Some(kind) => lay_archive(
                kind,
                &archive,
                (&game.dir, &target),
                (&extracted, &inner),
                progress,
            )?,
            None => replace_program(&archive, &game.dir, &target).map(|()| 1)?,
        };
        log::info!("{}: {files} files from {} ({tag})", game.title, asset.name);
        // The size changed: the run after the update measures it again.
        rewrite_game_json(game, |g| {
            record(&mut g.update, tag, Some(asset));
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
            build: format!("sha256:{name}"),
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

    fn platform(os: &str, arch: &str, target: &str) -> Platform {
        Platform {
            os: os.into(),
            arch: arch.into(),
            target: target.into(),
        }
    }

    fn picked(release: &Release, os: &str, arch: &str, pattern: &str) -> Option<String> {
        pick_asset(release, &platform(os, arch, "game"), pattern).map(|a| a.name.clone())
    }

    fn picked_for(release: &Release, target: &str) -> Option<String> {
        pick_asset(release, &platform("linux", "x64", target), "").map(|a| a.name.clone())
    }

    fn published(build: &str) -> Asset {
        Asset {
            build: build.into(),
            ..asset("Game.tar.gz")
        }
    }

    #[test]
    fn sha256_is_lowercase_hex() {
        assert_eq!(
            hex(&Sha256::digest(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn a_download_matching_its_digest_is_accepted() {
        assert!(check_digest(&published("sha256:AB12"), "ab12").is_ok());
    }

    #[test]
    fn a_download_differing_from_its_digest_is_refused() {
        assert!(check_digest(&published("sha256:ab12"), "cd34").is_err());
    }

    #[test]
    fn a_file_without_digest_is_not_checked() {
        assert!(check_digest(&published("2024-01-01T00:00:00Z"), "cd34").is_ok());
        assert!(check_digest(&published(""), "cd34").is_ok());
    }

    #[test]
    fn an_appimage_is_a_linux_x64_build() {
        let soh = release(&[
            "SoH-Dewey-Bravo-Mac.zip",
            "SoH-Dewey-Bravo-Win64.zip",
            "SoH-Dewey-Bravo.appimage",
        ]);
        assert_eq!(
            picked_for(&soh, "soh.AppImage").unwrap(),
            "SoH-Dewey-Bravo.appimage"
        );
        assert_eq!(picked(&soh, "linux", "arm64", ""), None);
        assert_eq!(
            picked(&soh, "windows", "x64", "").unwrap(),
            "SoH-Dewey-Bravo-Win64.zip"
        );
    }

    #[test]
    fn the_executable_decides_between_an_appimage_and_an_archive() {
        let both = release(&["Game-x86_64.AppImage", "Game-linux-x64.tar.gz"]);
        assert_eq!(
            picked_for(&both, "Game.AppImage").unwrap(),
            "Game-x86_64.AppImage"
        );
        assert_eq!(
            picked_for(&both, "bin/game").unwrap(),
            "Game-linux-x64.tar.gz"
        );
    }

    #[test]
    fn a_program_file_replaces_the_executable_and_nothing_else() {
        let dir = TempDir::new();
        let game = installed_game(&dir);
        dir.write("games/Banjo/soh.AppImage", b"old");
        let file = dir.write("new.part", b"new appimage");
        replace_program(&file, &game.dir, "./soh.AppImage").unwrap();
        assert_eq!(
            fs::read(game.dir.join("soh.AppImage")).unwrap(),
            b"new appimage"
        );
        assert!(platform::has_exec_permission(
            &game.dir.join("soh.AppImage")
        ));
        assert_eq!(fs::read(game.dir.join("saves/bk.bin")).unwrap(), b"my save");
    }

    #[test]
    fn a_program_file_needs_an_executable_to_replace() {
        let dir = TempDir::new();
        let game = installed_game(&dir);
        let file = dir.write("new.part", b"x");
        assert!(replace_program(&file, &game.dir, "").is_err());
        assert!(replace_program(&file, &game.dir, "saves").is_err());
        assert!(replace_program(&file, &game.dir, "../outside").is_err());
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
    fn a_file_is_fingerprinted_by_its_digest_else_its_upload_date() {
        let release = banjo();
        assert!(release.assets[0].build.starts_with("sha256:53515be8"));
        let json = r#"{"tag_name": "nightly", "published_at": "2024-05-14T09:41:23Z",
            "created_at": "2026-08-13T05:51:05Z",
            "assets": [{"name": "a.tar.gz", "size": 1, "browser_download_url": "u",
                "updated_at": "2026-08-13T05:58:24Z"}]}"#;
        let rolling = parse_release(json).unwrap();
        assert_eq!(rolling.assets[0].build, "2026-08-13T05:58:24Z");
        assert_eq!(rolling.date, "2026-08-13");
    }

    #[test]
    fn a_rolling_release_is_current_only_with_the_same_file() {
        let release = release(&["Game-linux-x64.tar.gz"]);
        let file = &release.assets[0];
        let mut update = Update {
            installed: "v1".into(),
            ..Default::default()
        };
        assert!(
            is_current(&update, &release, Some(file)),
            "tag only: as before"
        );
        record(&mut update, "v1", Some(file));
        assert!(is_current(&update, &release, Some(file)));
        let rebuilt = Asset {
            build: "sha256:other".into(),
            ..file.clone()
        };
        assert!(!is_current(&update, &release, Some(&rebuilt)));
        assert!(!is_current(&update, &release, None));
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
        let perfect_dark = release(&[
            "io.github.perfect_dark_pc_port.perfect_dark.flatpak",
            "pd-arm64-osx.tar.xz",
            "pd-i686-linux.tar.gz",
            "pd-i686-windows.zip",
            "pd-x86_64-linux.tar.gz",
        ]);
        assert_eq!(
            picked(&perfect_dark, "linux", "x64", "").unwrap(),
            "pd-x86_64-linux.tar.gz"
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
        // An OS other than this machine's, whichever it is.
        let foreign = if platform::os_tag() == "windows" {
            "linux"
        } else {
            "windows"
        };
        let mut game = Game::skeleton(Path::new("/g/G"));
        game.targets = vec![target(foreign, "x64")];
        let os_arch = |p: Platform| (p.os, p.arch);
        assert_eq!(os_arch(platform_of(&game)), (foreign.into(), "x64".into()));
        game.targets = vec![target(foreign, "any")];
        assert_eq!(platform_of(&game).arch, platform::arch_tag());
        game.targets.push(target(platform::os_tag(), "arm64"));
        assert_eq!(
            os_arch(platform_of(&game)),
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

    fn tar_gz_with_link(link: &str, target: &str) -> Vec<u8> {
        let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
            Vec::new(),
            flate2::Compression::fast(),
        ));
        let mut header = tar::Header::new_gnu();
        header.set_size(3);
        header.set_mode(0o755);
        header.set_cksum();
        builder
            .append_data(&mut header, "./game.bin", &b"bin"[..])
            .unwrap();
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_size(0);
        builder.append_link(&mut header, link, target).unwrap();
        builder.into_inner().unwrap().finish().unwrap()
    }

    #[cfg(unix)]
    #[test]
    fn extracts_the_links_of_a_tar_gz() {
        let dir = TempDir::new();
        let archive = dir.write("a.tar.gz", &tar_gz_with_link("./run", "game.bin"));
        let dest = dir.mkdir("out");
        extract(Archive::TarGz, &archive, &dest).unwrap();
        assert_eq!(
            fs::read_link(dest.join("run")).unwrap(),
            Path::new("game.bin")
        );
    }

    fn zip_with_links(links: &[(&str, &str)]) -> Vec<u8> {
        let mut zip = zip::ZipWriter::new(io::Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default();
        zip.start_file("game.bin", options).unwrap();
        zip.write_all(b"bin").unwrap();
        for (link, target) in links {
            zip.add_symlink(*link, *target, options).unwrap();
        }
        zip.finish().unwrap().into_inner()
    }

    #[cfg(unix)]
    #[test]
    fn extracts_the_links_of_a_zip_that_stay_inside() {
        let dir = TempDir::new();
        let links = [("bin/run", "../game.bin"), ("key", "/etc/passwd")];
        let archive = dir.write("a.zip", &zip_with_links(&links));
        let dest = dir.mkdir("out");
        extract(Archive::Zip, &archive, &dest).unwrap();
        assert_eq!(fs::read(dest.join("bin/run")).unwrap(), b"bin");
        assert!(paths::is_symlink(&dest.join("bin/run")));
        assert!(fs::symlink_metadata(dest.join("key")).is_err());
    }

    #[test]
    fn a_zip_wrapping_a_tar_gz_unpacks_both_and_keeps_exec_bits() {
        let dir = TempDir::new();
        let wrapped = tar_gz(&[("Game", b"bin"), ("assets/a", b"a")]);
        let archive = dir.write("a.zip", &zip_of("Game.tar.gz", &wrapped));
        let (outer, inner) = (dir.path().join("outer"), dir.path().join("inner"));
        let unpacked = unpack(Archive::Zip, &archive, &outer, &inner).unwrap();
        assert_eq!(unpacked, inner);
        assert_eq!(fs::read(inner.join("assets/a")).unwrap(), b"a");
        assert!(platform::has_exec_permission(&inner.join("Game")));
    }

    #[test]
    fn unpacked_archives_are_deleted_at_once() {
        let dir = TempDir::new();
        let archive = dir.write(
            "a.zip",
            &zip_of("Game.tar.gz", &tar_gz(&[("Game", b"bin")])),
        );
        let (outer, inner) = (dir.path().join("outer"), dir.path().join("inner"));
        unpack(Archive::Zip, &archive, &outer, &inner).unwrap();
        assert!(!archive.exists());
        assert!(!outer.join("Game.tar.gz").exists());
    }

    fn zip_of(name: &str, data: &[u8]) -> Vec<u8> {
        let mut zip = zip::ZipWriter::new(io::Cursor::new(Vec::new()));
        zip.start_file(name, zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(data).unwrap();
        zip.finish().unwrap().into_inner()
    }

    #[test]
    fn a_zipped_appimage_replaces_the_executable_even_in_a_folder() {
        for zipped in ["SoH-v2.AppImage", "SoH-v2/SoH-v2.AppImage"] {
            let dir = TempDir::new();
            let game = installed_game(&dir);
            dir.write("games/Banjo/soh.AppImage", b"old");
            let archive = dir.write("a.zip", &zip_of(zipped, b"new appimage"));
            let (outer, inner) = (dir.path().join("outer"), dir.path().join("inner"));
            let written = lay_archive(
                Archive::Zip,
                &archive,
                (&game.dir, "soh.AppImage"),
                (&outer, &inner),
                &Progress::default(),
            )
            .unwrap();
            assert_eq!(written, 1, "{zipped}");
            assert_eq!(
                fs::read(game.dir.join("soh.AppImage")).unwrap(),
                b"new appimage",
                "{zipped}"
            );
            assert!(!game.dir.join("SoH-v2.AppImage").exists(), "{zipped}");
        }
    }

    #[test]
    fn a_zipped_appimage_is_laid_as_a_file_when_the_target_is_no_appimage() {
        let dir = TempDir::new();
        let game = installed_game(&dir);
        dir.write("games/Banjo/run.sh", b"./Game.AppImage");
        let archive = dir.write("a.zip", &zip_of("Game.AppImage", b"new appimage"));
        let (outer, inner) = (dir.path().join("outer"), dir.path().join("inner"));
        lay_archive(
            Archive::Zip,
            &archive,
            (&game.dir, "run.sh"),
            (&outer, &inner),
            &Progress::default(),
        )
        .unwrap();
        assert_eq!(
            fs::read(game.dir.join("run.sh")).unwrap(),
            b"./Game.AppImage"
        );
        assert_eq!(
            fs::read(game.dir.join("Game.AppImage")).unwrap(),
            b"new appimage"
        );
    }

    #[test]
    fn a_32_bit_build_is_not_an_x64_build() {
        let release = release(&["pd-i686-linux.tar.gz", "pd-armhf-linux.tar.gz"]);
        assert_eq!(picked(&release, "linux", "x64", ""), None);
        assert_eq!(picked(&release, "linux", "arm64", ""), None);
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
        mark_installed(&game, "v1.0.2", Some(&asset("Banjo.tar.gz"))).unwrap();
        let text = fs::read_to_string(game.game_json_path()).unwrap();
        let back = Game::from_json(&game.dir, &text).unwrap();
        assert_eq!(back.update.installed, "v1.0.2");
        assert_eq!(back.update.build, "sha256:Banjo.tar.gz");
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
