//! Read-only HTTP/1.1 server (GET only, one thread per connection):
//!
//!   GET /api/v1/info               ServerInfo
//!   GET /api/v1/games              [RemoteGame]
//!   GET /api/v1/games/{id}/cover   cover image
//!   GET /api/v1/games/{id}/package package tar, streamed
//!   GET /api/v1/games/{id}/saves   RemoteSaves
//!   GET /api/v1/games/{id}/saves/device|backup   saves tar, streamed
//!
//! Games are looked up by folder name only; a client path never reaches the
//! filesystem. The live saves of the game running here are not sent: the
//! game may be writing them. Installed games are packed on the fly (no temporary file);
//! packaged-only games send their tar as is.

use std::fs::File;
use std::io::{self, BufRead, BufReader, BufWriter, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use log::{debug, info, warn};
use serde::Serialize;

use super::{decode_segment, discovery, RemoteGame, RemoteSaves, ServerInfo};
use crate::core::models::{Game, ImageSlot};
use crate::core::package::{self, Progress};
use crate::core::saver::{self, Side};
use crate::core::scanner;

const MAX_HEAD: u64 = 8 * 1024;
const READ_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a library scan answers requests: a peer browsing asks for the
/// list then every cover, and a scan reads every game.json and package.
const LIBRARY_TTL: Duration = Duration::from_secs(5);

/// What a request asks for.
#[derive(Debug, PartialEq, Eq)]
enum Route {
    Info,
    Games,
    Cover(String),
    Package(String),
    Saves(String),
    SaveCopy(String, Side),
}

fn side(name: &str) -> Option<Side> {
    match name {
        "device" => Some(Side::Device),
        "backup" => Some(Side::Backup),
        _ => None,
    }
}

/// Parse a request line (`GET /path HTTP/1.1`) into a route.
fn route(request_line: &str) -> Result<Route, (u16, &'static str)> {
    let mut parts = request_line.split_whitespace();
    let (Some(method), Some(target)) = (parts.next(), parts.next()) else {
        return Err((400, "Bad request"));
    };
    if method != "GET" {
        return Err((405, "Only GET is supported"));
    }
    let path = target.split('?').next().unwrap_or_default();
    let segments: Vec<&str> = path.trim_matches('/').split('/').collect();
    match segments.as_slice() {
        ["api", "v1", "info"] => Ok(Route::Info),
        ["api", "v1", "games"] => Ok(Route::Games),
        ["api", "v1", "games", id, "cover"] => decode_segment(id)
            .map(Route::Cover)
            .ok_or((400, "Bad game id")),
        ["api", "v1", "games", id, "package"] => decode_segment(id)
            .map(Route::Package)
            .ok_or((400, "Bad game id")),
        ["api", "v1", "games", id, "saves"] => decode_segment(id)
            .map(Route::Saves)
            .ok_or((400, "Bad game id")),
        ["api", "v1", "games", id, "saves", which] => {
            let side = side(which).ok_or((404, "Not found"))?;
            decode_segment(id)
                .map(|id| Route::SaveCopy(id, side))
                .ok_or((400, "Bad game id"))
        }
        _ => Err((404, "Not found")),
    }
}

/// Shared state of a running server.
pub struct Shared {
    games_dir: PathBuf,
    saves_root: PathBuf,
    info: ServerInfo,
    /// Folder name of the game running on this machine.
    playing: Mutex<Option<String>>,
    stop: AtomicBool,
    /// Packages being sent right now.
    pub sending: AtomicUsize,
    /// The last scan and when it was made.
    library: Mutex<Option<(Instant, Arc<Vec<Game>>)>>,
}

/// A running server; stops when dropped (and frees its port).
pub struct Server {
    shared: Arc<Shared>,
    addr: SocketAddr,
    threads: Vec<JoinHandle<()>>,
}

/// The HTTP listener on `port` and the discovery socket on the same number
/// (`None` when UDP has it taken: sharing works, discovery doesn't). Any
/// free port (0) is only checked free for TCP: another is drawn until UDP
/// has it free too.
fn bind(ip: IpAddr, port: u16) -> io::Result<(TcpListener, Option<UdpSocket>)> {
    const DRAWS: usize = 20;
    let mut draws = 1;
    // A loop: each draw is a new free port.
    loop {
        let listener = TcpListener::bind((ip, port))?;
        let number = listener.local_addr()?.port();
        match UdpSocket::bind((ip, number)) {
            Ok(udp) => return Ok((listener, Some(udp))),
            Err(_) if port == 0 && draws < DRAWS => draws += 1,
            Err(e) => {
                warn!("Discovery disabled (UDP port {number}): {e}");
                return Ok((listener, None));
            }
        }
    }
}

impl Server {
    /// Listen on `port` (TCP for HTTP, UDP for discovery) on every interface.
    pub fn start(shelf: &Shelf, port: u16) -> io::Result<Server> {
        Server::start_on(shelf, IpAddr::V4(Ipv4Addr::UNSPECIFIED), port)
    }

    fn start_on(shelf: &Shelf, ip: IpAddr, port: u16) -> io::Result<Server> {
        let (listener, udp) = bind(ip, port)?;
        let addr = listener.local_addr()?;
        let info = ServerInfo::new(shelf.name.clone(), shelf.id.clone(), addr.port());
        let shared = Arc::new(Shared::new(shelf, info));
        let accept_shared = Arc::clone(&shared);
        let mut threads = vec![std::thread::spawn(move || {
            accept_loop(listener, accept_shared)
        })];
        let protocols = if udp.is_some() {
            "TCP + UDP"
        } else {
            "TCP only, no discovery"
        };
        if let Some(socket) = udp {
            let udp_shared = Arc::clone(&shared);
            threads.push(std::thread::spawn(move || {
                discovery::answer_loop(socket, udp_shared)
            }));
        }
        info!(
            "Sharing {} and {} as \"{}\" on port {} ({protocols})",
            shelf.games_dir.display(),
            shelf.saves_root.display(),
            shared.info.name,
            addr.port()
        );
        Ok(Server {
            shared,
            addr,
            threads,
        })
    }

    pub fn port(&self) -> u16 {
        self.addr.port()
    }

    pub fn games_dir(&self) -> &Path {
        &self.shared.games_dir
    }

    pub fn sending(&self) -> usize {
        self.shared.sending.load(Ordering::Relaxed)
    }

    /// The game running on this machine (its folder name), if any.
    pub fn set_playing(&self, game: Option<String>) {
        *self
            .shared
            .playing
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = game;
    }
}

/// What a server shares, and under which name and id.
pub struct Shelf {
    pub games_dir: PathBuf,
    pub saves_root: PathBuf,
    pub name: String,
    pub id: String,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        // Wake the blocking accept so its loop sees the flag; the UDP loop
        // wakes on its read timeout. Joining frees the ports for a restart.
        let _ = TcpStream::connect((Ipv4Addr::LOCALHOST, self.addr.port()));
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

impl Shared {
    fn new(shelf: &Shelf, info: ServerInfo) -> Shared {
        Shared {
            games_dir: shelf.games_dir.clone(),
            saves_root: shelf.saves_root.clone(),
            info,
            playing: Mutex::new(None),
            stop: AtomicBool::new(false),
            sending: AtomicUsize::new(0),
            library: Mutex::new(None),
        }
    }

    pub fn info(&self) -> &ServerInfo {
        &self.info
    }

    pub fn stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    fn is_playing(&self, game: &Game) -> bool {
        let playing = self.playing.lock().unwrap_or_else(|e| e.into_inner());
        playing.as_deref() == Some(game.folder_name.as_str())
    }

    pub fn library(&self) -> Arc<Vec<Game>> {
        self.library_at(Instant::now())
    }

    /// The cached scan while fresh at `now`, else a new one. The lock is held
    /// while scanning, so simultaneous requests share one scan.
    fn library_at(&self, now: Instant) -> Arc<Vec<Game>> {
        let mut cache = self.library.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((_, games)) = cache.as_ref().filter(|(at, _)| now < *at + LIBRARY_TTL) {
            return Arc::clone(games);
        }
        let games = Arc::new(scanner::load_library(&self.games_dir));
        *cache = Some((now, Arc::clone(&games)));
        games
    }
}

fn accept_loop(listener: TcpListener, shared: Arc<Shared>) {
    for stream in listener.incoming() {
        if shared.stopped() {
            break;
        }
        let Ok(stream) = stream else { continue };
        let shared = Arc::clone(&shared);
        std::thread::spawn(move || {
            let peer = stream.peer_addr().ok();
            if let Err(e) = handle(stream, &shared) {
                debug!("Share connection {peer:?}: {e}");
            }
        });
    }
}

/// Read the request line, skip the headers (bounded), answer.
fn handle(stream: TcpStream, shared: &Shared) -> io::Result<()> {
    stream.set_read_timeout(Some(READ_TIMEOUT))?;
    let peer = stream
        .peer_addr()
        .map_or("a peer".to_string(), |a| a.ip().to_string());
    let mut reader = BufReader::new(stream.try_clone()?).take(MAX_HEAD);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    let mut header = String::new();
    // Headers carry nothing we use; read them so the client isn't reset.
    while reader.read_line(&mut header)? > 2 {
        header.clear();
    }
    let mut out = BufWriter::new(stream);
    match route(&request_line) {
        Ok(route) => answer(route, shared, &peer, &mut out),
        Err((status, message)) => send_error(&mut out, status, message),
    }?;
    out.flush()
}

fn find(shared: &Shared, id: &str) -> Option<Game> {
    shared
        .library()
        .iter()
        .find(|g| g.folder_name == id)
        .cloned()
}

/// `peer`: who asks, for the log.
fn answer(route: Route, shared: &Shared, peer: &str, out: &mut impl Write) -> io::Result<()> {
    match route {
        Route::Info => send_json(out, shared.info()),
        Route::Games => {
            let games: Vec<RemoteGame> = shared.library().iter().map(RemoteGame::from).collect();
            send_json(out, &games)
        }
        Route::Cover(id) => match find(shared, &id).and_then(|g| cover_of(&g)) {
            Some((bytes, mime)) => send(out, 200, mime, &bytes),
            None => send_error(out, 404, "No such cover"),
        },
        Route::Package(id) => match find(shared, &id) {
            Some(game) => {
                info!("Sending {} to {peer}", game.title);
                shared.sending.fetch_add(1, Ordering::Relaxed);
                let result = send_package(out, &game);
                shared.sending.fetch_sub(1, Ordering::Relaxed);
                info!(
                    "Sent {}{}",
                    game.title,
                    result
                        .as_ref()
                        .err()
                        .map_or(String::new(), |e| format!(" (stopped: {e})"))
                );
                result
            }
            None => send_error(out, 404, "No such game"),
        },
        Route::Saves(id) => match find(shared, &id) {
            Some(game) => send_json(out, &remote_saves(shared, &game)),
            None => send_error(out, 404, "No such game"),
        },
        Route::SaveCopy(id, side) => match find(shared, &id) {
            Some(game) => send_saves(out, shared, &game, side, peer),
            None => send_error(out, 404, "No such game"),
        },
    }
}

const PLAYING: &str = "The game is running there: its saves may be changing.";

fn remote_saves(shared: &Shared, game: &Game) -> RemoteSaves {
    let saves = saver::saves_of(game, &shared.saves_root);
    let playing = shared.is_playing(game);
    RemoteSaves {
        device: saver::offered(&saves, Side::Device).filter(|_| !playing),
        backup: saver::offered(&saves, Side::Backup),
        playing,
    }
}

fn send_saves(
    out: &mut impl Write,
    shared: &Shared,
    game: &Game,
    side: Side,
    peer: &str,
) -> io::Result<()> {
    if side == Side::Device && shared.is_playing(game) {
        return send_error(out, 409, PLAYING);
    }
    let saves = saver::saves_of(game, &shared.saves_root);
    if saver::offered(&saves, side).is_none() {
        return send_error(out, 404, "No saves");
    }
    let what = match side {
        Side::Device => "saves",
        Side::Backup => "save backup",
    };
    info!("Sending the {what} of {} to {peer}", game.title);
    write_head(out, 200, "application/x-tar", None)?;
    saver::send(&saves, side, out)
}

fn mime_of(name: &str) -> &'static str {
    match Path::new(name).extension().and_then(|e| e.to_str()) {
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("ico") => "image/x-icon",
        Some("webp") => "image/webp",
        _ => "image/png",
    }
}

fn cover_of(game: &Game) -> Option<(Vec<u8>, &'static str)> {
    let name = game.images.get(ImageSlot::Cover)?;
    let bytes = match game.package().filter(|_| !game.is_installed()) {
        Some(package) => package::read_data_file(package, name)?,
        None => std::fs::read(game.image_path(ImageSlot::Cover)?).ok()?,
    };
    Some((bytes, mime_of(name)))
}

/// The folder when installed (it is what plays), else the package file.
fn send_package(out: &mut impl Write, game: &Game) -> io::Result<()> {
    if let Some(package) = game.package().filter(|_| !game.is_installed()) {
        let mut file = File::open(package)?;
        let length = file.metadata()?.len();
        write_head(out, 200, "application/x-tar", Some(length))?;
        io::copy(&mut file, out)?;
        return Ok(());
    }
    write_head(out, 200, "application/x-tar", None)?;
    package::pack_to(game, &mut *out, &Progress::default())
        .map(|_| ())
        .map_err(io::Error::other)
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        _ => "Error",
    }
}

/// Status line and headers. Without a length the body ends when the
/// connection closes; the package's trailing checksums catch a cut stream.
fn write_head(
    out: &mut impl Write,
    status: u16,
    mime: &str,
    length: Option<u64>,
) -> io::Result<()> {
    write!(
        out,
        "HTTP/1.1 {status} {}\r\nContent-Type: {mime}\r\n",
        reason(status)
    )?;
    if let Some(length) = length {
        write!(out, "Content-Length: {length}\r\n")?;
    }
    write!(out, "Connection: close\r\n\r\n")
}

fn send(out: &mut impl Write, status: u16, mime: &str, body: &[u8]) -> io::Result<()> {
    write_head(out, status, mime, Some(body.len() as u64))?;
    out.write_all(body)
}

fn send_json<T: Serialize>(out: &mut impl Write, value: &T) -> io::Result<()> {
    let body = serde_json::to_vec(value).map_err(io::Error::other)?;
    send(out, 200, "application/json", &body)
}

fn send_error(out: &mut impl Write, status: u16, message: &str) -> io::Result<()> {
    let body = serde_json::json!({ "error": message }).to_string();
    send(out, status, "application/json", body.as_bytes())
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::core::app::GAME_DATA_DIR;
    use crate::core::models::GAME_JSON;
    use crate::core::share::encode_segment;
    use crate::core::testutil::TempDir;

    #[test]
    fn the_library_is_scanned_once_per_burst_of_requests() {
        let dir = TempDir::new();
        dir.mkdir("games/One");
        let shelf = shelf(&dir.path().join("games"));
        let shared = Shared::new(&shelf, ServerInfo::new("t".into(), "id".into(), 1));
        let start = Instant::now();
        assert_eq!(shared.library_at(start).len(), 1);

        dir.mkdir("games/Two");
        assert_eq!(shared.library_at(start + Duration::from_secs(1)).len(), 1);
        assert_eq!(shared.library_at(start + LIBRARY_TTL).len(), 2);
    }

    #[test]
    fn routes_only_known_get_paths() {
        assert_eq!(route("GET /api/v1/games HTTP/1.1"), Ok(Route::Games));
        assert_eq!(
            route("GET /api/v1/games/My%20Game/package?x=1 HTTP/1.1"),
            Ok(Route::Package("My Game".into()))
        );
        assert_eq!(
            route("GET /api/v1/games/My%20Game/saves/backup HTTP/1.1"),
            Ok(Route::SaveCopy("My Game".into(), Side::Backup))
        );
        assert_eq!(
            route("GET /api/v1/games/x/saves/other HTTP/1.1")
                .unwrap_err()
                .0,
            404
        );
        assert_eq!(route("POST /api/v1/games HTTP/1.1").unwrap_err().0, 405);
        assert_eq!(route("GET /etc/passwd HTTP/1.1").unwrap_err().0, 404);
        assert_eq!(route("").unwrap_err().0, 400);
    }

    #[test]
    fn any_free_port_is_free_for_discovery_too() {
        let localhost = IpAddr::V4(Ipv4Addr::LOCALHOST);
        for _ in 0..50 {
            let (tcp, udp) = bind(localhost, 0).unwrap();
            let udp = udp.expect("a UDP socket on the drawn port");
            assert_eq!(
                udp.local_addr().unwrap().port(),
                tcp.local_addr().unwrap().port()
            );
        }
    }

    #[test]
    fn a_fixed_port_taken_for_udp_disables_only_discovery() {
        let localhost = IpAddr::V4(Ipv4Addr::LOCALHOST);
        // A UDP port whose number is free for TCP (another is drawn when TCP
        // has it too); `_taken` holds it until the checks are done.
        let (_taken, port, tcp, udp) = (0..20)
            .find_map(|_| {
                let taken = UdpSocket::bind((localhost, 0)).unwrap();
                let port = taken.local_addr().unwrap().port();
                let (tcp, udp) = bind(localhost, port).ok()?;
                Some((taken, port, tcp, udp))
            })
            .expect("a UDP port whose number is free for TCP");
        assert_eq!(tcp.local_addr().unwrap().port(), port);
        assert!(udp.is_none());
    }

    /// `games_dir`, with the backups next to it.
    fn shelf(games_dir: &Path) -> Shelf {
        Shelf {
            games_dir: games_dir.to_path_buf(),
            saves_root: games_dir.with_file_name("saves-copies"),
            name: "test".into(),
            id: "test-id".into(),
        }
    }

    /// Loopback only, so tests never open a port on the network.
    pub fn local_server(games_dir: &Path) -> Server {
        Server::start_on(&shelf(games_dir), IpAddr::V4(Ipv4Addr::LOCALHOST), 0).unwrap()
    }

    fn get(port: u16, path: &str) -> ureq::Response {
        ureq::get(&format!("http://127.0.0.1:{port}{path}"))
            .call()
            .unwrap()
    }

    #[test]
    fn lists_and_streams_installed_games() {
        let dir = TempDir::new();
        dir.write("games/My Game/data.bin", &[3u8; 70_000]);
        let mut game = Game::skeleton(&dir.path().join("games/My Game"));
        game.year = Some(1999);
        dir.write(
            &format!("games/My Game/{GAME_DATA_DIR}/{GAME_JSON}"),
            game.to_json().as_bytes(),
        );
        let server = local_server(&dir.path().join("games"));

        let games: Vec<RemoteGame> =
            serde_json::from_reader(get(server.port(), "/api/v1/games").into_reader()).unwrap();
        assert_eq!(games.len(), 1);
        assert_eq!(
            (games[0].id.as_str(), games[0].year),
            ("My Game", Some(1999))
        );

        let path = format!("/api/v1/games/{}/package", encode_segment("My Game"));
        let target = dir.mkdir("client");
        let dest = package::unpack_from(
            get(server.port(), &path).into_reader(),
            &target,
            Some("My Game"),
            &Progress::default(),
        )
        .unwrap();
        assert_eq!(
            std::fs::read(dest.join("data.bin")).unwrap(),
            vec![3u8; 70_000]
        );
        assert_eq!(server.sending(), 0);
    }

    #[test]
    fn unknown_games_are_404() {
        let dir = TempDir::new();
        let server = local_server(dir.path());
        let err = ureq::get(&format!(
            "http://127.0.0.1:{}/api/v1/games/x/package",
            server.port()
        ))
        .call()
        .unwrap_err();
        assert!(matches!(err, ureq::Error::Status(404, _)));
    }

    #[test]
    fn restarts_on_the_same_port() {
        let dir = TempDir::new();
        let port = local_server(dir.path()).port();
        let again = Server::start_on(&shelf(dir.path()), IpAddr::V4(Ipv4Addr::LOCALHOST), port);
        assert_eq!(again.unwrap().port(), port);
    }

    /// `games/My Game` saving in `live/`, and the server sharing it.
    fn sharing_saves(dir: &TempDir) -> Server {
        let mut game = Game::skeleton(&dir.path().join("games/My Game"));
        game.save_paths = vec![crate::core::models::SavePath {
            os: String::new(),
            path: dir.path().join("live").to_string_lossy().into_owned(),
        }];
        dir.write(
            &format!("games/My Game/{GAME_DATA_DIR}/{GAME_JSON}"),
            game.to_json().as_bytes(),
        );
        local_server(&dir.path().join("games"))
    }

    fn saves_of(port: u16) -> RemoteSaves {
        let path = format!("/api/v1/games/{}/saves", encode_segment("My Game"));
        serde_json::from_reader(get(port, &path).into_reader()).unwrap()
    }

    fn status(port: u16, path: &str) -> u16 {
        match ureq::get(&format!("http://127.0.0.1:{port}{path}")).call() {
            Ok(response) => response.status(),
            Err(ureq::Error::Status(code, _)) => code,
            Err(e) => panic!("{e}"),
        }
    }

    #[test]
    fn a_games_saves_and_backup_are_listed_and_sent() {
        let dir = TempDir::new();
        dir.write("live/slot.sav", b"live");
        dir.write("saves-copies/My Game/live/slot.sav", b"backup");
        let server = sharing_saves(&dir);

        let saves = saves_of(server.port());
        assert_eq!(saves.device.map(|s| s.files), Some(1));
        assert_eq!(saves.backup.map(|s| s.files), Some(1));
        assert!(!saves.playing);
        let path = format!("/api/v1/games/{}/saves/backup", encode_segment("My Game"));
        let mut archive = tar::Archive::new(get(server.port(), &path).into_reader());
        let names: Vec<String> = archive
            .entries()
            .unwrap()
            .map(|e| e.unwrap().path().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["live", "live/slot.sav"]);
    }

    #[test]
    fn the_saves_of_the_game_running_here_are_not_sent() {
        let dir = TempDir::new();
        dir.write("live/slot.sav", b"live");
        dir.write("saves-copies/My Game/live/slot.sav", b"backup");
        let server = sharing_saves(&dir);
        server.set_playing(Some("My Game".into()));

        let saves = saves_of(server.port());
        assert!(saves.playing && saves.device.is_none() && saves.backup.is_some());
        let base = format!("/api/v1/games/{}/saves", encode_segment("My Game"));
        assert_eq!(status(server.port(), &format!("{base}/device")), 409);
        assert_eq!(status(server.port(), &format!("{base}/backup")), 200);
        server.set_playing(None);
        assert_eq!(status(server.port(), &format!("{base}/device")), 200);
    }

    #[test]
    fn missing_saves_and_games_are_404() {
        let dir = TempDir::new();
        let server = sharing_saves(&dir);
        let base = format!("/api/v1/games/{}/saves", encode_segment("My Game"));
        assert_eq!(status(server.port(), &format!("{base}/device")), 404);
        assert_eq!(status(server.port(), "/api/v1/games/Nope/saves"), 404);
        assert_eq!(
            status(server.port(), "/api/v1/games/..%2F..%2Fetc/saves/device"),
            404
        );
    }
}
