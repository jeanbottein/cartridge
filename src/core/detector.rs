//! Pipeline step 2: find executables for games that have no targets yet,
//! restore execute bits that copies lost, let Linux libraries load, plus
//! Proton save-folder detection for the game editor.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use log::{info, warn};

use super::models::{Game, SavePath, Target};
use super::paths;
use super::platform::{
    clear_exec_stack, has_exec_permission, has_exec_stack, inspect, is_32_bit, make_executable,
    needs_exec_bit, Program,
};
use super::scanner;
use super::{steam, unique};

const SKIP_DIRS: [&str; 4] = ["java", "jre", "lib", "__pycache__"];
const MAX_DEPTH: usize = 3;
/// Native programs and scripts found even without their execute bit (copies
/// over FAT drives, zips or web downloads drop it). No extension counts too;
/// libraries (`.so`, `.so.6`, `.dylib`) never match.
const PROGRAM_EXTENSIONS: [&str; 7] = ["appimage", "x86_64", "x86", "x64", "bin", "run", "sh"];
const MACOS_WORDS: [&str; 5] = ["mac", "macos", "macosx", "osx", "darwin"];
const ARM64_WORDS: [&str; 2] = ["arm64", "aarch64"];
/// 32-bit x86 counts as x64, as for `platform::arch_tag`.
const X64_WORDS: [&str; 5] = ["x64", "amd64", "x86", "i386", "i686"];

// ── Similarity (difflib.SequenceMatcher.ratio) ───────────────────────────

/// Longest common block in `a[alo..ahi]` × `b[blo..bhi]`, earliest on ties.
fn longest_match(
    a: &[char],
    b2j: &HashMap<char, Vec<usize>>,
    lo_hi: (usize, usize, usize, usize),
) -> (usize, usize, usize) {
    let (alo, ahi, blo, bhi) = lo_hi;
    let (mut best_i, mut best_j, mut best_k) = (alo, blo, 0);
    let mut lengths: HashMap<usize, usize> = HashMap::new();
    for (i, ch) in a.iter().enumerate().take(ahi).skip(alo) {
        let mut next = HashMap::new();
        for &j in b2j.get(ch).into_iter().flatten() {
            if j < blo {
                continue;
            }
            if j >= bhi {
                break;
            }
            let k = j.checked_sub(1).and_then(|p| lengths.get(&p)).unwrap_or(&0) + 1;
            next.insert(j, k);
            if k > best_k {
                (best_i, best_j, best_k) = (i + 1 - k, j + 1 - k, k);
            }
        }
        lengths = next;
    }
    (best_i, best_j, best_k)
}

fn matched_chars(
    a: &[char],
    b2j: &HashMap<char, Vec<usize>>,
    lo_hi: (usize, usize, usize, usize),
) -> usize {
    let (alo, ahi, blo, bhi) = lo_hi;
    let (i, j, k) = longest_match(a, b2j, lo_hi);
    if k == 0 {
        return 0;
    }
    let left = if alo < i && blo < j {
        matched_chars(a, b2j, (alo, i, blo, j))
    } else {
        0
    };
    let right = if i + k < ahi && j + k < bhi {
        matched_chars(a, b2j, (i + k, ahi, j + k, bhi))
    } else {
        0
    };
    k + left + right
}

/// Ratcliff/Obershelp similarity, equal to Python's `SequenceMatcher(None, a, b).ratio()`
/// for names under 200 chars (where difflib's autojunk heuristic never applies).
pub fn similarity(a: &str, b: &str) -> f64 {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    let mut b2j: HashMap<char, Vec<usize>> = HashMap::new();
    b.iter()
        .enumerate()
        .for_each(|(j, &c)| b2j.entry(c).or_default().push(j));
    let matches = matched_chars(&a, &b2j, (0, a.len(), 0, b.len()));
    2.0 * matches as f64 / (a.len() + b.len()) as f64
}

// ── Executable discovery ─────────────────────────────────────────────────

fn base_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// The candidate whose name best matches the folder name (first on ties).
fn find_best(game_dir: &Path, files: &[PathBuf]) -> Option<PathBuf> {
    let folder = base_name(game_dir);
    let score = |f: &PathBuf| {
        similarity(
            &folder,
            &f.file_stem().unwrap_or_default().to_string_lossy(),
        )
    };
    files
        .iter()
        .fold(None::<(&PathBuf, f64)>, |best, f| {
            let s = score(f);
            match best {
                Some((_, bs)) if bs >= s => best,
                _ => Some((f, s)),
            }
        })
        .map(|(f, _)| f.clone())
}

/// Files of `dir` and its subfolders up to `max_depth` levels (1 = `dir` only),
/// skipping hidden and runtime folders, never following folder symlinks. A
/// macOS `.app` bundle counts as a file: it is the program.
fn walk_files(dir: &Path, max_depth: usize) -> Vec<PathBuf> {
    if max_depth == 0 {
        return Vec::new();
    }
    let (dirs, files): (Vec<PathBuf>, Vec<PathBuf>) = paths::sorted_entries(dir)
        .into_iter()
        .map(|n| dir.join(n))
        .partition(|p| p.is_dir() && !is_app_bundle(p));
    let nested = dirs
        .into_iter()
        .filter(|d| {
            let name = base_name(d);
            !name.starts_with('.') && !SKIP_DIRS.contains(&name.as_str()) && !paths::is_symlink(d)
        })
        .flat_map(|d| walk_files(&d, max_depth - 1));
    files.into_iter().chain(nested).collect()
}

fn is_app_bundle(path: &Path) -> bool {
    path.extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("app"))
        && path.is_dir()
}

/// A bundle's program: in `Contents/MacOS/`, the one named like the bundle,
/// else the first there.
fn bundle_binary(bundle: &Path) -> Option<PathBuf> {
    let macos = bundle.join("Contents").join("MacOS");
    let named = macos.join(bundle.file_stem()?);
    if named.is_file() {
        return Some(named);
    }
    paths::sorted_entries(&macos)
        .into_iter()
        .map(|n| macos.join(n))
        .find(|p| p.is_file())
}

fn bundle_program(bundle: &Path) -> Program {
    let arch = match bundle_binary(bundle).and_then(|b| inspect(&b)) {
        Some(Program::Binary { arch, .. }) => arch,
        _ => None,
    };
    Program::Binary { os: "macos", arch }
}

fn is_exe_name(path: &Path) -> bool {
    base_name(path).to_lowercase().ends_with(".exe")
}

fn has_program_name(path: &Path) -> bool {
    path.extension().is_none_or(|ext| {
        PROGRAM_EXTENSIONS.contains(&ext.to_string_lossy().to_lowercase().as_str())
    })
}

/// The program `path` is, when worth offering as a target: a `.app` bundle,
/// a Windows `.exe` (never a `.dll`), or a native program or `#!` script
/// (with its execute bit, or named like one). Names first: only those files
/// are opened.
fn program_of(path: &Path) -> Option<Program> {
    if is_app_bundle(path) {
        return Some(bundle_program(path));
    }
    let named_like_one = is_exe_name(path) || has_exec_permission(path) || has_program_name(path);
    match named_like_one.then(|| inspect(path)).flatten()? {
        Program::Binary { os: "windows", .. } if !is_exe_name(path) => None,
        program => Some(program),
    }
}

/// Descend through folders that only contain a single subfolder (never into
/// a `.app` bundle: that is the game's program).
fn real_game_dir(game_dir: &Path) -> PathBuf {
    let mut dir = paths::normpath(game_dir);
    for _ in 0..=10 {
        let entries: Vec<PathBuf> = paths::sorted_entries(&dir)
            .into_iter()
            .filter(|n| !n.starts_with('.'))
            .map(|n| dir.join(n))
            .collect();
        let only_one_dir = entries.len() == 1 && entries[0].is_dir() && !is_app_bundle(&entries[0]);
        if !only_one_dir {
            break;
        }
        dir = entries[0].clone();
    }
    dir
}

/// Display title for a freshly detected game: the innermost meaningful folder.
pub fn title_for(game_dir: &Path) -> String {
    base_name(&real_game_dir(game_dir))
}

fn words(text: &str) -> impl Iterator<Item = &str> {
    text.split(|c: char| !c.is_ascii_alphanumeric())
}

/// OS named in a path (`game-linux64/run`, `game_osx/start.sh`), by whole
/// words: "mac" inside "machinarium" says nothing.
fn os_hint(text: &str) -> Option<&'static str> {
    if words(text).any(|w| w.starts_with("linux")) {
        Some("linux")
    } else if words(text).any(|w| MACOS_WORDS.contains(&w)) {
        Some("macos")
    } else {
        None
    }
}

/// Arch named in a path (`game-amd64`, `Game.x86_64`), by whole words: the
/// "x64" inside "box64" says nothing.
fn arch_hint(text: &str) -> Option<&'static str> {
    // One word for the split, which would cut it at its separator.
    let text = text.replace("x86_64", "x64").replace("x86-64", "x64");
    if words(&text).any(|w| ARM64_WORDS.contains(&w)) {
        Some("arm64")
    } else if words(&text).any(|w| X64_WORDS.contains(&w)) {
        Some("x64")
    } else {
        None
    }
}

/// `(os, arch)` of a target; an empty arch means any.
type Platform = (String, String);

/// Where a script runs: what its path names (`game-linux-amd64/start.sh`),
/// else where the programs beside it run, else Linux on any arch: the scripts
/// games ship are Linux launchers, and the answer must not depend on the
/// machine that scans.
fn script_platform(script: &Path, game_dir: &Path, beside: Option<&Platform>) -> Platform {
    let path = format!(
        "{}/{}",
        base_name(game_dir),
        paths::lossy(&paths::relpath(script, game_dir))
    )
    .to_lowercase();
    let (beside_os, beside_arch) =
        beside.map_or(("linux", ""), |(os, arch)| (os.as_str(), arch.as_str()));
    let os = os_hint(&path).unwrap_or(beside_os);
    let arch = arch_hint(&path).unwrap_or(beside_arch);
    (os.to_string(), arch.to_string())
}

/// The one platform the native programs of a game run on, if they agree.
fn native_platform(binaries: &[(Platform, PathBuf)]) -> Option<Platform> {
    let natives: Vec<&Platform> = unique(
        binaries
            .iter()
            .filter(|((os, _), _)| os != "windows")
            .map(|(platform, _)| platform),
    );
    (natives.len() == 1).then(|| natives[0].clone())
}

fn make_target(os: &str, arch: &str, exe: &Path, game_dir: &Path) -> Target {
    Target {
        os: os.to_string(),
        arch: arch.to_string(),
        target: paths::lossy(&paths::relpath(exe, game_dir)),
        start_in: paths::lossy(&paths::relpath(exe.parent().unwrap_or(game_dir), game_dir)),
        launch_options: String::new(),
    }
}

/// A launcher script: one that starts a native program of the game by name
/// (`./bin64/Chowdren`), so it already sets the folder and libraries the
/// program needs. Other scripts (installers, setup for the Windows build)
/// are targets only in a game without any program (a Java game's start
/// script).
fn launches_one_of(script: &Path, program_names: &[String]) -> bool {
    std::fs::read_to_string(script).is_ok_and(|text| {
        program_names
            .iter()
            .any(|name| text.contains(name.as_str()))
    })
}

/// The binaries minus the 32-bit ones that share a platform with a 64-bit
/// one (both are tagged x64), in the order found.
fn without_32_bit_twins(binaries: Vec<(Platform, PathBuf)>) -> Vec<(Platform, PathBuf)> {
    let tagged: Vec<(Platform, PathBuf, bool)> = binaries
        .into_iter()
        .map(|(platform, path)| {
            let narrow = is_32_bit(&path);
            (platform, path, narrow)
        })
        .collect();
    let has_64_bit =
        |platform: &Platform| tagged.iter().any(|(p, _, narrow)| p == platform && !narrow);
    tagged
        .iter()
        .filter(|(platform, _, narrow)| !narrow || !has_64_bit(platform))
        .map(|(platform, path, _)| (platform.clone(), path.clone()))
        .collect()
}

/// Every target detected in `game_dir`, one per (os, arch), the best-named of
/// each: the same on every machine, which picks its own at launch. Arch comes
/// from headers, not names: a "64" in a helper such as
/// `UnityCrashHandler64.exe` says nothing about the game.
pub fn collect_targets(game_dir: &Path) -> Vec<Target> {
    let real_dir = real_game_dir(game_dir);
    let found: Vec<(PathBuf, Program)> = walk_files(&real_dir, MAX_DEPTH)
        .into_iter()
        .filter_map(|p| program_of(&p).map(|program| (p, program)))
        .collect();
    let binaries: Vec<(Platform, PathBuf)> = found
        .iter()
        .filter_map(|(path, program)| match program {
            Program::Binary { os, arch } => Some((
                (os.to_string(), arch.unwrap_or_default().to_string()),
                path.clone(),
            )),
            Program::Script => None,
        })
        .collect();
    let native_names: Vec<String> = binaries
        .iter()
        .filter(|((os, _), _)| os != "windows")
        .map(|(_, path)| base_name(path))
        .collect();
    let no_program = binaries.is_empty();
    let binaries = without_32_bit_twins(binaries);
    let beside = native_platform(&binaries);
    let scripts: Vec<(Platform, PathBuf)> = found
        .iter()
        .filter(|(_, program)| *program == Program::Script)
        .map(|(path, _)| {
            (
                script_platform(path, game_dir, beside.as_ref()),
                path.clone(),
            )
        })
        .filter(|(_, path)| no_program || launches_one_of(path, &native_names))
        .collect();

    // A loop: groups keep the order programs were found in.
    let mut groups: Vec<(Platform, Vec<PathBuf>)> = Vec::new();
    for (key, exe) in binaries.into_iter().chain(scripts.iter().cloned()) {
        match groups.iter_mut().find(|(k, _)| *k == key) {
            Some((_, list)) => list.push(exe),
            None => groups.push((key, vec![exe])),
        }
    }
    groups
        .iter()
        .filter_map(|((os, arch), exes)| {
            let launcher = exes
                .iter()
                .find(|exe| scripts.iter().any(|(_, script)| script == *exe))
                .cloned();
            launcher
                .or_else(|| find_best(&real_dir, exes))
                .map(|best| make_target(os, arch, &best, game_dir))
        })
        .collect()
}

/// Fill targets + title for every game that has no targets yet, give native
/// targets back the execute bit a copy lost, and let Linux games' libraries
/// load on a recent glibc.
pub fn detect(games: &mut [Game]) {
    detect_missing_targets(games);
    remember_appids(games);
    let installed = || games.iter().filter(|g| g.is_installed());
    installed().for_each(restore_exec_bits);
    installed()
        .filter(|g| g.targets.iter().any(|t| t.os == "linux"))
        .for_each(clear_exec_stacks);
}

/// Library names: `libfoo.so`, `libfoo.so.10`.
fn is_library_name(path: &Path) -> bool {
    let name = base_name(path);
    name.ends_with(".so") || name.contains(".so.")
}

/// Libraries of `dir` at any depth (`lib64/`, `Game_Data/Plugins/x86_64/`),
/// never following symlinks or entering hidden folders.
fn library_files(dir: &Path) -> Vec<PathBuf> {
    let (dirs, files): (Vec<PathBuf>, Vec<PathBuf>) = paths::sorted_entries(dir)
        .into_iter()
        .filter(|name| !name.starts_with('.'))
        .map(|name| dir.join(name))
        .filter(|p| !paths::is_symlink(p))
        .partition(|p| p.is_dir());
    files
        .into_iter()
        .filter(|f| is_library_name(f))
        .chain(dirs.iter().flat_map(|d| library_files(d)))
        .collect()
}

/// glibc 2.41+ (SteamOS, Bazzite) refuses to load a library asking for an
/// executable stack, so FMOD audio in Celeste or Night in the Woods fails.
fn clear_exec_stacks(game: &Game) {
    for library in library_files(&game.dir)
        .iter()
        .filter(|l| has_exec_stack(l))
    {
        match clear_exec_stack(library) {
            Ok(()) => info!("  Cleared executable stack: {}", library.display()),
            Err(e) => warn!(
                "  Cannot clear executable stack of {}: {e}",
                library.display()
            ),
        }
    }
}

/// Store the AppID each game has now (from its current title), so a later
/// rename keeps its Steam shortcut, Proton prefix and saves.
fn remember_appids(games: &mut [Game]) {
    for game in games
        .iter_mut()
        .filter(|g| g.is_writable() && g.steam_appid.is_none())
    {
        if let Some(appid) = steam::appid_of(game) {
            game.steam_appid = Some(appid);
            game.needs_persist = true;
        }
    }
}

fn restore_exec_bits(game: &Game) {
    game.targets
        .iter()
        .for_each(|t| restore_exec_bit(&scanner::resolve_target(&game.dir, t).target));
}

/// Make a native program or script executable again if it lost the bit (logged).
pub fn restore_exec_bit(exe: &Path) {
    if has_exec_permission(exe) || !needs_exec_bit(exe) {
        return;
    }
    match make_executable(exe) {
        Ok(()) => info!("  Made executable: {}", exe.display()),
        Err(e) => warn!("  Cannot make {} executable: {e}", exe.display()),
    }
}

fn detect_missing_targets(games: &mut [Game]) {
    let incomplete: Vec<&mut Game> = games
        .iter_mut()
        .filter(|g| g.is_writable() && g.targets.is_empty())
        .collect();
    if incomplete.is_empty() {
        return;
    }
    info!("Detecting executables for {} game(s)", incomplete.len());
    for game in incomplete {
        let targets = collect_targets(&game.dir);
        if targets.is_empty() {
            info!("  No executable found: {}", game.folder_name);
            continue;
        }
        // Only a placeholder title (no game.json yet) is replaced.
        if game.title == game.folder_name {
            game.title = title_for(&game.dir);
        }
        game.resolved =
            scanner::pick_target(&targets).map(|t| scanner::resolve_target(&game.dir, t));
        game.targets = targets;
        game.needs_persist = true;
        info!(
            "  Detected: {} ({} target(s))",
            game.title,
            game.targets.len()
        );
    }
}

// ── Proton save detection ────────────────────────────────────────────────

/// Prefix subfolders searched, with the Windows location they stand for.
const PROTON_SEARCH_DIRS: [(&str, &str); 5] = [
    (
        "users/steamuser/AppData/LocalLow",
        "%USERPROFILE%/AppData/LocalLow",
    ),
    ("users/steamuser/AppData/Local", "%LOCALAPPDATA%"),
    ("users/steamuser/AppData/Roaming", "%APPDATA%"),
    ("users/steamuser/Documents", "%USERPROFILE%/Documents"),
    ("users/steamuser/Saved Games", "%USERPROFILE%/Saved Games"),
];

const PROTON_IGNORE_DIRS: [&str; 18] = [
    "Microsoft",
    "Temp",
    "dxvk",
    "CrashDumps",
    ".cef",
    "CEF",
    "desktop.ini",
    "Public",
    "Windows",
    "Contacts",
    "Favorites",
    "Links",
    "Pictures",
    "Music",
    "Videos",
    "Searches",
    "Steam",
    "SteamVR",
];

const PROTON_SEARCH_DEPTH: usize = 2;

fn alphanumeric_key(text: &str) -> String {
    text.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// Folders (relative to `base`, `base` itself included) up to the search depth.
fn proton_candidate_dirs(base: &Path, rel: PathBuf, depth: usize) -> Vec<PathBuf> {
    let children = if depth < PROTON_SEARCH_DEPTH {
        paths::visible_subdirs(&base.join(&rel))
            .into_iter()
            .map(|d| base_name(&d))
            .filter(|name| !PROTON_IGNORE_DIRS.contains(&name.as_str()))
            .flat_map(|name| proton_candidate_dirs(base, rel.join(name), depth + 1))
            .collect()
    } else {
        Vec::new()
    };
    std::iter::once(rel).chain(children).collect()
}

fn search_proton_dir(
    drive_c: &Path,
    title_key: &str,
    sub: &str,
    windows_root: &str,
) -> Vec<SavePath> {
    let base = drive_c.join(sub);
    if !base.is_dir() {
        return Vec::new();
    }
    proton_candidate_dirs(&base, PathBuf::new(), 0)
        .into_iter()
        .filter(|rel| {
            let name = if rel.as_os_str().is_empty() {
                base_name(&base)
            } else {
                base_name(rel)
            };
            let key = alphanumeric_key(&name);
            !key.is_empty() && key.contains(title_key)
        })
        .map(|rel| {
            let rel = paths::lossy(&rel).replace('\\', "/");
            let path = if rel.is_empty() {
                windows_root.to_string()
            } else {
                format!("{windows_root}/{rel}")
            };
            SavePath {
                os: "windows".into(),
                path,
            }
        })
        .collect()
}

/// Save folders inside a game's Proton prefix whose name contains the title,
/// as Windows-style paths (`%APPDATA%/...`).
pub fn detect_proton_save_paths(title: &str, appid: u32) -> Vec<SavePath> {
    let title_key = alphanumeric_key(title);
    if title_key.is_empty() {
        return Vec::new();
    }
    let drive_c = paths::expand(&steam::proton_prefix(appid));
    if !drive_c.is_dir() {
        return Vec::new();
    }
    unique(
        PROTON_SEARCH_DIRS
            .iter()
            .flat_map(|(sub, root)| search_proton_dir(&drive_c, &title_key, sub, root)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::platform::has_exec_stack;
    use crate::core::testutil::{elf_library, TempDir};

    #[test]
    fn similarity_matches_python_difflib() {
        // Golden values from difflib.SequenceMatcher(None, a, b).ratio()
        let cases = [
            ("Hollow Knight", "hollow_knight", 0.7692307692307693),
            ("Celeste", "Celeste.bin.x86_64", 0.56),
            ("abcabc", "cbacba", 0.5),
            ("Game", "UnityCrashHandler64", 0.17391304347826086),
            ("Stardew Valley", "StardewValley", 0.9629629629629629),
        ];
        for (a, b, expected) in cases {
            assert!((similarity(a, b) - expected).abs() < 1e-12, "{a} vs {b}");
        }
    }

    #[cfg(unix)]
    /// A 64-bit x86 executable (ET_EXEC) header.
    fn elf() -> Vec<u8> {
        let mut bytes = b"\x7fELF\x02\x01".to_vec();
        bytes.resize(64, 0);
        bytes[16] = 2;
        bytes[18] = 0x3E;
        bytes
    }

    #[cfg(unix)]
    fn make_exec(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn detects_one_target_per_os_arch_with_best_name() {
        let dir = TempDir::new();
        let game = dir.mkdir("Celeste");
        for name in [
            "Celeste/wrapper/Celeste.bin.x86_64",
            "Celeste/wrapper/crashpad",
        ] {
            make_exec(&dir.write(name, &elf()));
        }
        let mut pe = b"MZ".to_vec();
        pe.resize(64, 0);
        dir.write("Celeste/wrapper/Celeste.exe", &pe);
        dir.write("Celeste/readme.txt", b"hi");

        let targets = collect_targets(&game);
        assert_eq!(title_for(&game), "Celeste");
        let native = targets.iter().find(|t| t.os == "linux").unwrap();
        assert_eq!(native.arch, "x64");
        assert_eq!(native.target, "wrapper/Celeste.bin.x86_64");
        assert_eq!(native.start_in, "wrapper");
        let windows = targets.iter().find(|t| t.os == "windows").unwrap();
        assert_eq!(windows.target, "wrapper/Celeste.exe");
    }

    /// The itch.io Linux build of Celeste, as installed: a launcher script
    /// beside a 32-bit and a 64-bit binary, which share the x64 target.
    #[cfg(unix)]
    #[test]
    fn a_launcher_script_beats_the_32_bit_binary_beside_it() {
        let dir = TempDir::new();
        let game = dir.mkdir("celeste");
        let mut elf_i386 = elf();
        elf_i386[18] = 0x03;
        make_exec(&dir.write("celeste/Celeste.bin.x86", &elf_i386));
        make_exec(&dir.write("celeste/Celeste.bin.x86_64", &elf()));
        make_exec(&dir.write("celeste/Celeste", b"#!/bin/bash\n./Celeste.bin.x86_64 $@\n"));

        let targets = collect_targets(&game);
        let native = targets.iter().find(|t| t.os == "linux").unwrap();
        assert_eq!(native.arch, "x64");
        assert_eq!(native.target, "Celeste");
    }

    /// The same build without its launcher script: the 64-bit binary, never
    /// its 32-bit twin, though "Celeste.bin" is the closer name.
    #[cfg(unix)]
    #[test]
    fn a_64_bit_build_beats_its_32_bit_twin() {
        let dir = TempDir::new();
        let game = dir.mkdir("celeste");
        let mut elf_i386 = elf();
        elf_i386[4] = 1;
        elf_i386[18] = 0x03;
        make_exec(&dir.write("celeste/Celeste.bin.x86", &elf_i386));
        make_exec(&dir.write("celeste/Celeste.bin.x86_64", &elf()));

        let targets = collect_targets(&game);
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].target, "Celeste.bin.x86_64");
    }

    /// The itch.io Linux build of Baba Is You: its data sits beside the
    /// `bin32/` and `bin64/` folders, which only `run.sh` starts it from.
    #[cfg(unix)]
    #[test]
    fn the_launcher_script_of_builds_in_arch_folders_is_the_target() {
        let dir = TempDir::new();
        let game = dir.mkdir("baba");
        let mut elf_i386 = elf();
        elf_i386[4] = 1;
        elf_i386[18] = 0x03;
        make_exec(&dir.write("baba/Baba Is You/bin32/Chowdren", &elf_i386));
        make_exec(&dir.write("baba/Baba Is You/bin64/Chowdren", &elf()));
        make_exec(&dir.write(
            "baba/Baba Is You/run.sh",
            b"#!/bin/sh\ncd \"$GAMEDIR\"\n\"./bin64/Chowdren\"\n",
        ));

        let targets = collect_targets(&game);
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].target, "Baba Is You/run.sh");
        assert_eq!(targets[0].start_in, "Baba Is You");
    }

    #[cfg(unix)]
    #[test]
    fn a_launcher_script_beats_a_better_named_program_other_scripts_do_not() {
        let dir = TempDir::new();
        let game = dir.mkdir("SuperGame");
        make_exec(&dir.write("SuperGame/SuperGame.x86_64", &elf()));
        make_exec(&dir.write(
            "SuperGame/install.sh",
            b"#!/bin/sh\ncp -r . ~/.local/share/supergame\n",
        ));
        assert_eq!(collect_targets(&game)[0].target, "SuperGame.x86_64");

        make_exec(&dir.write(
            "SuperGame/start.sh",
            b"#!/bin/sh\nLD_LIBRARY_PATH=lib ./SuperGame.x86_64\n",
        ));
        assert_eq!(collect_targets(&game)[0].target, "start.sh");
    }

    #[cfg(unix)]
    #[test]
    fn a_lone_32_bit_build_is_still_a_target() {
        let dir = TempDir::new();
        let game = dir.mkdir("Old Game");
        let mut elf_i386 = elf();
        elf_i386[4] = 1;
        elf_i386[18] = 0x03;
        make_exec(&dir.write("Old Game/game.x86", &elf_i386));

        let targets = collect_targets(&game);
        assert_eq!(targets[0].arch, "x64");
        assert_eq!(targets[0].target, "game.x86");
    }

    #[test]
    fn libraries_of_linux_games_lose_their_executable_stack_request() {
        let dir = TempDir::new();
        let linux_game = dir.mkdir("celeste");
        let fmod = dir.write("celeste/lib64/libfmod.so.10", &elf_library(2, 7));
        let plugin = dir.write(
            "celeste/Game_Data/Plugins/x86_64/libfmod.so",
            &elf_library(2, 7),
        );
        let windows_game = dir.mkdir("Windows Game");
        let bundled = dir.write("Windows Game/wine/libwine.so.1", &elf_library(2, 7));
        let game = |dir, os: &str| Game {
            dir,
            targets: vec![Target {
                os: os.into(),
                target: "game".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut games = [game(linux_game, "linux"), game(windows_game, "windows")];
        detect(&mut games);
        assert!(!has_exec_stack(&fmod));
        assert!(!has_exec_stack(&plugin));
        assert!(has_exec_stack(&bundled));
    }

    /// A setup script for the Windows build (NOLF's French audio fix) is no
    /// way to start the game: Proton is.
    #[cfg(unix)]
    #[test]
    fn a_script_beside_windows_programs_only_is_no_target() {
        let dir = TempDir::new();
        let game = dir.mkdir("NoOneLivesForever");
        dir.write("NoOneLivesForever/NOLF.EXE", &pe(0x014C));
        make_exec(&dir.write(
            "NoOneLivesForever/setup_steamos.sh",
            b"#!/bin/bash\nGAME_PATH=$(realpath \"NOLF.exe\")\nprotontricks $APPID dsound\n",
        ));

        let targets = collect_targets(&game);
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].os, "windows");
    }

    #[cfg(unix)]
    #[test]
    fn a_script_that_launches_no_program_of_the_game_is_no_target() {
        let dir = TempDir::new();
        let game = dir.mkdir("Game");
        make_exec(&dir.write("Game/Game.x86_64", &elf()));
        make_exec(&dir.write(
            "Game/install.sh",
            b"#!/bin/sh\ncp -r . ~/.local/share/game\n",
        ));

        let targets = collect_targets(&game);
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].target, "Game.x86_64");
    }

    /// Copies from exFAT mark every file executable: Unity's managed DLLs
    /// are PE files, its UnityPlayer an ELF library under any name.
    #[cfg(unix)]
    #[test]
    fn libraries_marked_executable_are_no_programs() {
        let dir = TempDir::new();
        let game = dir.mkdir("Islands");
        make_exec(&dir.write("Islands/Islands_Linux.x86_64", &elf()));
        make_exec(&dir.write(
            "Islands/Islands_Linux_Data/Managed/Assembly-CSharp.dll",
            &pe(0x014C),
        ));
        make_exec(&dir.write("Islands/UnityPlayer_s.debug", &elf_library(2, 6)));

        let targets = collect_targets(&game);
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].target, "Islands_Linux.x86_64");
    }

    /// A Windows program for `machine` (0x8664 = x64, 0xAA64 = arm64).
    fn pe(machine: u16) -> Vec<u8> {
        let mut bytes = b"MZ".to_vec();
        bytes.resize(64, 0);
        bytes[60] = 64; // PE header right after the DOS header
        bytes.extend(b"PE\0\0");
        bytes.extend(machine.to_le_bytes());
        bytes
    }

    #[test]
    fn a_64_in_a_helper_name_does_not_beat_the_game_exe() {
        let dir = TempDir::new();
        let game = dir.mkdir("Hollow Knight");
        dir.write("Hollow Knight/hollow_knight.exe", &pe(0x8664));
        dir.write("Hollow Knight/UnityCrashHandler64.exe", &pe(0x8664));

        let targets = collect_targets(&game);
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].target, "hollow_knight.exe");
    }

    #[test]
    fn each_arch_a_game_is_built_for_gets_its_own_target() {
        let dir = TempDir::new();
        let game = dir.mkdir("Game");
        dir.write("Game/Game.exe", &pe(0xAA64));
        dir.write("Game/Game-x64.exe", &pe(0x8664));

        let targets: Vec<(String, String)> = collect_targets(&game)
            .into_iter()
            .map(|t| (t.arch, t.target))
            .collect();
        assert!(targets.contains(&("arm64".into(), "Game.exe".into())));
        assert!(targets.contains(&("x64".into(), "Game-x64.exe".into())));
    }

    /// A universal (x64 + arm64) Mach-O header.
    fn universal_macho() -> Vec<u8> {
        let mut bytes = 0xCAFE_BABEu32.to_be_bytes().to_vec();
        bytes.extend(2u32.to_be_bytes());
        for cpu in [0x0100_0007u32, 0x0100_000C] {
            bytes.extend(cpu.to_be_bytes());
            bytes.extend([0; 16]);
        }
        bytes
    }

    #[test]
    fn an_app_bundle_is_one_macos_target_and_no_folder_to_descend() {
        let dir = TempDir::new();
        let game = dir.mkdir("Game");
        dir.write("Game/Game.app/Contents/MacOS/Game", &universal_macho());
        dir.write("Game/Game.app/Contents/Info.plist", b"<plist/>");

        let targets = collect_targets(&game);
        assert_eq!(title_for(&game), "Game");
        assert_eq!(targets.len(), 1);
        assert_eq!(
            (targets[0].os.as_str(), targets[0].arch.as_str()),
            ("macos", "")
        );
        assert_eq!(targets[0].target, "Game.app");
    }

    #[test]
    fn detection_stores_the_appid_the_game_has_now_and_keeps_a_stored_one() {
        let dir = TempDir::new();
        let game_dir = dir.mkdir("Game");
        dir.write("Game/Game.exe", &pe(0x8664));
        let target = Target {
            target: "Game.exe".into(),
            os: "windows".into(),
            ..Default::default()
        };
        let mut games = [
            Game {
                targets: vec![target.clone()],
                resolved: Some(scanner::resolve_target(&game_dir, &target)),
                ..Game::skeleton(&game_dir)
            },
            Game {
                targets: vec![target.clone()],
                resolved: Some(scanner::resolve_target(&game_dir, &target)),
                steam_appid: Some(42),
                ..Game::skeleton(&game_dir)
            },
        ];

        detect(&mut games);
        let exe = scanner::resolve_target(&game_dir, &target).target;
        let expected = steam::generate_appid("Game", &paths::lossy(&exe));
        assert_eq!(games[0].steam_appid, Some(expected));
        assert!(games[0].needs_persist);
        assert_eq!(games[1].steam_appid, Some(42));
        assert!(!games[1].needs_persist);
    }

    #[test]
    fn detection_keeps_a_title_the_game_json_already_has() {
        let dir = TempDir::new();
        let game_dir = dir.mkdir("HK_v1.5");
        dir.write("HK_v1.5/hollow_knight.exe", &pe(0x8664));
        let mut games = [Game {
            title: "Hollow Knight".into(),
            ..Game::skeleton(&game_dir)
        }];

        detect(&mut games);
        assert_eq!(games[0].targets.len(), 1);
        assert_eq!(games[0].title, "Hollow Knight");
    }

    /// Write a file without execute bits (as a copy that lost them leaves it).
    #[cfg(unix)]
    fn write_644(dir: &TempDir, name: &str, content: &[u8]) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.write(name, content);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        path
    }

    #[cfg(unix)]
    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[cfg(unix)]
    #[test]
    fn programs_that_lost_their_execute_bit_are_candidates_libraries_are_not() {
        let dir = TempDir::new();
        let candidate = |name: &str| program_of(&write_644(&dir, name, &elf())).is_some();
        assert!(candidate("Strange Flesh.AppImage"));
        assert!(candidate("BanjoRecompiled"));
        assert!(candidate("Celeste.bin.x86_64"));
        assert!(!candidate("libgame.so"));
        assert!(!candidate("libgame.so.6"));
        assert!(program_of(&write_644(&dir, "notes", b"not a program")).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn detect_finds_and_repairs_a_program_without_execute_bit() {
        let dir = TempDir::new();
        let game_dir = dir.mkdir("Strange Flesh");
        let exe = write_644(&dir, "Strange Flesh/Strange Flesh.AppImage", &elf());
        let mut games = [Game {
            dir: game_dir,
            ..Default::default()
        }];
        detect(&mut games);
        assert_eq!(games[0].targets[0].target, "Strange Flesh.AppImage");
        assert_eq!(mode(&exe), 0o755);
    }

    #[cfg(unix)]
    #[test]
    fn known_targets_get_their_execute_bit_back_text_files_do_not() {
        let dir = TempDir::new();
        let game_dir = dir.mkdir("Game");
        let exe = write_644(&dir, "Game/game", &elf());
        let script = write_644(&dir, "Game/start.txt", b"echo hi");
        let target = |name: &str| Target {
            os: "linux".into(),
            target: name.into(),
            ..Default::default()
        };
        let mut games = [Game {
            dir: game_dir,
            targets: vec![target("game"), target("start.txt")],
            ..Default::default()
        }];
        detect(&mut games);
        assert_eq!(mode(&exe), 0o755);
        assert_eq!(mode(&script), 0o644);
    }

    #[cfg(unix)]
    #[test]
    fn a_launcher_script_takes_its_os_and_arch_from_the_folder_name() {
        let dir = TempDir::new();
        let game = dir.mkdir("lennas-inception-linux-amd64-stable");
        make_exec(&dir.write(
            "lennas-inception-linux-amd64-stable/lennasinception",
            b"#!/bin/bash\nexec ./jre/bin/java -jar lib/libloader.jar",
        ));
        make_exec(&dir.write("lennas-inception-linux-amd64-stable/jre/bin/java", &elf()));

        let targets = collect_targets(&game);
        assert_eq!(targets.len(), 1);
        assert_eq!(
            (targets[0].os.as_str(), targets[0].arch.as_str()),
            ("linux", "x64")
        );
        assert_eq!(targets[0].target, "lennasinception");
    }

    #[cfg(unix)]
    #[test]
    fn a_script_without_hints_runs_where_the_programs_beside_it_do() {
        let dir = TempDir::new();
        let game = dir.mkdir("Celeste");
        make_exec(&dir.write("Celeste/Celeste", b"#!/bin/sh\n./Celeste.bin.x86_64"));
        make_exec(&dir.write("Celeste/Celeste.bin.x86_64", &elf()));

        let targets = collect_targets(&game);
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].os, "linux");
        assert_eq!(targets[0].target, "Celeste");
    }

    #[cfg(unix)]
    #[test]
    fn a_script_alone_is_a_linux_launcher_for_any_arch() {
        let dir = TempDir::new();
        let game = dir.mkdir("Game");
        make_exec(&dir.write("Game/start.sh", b"#!/bin/sh\necho hi"));

        let targets = collect_targets(&game);
        assert_eq!(
            (targets[0].os.as_str(), targets[0].arch.as_str()),
            ("linux", "")
        );
    }

    #[test]
    fn os_and_arch_hints_are_read_from_names() {
        assert_eq!(os_hint("game-linux64/run"), Some("linux"));
        assert_eq!(os_hint("game_osx/run.sh"), Some("macos"));
        assert_eq!(os_hint("machinarium/run.sh"), None);
        assert_eq!(arch_hint("game-amd64"), Some("x64"));
        assert_eq!(arch_hint("game-aarch64"), Some("arm64"));
        assert_eq!(arch_hint("celeste.bin.x86_64"), Some("x64"));
        assert_eq!(arch_hint("game-2064"), None);
        assert_eq!(arch_hint("box64-wrapper"), None);
    }

    #[cfg(unix)]
    #[test]
    fn scripts_that_lost_their_execute_bit_are_candidates_when_named_like_programs() {
        let dir = TempDir::new();
        let candidate = |name: &str| program_of(&write_644(&dir, name, b"#!/bin/sh\n")).is_some();
        assert!(candidate("start.sh"));
        assert!(candidate("lennasinception"));
        assert!(!candidate("notes.txt"));
    }

    #[cfg(unix)]
    #[test]
    fn a_script_target_gets_its_execute_bit_back() {
        let dir = TempDir::new();
        let script = write_644(&dir, "start.sh", b"#!/bin/sh\n");
        restore_exec_bit(&script);
        assert_eq!(mode(&script), 0o755);
    }

    #[test]
    fn title_descends_single_child_folders() {
        let dir = TempDir::new();
        dir.write("Pack/Real Game/run.sh", b"");
        dir.mkdir("Pack/.cartridge");
        assert_eq!(title_for(&dir.path().join("Pack")), "Real Game");
    }

    #[test]
    fn walk_respects_depth_and_skips_runtime_dirs() {
        let dir = TempDir::new();
        dir.write("a.txt", b"");
        dir.write("lib/skip.so", b"");
        dir.write("one/two/three/deep.txt", b"");
        dir.write("one/b.txt", b"");
        let names: Vec<String> = walk_files(dir.path(), 2)
            .iter()
            .map(|p| base_name(p))
            .collect();
        assert_eq!(names, vec!["a.txt", "b.txt"]);
    }

    #[test]
    fn proton_candidates_include_base_and_two_levels() {
        let dir = TempDir::new();
        dir.mkdir("Studio/Game Saves/too/deep");
        dir.mkdir("Microsoft/Game");
        let found: Vec<String> = proton_candidate_dirs(dir.path(), PathBuf::new(), 0)
            .iter()
            .map(|p| paths::lossy(p))
            .collect();
        let nested = paths::lossy(&Path::new("Studio").join("Game Saves"));
        assert_eq!(found, vec!["", "Studio", nested.as_str()]);
    }
}
