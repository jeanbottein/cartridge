//! `saves-links/`: a tree of symlinks to the live save folders, for
//! Syncthing & co. Not built for now: it waits for its own rework.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use log::{debug, info};

use super::paths;

fn symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    return std::os::unix::fs::symlink(target, link);
    #[cfg(windows)]
    return if target.is_dir() {
        std::os::windows::fs::symlink_dir(target, link)
    } else {
        std::os::windows::fs::symlink_file(target, link)
    };
}

/// Remove a symlink (Windows removes folder links with `remove_dir`).
fn remove_link(link: &Path) -> std::io::Result<()> {
    std::fs::remove_file(link).or_else(|_| std::fs::remove_dir(link))
}

/// Remove a folder of our links (left when a game had several save paths).
/// A folder holding anything but symlinks is the user's: it stays.
fn remove_link_folder(dir: &Path) -> bool {
    let entries: Vec<PathBuf> = paths::sorted_entries(dir)
        .into_iter()
        .map(|n| dir.join(n))
        .collect();
    if !entries.iter().all(|p| paths::is_symlink(p)) {
        return false;
    }
    entries.iter().for_each(|link| {
        let _ = remove_link(link);
    });
    std::fs::remove_dir(dir).is_ok()
}

/// Signature of `symlink(target, link)`, injected so tests can stand in for
/// a filesystem without links.
type MakeLink = fn(&Path, &Path) -> std::io::Result<()>;

/// `link` → `target`; a real file or folder already there is the user's and
/// is left alone.
fn create_symlink(link: &Path, target: &Path, make_link: MakeLink) -> std::io::Result<()> {
    if paths::is_symlink(link) {
        if std::fs::read_link(link).is_ok_and(|t| t == target) {
            return Ok(());
        }
        let _ = remove_link(link);
    } else if link.exists() && !(link.is_dir() && remove_link_folder(link)) {
        debug!("  {} exists and is not a symlink, skipping", link.display());
        return Ok(());
    }
    if let Some(dir) = link.parent() {
        std::fs::create_dir_all(dir)?;
    }
    make_link(target, link)
}

/// The folders made for a link that could not be created, up to `root`
/// (only the empty ones go).
fn remove_empty_parents(link: &Path, root: &Path) {
    link.ancestors()
        .skip(1)
        .take_while(|dir| dir.starts_with(root))
        .for_each(|dir| {
            let _ = std::fs::remove_dir(dir);
        });
}

fn remove_stale_symlinks(root: &Path, active: &HashSet<String>) {
    for name in paths::sorted_entries(root)
        .into_iter()
        .filter(|n| !active.contains(n))
    {
        let path = root.join(&name);
        if !paths::is_symlink(&path) {
            if path.is_dir() && remove_link_folder(&path) {
                info!("  Removed stale symlinks: {name}/");
            }
            continue;
        }
        info!("  Removing stale symlink: {name}");
        if let Err(e) = remove_link(&path) {
            debug!("  Could not remove stale symlink {}: {e}", path.display());
        }
    }
}

/// A symlink to create: `root/<top>` or `root/<top>/<sub>` → `source`.
struct LinkEntry {
    top: String,
    sub: Option<String>,
    source: PathBuf,
}

/// The links are an optional view on the saves: `root` only appears with
/// the first link, and where links can't be made (FAT/exFAT, some network
/// shares, Windows without the right) there is no tree, without a warning.
fn build_symlink_tree(entries: &[LinkEntry], root: &Path, make_link: MakeLink) {
    let mut created = HashSet::new();
    for entry in entries.iter().filter(|e| e.source.is_dir()) {
        let top = root.join(&entry.top);
        let link = match &entry.sub {
            None => top,
            Some(sub) => {
                // A link left from when the game had one save path: links
                // created through it would land in the live save folder.
                if paths::is_symlink(&top) {
                    let _ = remove_link(&top);
                }
                top.join(sub)
            }
        };
        if let Err(e) = create_symlink(&link, &entry.source, make_link) {
            debug!("  No save links in {}: {e}", root.display());
            remove_empty_parents(&link, root);
            return;
        }
        created.insert(entry.top.clone());
    }
    remove_stale_symlinks(root, &created);
    if !created.is_empty() {
        info!(
            "  Symlink tree updated at {} ({} entries)",
            root.display(),
            created.len()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::testutil::TempDir;

    fn refuse_links(_: &Path, _: &Path) -> std::io::Result<()> {
        Err(std::io::ErrorKind::Unsupported.into())
    }

    fn one_link(dir: &TempDir) -> LinkEntry {
        LinkEntry {
            top: "Game".into(),
            sub: Some("saves".into()),
            source: dir.mkdir("live/saves"),
        }
    }

    #[test]
    fn no_save_to_link_creates_no_folder() {
        let dir = TempDir::new();
        let root = dir.path().join("saves-links");

        build_symlink_tree(&[], &root, symlink);
        assert!(!root.exists());
    }

    #[test]
    fn a_filesystem_without_links_leaves_no_folder_behind() {
        let dir = TempDir::new();
        let root = dir.path().join("saves-links");

        build_symlink_tree(&[one_link(&dir)], &root, refuse_links);
        assert!(!root.exists());
    }

    #[test]
    fn a_filesystem_without_links_keeps_what_the_folder_held() {
        let dir = TempDir::new();
        let root = dir.path().join("saves-links");
        dir.write("saves-links/README.txt", b"old cartridge");

        build_symlink_tree(&[one_link(&dir)], &root, refuse_links);
        assert!(root.join("README.txt").is_file());
        assert!(!root.join("Game").exists());
    }

    #[cfg(unix)]
    #[test]
    fn back_to_one_save_path_replaces_the_folder_of_links_with_a_link() {
        let dir = TempDir::new();
        let saves = dir.mkdir("live/saves");
        let root = dir.mkdir("saves-links");
        dir.mkdir("saves-links/Game");
        std::os::unix::fs::symlink(&saves, root.join("Game/saves")).unwrap();
        std::os::unix::fs::symlink(dir.mkdir("live/old"), root.join("Game/old")).unwrap();
        let entry = LinkEntry {
            top: "Game".into(),
            sub: None,
            source: saves.clone(),
        };

        build_symlink_tree(&[entry], &root, symlink);
        assert_eq!(std::fs::read_link(root.join("Game")).unwrap(), saves);
    }

    #[cfg(unix)]
    #[test]
    fn a_folder_holding_real_files_is_never_replaced_or_removed() {
        let dir = TempDir::new();
        let saves = dir.mkdir("live/saves");
        let root = dir.mkdir("saves-links");
        dir.write("saves-links/Game/mine.txt", b"keep");
        dir.write("saves-links/Gone/mine.txt", b"keep");
        let entry = LinkEntry {
            top: "Game".into(),
            sub: None,
            source: saves,
        };

        build_symlink_tree(&[entry], &root, symlink);
        assert!(root.join("Game/mine.txt").is_file());
        assert!(root.join("Gone/mine.txt").is_file());
    }

    #[cfg(unix)]
    #[test]
    fn a_removed_game_with_several_save_paths_loses_its_folder_of_links() {
        let dir = TempDir::new();
        let root = dir.mkdir("saves-links");
        dir.mkdir("saves-links/Gone");
        std::os::unix::fs::symlink(dir.mkdir("live/a"), root.join("Gone/a")).unwrap();

        build_symlink_tree(&[], &root, symlink);
        assert!(!root.join("Gone").exists());
    }

    #[cfg(unix)]
    #[test]
    fn second_save_path_replaces_the_game_link_with_a_folder() {
        let dir = TempDir::new();
        let first = dir.mkdir("live/first");
        let second = dir.mkdir("live/second");
        let root = dir.mkdir("saves-links");
        std::os::unix::fs::symlink(&first, root.join("Game")).unwrap();
        let entries = [&first, &second].map(|source| LinkEntry {
            top: "Game".into(),
            sub: source.file_name().map(|n| n.to_string_lossy().into_owned()),
            source: source.clone(),
        });

        build_symlink_tree(&entries, &root, symlink);
        assert!(!paths::is_symlink(&root.join("Game")));
        assert_eq!(
            std::fs::read_link(root.join("Game/second")).unwrap(),
            second
        );
        assert_eq!(paths::sorted_entries(&first), Vec::<String>::new());
    }
}
