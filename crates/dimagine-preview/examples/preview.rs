use std::{env, path::PathBuf, process};

use dimagine_preview::{ensure, Kind};

fn main() {
    let mut args = env::args_os().skip(1);
    let image = args.next().map(PathBuf::from).unwrap_or_else(|| {
        eprintln!("usage: preview <image-path>");
        process::exit(2);
    });
    if args.next().is_some() {
        eprintln!("usage: preview <image-path>");
        process::exit(2);
    }
    let root = image.parent().unwrap_or_else(|| std::path::Path::new("."));
    match ensure(root, &image, &[Kind::Thumb, Kind::View]).and_then(|renditions| {
        serde_json::to_string_pretty(&renditions)
            .map_err(|e| dimagine_preview::PreviewError::Decode(e.to_string()))
    }) {
        Ok(json) => println!("{json}"),
        Err(error) => {
            eprintln!("{error}");
            process::exit(1);
        }
    }
}
