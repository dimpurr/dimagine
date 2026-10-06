//! `dimagine serve`: read-only web viewer for a dimagine library (HLD module
//! `serve`), built on the `dimagine-serve` public API: `FsCatalog`,
//! `CachedPreview`, `ServeConfig`, `router` and `serve`. Compiled when the
//! `serve` feature is on and not switched off at runtime (ADR-013).
//!
//! Accounts live outside the library in the state directory selected by
//! `--data-dir`. With the default `--auth account` the viewer sends every page
//! to `/setup` until an owner account exists; there is no setup code — the
//! first visitor creates the owner, and the server keeps reminding the
//! operator to do so itself. `DIMAGINE_PASSCODE` keeps the legacy
//! shared-passcode mode while no account exists. `--auth none` removes login
//! altogether (every page is public and carries a banner, and the accounts
//! store is not needed). Binding defaults to loopback: other devices need an
//! explicit `--bind`, and a default passcode on a non-loopback bind earns a
//! warning from the serve crate itself.

use std::io::Write;
use std::net::{IpAddr, SocketAddr};
use std::path::Path;
use std::process::ExitCode;

use clap::{Arg, ArgMatches, Command};
use dimagine_serve::{
    accounts::AccountsStore, remind_until_owner_exists, router_from, serve, AuthMode,
    CachedPreview, FsCatalog, ServeConfig, NO_OWNER_REMINDER_INTERVAL,
};
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
            Arg::new("secure_cookies")
                .long("secure-cookies")
                .action(clap::ArgAction::SetTrue)
                .help("Always set the Secure attribute on the session cookie (when serving directly over HTTPS)."),
        )
        .arg(
            Arg::new("auth")
                .long("auth")
                .value_name("MODE")
                .value_parser(["account", "none"])
                .default_value("account")
                .help("Who may look at the library: 'account' (owner account, or DIMAGINE_PASSCODE until one exists, default) or 'none' (no login at all)."),
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
             Otherwise /setup creates the owner account: create it right after the first \
             start, before sharing the address, or the first visitor can claim the server."
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
    let trusted_proxies: Vec<IpAddr> = sub
        .get_many::<IpAddr>("trusted_proxy")
        .map(|vals| vals.copied().collect())
        .unwrap_or_default();
    let data_dir = sub
        .get_one::<std::path::PathBuf>("data_dir")
        .cloned()
        .unwrap_or_else(dimagine_serve::accounts::default_data_dir);
    let auth = match sub.get_one::<String>("auth").map(String::as_str) {
        None => AuthMode::Account,
        Some(name) => match AuthMode::from_name(name) {
            Some(mode) => mode,
            None => {
                emit_failure(json, &format!("unknown --auth mode: {name}"));
                return ExitCode::from(2);
            }
        },
    };
    // `--auth none` is no login at all, so the passcode has nothing to guard.
    let passcode = if auth == AuthMode::Account {
        passcode_from_env()
    } else {
        None
    };
    // A store is only needed when clients must sign in. An unreadable,
    // malformed, or version-skewed accounts.json must stop such a server:
    // falling back to "no users" would reopen public first-run setup. In
    // `--auth none` mode the store is never read, so a broken one is no
    // obstacle.
    let accounts = AccountsStore::new(&data_dir);
    let mut setup_pending = false;
    if auth == AuthMode::Account {
        match accounts.has_users() {
            Ok(has_users) => setup_pending = !has_users && passcode.is_none(),
            Err(err) => {
                emit_failure(json, &err.to_string());
                return ExitCode::from(1);
            }
        }
    }
    let port = sub.get_one::<u16>("port").copied().unwrap_or(DEFAULT_PORT);
    let bind = sub
        .get_one::<IpAddr>("bind")
        .copied()
        .unwrap_or(IpAddr::from([127, 0, 0, 1]));
    if auth == AuthMode::None && !bind.is_loopback() {
        // Start anyway: reaching the address from elsewhere is the point of
        // --auth none, and the risk is the user's own to take.
        eprintln!(
            "WARNING: --auth none is serving the library without a login on {bind}, \
             so anyone who can reach this address can see it"
        );
    }
    // A proxy whose address family can never equal a connecting peer's
    // address would silently disable per-client limiting (every client
    // collapses onto the peer address), so say so at startup.
    for proxy in &trusted_proxies {
        if proxy.is_ipv4() != bind.is_ipv4() {
            eprintln!(
                "WARNING: --trusted-proxy {proxy} is {} but --bind {bind} is {}; this proxy can never match a connecting peer",
                if proxy.is_ipv4() { "IPv4" } else { "IPv6" },
                if bind.is_ipv4() { "IPv4" } else { "IPv6" },
            );
        }
    }
    let config = ServeConfig {
        passcode,
        trusted_proxies,
        data_dir,
        auth,
        https: sub.get_flag("secure_cookies"),
        ..ServeConfig::default()
    };
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
            let document = serde_json::json!({
                "schema": SCHEMA,
                "library": library_dir.display().to_string(),
                "address": local,
                "auth": auth.name(),
            });
            // One compact line: long-running processes emit protocol lines,
            // read one line at a time by tools.
            println!("{}", serde_json::to_string(&document).unwrap_or_default());
        } else {
            println!("serving {} at {local}", library_dir.display());
        }
        let _ = std::io::stdout().flush();
        // While no owner account exists, keep saying so on stderr: the setup
        // page has no code, so nothing else would tell the operator that the
        // server is still claimable.
        if setup_pending {
            tokio::spawn(remind_until_owner_exists(
                accounts,
                NO_OWNER_REMINDER_INTERVAL,
                |message| eprintln!("{message}"),
            ));
        }
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
