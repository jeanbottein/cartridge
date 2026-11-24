//! LAN sharing: a read-only HTTP server that lists and streams games as
//! packages, plus UDP broadcast discovery. No auth: meant for a home LAN.

pub mod client;
pub mod discovery;
pub mod server;

use std::cmp::Ordering;

use serde::{Deserialize, Serialize};

use super::config::Config;
use super::models::{Game, ImageSlot, Target};

/// Bumped on incompatible API changes; peers with another one are shown,
/// with which side to update, but not listed.
/// 2: renamed to •cartridge (the discovery query changed too).
pub const PROTOCOL: u32 = 2;
pub const DEFAULT_PORT: u16 = 47800;

/// `SHARE_PORT`, else the default.
pub fn port(cfg: &Config) -> u16 {
    cfg.get("SHARE_PORT")
        .and_then(|p| p.trim().parse().ok())
        .unwrap_or(DEFAULT_PORT)
}

/// Name shown to other machines: the host name.
pub fn machine_name() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .ok()
        .or_else(|| {
            let out = std::process::Command::new("hostname").output().ok()?;
            String::from_utf8(out.stdout).ok()
        })
        .map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "cartridge".into())
}

/// One game as listed by a server (`GET /api/v1/games`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RemoteGame {
    /// Folder name; also the id in URLs.
    pub id: String,
    pub title: String,
    pub year: Option<u32>,
    pub players: String,
    pub description: String,
    /// Bytes of the game's own files.
    pub size: Option<u64>,
    pub targets: Vec<Target>,
    pub installed: bool,
    pub has_cover: bool,
}

impl From<&Game> for RemoteGame {
    fn from(game: &Game) -> RemoteGame {
        RemoteGame {
            id: game.folder_name.clone(),
            title: game.title.clone(),
            year: game.year,
            players: game.players.clone(),
            description: game.description.clone(),
            size: game.size,
            targets: game.targets.clone(),
            installed: game.is_installed(),
            has_cover: game.images.get(ImageSlot::Cover).is_some(),
        }
    }
}

/// `GET /api/v1/info` and discovery replies.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerInfo {
    pub name: String,
    pub port: u16,
    pub version: String,
    pub protocol: u32,
}

impl ServerInfo {
    pub fn new(name: String, port: u16) -> ServerInfo {
        ServerInfo {
            name,
            port,
            version: super::self_update::VERSION.into(),
            protocol: PROTOCOL,
        }
    }

    /// Why this machine and the server can't share, if they can't.
    pub fn mismatch(&self) -> Option<String> {
        let which = match self.protocol.cmp(&PROTOCOL) {
            Ordering::Equal => return None,
            Ordering::Greater => "a newer •cartridge: update this one",
            Ordering::Less => "an older •cartridge: update it",
        };
        Some(format!("{} runs {which}", self.name))
    }
}

/// Percent-encode a URL path segment.
pub fn encode_segment(text: &str) -> String {
    text.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b'.' | b'-' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// Decode a percent-encoded URL path segment; `None` when malformed.
pub fn decode_segment(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    // A loop: each step consumes one or three bytes.
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_server_of_another_protocol_says_which_side_to_update() {
        let mut info = ServerInfo::new("deck".into(), 1);
        assert_eq!(info.mismatch(), None);
        info.protocol = PROTOCOL + 1;
        assert!(info.mismatch().unwrap().contains("update this one"));
        info.protocol = PROTOCOL - 1;
        assert!(info.mismatch().unwrap().contains("update it"));
    }

    #[test]
    fn segments_round_trip() {
        let name = "Other Game: Part/Two é";
        let encoded = encode_segment(name);
        assert!(!encoded.contains(' ') && !encoded.contains('/'));
        assert_eq!(decode_segment(&encoded).as_deref(), Some(name));
        assert_eq!(decode_segment("bad%2"), None);
        assert_eq!(decode_segment("bad%zz"), None);
    }

    #[test]
    fn port_falls_back_to_default() {
        let cfg = Config::from_raw("/ws/system/config.txt".into(), Default::default());
        assert_eq!(port(&cfg), DEFAULT_PORT);
    }
}
