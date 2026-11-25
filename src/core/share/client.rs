//! Client side: find peers, list their games, and copy a game either as a
//! package (Download: kept as `Foo.cartridge.tar`, e.g. on a library machine)
//! or straight into a folder (Install: unpacked while it streams, no tar on disk).
//! A game's saves or save backup can be copied from a peer too.

use std::fs::{self, File};
use std::io::{self, BufWriter, Read, Write};
use std::net::{SocketAddr, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::discovery::{self, Peer};
use super::{encode_segment, instance_id, port, RemoteGame, RemoteSaves, ServerInfo};
use crate::core::config::Config;
use crate::core::package::{self, Progress};
use crate::core::saver::{self, Saves, Side};

const LIST_TIMEOUT: Duration = Duration::from_secs(10);
/// Per read: a stalled stream fails instead of hanging; a long one is fine.
const STREAM_TIMEOUT: Duration = Duration::from_secs(60);

fn request(url: &str, read_timeout: Duration) -> Result<ureq::Response, Box<ureq::Error>> {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(5))
        .timeout_read(read_timeout)
        .build()
        .get(url)
        .call()
        .map_err(Box::new)
}

fn get(url: &str, read_timeout: Duration) -> Result<ureq::Response, String> {
    request(url, read_timeout).map_err(|e| message_of(*e))
}

/// The server's own `{"error": ...}` when it sent one.
fn message_of(error: ureq::Error) -> String {
    let fallback = error.to_string();
    let ureq::Error::Status(_, response) = error else {
        return fallback;
    };
    read_body(response, MAX_BODY)
        .ok()
        .and_then(|body| serde_json::from_slice::<serde_json::Value>(&body).ok())
        .and_then(|body| Some(body.get("error")?.as_str()?.to_string()))
        .unwrap_or(fallback)
}

/// Covers are a few hundred KB, game lists well under a MB: a broken or
/// hostile peer can't fill memory with either.
const MAX_BODY: u64 = 16 * 1024 * 1024;

/// The whole body, unless it is longer than `limit`.
fn read_body(response: ureq::Response, limit: u64) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    response
        .into_reader()
        .take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() as u64 > limit {
        return Err(format!("answer larger than {limit} bytes"));
    }
    Ok(bytes)
}

fn get_json<T: serde::de::DeserializeOwned>(url: &str) -> Result<T, String> {
    let body = read_body(get(url, LIST_TIMEOUT)?, MAX_BODY)?;
    serde_json::from_slice(&body).map_err(|e| e.to_string())
}

fn api(addr: SocketAddr, path: &str) -> String {
    format!("http://{addr}/api/v1/{path}")
}

fn game_url(addr: SocketAddr, id: &str, what: &str) -> String {
    api(addr, &format!("games/{}/{what}", encode_segment(id)))
}

/// The server at `host[:port]`, if one answers (of any protocol: see
/// [`ServerInfo::mismatch`]).
pub fn probe(host: &str, default_port: u16) -> Option<Peer> {
    let with_port = if host.contains(':') {
        host.to_string()
    } else {
        format!("{host}:{default_port}")
    };
    let addr = with_port.to_socket_addrs().ok()?.next()?;
    let info: ServerInfo = get_json(&api(addr, "info")).ok()?;
    Some(Peer { info, addr })
}

/// `SHARE_PEERS` (`host[:port], ...`) for networks where broadcast doesn't reach.
fn configured_peers(cfg: &Config) -> Vec<Peer> {
    cfg.get("SHARE_PEERS")
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .filter_map(|entry| probe(entry, port(cfg)))
        .collect()
}

/// Servers answering the LAN broadcast, plus the configured ones; not this
/// app's own server, which answers too.
pub fn find_peers(cfg: &Config) -> Vec<Peer> {
    let found = discovery::discover(port(cfg), Duration::from_secs(1));
    let configured = configured_peers(cfg);
    distinct(found.into_iter().chain(configured))
        .into_iter()
        .filter(|peer| peer.info.id != instance_id())
        .collect()
}

/// Each server once: one found by broadcast and listed in SHARE_PEERS, or
/// reached at two addresses, has the same id (older servers have none:
/// told apart by address only).
fn distinct(peers: impl IntoIterator<Item = Peer>) -> Vec<Peer> {
    let same =
        |a: &Peer, b: &Peer| a.addr == b.addr || (!a.info.id.is_empty() && a.info.id == b.info.id);
    peers.into_iter().fold(Vec::new(), |mut acc, peer| {
        if !acc.iter().any(|p| same(p, &peer)) {
            acc.push(peer);
        }
        acc
    })
}

pub fn list(addr: SocketAddr) -> Result<Vec<RemoteGame>, String> {
    get_json(&api(addr, "games"))
}

pub fn cover(addr: SocketAddr, id: &str) -> Option<Vec<u8>> {
    let response = get(&game_url(addr, id, "cover"), LIST_TIMEOUT).ok()?;
    read_body(response, MAX_BODY).ok()
}

/// The saves of game `id` on a server sharing saves ([`super::SAVES`]);
/// `None` when it doesn't have the game.
pub fn saves(addr: SocketAddr, id: &str) -> Result<Option<RemoteSaves>, String> {
    let response = match request(&game_url(addr, id, "saves"), LIST_TIMEOUT) {
        Ok(response) => response,
        Err(e) if matches!(*e, ureq::Error::Status(404, _)) => return Ok(None),
        Err(e) => return Err(message_of(*e)),
    };
    let body = read_body(response, MAX_BODY)?;
    serde_json::from_slice(&body)
        .map(Some)
        .map_err(|e| e.to_string())
}

/// Copy `side` of game `id` on the server into `into` here (see
/// [`saver::receive`]); `announced`: the bytes its listing gave. The save
/// folders here that weren't sent, by name.
pub fn copy_saves(
    addr: SocketAddr,
    id: &str,
    side: Side,
    saves: &Saves,
    into: Side,
    announced: u64,
) -> Result<Vec<String>, String> {
    let which = match side {
        Side::Device => "saves/device",
        Side::Backup => "saves/backup",
    };
    let response = get(&game_url(addr, id, which), STREAM_TIMEOUT)?;
    // The game may save between the listing and the copy: room for that,
    // not for a peer filling the disk.
    let limit = announced.saturating_mul(2).max(MAX_BODY);
    saver::receive(saves, into, response.into_reader(), limit)
}

/// Why `id` can't be copied into `games_dir`, if it can't: the game is
/// already here, installed or packaged.
pub fn conflict(games_dir: &Path, id: &str) -> Option<String> {
    if games_dir.join(id).exists() {
        return Some(format!("{id} is already installed"));
    }
    package::package_path(games_dir, id)
        .exists()
        .then(|| format!("{id} is already here as a package"))
}

fn check_free(games_dir: &Path, id: &str) -> Result<(), String> {
    conflict(games_dir, id).map_or(Ok(()), Err)
}

fn stream(addr: SocketAddr, id: &str) -> Result<impl Read, String> {
    Ok(get(&game_url(addr, id, "package"), STREAM_TIMEOUT)?.into_reader())
}

/// Copy a remote game into `games_dir/<id>/`, unpacked as it arrives.
pub fn install(
    addr: SocketAddr,
    id: &str,
    games_dir: &Path,
    progress: &Progress,
) -> Result<PathBuf, String> {
    check_free(games_dir, id)?;
    package::unpack_from(stream(addr, id)?, games_dir, Some(id), progress)
}

/// Writes everything read through it to `file` as well.
struct Tee<R, W> {
    inner: R,
    copy: W,
}

impl<R: Read, W: Write> Read for Tee<R, W> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.copy.write_all(&buf[..n])?;
        Ok(n)
    }
}

/// Copy a remote game as `games_dir/<id>.cartridge.tar`, verified while it
/// downloads (one pass) and renamed into place only once complete.
pub fn download(
    addr: SocketAddr,
    id: &str,
    games_dir: &Path,
    progress: &Progress,
) -> Result<PathBuf, String> {
    check_free(games_dir, id)?;
    let target = package::package_path(games_dir, id);
    let name = target.file_name().unwrap_or_default().to_string_lossy();
    let staging = games_dir.join(package::staging_name(&name));
    let result = (|| {
        let file = File::create(&staging).map_err(|e| format!("{}: {e}", staging.display()))?;
        let mut tee = Tee {
            inner: stream(addr, id)?,
            copy: BufWriter::new(file),
        };
        let manifest = package::verify_from(&mut tee, progress)?;
        if manifest.folder_name != id {
            return Err(format!(
                "Server sent {} instead of {id}",
                manifest.folder_name
            ));
        }
        // The tar reader stops at the end marker; keep the padding after it.
        io::copy(&mut tee, &mut io::sink()).map_err(|e| e.to_string())?;
        tee.copy.flush().map_err(|e| e.to_string())?;
        fs::rename(&staging, &target).map_err(|e| format!("{}: {e}", target.display()))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&staging);
    }
    result.map(|()| target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::app::GAME_DATA_DIR;
    use crate::core::models::{Game, GAME_JSON};
    use crate::core::share::server::tests::local_server;
    use crate::core::testutil::TempDir;

    #[test]
    fn a_body_past_the_limit_is_refused_not_read_whole() {
        let response = |body: &str| ureq::Response::new(200, "OK", body).unwrap();
        assert_eq!(read_body(response("abcd"), 4), Ok(b"abcd".to_vec()));
        assert!(read_body(response("abcde"), 4).is_err());
    }

    /// A server sharing `server/games` with one installed game, "My Game".
    fn sharing(dir: &TempDir) -> crate::core::share::server::Server {
        dir.write("server/games/My Game/data.bin", &[5u8; 40_000]);
        let game = Game::skeleton(&dir.path().join("server/games/My Game"));
        dir.write(
            &format!("server/games/My Game/{GAME_DATA_DIR}/{GAME_JSON}"),
            game.to_json().as_bytes(),
        );
        local_server(&dir.path().join("server/games"))
    }

    fn addr(port: u16) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], port))
    }

    #[test]
    fn a_server_reached_twice_is_listed_once() {
        let peer = |id: &str, port| Peer {
            info: ServerInfo::new("deck".into(), id.into(), port),
            addr: addr(port),
        };
        let peers = distinct([
            peer("a", 1),
            peer("a", 2),
            peer("b", 1),
            peer("", 3),
            peer("", 4),
        ]);
        let ports: Vec<u16> = peers.iter().map(|p| p.addr.port()).collect();
        assert_eq!(ports, [1, 3, 4]);
    }

    #[test]
    fn probes_a_host() {
        let dir = TempDir::new();
        let server = sharing(&dir);
        let peer = probe(&format!("127.0.0.1:{}", server.port()), 1).unwrap();
        assert_eq!(peer.info.name, "test");
        assert!(probe("127.0.0.1:1", 1).is_none());
    }

    #[test]
    fn lists_remote_games() {
        let dir = TempDir::new();
        let server = sharing(&dir);
        let games = list(addr(server.port())).unwrap();
        assert_eq!(games[0].id, "My Game");
        assert!(games[0].installed);
    }

    #[test]
    fn install_unpacks_without_keeping_a_package() {
        let dir = TempDir::new();
        let server = sharing(&dir);
        let games_dir = dir.mkdir("client");
        let progress = Progress::default();
        let dest = install(addr(server.port()), "My Game", &games_dir, &progress).unwrap();
        assert_eq!(fs::read(dest.join("data.bin")).unwrap(), vec![5u8; 40_000]);
        assert!(progress.done() >= 40_000 && progress.done() == progress.total());
        assert_eq!(fs::read_dir(&games_dir).unwrap().count(), 1);
    }

    #[test]
    fn download_keeps_a_valid_package_that_can_be_served_again() {
        let dir = TempDir::new();
        let server = sharing(&dir);
        let games_dir = dir.mkdir("library");
        let package = download(
            addr(server.port()),
            "My Game",
            &games_dir,
            &Progress::default(),
        )
        .unwrap();
        assert_eq!(fs::read_dir(&games_dir).unwrap().count(), 1);
        let unpacked = dir.mkdir("unpacked");
        package::unpack(&package, &unpacked, &Progress::default()).unwrap();

        // The library machine serves the package as is.
        let relay = local_server(&games_dir);
        let games = list(addr(relay.port())).unwrap();
        assert!(!games[0].installed);
        let again = dir.mkdir("again");
        install(addr(relay.port()), "My Game", &again, &Progress::default()).unwrap();
    }

    #[test]
    fn refuses_games_already_here() {
        let dir = TempDir::new();
        let server = sharing(&dir);
        let games_dir = dir.mkdir("client");
        dir.mkdir("client/My Game");
        let err = download(
            addr(server.port()),
            "My Game",
            &games_dir,
            &Progress::default(),
        );
        assert!(err.unwrap_err().contains("already installed"));
        assert_eq!(fs::read_dir(&games_dir).unwrap().count(), 1);
    }

    #[test]
    fn missing_game_leaves_no_partial_file() {
        let dir = TempDir::new();
        let server = sharing(&dir);
        let games_dir = dir.mkdir("client");
        assert!(download(
            addr(server.port()),
            "Nope",
            &games_dir,
            &Progress::default()
        )
        .is_err());
        assert_eq!(fs::read_dir(&games_dir).unwrap().count(), 0);
    }

    /// "My Game" there saving in `there/live`, and here in `live`.
    fn sharing_saves(dir: &TempDir) -> (crate::core::share::server::Server, Saves) {
        let save_path = |root: &str| crate::core::models::SavePath {
            os: String::new(),
            path: dir.path().join(root).to_string_lossy().into_owned(),
        };
        let mut game = Game::skeleton(&dir.path().join("server/games/My Game"));
        game.save_paths = vec![save_path("there/live")];
        dir.write(
            &format!("server/games/My Game/{GAME_DATA_DIR}/{GAME_JSON}"),
            game.to_json().as_bytes(),
        );
        let mut here = Game::skeleton(&dir.path().join("games/My Game"));
        here.resolved_save_paths = vec![dir.path().join("live")];
        let saves = saver::saves_of(&here, &dir.path().join("saves-copies"));
        (local_server(&dir.path().join("server/games")), saves)
    }

    #[test]
    fn copies_a_peers_saves_into_the_saves_here() {
        let dir = TempDir::new();
        dir.write("there/live/slot.sav", b"there");
        dir.write("live/slot.sav", b"here");
        let (server, saves) = sharing_saves(&dir);
        let remote = self::saves(addr(server.port()), "My Game")
            .unwrap()
            .unwrap();
        let bytes = remote.device.unwrap().bytes;

        let missing = copy_saves(
            addr(server.port()),
            "My Game",
            Side::Device,
            &saves,
            Side::Device,
            bytes,
        );
        assert_eq!(missing, Ok(vec![]));
        assert_eq!(
            fs::read_to_string(dir.path().join("live/slot.sav")).unwrap(),
            "there"
        );
        assert_eq!(remote.backup, None);
    }

    #[test]
    fn a_peer_without_the_game_has_no_saves_and_refusals_say_why() {
        let dir = TempDir::new();
        dir.write("there/live/slot.sav", b"there");
        let (server, saves) = sharing_saves(&dir);
        assert_eq!(self::saves(addr(server.port()), "Nope"), Ok(None));

        server.set_playing(Some("My Game".into()));
        let refused = copy_saves(
            addr(server.port()),
            "My Game",
            Side::Device,
            &saves,
            Side::Device,
            5,
        );
        assert!(refused.unwrap_err().contains("running there"));
        assert!(!dir.path().join("live").exists());
    }
}
