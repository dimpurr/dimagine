use dimagine_serve::{router, serve, CachedPreview, FsCatalog, ServeConfig};
use std::{
    env, fs,
    net::SocketAddr,
    path::{Path, PathBuf},
    process::ExitCode,
};

fn is_image(path: &Path) -> bool {
    const EXTS: &[&str] = &[
        "jpg", "jpeg", "png", "gif", "webp", "avif", "heic", "heif", "tif", "tiff", "bmp",
    ];
    path.extension()
        .and_then(|x| x.to_str())
        .is_some_and(|x| EXTS.contains(&x.to_ascii_lowercase().as_str()))
}

fn ignored_name(s: &str) -> bool {
    s.starts_with('.')
        || s.starts_with("._")
        || s == "Thumbs.db"
        || s.eq_ignore_ascii_case("desktop.ini")
}

/// Walk the library tree, returning image paths and the directories
/// that could not be read. An unreadable directory is reported, not
/// silently skipped: its images are unknown, not absent.
fn find_all_images(root: &Path) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let mut images = Vec::new();
    let mut unreadable = Vec::new();
    let mut queue = vec![root.to_path_buf()];
    while let Some(dir) = queue.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            unreadable.push(dir);
            continue;
        };
        for entry in entries.flatten() {
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_symlink() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if ignored_name(&name) {
                continue;
            }
            let path = entry.path();
            if ft.is_dir() {
                queue.push(path);
            } else if ft.is_file() && is_image(&path) {
                images.push(path);
            }
        }
    }
    images.sort();
    unreadable.sort();
    (images, unreadable)
}

fn kind_cache_name(kind: dimagine_preview::Kind) -> &'static str {
    match kind {
        dimagine_preview::Kind::Thumb => "thumb",
        dimagine_preview::Kind::View => "view",
    }
}

/// Whether a rendition for `hash` already sits in the library cache
/// (`.dimagine/cache/previews/<shard>/<hash>-<kind>.<ext>`).
fn rendition_exists(root: &Path, hash: &str, kind: dimagine_preview::Kind) -> bool {
    let shard = root.join(".dimagine/cache/previews").join(&hash[..2]);
    let Ok(entries) = fs::read_dir(shard) else {
        return false;
    };
    let prefix = format!("{hash}-{}.", kind_cache_name(kind));
    entries
        .flatten()
        .any(|entry| entry.file_name().to_string_lossy().starts_with(&prefix))
}

/// Outcome of one pre-generation run.
struct PregenerateReport {
    total_images: usize,
    /// Renditions that did not exist before this run.
    generated: usize,
    /// Renditions that already existed before this run.
    cached: usize,
    failed: Vec<(PathBuf, String)>,
    unreadable: Vec<PathBuf>,
}

fn pregenerate(canonical_root: &Path) -> PregenerateReport {
    let (images, unreadable) = find_all_images(canonical_root);
    println!(
        "Found {} images in library {}",
        images.len(),
        canonical_root.display()
    );
    let kinds = [dimagine_preview::Kind::Thumb, dimagine_preview::Kind::View];
    let mut report = PregenerateReport {
        total_images: images.len(),
        generated: 0,
        cached: 0,
        failed: Vec::new(),
        unreadable,
    };
    for (idx, img_path) in images.iter().enumerate() {
        let rel_display = img_path
            .strip_prefix(canonical_root)
            .unwrap_or(img_path)
            .display();
        // Classify by whether each rendition existed before this run,
        // keyed by content hash, not by modification time.
        let hash = dimagine_preview::sha256_file(img_path).ok();
        let existed: Vec<bool> = kinds
            .iter()
            .map(|kind| {
                hash.as_deref()
                    .is_some_and(|hash| rendition_exists(canonical_root, hash, *kind))
            })
            .collect();
        match dimagine_preview::ensure(canonical_root, img_path, &kinds) {
            Ok(renditions) => {
                let mut img_gen = 0;
                let mut img_cached = 0;
                for rendition in renditions {
                    let existed_before = kinds
                        .iter()
                        .position(|kind| *kind == rendition.kind)
                        .is_some_and(|index| existed[index]);
                    if existed_before {
                        img_cached += 1;
                    } else {
                        img_gen += 1;
                    }
                }
                report.generated += img_gen;
                report.cached += img_cached;
                println!(
                    "[{}/{}] {}: generated {}, cached {}",
                    idx + 1,
                    images.len(),
                    rel_display,
                    img_gen,
                    img_cached
                );
            }
            Err(e) => {
                println!(
                    "[{}/{}] {}: FAILED ({})",
                    idx + 1,
                    images.len(),
                    rel_display,
                    e
                );
                report.failed.push((img_path.clone(), e.to_string()));
            }
        }
    }
    report
}

async fn pregenerate_all(library: &Path) -> Result<ExitCode, Box<dyn std::error::Error>> {
    let canonical_root = fs::canonicalize(library)?;
    let report = pregenerate(&canonical_root);
    println!("\n=== Pre-generation Summary ===");
    println!("Total images: {}", report.total_images);
    println!("Renditions generated: {}", report.generated);
    println!("Renditions cached: {}", report.cached);
    println!("Failed images: {}", report.failed.len());
    if !report.failed.is_empty() {
        println!("\nFailures:");
        for (path, reason) in &report.failed {
            let rel = path.strip_prefix(&canonical_root).unwrap_or(path);
            println!("  - {}: {}", rel.display(), reason);
        }
    }
    if !report.unreadable.is_empty() {
        eprintln!("\nUnreadable directories ({}):", report.unreadable.len());
        for dir in &report.unreadable {
            let rel = dir.strip_prefix(&canonical_root).unwrap_or(dir);
            eprintln!("  - {}", rel.display());
        }
        // Exit 3: the library was not fully read.
        return Ok(ExitCode::from(3));
    }
    Ok(ExitCode::SUCCESS)
}

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(code) => code,
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::from(1)
        }
    }
}

async fn run() -> Result<ExitCode, Box<dyn std::error::Error>> {
    let mut args = env::args().skip(1);
    let library = args
        .next()
        .map(PathBuf::from)
        .ok_or("usage: serve <library> [--port N] [--pregenerate]")?;
    let mut port = 3000u16;
    let mut pregenerate = false;
    while let Some(arg) = args.next() {
        if arg == "--port" {
            port = args.next().ok_or("--port requires a value")?.parse()?;
        } else if arg == "--pregenerate" {
            pregenerate = true;
        } else {
            return Err(format!("unknown argument: {arg}").into());
        }
    }
    if pregenerate {
        return pregenerate_all(&library).await;
    }
    let catalog = FsCatalog::new(&library)?;
    let previews = CachedPreview::new(&library);
    let mut config = ServeConfig::default();
    if let Ok(passcode) = env::var("DIMAGINE_PASSCODE") {
        config.passcode = Some(passcode);
    }
    let app = router(catalog, previews, config.clone());
    let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], port))).await?;
    println!("Listening on http://{}", listener.local_addr()?);
    serve(listener, app, &config).await?;
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smallest valid 1x1 PNG; decodable by the preview pipeline.
    const TINY_PNG: &[u8] = &[
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f,
        0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78, 0xda, 0x63, 0xfc,
        0xcf, 0xc0, 0x50, 0x0f, 0x00, 0x04, 0x85, 0x01, 0x80, 0x84, 0xa9, 0x8c, 0x21, 0x00, 0x00,
        0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ];

    #[test]
    fn pregenerate_classifies_by_existence_not_mtime() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("cat.png"), TINY_PNG).unwrap();
        let canonical = fs::canonicalize(root.path()).unwrap();
        let first = pregenerate(&canonical);
        assert_eq!(first.total_images, 1);
        assert_eq!(first.generated, 2);
        assert_eq!(first.cached, 0);
        assert!(first.unreadable.is_empty());
        // A second run finds the renditions already in the
        // cache, even though their mtime is older than now.
        let second = pregenerate(&canonical);
        assert_eq!(second.generated, 0);
        assert_eq!(second.cached, 2);
        assert!(second.unreadable.is_empty());
    }

    #[test]
    #[cfg(unix)]
    fn unreadable_directories_are_reported_not_skipped() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("cat.png"), TINY_PNG).unwrap();
        fs::create_dir(root.path().join("locked")).unwrap();
        fs::write(root.path().join("locked/inner.png"), TINY_PNG).unwrap();
        let locked = fs::canonicalize(root.path().join("locked")).unwrap();
        let mut perms = fs::metadata(&locked).unwrap().permissions();
        perms.set_mode(0o000);
        fs::set_permissions(&locked, perms.clone()).unwrap();
        let canonical = fs::canonicalize(root.path()).unwrap();
        let (images, unreadable) = find_all_images(&canonical);
        let readable_here = fs::read_dir(&locked).is_ok();
        // Restore access so the temp directory can be removed.
        perms.set_mode(0o755);
        fs::set_permissions(&locked, perms).unwrap();
        if readable_here {
            // Running as a privileged user: mode bits do not
            // block reads, so nothing is unreadable here.
            assert_eq!(images.len(), 2);
            assert!(!unreadable.contains(&locked));
        } else {
            assert_eq!(images.len(), 1);
            assert!(unreadable.contains(&locked));
        }
    }
}
