//! Editor actions that take a game back: reset its metadata so the pipeline
//! detects and enriches it again, or delete it from the library.

use std::path::{Path, PathBuf};

use super::models::{Game, ImageSlot};
use super::{package, persister};

/// Forget what `game.json` holds, and the artwork downloaded for it, so the
/// next run detects the program and fetches the metadata again. The AppID
/// stays: it keeps the Steam shortcut, the Proton prefix and its saves.
pub fn reset_metadata(game: &Game) -> std::io::Result<()> {
    ImageSlot::ALL
        .iter()
        .filter_map(|&slot| own_image(game, slot))
        .for_each(remove_image);
    let mut reset = Game {
        steam_appid: game.steam_appid,
        ..Game::skeleton(&game.dir)
    };
    persister::write_game_json(&mut reset)
}

/// An image file in the game's `.cartridge/`: a name with a folder in it
/// (`../cover.png`, edited by hand) points elsewhere and is left alone.
fn own_image(game: &Game, slot: ImageSlot) -> Option<PathBuf> {
    let name = game.images.get(slot)?;
    let plain = Path::new(name).file_name().is_some_and(|f| f == name);
    plain.then(|| game.data_dir().join(name))
}

fn remove_image(path: PathBuf) {
    match std::fs::remove_file(&path) {
        Ok(()) => log::info!("Deleted image file: {}", path.display()),
        Err(e) if path.exists() => log::error!("Cannot delete {}: {e}", path.display()),
        Err(_) => {}
    }
}

/// Take a game folder out of the library at once: renamed to a hidden
/// staging name (which the scanner skips), for [`std::fs::remove_dir_all`]
/// to delete in the background. Returns the staged path.
pub fn stage_for_deletion(game_dir: &Path) -> std::io::Result<PathBuf> {
    let name = game_dir
        .file_name()
        .ok_or_else(|| std::io::Error::other("not a game folder"))?
        .to_string_lossy();
    let staged = game_dir.with_file_name(package::staging_name(&format!("{name}.deleted")));
    std::fs::rename(game_dir, &staged)?;
    Ok(staged)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::scanner;
    use crate::core::testutil::TempDir;

    const GAME_JSON: &[u8] = br#"{
        "title": "Some Game",
        "targets": [{"os": "linux", "arch": "x64", "target": "game.x86", "startIn": "."}],
        "images": {"cover": "cover.png", "icon": "../outside.png"},
        "steamgriddb_id": 7,
        "steam_appid": 3000000001,
        "year": 2018,
        "update": {"url": "https://github.com/owner/game"}
    }"#;

    #[test]
    fn a_reset_game_keeps_only_its_appid_and_loses_its_own_artwork() {
        let dir = TempDir::new();
        dir.write("games/some-game/.cartridge/game.json", GAME_JSON);
        let cover = dir.write("games/some-game/.cartridge/cover.png", b"png");
        let outside = dir.write("games/some-game/outside.png", b"png");
        let game = scanner::load_game(&dir.path().join("games/some-game")).unwrap();

        reset_metadata(&game).unwrap();

        let reset = scanner::load_game(&game.dir).unwrap();
        assert_eq!(reset.title, "some-game");
        assert!(reset.targets.is_empty());
        assert_eq!(reset.steamgriddb_id, None);
        assert_eq!(reset.year, None);
        assert!(reset.update.url.is_empty());
        assert_eq!(reset.images.cover, None);
        assert_eq!(reset.steam_appid, Some(3_000_000_001));
        assert!(!cover.exists());
        assert!(outside.exists());
    }

    #[test]
    fn a_staged_game_leaves_the_library_at_once() {
        let dir = TempDir::new();
        dir.write("games/some-game/game.x86_64", b"elf");
        dir.mkdir("games/other-game");
        let games_dir = dir.path().join("games");

        let staged = stage_for_deletion(&games_dir.join("some-game")).unwrap();

        assert!(staged.join("game.x86_64").is_file());
        let names: Vec<String> = scanner::load_library(&games_dir)
            .into_iter()
            .map(|g| g.folder_name)
            .collect();
        assert_eq!(names, ["other-game"]);
    }
}
