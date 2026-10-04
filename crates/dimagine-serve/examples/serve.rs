use dimagine_serve::{router, serve, CachedPreview, FsCatalog, ServeConfig};
use std::{
    env, fs,
    net::SocketAddr,
    path::{Path, PathBuf},
    time::SystemTime,
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

fn find_all_images(root: &Path) -> Vec<PathBuf> {
    let mut images = Vec::new();
    let mut queue = vec![root.to_path_buf()];
    while let Some(dir) = queue.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
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
    images
}

async fn pregenerate_all(library: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let canonical_root = fs::canonicalize(library)?;
    let images = find_all_images(&canonical_root);
    println!(
        "Found {} images in library {}",
        images.len(),
        library.display()
    );
    let mut generated_count = 0usize;
    let mut cached_count = 0usize;
    let mut failed: Vec<(PathBuf, String)> = Vec::new();

    let kinds = [dimagine_preview::Kind::Thumb, dimagine_preview::Kind::View];

    for (idx, img_path) in images.iter().enumerate() {
        let rel_display = img_path
            .strip_prefix(&canonical_root)
            .unwrap_or(img_path)
            .display();
        let step_start = SystemTime::now();
        match dimagine_preview::ensure(&canonical_root, img_path, &kinds) {
            Ok(renditions) => {
                let mut img_gen = 0;
                let mut img_cached = 0;
                for r in renditions {
                    let is_new = fs::metadata(&r.path)
                        .ok()
                        .and_then(|m| m.modified().ok())
                        .is_some_and(|t| t >= step_start);
                    if is_new {
                        img_gen += 1;
                    } else {
                        img_cached += 1;
                    }
                }
                generated_count += img_gen;
                cached_count += img_cached;
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
                failed.push((img_path.clone(), e.to_string()));
            }
        }
    }

    println!("\n=== Pre-generation Summary ===");
    println!("Total images: {}", images.len());
    println!("Renditions generated: {}", generated_count);
    println!("Renditions cached: {}", cached_count);
    println!("Failed images: {}", failed.len());
    if !failed.is_empty() {
        println!("\nFailures:");
        for (path, reason) in &failed {
            let rel = path.strip_prefix(&canonical_root).unwrap_or(path);
            println!("  - {}: {}", rel.display(), reason);
        }
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
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
        config.passcode = passcode;
    }
    let app = router(catalog, previews, config.clone());
    let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], port))).await?;
    println!("Listening on http://{}", listener.local_addr()?);
    serve(listener, app, &config).await?;
    Ok(())
}
