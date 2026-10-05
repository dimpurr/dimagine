//! `dimagine serve`: read-only, passcode-protected web viewer for a dimagine
//! library (HLD module `serve`), built on the `dimagine-serve` public API:
//! `FsCatalog`, `CachedPreview`, `ServeConfig`, `router` and `serve`.
//! Compiled when the `serve` feature is on and not switched off at runtime
//! (ADR-013).
//!
//! Accounts live outside the library in the state directory selected by
//! `--data-dir`. On first run (no accounts yet) the viewer redirects every
//! page to `/setup`, which needs the one-time setup code printed at startup;
//! `DIMAGINE_PASSCODE` keeps the legacy shared-passcode mode only while no
//! account exists. Binding defaults to loopback: other devices need an
//! explicit `--bind`, and a default passcode on a non-loopback bind earns a
//! warning from the serve crate itself.

use std::io::Write;
use std::net::{IpAddr, SocketAddr};
use std::path::Path;
use std::process::ExitCode;

use clap::{Arg, ArgMatches, Command};
use dimagine_serve::{router_from, serve, CachedPreview, FsCatalog, ServeConfig};
use std::sync::Arc;

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
        .arg(
            Arg::new("trusted_proxy")
                .long("trusted-proxy")
                .value_name("IP")
                .value_parser(clap::value_parser!(IpAddr))
                .action(clap::ArgAction::Append)
                .help("Trusted reverse proxy IP address (repeatable)."),
        )
        .arg(
            Arg::new("data_dir")
                .long("data-dir")
                .value_name("DIR")
                .value_parser(clap::value_parser!(std::path::PathBuf))
                .help("State directory where accounts are stored (default: $XDG_STATE_HOME/dimagine or ~/.local/state/dimagine)."),
        )
        .after_help(format!(
            "If no users exist and {PASSCODE_ENV} is set, passcode mode is active. \
             Otherwise, first-run setup requires the one-time code printed at startup."
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
    let trusted_proxies = sub
        .get_many::<IpAddr>("trusted_proxy")
        .map(|vals| vals.copied().collect())
        .unwrap_or_default();
    let data_dir = sub
        .get_one::<std::path::PathBuf>("data_dir")
        .cloned()
        .unwrap_or_else(dimagine_serve::accounts::default_data_dir);
    let passcode = passcode_from_env();
    let accounts = dimagine_serve::accounts::AccountsStore::new(&data_dir);
    // An unreadable, malformed, or version-skewed accounts.json must stop the
    // server: falling back to "no users" would reopen public first-run setup.
    let has_users = match accounts.has_users() {
        Ok(has_users) => has_users,
        Err(err) => {
            emit_failure(json, &err.to_string());
            return ExitCode::from(1);
        }
    };
    let setup_code = if !has_users && passcode.is_none() {
        Some(dimagine_serve::generate_setup_code())
    } else {
        None
    };
    let config = ServeConfig {
        passcode,
        trusted_proxies,
        data_dir,
        setup_code: setup_code.clone(),
        ..ServeConfig::default()
    };
    let port = sub.get_one::<u16>("port").copied().unwrap_or(DEFAULT_PORT);
    let bind = sub
        .get_one::<IpAddr>("bind")
        .copied()
        .unwrap_or(IpAddr::from([127, 0, 0, 1]));
    let address = SocketAddr::new(bind, port);
    let app = match router_from(Arc::new(catalog), Arc::new(previews), config.clone()) {
        Ok(app) => app,
        Err(err) => {
            emit_failure(json, &err.to_string());
            return ExitCode::from(1);
        }
    };
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
            let mut document = serde_json::json!({
                "schema": SCHEMA,
                "library": library_dir.display().to_string(),
                "address": local,
            });
            if let Some(ref code) = setup_code {
                document["setup_code"] = serde_json::Value::String(code.clone());
            }
            // One compact line: long-running processes emit protocol lines,
            // read one line at a time by tools.
            println!("{}", serde_json::to_string(&document).unwrap_or_default());
        } else {
            println!("serving {} at {local}", library_dir.display());
            if let Some(ref code) = setup_code {
                println!("One-time setup code: {code}");
            }
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

fn passcode_from_env() -> Option<String> {
    std::env::var(PASSCODE_ENV)
        .ok()
        .filter(|passcode| !passcode.is_empty())
}
