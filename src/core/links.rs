//! Symlinks inside games (packages, update archives) on any filesystem.
//! Where one can't be made (FAT, exFAT, some network shares, Windows), the
//! file or folder it points to is copied in its place once everything else
//! is written, so `libfoo.so -> libfoo.so.1` still loads.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use super::paths;

#[cfg(unix)]
pub(crate) fn make_symlink(target: &Path, link: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

/// Windows needs a privilege for symlinks (and to know file from folder):
/// copies stand in for them.
#[cfg(not(unix))]
pub(crate) fn make_symlink(_target: &Path, _link: &Path) -> io::Result<()> {
    Err(io::ErrorKind::Unsupported.into())
}

/// `target` as `link` would reach it, without touching the filesystem.
fn reached_by(link: &Path, target: &Path) -> PathBuf {
    paths::normpath(&link.parent().unwrap_or(link).join(target))
}

/// `link` → `target` stays inside `root` (judged by the names only).
pub(crate) fn points_inside(root: &Path, link: &Path, target: &Path) -> bool {
    reached_by(link, target).starts_with(paths::normpath(root))
}

/// The links of one extraction, and those the filesystem refused.
pub(crate) struct Links {
    make: fn(&Path, &Path) -> io::Result<()>,
    /// `(link, target)`, to copy by `copy_refused`.
    refused: Vec<(PathBuf, PathBuf)>,
}

impl Default for Links {
    fn default() -> Self {
        Self {
            make: make_symlink,
            refused: Vec::new(),
        }
    }
}

impl Links {
    /// `link` → `target`, or noted for `copy_refused` when the filesystem
    /// refuses links.
    pub fn add(&mut self, target: &Path, link: &Path) -> io::Result<()> {
        if let Some(dir) = link.parent() {
            fs::create_dir_all(dir)?;
        }
        if let Err(e) = (self.make)(target, link) {
            log::debug!("{}: no link here ({e}), copying its target", link.display());
            self.refused
                .push((link.to_path_buf(), target.to_path_buf()));
        }
        Ok(())
    }

    /// Copy what each refused link points to in its place, once everything
    /// else is written. A link to another refused link waits for its copy;
    /// one to nothing, or outside `root`, is left out.
    pub fn copy_refused(self, root: &Path) -> io::Result<()> {
        let root = root.canonicalize()?;
        let mut pending = self.refused;
        // A loop: each pass copies what exists, which a chain of links needs.
        while !pending.is_empty() {
            let (ready, waiting): (Vec<_>, Vec<_>) = pending
                .into_iter()
                .partition(|(link, target)| reached_by(link, target).exists());
            if ready.is_empty() {
                waiting.iter().for_each(|(link, target)| {
                    log::debug!("{} -> {}: nothing there", link.display(), target.display());
                });
                return Ok(());
            }
            ready
                .iter()
                .try_for_each(|(link, target)| copy_in_place(&root, link, target))?;
            pending = waiting;
        }
        Ok(())
    }
}

/// Copy what `link` points to as `link` itself.
fn copy_in_place(root: &Path, link: &Path, target: &Path) -> io::Result<()> {
    let source = reached_by(link, target).canonicalize()?;
    let parent = link.parent().unwrap_or(link).canonicalize()?;
    // Outside the game, or a folder holding the link (copying it would never end).
    if !source.starts_with(root) || parent.starts_with(&source) {
        log::debug!("{} -> {}: left out", link.display(), target.display());
        return Ok(());
    }
    if fs::symlink_metadata(link).is_ok() {
        return Ok(());
    }
    if source.is_dir() {
        copy_dir(&source, link)
    } else {
        fs::copy(&source, link).map(|_| ())
    }
}

fn copy_dir(from: &Path, to: &Path) -> io::Result<()> {
    fs::create_dir_all(to)?;
    fs::read_dir(from)?.try_for_each(|entry| {
        let entry = entry?;
        let dest = to.join(entry.file_name());
        if entry.path().is_dir() {
            copy_dir(&entry.path(), &dest)
        } else {
            fs::copy(entry.path(), dest).map(|_| ())
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::testutil::TempDir;

    /// A filesystem without links (FAT, exFAT).
    fn refusing() -> Links {
        Links {
            make: |_, _| Err(io::ErrorKind::Unsupported.into()),
            refused: Vec::new(),
        }
    }

    #[test]
    fn a_refused_link_becomes_a_copy_of_its_file() {
        let dir = TempDir::new();
        dir.write("game/libfoo.so.1", b"lib");
        let mut links = refusing();
        links
            .add(Path::new("libfoo.so.1"), &dir.path().join("game/libfoo.so"))
            .unwrap();

        links.copy_refused(&dir.path().join("game")).unwrap();
        assert_eq!(fs::read(dir.path().join("game/libfoo.so")).unwrap(), b"lib");
    }

    #[test]
    fn a_chain_of_refused_links_is_copied_from_its_end() {
        let dir = TempDir::new();
        let game = dir.mkdir("game");
        dir.write("game/libfoo.so.1.2", b"lib");
        let mut links = refusing();
        links
            .add(Path::new("libfoo.so.1"), &game.join("libfoo.so"))
            .unwrap();
        links
            .add(Path::new("libfoo.so.1.2"), &game.join("libfoo.so.1"))
            .unwrap();

        links.copy_refused(&game).unwrap();
        assert_eq!(fs::read(game.join("libfoo.so")).unwrap(), b"lib");
        assert_eq!(fs::read(game.join("libfoo.so.1")).unwrap(), b"lib");
    }

    #[test]
    fn a_refused_folder_link_becomes_a_copy_of_the_folder() {
        let dir = TempDir::new();
        let game = dir.mkdir("game");
        dir.write("game/data/levels/1.map", b"map");
        let mut links = refusing();
        links
            .add(Path::new("../data"), &game.join("bin/data"))
            .unwrap();

        links.copy_refused(&game).unwrap();
        assert_eq!(
            fs::read(game.join("bin/data/levels/1.map")).unwrap(),
            b"map"
        );
    }

    #[test]
    fn a_refused_link_outside_the_game_copies_nothing() {
        let dir = TempDir::new();
        let game = dir.mkdir("game");
        dir.write("secret", b"private");
        let mut links = refusing();
        links
            .add(Path::new("../secret"), &game.join("secret"))
            .unwrap();

        links.copy_refused(&game).unwrap();
        assert!(!game.join("secret").exists());
    }

    #[test]
    fn a_refused_link_to_nothing_or_to_its_own_folder_copies_nothing() {
        let dir = TempDir::new();
        let game = dir.mkdir("game");
        let mut links = refusing();
        links
            .add(Path::new("gone"), &game.join("dangling"))
            .unwrap();
        links.add(Path::new(".."), &game.join("sub/up")).unwrap();

        links.copy_refused(&game).unwrap();
        assert!(!game.join("dangling").exists());
        assert!(!game.join("sub/up").exists());
    }

    #[test]
    fn a_link_stays_inside_only_by_its_resolved_names() {
        let root = Path::new("/games/Game");
        assert!(points_inside(
            root,
            &root.join("bin/run"),
            Path::new("../game.bin")
        ));
        assert!(!points_inside(
            root,
            &root.join("run"),
            Path::new("../../etc/passwd")
        ));
        assert!(!points_inside(
            root,
            &root.join("run"),
            Path::new("/etc/passwd")
        ));
    }

    #[cfg(unix)]
    #[test]
    fn a_link_is_made_where_the_filesystem_allows_it() {
        let dir = TempDir::new();
        let game = dir.mkdir("game");
        let mut links = Links::default();
        links.add(Path::new("game.bin"), &game.join("run")).unwrap();

        links.copy_refused(&game).unwrap();
        assert_eq!(
            fs::read_link(game.join("run")).unwrap(),
            Path::new("game.bin")
        );
    }
}
