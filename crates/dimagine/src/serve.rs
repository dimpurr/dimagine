//! `dimagine serve`: read-only, passcode-protected web viewer for a dimagine
//! library (HLD module `serve`), built on the `dimagine-serve` public API:
//! `FsCatalog`, `CachedPreview`, `ServeConfig`, `router` and `serve`.
//! Compiled when the `serve` feature is on and not switched off at runtime
//! (ADR-013).
//!
//! The passcode comes from the `DIMAGINE_PASSCODE` environment variable; when
//! it is unset (or empty) the viewer falls back to the crate's built-in
//! default. Binding defaults to loopback: other devices need an explicit
//! `--bind`, and a default passcode on a non-loopback bind earns a warning
//! from the serve crate itself.

use std::io::Write;
use std::net::{IpAddr, SocketAddr};
use std::path::Path;
use std::process::ExitCode;

use clap::{Arg, ArgMatches, Command};
use dimagine_serve::{router, serve, CachedPreview, FsCatalog, ServeConfig};

use crate::emit_failure;

const SCHEMA: &str = "dimagine.serve/0.1";
/// Default viewer port. Unregistered in the IANA service-names registry as
/// of 2026-10-05, and unrelated to any well-known service.
const DEFAULT_PORT: u16 = 8917;
/// Passcode environment variable, matching the serve crate's convention.
const PASSCODE_ENV: &str = "DIMAGINE_PASSCODE";

pub fn command() -> Command {
    Command::new("serve")
        .about("Read-only web viewer for other devices (stop with Ctrl-C).")
        .arg(
            Arg::new("port")
                .long("port")
                .value_name("N")
                .value_parser(clap::value_parser!(u16))
                .default_value("8917")
                .help("TCP port to listen on (default: 8917)."),
        )
        .arg(
            Arg::new("bind")
                .long("bind")
                .value_name("ADDR")
                .value_parser(clap::value_parser!(IpAddr))
                .default_value("127.0.0.1")
                .help("IP address to listen on (default: 127.0.0.1, reachable only from this machine)."),
        )
        .after_help(format!(
            "The passcode comes from the {PASSCODE_ENV} environment variable; \
             when unset, the viewer's built-in default applies."
        ))
}

pub fn run(sub: &ArgMatches, library_dir: &Path) -> ExitCode {
    let json = sub.get_flag("json");
    if !library_dir.is_dir() {
        emit_failure(json, &format!("not a folder: {}", library_dir.display()));
        return ExitCode::from(1);
    }
    let catalog = match FsCatalog::new(library_dir) {
        Ok(catalog) => catalog,
        Err(error) => {
            emit_failure(json, &error.to_string());
            return ExitCode::from(1);
        }
    };
    let previews = CachedPreview::new(library_dir);
    let config = ServeConfig {
        passcode: passcode_from_env(),
        ..ServeConfig::default()
    };
    let port = sub.get_one::<u16>("port").copied().unwrap_or(DEFAULT_PORT);
    let bind = sub
        .get_one::<IpAddr>("bind")
        .copied()
        .unwrap_or(IpAddr::from([127, 0, 0, 1]));
    let address = SocketAddr::new(bind, port);
    let app = router(catalog, previews, config.clone());
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            emit_failure(json, &format!("cannot start the runtime: {error}"));
            return ExitCode::from(1);
        }
    };
    runtime.block_on(async move {
        let listener = match tokio::net::TcpListener::bind(address).await {
            Ok(listener) => listener,
            Err(error) => {
                emit_failure(json, &format!("cannot listen on {address}: {error}"));
                return ExitCode::from(1);
            }
        };
        let local = listener
            .local_addr()
            .map(|addr| format!("http://{addr}"))
            .unwrap_or_else(|_| format!("http://{address}"));
        if json {
            let document = serde_json::json!({
                "schema": SCHEMA,
                "library": library_dir.display().to_string(),
                "address": local,
            });
            // One compact line: long-running processes emit protocol lines,
            // read one line at a time by tools.
            println!("{}", serde_json::to_string(&document).unwrap_or_default());
        } else {
            println!("serving {} at {local}", library_dir.display());
        }
        let _ = std::io::stdout().flush();
        match serve(listener, app, &config).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                emit_failure(json, &format!("the viewer stopped: {error}"));
                ExitCode::from(1)
            }
        }
    })
}

fn passcode_from_env() -> String {
    std::env::var(PASSCODE_ENV)
        .ok()
        .filter(|passcode| !passcode.is_empty())
        .unwrap_or_else(|| ServeConfig::default().passcode)
}
