//! Current OS/arch tags and executable header parsing (ELF / PE / Mach-O).

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

/// Canonical OS tag stored in `game.json` targets.
pub fn os_tag() -> &'static str {
    match std::env::consts::OS {
        "linux" => "linux",
        "windows" => "windows",
        "macos" => "macos",
        _ => "other",
    }
}

/// Canonical arch tag; 32-bit x86 counts as x64 (x64 covers legacy x86).
pub fn arch_tag() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" | "arm" => "arm64",
        "x86_64" | "x86" => "x64",
        _ => "other",
    }
}

fn elf_arch(machine: u16) -> Option<&'static str> {
    match machine {
        0x03 | 0x3E => Some("x64"),
        0xB7 | 0x28 => Some("arm64"),
        _ => None,
    }
}

fn pe_arch(machine: u16) -> Option<&'static str> {
    match machine {
        0x014C | 0x8664 => Some("x64"),
        0xAA64 => Some("arm64"),
        _ => None,
    }
}

fn macho_arch(cpu: u32) -> Option<&'static str> {
    match cpu {
        7 | 0x0100_0007 => Some("x64"),
        12 | 0x0100_000C => Some("arm64"),
        _ => None,
    }
}

const MACHO_MAGICS: [u32; 2] = [0xFEED_FACE, 0xFEED_FACF];
const FAT_MAGIC: u32 = 0xCAFE_BABE;
const MAX_FAT_ARCHES: usize = 20;

fn u16_le(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

fn u32_le(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

fn u32_be(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(b[at..at + 4].try_into().unwrap())
}

fn read_header(file: &mut File) -> Vec<u8> {
    let mut header = Vec::with_capacity(64);
    let _ = file.take(64).read_to_end(&mut header);
    header
}

fn read_pe_arch(file: &mut File, header: &[u8]) -> Option<&'static str> {
    if header.len() < 64 {
        return None;
    }
    file.seek(SeekFrom::Start(u32_le(header, 60) as u64)).ok()?;
    let mut sig = [0u8; 6];
    file.read_exact(&mut sig).ok()?;
    if &sig[..4] != b"PE\0\0" {
        return None;
    }
    pe_arch(u16_le(&sig, 4))
}

/// Arch of a Mach-O header: `Some(None)` for a universal binary built for
/// several, `None` when not Mach-O. A fat header is also Java's class-file
/// magic: real ones list few arches (the class version reads as thousands).
fn read_macho_arch(header: &[u8]) -> Option<Option<&'static str>> {
    if header.len() < 8 {
        return None;
    }
    if MACHO_MAGICS.contains(&u32_le(header, 0)) {
        return Some(macho_arch(u32_le(header, 4)));
    }
    if MACHO_MAGICS.contains(&u32_be(header, 0)) {
        return Some(macho_arch(u32_be(header, 4)));
    }
    let count = u32_be(header, 4) as usize;
    if u32_be(header, 0) != FAT_MAGIC || count == 0 || count >= MAX_FAT_ARCHES {
        return None;
    }
    // Entries are 20 bytes from offset 8, cputype first: those the header holds.
    let arches: Vec<Option<&'static str>> = crate::core::unique(
        (0..count)
            .map(|i| 8 + i * 20)
            .take_while(|at| at + 4 <= header.len())
            .map(|at| macho_arch(u32_be(header, at))),
    );
    Some(if arches.len() == 1 { arches[0] } else { None })
}

/// What the detector can run, from a file's first bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Program {
    /// ELF (linux), PE (windows) or Mach-O (macos); `arch` is `None` for a
    /// universal Mach-O or a machine we have no tag for.
    Binary {
        os: &'static str,
        arch: Option<&'static str>,
    },
    /// A `#!` script: runs wherever its interpreter does.
    Script,
}

/// The program `path` is, or `None` for anything else (text, data, libraries
/// are told apart by name, not here).
pub fn inspect(path: &Path) -> Option<Program> {
    let mut file = File::open(path).ok()?;
    let header = read_header(&mut file);
    let binary = |os, arch| Some(Program::Binary { os, arch });
    if header.starts_with(b"#!") {
        Some(Program::Script)
    } else if header.starts_with(b"\x7fELF") {
        binary(
            "linux",
            (header.len() >= 20)
                .then(|| elf_arch(u16_le(&header, 18)))
                .flatten(),
        )
    } else if header.starts_with(b"MZ") {
        binary("windows", read_pe_arch(&mut file, &header))
    } else {
        read_macho_arch(&header).and_then(|arch| binary("macos", arch))
    }
}

/// A program this OS runs itself (ELF, Mach-O, script), so it needs the
/// execute bit, unlike a Windows `.exe`, which Proton starts.
pub fn needs_exec_bit(path: &Path) -> bool {
    matches!(
        inspect(path),
        Some(
            Program::Script
                | Program::Binary {
                    os: "linux" | "macos",
                    ..
                }
        )
    )
}

/// Add the execute bits its read bits allow (644 -> 755), like `chmod +x`.
pub fn make_executable(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(path)?.permissions().mode();
        std::fs::set_permissions(
            path,
            std::fs::Permissions::from_mode(mode | (mode & 0o444) >> 2),
        )
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

/// Has an execute permission bit (always true where permissions don't exist).
pub fn has_exec_permission(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        path.exists()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::testutil::TempDir;

    fn binary(os: &'static str, arch: Option<&'static str>) -> Option<Program> {
        Some(Program::Binary { os, arch })
    }

    fn elf(machine: u8) -> Vec<u8> {
        let mut bytes = b"\x7fELF".to_vec();
        bytes.resize(64, 0);
        bytes[18] = machine;
        bytes
    }

    fn pe(machine: u16) -> Vec<u8> {
        let mut bytes = b"MZ".to_vec();
        bytes.resize(64, 0);
        bytes[60] = 64;
        bytes.extend_from_slice(b"PE\0\0");
        bytes.extend_from_slice(&machine.to_le_bytes());
        bytes
    }

    /// A universal Mach-O holding `cpus` (7 = x64, 12 = arm64, ABI64 bit set).
    fn fat_macho(cpus: &[u32]) -> Vec<u8> {
        let mut bytes = FAT_MAGIC.to_be_bytes().to_vec();
        bytes.extend((cpus.len() as u32).to_be_bytes());
        for cpu in cpus {
            bytes.extend((cpu | 0x0100_0000).to_be_bytes());
            bytes.extend([0; 16]);
        }
        bytes
    }

    #[test]
    fn a_binary_tells_its_os_and_arch() {
        let dir = TempDir::new();
        let inspect_bytes = |bytes: &[u8]| inspect(&dir.write("program", bytes));
        let mut macho = 0xFEED_FACFu32.to_le_bytes().to_vec();
        macho.extend(0x0100_000Cu32.to_le_bytes());
        assert_eq!(inspect_bytes(&elf(0x3E)), binary("linux", Some("x64")));
        assert_eq!(inspect_bytes(&pe(0xAA64)), binary("windows", Some("arm64")));
        assert_eq!(inspect_bytes(&macho), binary("macos", Some("arm64")));
        assert_eq!(inspect_bytes(&elf(0xF3)), binary("linux", None));
    }

    #[test]
    fn a_universal_macho_runs_on_every_arch_it_holds() {
        let dir = TempDir::new();
        let inspect_bytes = |bytes: &[u8]| inspect(&dir.write("program", bytes));
        assert_eq!(inspect_bytes(&fat_macho(&[7, 12])), binary("macos", None));
        assert_eq!(
            inspect_bytes(&fat_macho(&[12])),
            binary("macos", Some("arm64"))
        );
    }

    #[test]
    fn a_java_class_file_is_not_a_universal_macho() {
        let dir = TempDir::new();
        let class = [0xCA, 0xFE, 0xBA, 0xBE, 0x00, 0x00, 0x00, 0x34];
        assert_eq!(inspect(&dir.write("Game.class", &class)), None);
    }

    #[test]
    fn scripts_start_with_a_shebang_and_text_is_no_program() {
        let dir = TempDir::new();
        let inspect_bytes = |bytes: &[u8]| inspect(&dir.write("file", bytes));
        assert_eq!(
            inspect_bytes(b"#!/bin/bash\necho hi"),
            Some(Program::Script)
        );
        assert_eq!(inspect_bytes(b"echo hi"), None);
        assert_eq!(inspect_bytes(b""), None);
    }

    #[test]
    fn programs_this_os_runs_itself_need_the_exec_bit() {
        let dir = TempDir::new();
        assert!(needs_exec_bit(&dir.write("game", &elf(0x3E))));
        assert!(needs_exec_bit(&dir.write("start.sh", b"#!/bin/sh")));
        assert!(!needs_exec_bit(&dir.write("game.exe", &pe(0x8664))));
        assert!(!needs_exec_bit(&dir.write("readme.txt", b"hello")));
    }

    #[cfg(unix)]
    #[test]
    fn make_executable_adds_x_where_r_is_set() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new();
        let mode_after = |mode: u32| {
            let path = dir.write("file", b"");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
            make_executable(&path).unwrap();
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777
        };
        assert_eq!(mode_after(0o644), 0o755);
        assert_eq!(mode_after(0o600), 0o700);
    }
}
