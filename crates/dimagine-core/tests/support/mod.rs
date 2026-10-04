//! Shared helpers for the integration tests: throwaway libraries under
//! `CARGO_TARGET_TMPDIR` (inside `target/`, gitignored, removed on drop) and
//! the committed tiny fixtures under `tests/fixtures/`.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use dimagine_core::Library;

pub struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

impl TempDir {
    pub fn path(&self) -> &Path {
        &self.0
    }
}

/// A fresh empty library folder. Falls back to the system temp dir when the
/// target temp dir is not provided (e.g. IDE test runners).
pub fn tmp(tag: &str) -> TempDir {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let name = format!("dimagine-{}-{}-{n}.tmp", tag, std::process::id());
    let dir = match std::env::var("CARGO_TARGET_TMPDIR") {
        Ok(target_short) => PathBuf::from(target_short).join(name),
        Err(_) => std::env::temp_dir().join(name),
    };
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create test library dir");
    TempDir(dir)
}

/// Write a file (creating parent folders) with bytes or text.
pub fn write(root: &Path, rel: &str, content: impl AsRef<[u8]>) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, content).unwrap_or_else(|err| panic!("write {rel}: {err}"));
}

/// Write a file at a native (possibly non-UTF-8) library-relative path.
#[cfg(unix)]
pub fn write_os(root: &Path, rel: &[u8], content: impl AsRef<[u8]>) {
    use std::os::unix::ffi::OsStrExt;
    let path = root.join(std::ffi::OsStr::from_bytes(rel));
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(&path, content).unwrap_or_else(|err| panic!("write: {err}"));
}

pub fn fixture_bytes(name: &str) -> Vec<u8> {
    let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures");
    std::fs::read(base.join(name)).unwrap_or_else(|err| panic!("read fixture {name}: {err}"))
}

pub fn png() -> Vec<u8> {
    fixture_bytes("pixel.png")
}

pub fn jpg() -> Vec<u8> {
    fixture_bytes("pixel.jpg")
}

pub fn gif() -> Vec<u8> {
    fixture_bytes("pixel.gif")
}

pub fn bmp() -> Vec<u8> {
    fixture_bytes("pixel.bmp")
}

pub fn tiff() -> Vec<u8> {
    fixture_bytes("pixel.tiff")
}

pub fn webp() -> Vec<u8> {
    fixture_bytes("pixel.webp")
}

pub fn avif() -> Vec<u8> {
    fixture_bytes("pixel.avif")
}

pub fn heic() -> Vec<u8> {
    fixture_bytes("pixel.heic")
}

pub fn open_library(dir: &Path) -> Library {
    Library::open(dir).unwrap_or_else(|err| panic!("open library: {err}"))
}
