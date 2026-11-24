//! Throwaway directories for filesystem tests.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new() -> TempDir {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("cartridge-test-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    /// Write `content` at `rel` (parents created) and return its path.
    pub fn write(&self, rel: &str, content: &[u8]) -> PathBuf {
        let path = self.0.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, content).unwrap();
        path
    }

    pub fn mkdir(&self, rel: &str) -> PathBuf {
        let path = self.0.join(rel);
        std::fs::create_dir_all(&path).unwrap();
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A little-endian x86 ELF library (`class` 1 = 32-bit, 2 = 64-bit) whose one
/// program header is `PT_GNU_STACK` with `flags` (7 = RWX, 6 = RW).
pub fn elf_library(class: u8, flags: u32) -> Vec<u8> {
    const PT_GNU_STACK: u32 = 0x6474_E551;
    let is_32 = class == 1;
    let (header_size, entry_size, flags_in_entry) = if is_32 { (52, 32, 24) } else { (64, 56, 4) };
    let mut bytes = b"\x7fELF".to_vec();
    bytes.extend([class, 1]);
    bytes.resize(header_size, 0);
    bytes[16] = 3; // ET_DYN
    bytes[18] = if is_32 { 0x03 } else { 0x3E };
    let count_at = if is_32 {
        bytes[0x1C..0x20].copy_from_slice(&(header_size as u32).to_le_bytes());
        0x2A
    } else {
        bytes[0x20..0x28].copy_from_slice(&(header_size as u64).to_le_bytes());
        0x36
    };
    bytes[count_at..count_at + 2].copy_from_slice(&(entry_size as u16).to_le_bytes());
    bytes[count_at + 2..count_at + 4].copy_from_slice(&1u16.to_le_bytes());
    let mut entry = vec![0; entry_size];
    entry[..4].copy_from_slice(&PT_GNU_STACK.to_le_bytes());
    entry[flags_in_entry..flags_in_entry + 4].copy_from_slice(&flags.to_le_bytes());
    bytes.extend(entry);
    bytes
}
