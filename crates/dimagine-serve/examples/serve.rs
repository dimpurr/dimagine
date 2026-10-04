use dimagine_serve::{router, serve, FsCatalog, OriginalPreview, ServeConfig};
use std::{env, net::SocketAddr, path::PathBuf};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = env::args().skip(1);
    let library = args
        .next()
        .map(PathBuf::from)
        .ok_or("usage: serve <library> [--port N]")?;
    let mut port = 3000u16;
    while let Some(arg) = args.next() {
        if arg == "--port" {
            port = args.next().ok_or("--port requires a value")?.parse()?;
        } else {
            return Err(format!("unknown argument: {arg}").into());
        }
    }
    let catalog = FsCatalog::new(library)?;
    let mut config = ServeConfig::default();
    if let Ok(passcode) = env::var("DIMAGINE_PASSCODE") {
        config.passcode = passcode;
    }
    let app = router(catalog, OriginalPreview, config.clone());
    let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], port))).await?;
    println!("Listening on http://{}", listener.local_addr()?);
    serve(listener, app, &config).await?;
    Ok(())
}
