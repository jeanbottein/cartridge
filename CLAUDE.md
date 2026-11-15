# AGENTS.md – AI Agent Guide for •cartridge

## Project Overview

•cartridge (French for "cartridge") is a Rust app for managing DRM-free games on Linux (primarily Steam Deck). One binary: a themed macroquad UI by default, a headless library with `--serve`. The backend is a pipeline of phases over an in-memory game list, run by the autopilot at launch and whenever `games/` changes (no manual run).

## Pipeline

```
core::autopilot (worker thread: run at launch, then each settled change of games/)
core::pipeline::Pipeline (one fixed sequence; each phase only does what is missing;
                          steps 2-4 run per batch of 10 games, each batch written before the next)
 0. migrations::run_all      # Idempotent data migrations
 1. scanner::scan            # Parse .cartridge/game.json into Vec<Game>
 2. detector::detect         # Fill missing targets (exe detection), restore lost exec bits
    sizer::measure           #   + size on disk into game.json (only when unknown: kept until reinstalled)
 3. sgdb::enrich             # SteamGridDB names/IDs/artwork (if API key)
 4. persister::persist       # Write game.json + download images, always in each game's .cartridge/
 5. steam::clean             # Remove this cartridge's shortcuts for games no longer in Steam
 6. steam::export            # Insert/update shortcuts + Proton compat
    steam_self::register     #   + •cartridge itself (STEAM_ADD_SELF)
 7. manifest::write          # manifests.json (MANIFEST_EXPORT)
 8. mods::run                # Keep the mods the user turned on applied (system/mods.json)
 (saver.rs: per-game save backup/restore, by hand from the SAVES tab, never by the pipeline)
 (updater.rs: game updates from GitHub releases, by hand from the UPDATES tab, never by the pipeline)
```

Migrations: `launch_manifest.json` → `.cartridge/game.json`, nested → flat `savePaths`, arch `x86_64` → `x64`. Add new ones to `migrations::run_all`; keep them idempotent.

## Architecture

```
src/
  main.rs, cli.rs        # Args: UI (default), --serve, --pack/--unpack, --peers, --download/--install
  headless.rs            # --serve (autopilot + LAN server) and the one-shot commands
  core/                  # Backend, no UI code
    app.rs               # App identity, fixed folder names, cartridge id, system/ lookup
    config.rs            # config.txt parse / relative path resolution / in-place updates
    models.rs            # Game, Target, SavePath, Images; game.json (de)serialization
    paths.rs, platform.rs# normpath/expandvars/relpath; OS/arch tags, ELF/PE/Mach-O headers
    migrations.rs scanner.rs detector.rs sgdb.rs persister.rs sizer.rs
    package.rs           # Foo.cartridge.tar: pack/unpack with progress + crc verification
    share/               # LAN sharing: server.rs (HTTP, GET only), discovery.rs (UDP broadcast), client.rs
    vdf.rs               # Binary (shortcuts.vdf) + text (config.vdf) KeyValues
    steam/               # users, selection (steam.json), host (detect/restart), shortcuts (vdf), clean, export, compat (Proton builds)
    steam_self.rs manifest.rs bps.rs
    saver.rs             # SAVES: saves-copies/<game folder>/<save folder>/; backup under a hidden name, renamed once complete; restore empties then copies
    save_links.rs        # saves-links/ symlink tree (not built for now)
    proton.rs            # Windows games with Steam's Proton + runtime, in the shortcut's compatdata prefix; winetricks
    patcher.rs mods.rs   # mod file engine (state/apply/restore) + mod model (list, on/off)
    self_update.rs       # the app's own update: release of CARTRIDGE_REPO (baked in by CI), only the app swapped (old one moved aside, removed at next launch)
    updater.rs           # game.json "update": latest GitHub release, asset pick by os/arch, download + upsert over the game folder (never deletes, never writes .cartridge/) or AppImage over the executable
    pipeline.rs init.rs logging.rs
    autopilot.rs         # Worker + Watcher: lists games/ every 2 s (served from the kernel cache), waits for new entries to stop changing
  ui/                    # macroquad UI
    app.rs               # Shell: icon tabs (current one named), footer hints, game session lock, autopilot (paused while a game runs or the editor is open), warning toasts
    library.rs editor.rs settings.rs setup.rs
    form.rs list.rs keyboard.rs browser.rs dialog.rs   # widgets
    network.rs jobs.rs info.rs   # Network tab, background transfers, shared info panel
    mods.rs              # MODS tab: mods with status, A turns one on/off
    saves.rs             # SAVES tab: device and backup cards of the focused game, A back up / X restore (confirmed)
    app_update.rs        # Settings > APP row: check (on demand, 5 min cooldown), confirm, install in the background, "restart to finish"
    updates.rs           # UPDATES tab: games with a release page; X checks GitHub (only on demand, 5 min cooldown), A updates (confirmed), Y marks installed
    style.rs             # Shared look: backdrop, cards, highlight, panels, bars, toasts, chips, badges
    covers.rs launcher.rs music.rs
    engine/              # canvas (design px, drawn at native res), input, text, shape, icon (tab icons from shapes), theme
assets/                  # Embedded: fonts (VT323, Barlow), icon, steam-art, config-default.txt, games_locations.json
testdata/                # Golden fixtures produced by the former Python implementation
```

## Data Structure

```
CARTRIDGE/                                                            # Config::cartridge_dir()
  system/config.txt, README.txt, steamgriddb_cache.json, ui-thumbs/   # workspace: Config::system_dir()
  cartridge (binary / .AppImage / .app)                               # portable: next to system/
  games/MyGame/.cartridge/game.json, cover.png, icon.png, hero.png, logo.png
  games/MyGame/.cartridge/unpacked_from.json   # id of the package it came from
  games/Other.cartridge.tar                    # packaged game (not unpacked yet)
```

Folder names are fixed, whatever the binary is called: per-game metadata is always `.cartridge/` (part of the game and package format), the workspace is always `system/`. `system/` only counts with a `config.txt` or our README (the name is common). A `.cartridge/` folder next to the app is not a workspace (`core::app::workspace_in`).

Packages: a plain tar of `Foo/` with `.cartridge/package.json` first and `.cartridge/checksums.json` last. The scanner merges `Foo.cartridge.tar` and `Foo/` into one game (`models::Install`). Packaged-only games are read-only: phases that write (detector, sgdb, persister, editor) skip them. Staging uses hidden `.Foo…part` names, which the scanner ignores.

## Key Conventions

- **Versions, newer vs older apps:** bump `models::SCHEMA_VERSION` on any change to what game.json holds: an older app reads a newer game.json but never writes it (`Game::is_writable`; migrations skip it too), since it would drop what it doesn't know. Package `FORMAT` only for the tar layout, share `PROTOCOL` only for the HTTP API; peers of another protocol are listed by name with which side to update.
- **Self-update:** `CARTRIDGE_REPO` (`owner/name`) is baked in at build time (`option_env!`, set to `github.repository` in release.yml, passed by container.sh); without it there's no APP section. Tags must be Cargo.toml's version (`v` + version, checked in CI). The app is found in the release archive by its `app_name` (`Cartridge.AppImage` / `Cartridge.app` / `cartridge.exe`).
- **Compatibility is the contract:** game.json (4-space JSON), shortcuts.vdf, AppIDs (`crc32(exe+title) | 0x80000000`, computed once and kept in game.json `steam_appid` so a rename keeps the shortcut, Proton prefix and saves: always go through `steam::appid_for`/`appid_of`), path normalization must stay byte-compatible with existing cartridges. Save backups are `saves-copies/<game folder>/<save folder>/` (folder names survive a rename). Golden tests in `testdata/` guard this.
- **Fixed layout, no path settings:** `games/`, `mods/`, `saves-copies/`, `saves-links/` sit next to `system/` (`core::app::FOLDERS`, `Config::games_dir()`/`mods_dir()`/...). Users who want a folder elsewhere replace it with a symlink.
- **`Config`** holds the values as typed in `config.txt` (no value is a path).
- **UI look:** layout is in design pixels (432 px tall canvas), rendered at the window's native resolution, so 480p and 4K share one layout. Screens draw through `ui::style` and `theme::*()` colors, never raw colors; `THEME` (Settings > DISPLAY) swaps palette, typeface and corners, never positions. Add a theme to `engine::theme::THEMES`. `WINDOW_MODE`: borderless (fullscreen, default) or a resizable window; gamescope is always fullscreen.
- **Windows games:** the UI runs them itself with Steam's Proton (`core::proton`), the way Steam runs the shortcut (runtime `_v2-entry-point` → `proton waitforexitandrun`, `STEAM_COMPAT_DATA_PATH=compatdata/<shortcut appid>`), so prefix and saves are shared with the Steam shortcut. Per-game settings: `"proton": {version, winetricks, env}` in game.json. Build: game.json `version` → the shortcut's own `CompatToolMapping` entry → Steam's default (`0`) → newest Proton (`proton::Installed::tool_for`), so Steam and cartridge share the prefix's build. A non-empty `env` (plain names only: Steam runs LaunchOptions in a shell) owns the shortcut's LaunchOptions; otherwise options set in Steam are kept. `version` pins the shortcut's compat tool only when installed. winetricks downloads: `W_CACHE` is `system/winetricks` with `PROTON_KEEP_DOWNLOADS`, else the user cache with links to what `system/winetricks` holds.
- **Debug mode:** a file named `debug` next to the app (`core::app::debug_mode`) shows Settings > DEBUG; `DEBUG_*` keys apply only then. They are interface-only (`DEBUG_FAKE_PROGRESS` loops a fake progress card, drawn by the UI; the autopilot never sees it).
- Flags: `is_true` for opt-in keys (STEAM_ADD_SELF, SHARE_ENABLED, ...), `is_not_false` for opt-out keys (MANIFEST_EXPORT, MUSIC_ENABLED).
- **Steam, per game:** the user adds/removes each game (Library, Y), remembered in `system/steam.json`; the pipeline keeps exactly those shortcuts current. Ownership tag `cartridge:<id>` (`system/cartridge-id`) plus the folder-name label as a visible tag; the self-shortcut (`STEAM_ADD_SELF`, first SETTINGS option) uses `<owner tag>-self` and is applied as soon as it's toggled. Steam reads shortcuts at startup: the UI says "restart Steam" when it runs.
- **Shared files:** the autopilot runs while the UI stays usable, so every writer of Steam's files takes `steam::files_lock()` and every mod apply/restore takes the lock in `mods`. Opening the editor pauses the autopilot.
- **Logging:** `log` macros; `core::logging` prints to stderr, or to the systemd journal (`journalctl --user -t cartridge`) when stderr is no terminal; the UI toasts warnings and errors. No log file.
- **Game updates:** `"update": {url, asset, installed, build}` in game.json (omitted when empty). `url` is a GitHub project page; `asset` an optional part of the release file name (else picked from the target's os/arch, Flatpak skipped); `installed` the tag last installed; `build` that file's fingerprint (GitHub sha256 digest, else upload date), so rolling releases (`ci-dev-build`, `nightly`) update when CI rebuilds; empty compares the tag only. An update is an upsert like a Windows installer: archive files are added or replaced, nothing is deleted (saves, configs, ROMs, mods stay). An archive that only wraps another (zip holding a .tar.gz) is unpacked twice; a release file that is the program itself (`.AppImage`, Linux x64 unless named otherwise; also zipped alone when the target is an AppImage) replaces the target's executable, and is preferred over an archive when the target is an AppImage. The autopilot pauses while one runs.
- **Minimal dependencies:** macroquad, gilrs (feature `gamepad`), rodio (feature `music`, ALSA at build time), serde/serde_json, image, ureq, crc32fast, tar, flate2 + zip (update archives, inflate only), log, chrono (`clock` only: local save dates).

## Dev Workflow

The host (Bazzite) has no Rust toolchain on purpose: build in the container.

```bash
./container.sh                         # cargo build --release (any cargo args work)
./container.sh test
./container.sh clippy --all-targets
./target/release/cartridge             # run on the host (UI)
```

Releases (`v*` tags, or run the Release workflow by hand) ship ready-to-use
cartridge archives (`package.sh`, template in `packaging/cartridge/`): a
`Cartridge/` folder with the app (`Cartridge.AppImage` via `appimage.sh`,
Apple Silicon `Cartridge.app` via `macapp.sh`, `cartridge.exe`), READMEs and an
empty `system/`. No `config.txt` is shipped: the app creates it on first
launch, so extracting a newer release over a cartridge keeps its settings.
CI extracts each archive and runs the app inside it. Bare binaries are only
published with `PUBLISH_BARE_BINARIES: true` in `release.yml`. In an AppImage
or `.app` the binary sits in a mount/bundle: `core::app::launched_file()`
returns the `.AppImage` / `.app` instead, for portable mode and the Steam
self-shortcut.

With a native toolchain instead:

```bash
cargo run                              # UI
cargo run -- --serve                   # headless library (autopilot + sharing), logs in the terminal
cargo test && cargo clippy --all-targets
cargo build --no-default-features      # without libudev/ALSA (no gamepad, no music)
```

Edit `system/config.txt` (seeded from `assets/config-default.txt`).

## Testing

Unit tests live next to the code (`cargo test`); filesystem tests use `core::testutil::TempDir`. For UI changes, run under Xvfb (`Xvfb :99 &`, `DISPLAY=:99 cargo run`) and drive with `xdotool` / screenshot with `import -window root`.
