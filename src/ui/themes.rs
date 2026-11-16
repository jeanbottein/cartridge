//! Theme folders: `system/themes/<name>/theme.json` (plus the font file it
//! names), each one a theme named after its folder. The folder is created
//! with Saturday Morning, a theme and a template to copy, only when missing:
//! a theme the user deleted stays deleted.

use std::collections::HashSet;
use std::path::Path;

use log::warn;

use super::engine::theme::{self, Theme};
use crate::core::paths;

const THEMES_DIR: &str = "themes";
const THEME_FILE: &str = "theme.json";

/// What a new `system/themes/` holds.
const TEMPLATE: [(&str, &[u8]); 4] = [
    (
        "README.txt",
        include_bytes!("../../assets/themes/README.txt"),
    ),
    (
        "Saturday Morning/theme.json",
        include_bytes!("../../assets/themes/Saturday Morning/theme.json"),
    ),
    (
        "Saturday Morning/Chewy-Regular.ttf",
        include_bytes!("../../assets/themes/Saturday Morning/Chewy-Regular.ttf"),
    ),
    (
        "Saturday Morning/Chewy-LICENSE.txt",
        include_bytes!("../../assets/themes/Saturday Morning/Chewy-LICENSE.txt"),
    ),
];

/// Create `system/themes/` with the template, unless it exists.
pub fn seed(system_dir: &Path) {
    let dir = system_dir.join(THEMES_DIR);
    if dir.exists() {
        return;
    }
    for (name, bytes) in TEMPLATE {
        let path = dir.join(name);
        let written = path
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|_| paths::write_atomic(&path, bytes));
        if let Err(e) = written {
            warn!("Couldn't write {}: {e}", path.display());
        }
    }
}

/// The themes of `system/themes/`, by folder name. A folder without a
/// `theme.json` isn't a theme; one that doesn't load is skipped with a warning.
pub fn load(system_dir: &Path) -> Vec<Theme> {
    let Ok(entries) = std::fs::read_dir(system_dir.join(THEMES_DIR)) else {
        return Vec::new();
    };
    let mut folders: Vec<_> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.join(THEME_FILE).is_file())
        .collect();
    folders.sort();
    let mut ids = HashSet::new();
    folders
        .iter()
        .filter_map(|dir| read(dir))
        .filter(|theme| {
            // Ids match in any case: only the first of `Neon` and `neon` could be picked.
            let unique = ids.insert(theme.id.to_lowercase());
            if !unique {
                warn!("Theme {}: another folder has this name", theme.id);
            }
            unique
        })
        .collect()
}

fn read(dir: &Path) -> Option<Theme> {
    let name = dir.file_name()?.to_string_lossy().to_string();
    if theme::is_built_in(&name) {
        warn!("Theme {name}: a built-in theme has this name, rename its folder");
        return None;
    }
    if name.contains('#') {
        // config.txt cuts values at '#' (comments): THEME couldn't name it.
        warn!("Theme {name}: a theme folder can't have '#' in its name");
        return None;
    }
    let loaded = std::fs::read_to_string(dir.join(THEME_FILE))
        .map_err(|e| e.to_string())
        .and_then(|json| theme::from_json(&name, dir, &json));
    loaded.map_err(|e| warn!("Theme {name}: {e}")).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::testutil::TempDir;

    #[test]
    fn the_template_is_written_once_and_loads() {
        let dir = TempDir::new();
        seed(dir.path());
        let themes = load(dir.path());
        assert_eq!(themes.len(), 1);
        assert_eq!(themes[0].name, "Saturday Morning");
        assert_eq!(themes[0].tabs.len(), 6);
    }

    #[test]
    fn an_existing_themes_folder_is_left_alone() {
        let dir = TempDir::new();
        dir.mkdir("themes");
        seed(dir.path());
        assert!(load(dir.path()).is_empty());
    }

    #[test]
    fn only_loadable_folders_with_theme_json_are_themes() {
        let dir = TempDir::new();
        dir.write("themes/Good/theme.json", b"{}");
        dir.write("themes/Broken/theme.json", b"{ nope");
        dir.write("themes/Retro/theme.json", b"{}");
        dir.write("themes/good/theme.json", b"{}");
        dir.write("themes/No #1/theme.json", b"{}");
        dir.mkdir("themes/Empty");
        let names: Vec<String> = load(dir.path()).into_iter().map(|t| t.name).collect();
        assert_eq!(names, ["Good"]);
    }
}
