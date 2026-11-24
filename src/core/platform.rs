//! Current OS/arch tags and executable header parsing (ELF / PE / Mach-O).

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
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

const ELF_CLASS_32: u8 = 1;
const ELF_LITTLE_ENDIAN: u8 = 1;
const PE_I386: u16 = 0x014C;
const PT_GNU_STACK: u32 = 0x6474_E551;
const PF_X: u32 = 1;

const MACHO_MAGICS: [u32; 2] = [0xFEED_FACE, 0xFEED_FACF];
const FAT_MAGIC: u32 = 0xCAFE_BABE;
const MAX_FAT_ARCHES: usize = 20;

fn u16_le(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

fn u32_le(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

fn u64_le(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

fn u32_be(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(b[at..at + 4].try_into().unwrap())
}

fn read_header(file: &mut File) -> Vec<u8> {
    let mut header = Vec::with_capacity(64);
    let _ = file.take(64).read_to_end(&mut header);
    header
}

fn read_pe_machine(file: &mut File, header: &[u8]) -> Option<u16> {
    if header.len() < 64 {
        return None;
    }
    file.seek(SeekFrom::Start(u32_le(header, 60) as u64)).ok()?;
    let mut sig = [0u8; 6];
    file.read_exact(&mut sig).ok()?;
    if &sig[..4] != b"PE\0\0" {
        return None;
    }
    Some(u16_le(&sig, 4))
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
        binary(
            "windows",
            read_pe_machine(&mut file, &header).and_then(pe_arch),
        )
    } else {
        read_macho_arch(&header).and_then(|arch| binary("macos", arch))
    }
}

/// A 32-bit x86 ELF or PE program. Its arch tag is x64 (x64 machines run
/// it), but a 64-bit build beside it is the one to run: the 32-bit libraries
/// it needs are rarely installed.
pub fn is_32_bit(path: &Path) -> bool {
    let Ok(mut file) = File::open(path) else {
        return false;
    };
    let header = read_header(&mut file);
    if header.starts_with(b"\x7fELF") {
        header.get(4) == Some(&ELF_CLASS_32)
    } else if header.starts_with(b"MZ") {
        read_pe_machine(&mut file, &header) == Some(PE_I386)
    } else {
        false
    }
}

/// Where the flags of an ELF file's `PT_GNU_STACK` header sit, and their
/// value, when they ask for an executable stack. Little-endian only: the
/// x86 and ARM builds games ship.
fn exec_stack_flags(file: &mut File) -> Option<(u64, u32)> {
    let header = read_header(file);
    if !header.starts_with(b"\x7fELF") || header.len() < 64 || header[5] != ELF_LITTLE_ENDIAN {
        return None;
    }
    let (table_at, entry_size, count, flags_in_entry) = match header[4] {
        ELF_CLASS_32 => (
            u32_le(&header, 0x1C) as u64,
            u16_le(&header, 0x2A),
            u16_le(&header, 0x2C),
            24,
        ),
        _ => (
            u64_le(&header, 0x20),
            u16_le(&header, 0x36),
            u16_le(&header, 0x38),
            4,
        ),
    };
    file.seek(SeekFrom::Start(table_at)).ok()?;
    let mut table = Vec::new();
    file.take(entry_size as u64 * count as u64)
        .read_to_end(&mut table)
        .ok()?;
    let stack_entry = table
        .chunks_exact(entry_size.max(1) as usize)
        .position(|entry| entry.len() >= flags_in_entry + 4 && u32_le(entry, 0) == PT_GNU_STACK)?;
    let at = stack_entry * entry_size as usize + flags_in_entry;
    let flags = u32_le(&table, at);
    (flags & PF_X != 0).then_some((table_at + at as u64, flags))
}

/// An ELF library asking for an executable stack, which glibc 2.41+ refuses
/// to load ("cannot enable executable stack"): FMOD and old Unity Mono builds.
pub fn has_exec_stack(path: &Path) -> bool {
    File::open(path).is_ok_and(|mut file| exec_stack_flags(&mut file).is_some())
}

/// Drop the executable-stack request, like `patchelf --clear-execstack`:
/// none of these libraries runs code from its stack.
pub fn clear_exec_stack(path: &Path) -> std::io::Result<()> {
    let mut file = File::options().read(true).write(true).open(path)?;
    let Some((at, flags)) = exec_stack_flags(&mut file) else {
        return Ok(());
    };
    file.seek(SeekFrom::Start(at))?;
    file.write_all(&(flags & !PF_X).to_le_bytes())
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
    use crate::core::testutil::{elf_library, TempDir};

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

    #[test]
    fn a_library_asking_for_an_executable_stack_is_found_in_32_and_64_bit() {
        let dir = TempDir::new();
        assert!(has_exec_stack(
            &dir.write("lib64/libfmod.so.10", &elf_library(2, 7))
        ));
        assert!(has_exec_stack(
            &dir.write("lib/libfmod.so.10", &elf_library(1, 7))
        ));
        assert!(!has_exec_stack(
            &dir.write("libSDL2.so", &elf_library(2, 6))
        ));
        assert!(!has_exec_stack(&dir.write("notes.so", b"not a library")));
    }

    #[test]
    fn clearing_the_executable_stack_keeps_read_and_write() {
        let dir = TempDir::new();
        for class in [1, 2] {
            let library = dir.write("libfmod.so.10", &elf_library(class, 7));
            clear_exec_stack(&library).unwrap();
            assert!(!has_exec_stack(&library));
            assert_eq!(std::fs::read(&library).unwrap(), elf_library(class, 6));
        }
    }

    #[test]
    fn a_32_bit_x86_program_is_told_apart_from_a_64_bit_one() {
        let dir = TempDir::new();
        let mut elf_32 = elf(0x03);
        elf_32[4] = 1;
        let mut elf_64 = elf(0x3E);
        elf_64[4] = 2;
        assert!(is_32_bit(&dir.write("Game.bin.x86", &elf_32)));
        assert!(!is_32_bit(&dir.write("Game.bin.x86_64", &elf_64)));
        assert!(is_32_bit(&dir.write("Game32.exe", &pe(0x014C))));
        assert!(!is_32_bit(&dir.write("Game.exe", &pe(0x8664))));
        assert!(!is_32_bit(&dir.write("start.sh", b"#!/bin/sh")));
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
