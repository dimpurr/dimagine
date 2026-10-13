//! `dimagine serve`: read-only web viewer for a dimagine library (HLD module
//! `serve`), built on the `dimagine-serve` public API: `FsCatalog`,
//! `CachedPreview`, `ServeConfig` and `router_from`. The server loop is the
//! CLI's own, because stopping on a signal needs `axum::serve`'s graceful
//! shutdown. Compiled when the `serve` feature is on and not switched off at
//! runtime (ADR-013).
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
//! warning at startup. The passcode itself stays a secret: neither stdout nor
//! stderr ever carries it, not even when a client sends the wrong one.

use std::future::IntoFuture;
use std::io::Write;
use std::net::{IpAddr, SocketAddr};
use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

use clap::{Arg, ArgMatches, Command};
use dimagine_serve::{
    accounts::AccountsStore, remind_until_owner_exists, router_from, AuthMode, CachedPreview,
    FsCatalog, ServeConfig, NO_OWNER_REMINDER_INTERVAL,
};
use std::sync::Arc;

use crate::emit_failure;

const SCHEMA: &str = "dimagine.serve/0.1";
/// Default viewer port. Unregistered in the IANA service-names registry as
/// of 2026-10-05, and unrelated to any well-known service.
const DEFAULT_PORT: u16 = 8917;
/// Passcode environment variable, matching the serve crate's convention.
const PASSCODE_ENV: &str = "DIMAGINE_PASSCODE";
/// How long a shutdown signal waits for the requests already in flight.
/// Long enough for a phone on a slow link to finish one media response,
/// short enough that stopping the viewer never feels stuck.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

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
            Arg::new("rescan_interval")
                .long("rescan-interval")
                .value_name("SECS")
                .value_parser(clap::value_parser!(u64))
                .default_value("300")
                .help("Interval in seconds between background library rescans (default: 300, 0 disables)."),
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
        .arg(
            Arg::new("write")
                .long("write")
                .action(clap::ArgAction::SetTrue)
                .help("Open the write API (ADR-018). Off by default: without it every write route is absent. Requires --public-origin."),
        )
        .arg(
            Arg::new("public_origin")
                .long("public-origin")
                .value_name("ORIGIN")
                .help("The exact origin the server is reached at (scheme://host[:port]) for write CSRF checks (ADR-018 §3)."),
        )
        .arg(
            Arg::new("allow_anonymous_write")
                .long("allow-anonymous-write")
                .action(clap::ArgAction::SetTrue)
                .help("Let a no-login server (`--auth none`) on a loopback bind accept writes without a session (ADR-018 §2)."),
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
    let rescan_interval = sub
        .get_one::<u64>("rescan_interval")
        .copied()
        .unwrap_or(300);
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
    // ADR-018 §1–§3: the write surface is off unless asked for, and turning it
    // on has preconditions the server will not run without. These are checked
    // at startup, not at request time, so a misconfiguration that would leave
    // writes half-open or unverifiable refuses to start.
    let write = sub.get_flag("write");
    let allow_anonymous_write = sub.get_flag("allow_anonymous_write");
    let public_origin = sub.get_one::<String>("public_origin").cloned();
    if let Some(reason) = write_flag_refusal(
        write,
        allow_anonymous_write,
        auth,
        bind,
        public_origin.as_deref(),
    ) {
        emit_failure(json, reason);
        return ExitCode::from(2);
    }
    let config = ServeConfig {
        passcode,
        trusted_proxies,
        data_dir,
        auth,
        rescan_interval,
        https: sub.get_flag("secure_cookies"),
        write,
        allow_anonymous_write,
        public_origin: if write { public_origin } else { None },
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
        // The viewer drives `axum::serve` itself instead of calling
        // `dimagine_serve::serve`, because stopping on a signal — stop
        // accepting, then let the requests already in flight finish — is a
        // property of the serve builder. The startup check that call made is
        // made here too: a default passcode on an address others can reach
        // deserves a warning whoever runs the router.
        if config.passcode == ServeConfig::default().passcode && !bind.is_loopback() {
            eprintln!("WARNING: the default passcode is active on a non-loopback address");
        }
        let server = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(shutdown_signal())
        .into_future();
        let stopped = tokio::select! {
            drained = server => drained,
            _ = async {
                shutdown_signal().await;
                tokio::time::sleep(SHUTDOWN_GRACE).await;
            } => {
                eprintln!(
                    "dimagine: requests were still running {SHUTDOWN_GRACE:?} after the stop \
                     signal, so the viewer stopped without waiting for them"
                );
                Ok(())
            }
        };
        match stopped {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                emit_failure(json, &format!("the viewer stopped: {error}"));
                ExitCode::from(1)
            }
        }
    })
}

/// Resolve when the operator asks the viewer to stop: Ctrl-C (SIGINT) or
/// SIGTERM, which are how a foreground server gets stopped.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut interrupt =
            signal(SignalKind::interrupt()).expect("cannot install a SIGINT handler");
        let mut terminate =
            signal(SignalKind::terminate()).expect("cannot install a SIGTERM handler");
        tokio::select! {
            _ = interrupt.recv() => {}
            _ = terminate.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c()
            .await
            .expect("cannot install a Ctrl-C handler");
    }
}

fn passcode_from_env() -> Option<String> {
    std::env::var(PASSCODE_ENV)
        .ok()
        .filter(|passcode| !passcode.is_empty())
}

/// Whether a `--public-origin` value is a bare origin: a scheme, `://`, a
/// non-empty authority, and nothing after it (no path, no trailing slash). A
/// request's `Origin` header carries no path, so an origin with one could
/// never be matched and the CSRF check would always fail.
fn origin_of_is_wellformed(origin: &str) -> bool {
    match origin.split_once("://") {
        Some((scheme, authority)) => {
            !scheme.is_empty() && !authority.is_empty() && !authority.contains('/')
        }
        None => false,
    }
}

/// Why this set of write flags must not start the server (ADR-018 §1–§3), or
/// `None` when it may. Decided once, at startup: a server that cannot verify
/// the CSRF origin, or that would take session-less writes from anywhere but
/// loopback, is a server that should not be running at all.
fn write_flag_refusal(
    write: bool,
    allow_anonymous_write: bool,
    auth: AuthMode,
    bind: IpAddr,
    public_origin: Option<&str>,
) -> Option<&'static str> {
    if write && !public_origin.is_some_and(origin_of_is_wellformed) {
        // Every write compares the request's `Origin` (or `Referer`) against
        // this exact origin; with none there is nothing to check against, so
        // the CSRF defence (ADR-018 §3) would be unverifiable.
        return Some(
            "--write needs --public-origin set to the exact origin \
             (scheme://host[:port], with no path or trailing slash)",
        );
    }
    if allow_anonymous_write {
        // ADR-018 §2: anonymous writes exist only for a no-login server, and
        // only on a loopback bind it cannot be reached from elsewhere. This is
        // a hard refusal, not a warning.
        if auth != AuthMode::None {
            return Some(
                "--allow-anonymous-write only applies to --auth none; \
                 a server with accounts writes only through an owner session",
            );
        }
        if !bind.is_loopback() {
            return Some(
                "--allow-anonymous-write is refused on a non-loopback bind \
                 (writes without a session are a loopback-only mode)",
            );
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    const ORIGIN: Option<&str> = Some("https://dimagine.example");
    const LOOPBACK: IpAddr = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1));
    const PUBLIC: IpAddr = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7));

    #[test]
    fn writes_need_an_exact_public_origin() {
        // `--write` without an origin to compare a request's `Origin` against
        // refuses to start (ADR-018 §3).
        assert!(
            write_flag_refusal(true, false, AuthMode::Account, LOOPBACK, None).is_some(),
            "no origin"
        );
        // An origin with a path could never equal an `Origin` header.
        assert!(
            write_flag_refusal(
                true,
                false,
                AuthMode::Account,
                LOOPBACK,
                Some("https://dimagine.example/settings")
            )
            .is_some(),
            "origin with a path"
        );
        assert!(
            write_flag_refusal(true, false, AuthMode::Account, PUBLIC, ORIGIN).is_none(),
            "a well-formed origin on any bind"
        );
        // With writes off there is nothing to refuse (ADR-018 §1).
        assert!(write_flag_refusal(false, false, AuthMode::Account, PUBLIC, None).is_none());
    }

    #[test]
    fn anonymous_write_is_no_login_and_loopback_only() {
        // The one combination ADR-018 §2 allows: `--auth none` bound to
        // loopback, IPv4 and IPv6 alike.
        assert!(write_flag_refusal(true, true, AuthMode::None, LOOPBACK, ORIGIN).is_none());
        assert!(write_flag_refusal(
            true,
            true,
            AuthMode::None,
            IpAddr::V6(Ipv6Addr::LOCALHOST),
            ORIGIN
        )
        .is_none());
        // The same flags on a bind others can reach refuse to start outright —
        // not a warning, which is the point of the item.
        assert!(
            write_flag_refusal(true, true, AuthMode::None, PUBLIC, ORIGIN).is_some(),
            "anonymous write on a non-loopback bind"
        );
        // With accounts, an anonymous write is not a thing at all.
        assert!(
            write_flag_refusal(true, true, AuthMode::Account, LOOPBACK, ORIGIN).is_some(),
            "anonymous write in account mode"
        );
    }
}
