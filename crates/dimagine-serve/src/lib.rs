//! Read-only web viewer for a dimagine library. Clients are either
//! signed in with a single owner account (ADR-014), with the legacy
//! shared passcode until an account exists, or not authenticated at
//! all (`AuthMode::None`). Accounts live outside the library in a
//! state directory; the first visitor creates the owner account on
//! `/setup`, with no code to copy.
//!
//! The library pages are split into `pages`, `ui`, `assets`,
//! `view_query`, `catalog`, `media` and `index_sync`; this module is
//! the router, the application state and the auth middleware.

pub mod accounts;
pub mod assets;
pub mod catalog;
pub mod index_sync;
pub(crate) mod media;
pub mod pages;
pub mod ui;
pub mod view_query;

pub use ui::shell::Frame;

pub use catalog::*;
pub use index_sync::*;
pub use view_query::*;

use axum::{
    body::Body,
    extract::{Form, State},
    http::{header, HeaderMap, HeaderValue, Request, StatusCode},
    middleware::{self, Next},
    response::{Html, IntoResponse, Redirect, Response},
    routing::get,
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use subtle::ConstantTimeEq;

use crate::accounts::{is_weak_password, WEAK_PASSWORD_LENGTH};
use crate::pages::{
    collections::collection_json, collections::collection_redirect, collections::collections_page,
    folders::folder_json, folders::folder_redirect, folders::folder_root_json,
    folders::folders_page, image::image_json, image::image_page, library::library_page,
    library::sidebar_json, library::view_json, search::search_page,
};
use crate::ui::{layout, shell::Destination};

const SESSION_SECONDS: u64 = 60 * 60 * 24 * 30;

/// How the viewer authenticates its clients.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum AuthMode {
    /// A login is required: the owner account, or the legacy shared
    /// passcode while no account exists. This is the default.
    #[default]
    Account,
    /// No login at all. Every page is reachable without a session and
    /// carries [`NO_LOGIN_BANNER`]; the accounts store is not read at
    /// all.
    None,
}

impl AuthMode {
    /// Whether HTML pages must carry the "no login" banner.
    pub fn shows_banner(self) -> bool {
        self == AuthMode::None
    }

    /// The `--auth` spelling of this mode, as it appears in the
    /// startup document and in help text.
    pub fn name(self) -> &'static str {
        match self {
            AuthMode::Account => "account",
            AuthMode::None => "none",
        }
    }

    /// Parse a mode from its `--auth` spelling.
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "account" => Some(AuthMode::Account),
            "none" => Some(AuthMode::None),
            _ => None,
        }
    }
}

/// Banner shown at the top of every page when no login is required.
pub const NO_LOGIN_BANNER: &str =
    "No login: anyone who can reach this address can see this library.";

/// Reminder logged while no owner account exists yet. It names the
/// setup page and the claim risk, and carries no secret: there is no
/// code to leak.
pub const NO_OWNER_REMINDER: &str = "No owner account yet: open /setup to create it. \
     Until then the first visitor can claim this server.";

/// How long the no-owner reminder waits before repeating itself.
pub const NO_OWNER_REMINDER_INTERVAL: Duration = Duration::from_secs(10 * 60);

/// Viewer configuration.
#[derive(Clone, Debug)]
pub struct ServeConfig {
    /// Shared passcode (legacy compatibility mode).
    pub passcode: Option<String>,
    /// Cookie name used for the in-memory session.
    pub cookie_name: String,
    /// Set this when the viewer is served directly over HTTPS.
    pub https: bool,
    /// Trusted reverse proxy IP addresses.
    pub trusted_proxies: Vec<std::net::IpAddr>,
    /// State directory for accounts.json.
    pub data_dir: PathBuf,
    /// Whether clients must sign in at all.
    pub auth: AuthMode,
    /// Seconds between background library rescans (default: 300, 0
    /// disables).
    pub rescan_interval: u64,
}

impl Default for ServeConfig {
    fn default() -> Self {
        Self {
            passcode: Some("2333".to_string()),
            cookie_name: "dimagine_session".to_string(),
            https: false,
            trusted_proxies: Vec::new(),
            data_dir: accounts::default_data_dir(),
            auth: AuthMode::Account,
            rescan_interval: 300,
        }
    }
}

/// Log [`NO_OWNER_REMINDER`] now and every `interval` after that,
/// until an owner account exists or the store stops being readable.
///
/// A broken store stops the loop rather than repeating the reminder:
/// the caller is expected to have refused to start in that case, and
/// an unreadable store is not "no owner yet".
pub async fn remind_until_owner_exists<F>(
    accounts: accounts::AccountsStore,
    interval: Duration,
    log: F,
) where
    F: Fn(&str),
{
    loop {
        match accounts.has_users() {
            Ok(true) | Err(_) => return,
            Ok(false) => {}
        }
        log(NO_OWNER_REMINDER);
        tokio::time::sleep(interval).await;
    }
}

/// Resolve client IP for rate limiting and logging.
///
/// When the direct TCP peer is a trusted reverse proxy, the client IP
/// is extracted from the LAST hop of the `X-Forwarded-For` header.
/// For untrusted peers (or if the header is absent/malformed), the
/// TCP peer IP is used directly, ignoring the header.
pub fn resolve_client_ip(
    peer_addr: Option<std::net::SocketAddr>,
    headers: &HeaderMap,
    trusted_proxies: &[std::net::IpAddr],
) -> String {
    let Some(peer) = peer_addr else {
        return "unknown".to_string();
    };
    let peer_ip = peer.ip();
    if trusted_proxies.contains(&peer_ip) {
        if let Some(forwarded) = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
            if let Some(last_hop) = forwarded.split(',').next_back().map(str::trim) {
                if let Ok(ip) = last_hop.parse::<std::net::IpAddr>() {
                    return ip.to_string();
                }
            }
        }
    }
    peer_ip.to_string()
}

#[derive(Clone)]
pub(crate) struct AppState {
    pub(crate) catalog: Arc<dyn Catalog>,
    pub(crate) previews: Arc<dyn PreviewProvider>,
    pub(crate) config: ServeConfig,
    pub(crate) accounts: accounts::AccountsStore,
    /// Serialises owner creation: one critical section from the
    /// re-check to the write, so two concurrent setup requests cannot
    /// both create an owner.
    ///
    /// In-process only. It closes the window for every request this
    /// server handles, but the store underneath is load → push → rename
    /// with no file lock, so two servers sharing one `--data-dir` can
    /// each create an owner and the second write wins (RW25b review Low).
    /// A second viewer on a live state directory is operator error — the
    /// two would also disagree about sessions, throttles and the index —
    /// so the residual window is documented rather than locked; a file
    /// lock, or refusing to start when another server holds the
    /// directory, is the natural follow-up.
    pub(crate) setup_lock: Arc<Mutex<()>>,
    pub(crate) sessions: Arc<Mutex<HashMap<String, u64>>>,
    pub(crate) throttles: Arc<Mutex<ThrottleState>>,
    pub(crate) admission: Arc<tokio::sync::Semaphore>,
    pub(crate) login_slots: Arc<tokio::sync::Semaphore>,
    pub(crate) index: Arc<IndexHandle>,
}

impl AppState {
    /// The no-login banner text, when the mode calls for it. Every
    /// page frame carries it, so no page can be mistaken for a
    /// login-protected one.
    pub(crate) fn banner(&self) -> Option<&'static str> {
        self.config.auth.shows_banner().then_some(NO_LOGIN_BANNER)
    }
}

/// At most this many login attempts may be in flight at once, across
/// all clients. Guesses are refused here — before the passcode is
/// looked at — so a brute-force wave cannot test many passcodes at
/// the same time.
pub const LOGIN_CONCURRENCY_LIMIT: usize = 2;

/// The longest one login attempt holds an in-flight slot: the
/// escalating delay caps here, and the argon2 check that follows
/// is shorter. An attempt refused because the slots are busy is
/// told to come back after the delay plus that check, so
/// `Retry-After` never promises a slot a slow attempt still holds
/// (RW19b review Low: `1` was shorter than the wait a busy slot
/// could impose; RW54 review Low #4: the check still holds the
/// slot after the delay ends, so the refusal hint rounds the
/// whole hold up to the next whole second).
pub const LOGIN_MAX_DELAY_SECS: u64 = 5;

/// Attempts one client may spend inside `LOGIN_BUDGET_WINDOW_SECS`
/// before the server stops comparing its guesses. Charged before the
/// comparison, so the passcode is never even looked at once the
/// budget is gone.
pub const LOGIN_FAILURE_BUDGET: u32 = 10;

/// Attempts all clients together may spend inside
/// `LOGIN_BUDGET_WINDOW_SECS`. This is the backstop that keeps the
/// per-client budget from being farmed out across many source
/// addresses, and it is not affected by client eviction.
pub const LOGIN_GLOBAL_FAILURE_BUDGET: u32 = 100;

/// Length of the login attempt budget window. Budgets are restored,
/// never permanently withdrawn, so there is no lockout.
pub const LOGIN_BUDGET_WINDOW_SECS: u64 = 15 * 60;

/// Clients kept in memory. Past this the least recently seen one is
/// dropped; forgetting a client only restores that client's own
/// budget, never the global one.
const MAX_TRACKED_CLIENTS: usize = 1024;

/// Bound on concurrently expensive requests: directory listings and
/// media streaming (which may generate previews). Excess requests get
/// a 503 with Retry-After instead of queueing without limit.
const ADMISSION_LIMIT: usize = 8;

pub(crate) fn acquire_admission(state: &AppState) -> Option<tokio::sync::OwnedSemaphorePermit> {
    state.admission.clone().try_acquire_owned().ok()
}

pub(crate) fn admission_denied() -> Response {
    let mut response = StatusCode::SERVICE_UNAVAILABLE.into_response();
    response
        .headers_mut()
        .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
    response
}

pub(crate) fn json_result<T: Serialize>(result: Result<T, CatalogError>) -> Response {
    match result {
        Ok(value) => Json(value).into_response(),
        Err(e) => error_response(e),
    }
}

/// The answer when the index cannot answer a request.
///
/// A list page must never fall back to "there is nothing here": the
/// library is not empty, the index is unusable, and those are
/// different facts. So this is 503 with the reason, shaped to match
/// what the caller would have returned.
pub(crate) fn index_failure(state: &AppState, as_json: bool) -> Response {
    let reason = index_reason(state);
    if as_json {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": reason })),
        )
            .into_response()
    } else {
        error_page(state, StatusCode::SERVICE_UNAVAILABLE, &reason)
    }
}

/// Why the index is unusable, in words a person can act on.
fn index_reason(state: &AppState) -> String {
    match state.index.sidebar_data() {
        Ok(_) => "The library index could not answer this query.".to_owned(),
        Err(error) => format!("The library index is unavailable: {error}."),
    }
}

pub(crate) fn error_response(e: CatalogError) -> Response {
    match e {
        CatalogError::NotFound => StatusCode::NOT_FOUND.into_response(),
        CatalogError::Forbidden => StatusCode::FORBIDDEN.into_response(),
        CatalogError::Unreadable => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// An error a browser renders as a page: the same frame, banner and
/// navigation as every other page, with the status and one line a
/// person can act on. Error pages are HTML pages like any other, so
/// under `--auth none` they carry the banner too and can never be
/// mistaken for a login-protected page.
pub(crate) fn error_page(state: &AppState, status: StatusCode, message: &str) -> Response {
    let title = error_page_title(status);
    // W43: the §4.8 error anatomy — a neutral surface whose only red thing
    // is the status glyph, never a big red block.
    let body = crate::ui::components::error_state(title, message);
    let frame = Frame {
        banner: state.banner(),
        ..Frame::new(title, Destination::Library, body)
    };
    let mut response = (status, Html(frame.render())).into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// The heading of an error page. The status code is in the response
/// line already; this is the line a person reads on the page.
fn error_page_title(status: StatusCode) -> &'static str {
    match status {
        StatusCode::NOT_FOUND => "Not found",
        StatusCode::FORBIDDEN => "Not viewable",
        StatusCode::INTERNAL_SERVER_ERROR => "Server error",
        StatusCode::SERVICE_UNAVAILABLE => "Index unavailable",
        _ => "Something went wrong",
    }
}

/// A catalog error that answers a page request, as an error page: the
/// statuses match [`error_response`] for the JSON routes, but a
/// browser receives a page it can read, with the banner every page
/// carries.
pub(crate) fn catalog_error_page(state: &AppState, e: CatalogError) -> Response {
    match e {
        CatalogError::NotFound => error_page(
            state,
            StatusCode::NOT_FOUND,
            "This image is not in this library.",
        ),
        CatalogError::Forbidden => error_page(
            state,
            StatusCode::FORBIDDEN,
            "This address is outside this library and cannot be shown.",
        ),
        CatalogError::Unreadable => error_page(
            state,
            StatusCode::INTERNAL_SERVER_ERROR,
            "This image could not be read.",
        ),
    }
}

/// Attempts spent by one client (or by every client together) inside
/// the current budget window.
#[derive(Clone, Copy, Default)]
struct LoginBudget {
    spent: u32,
    window_start: u64,
    last_seen: u64,
}

impl LoginBudget {
    /// Attempts spent in the window containing `now`, starting a
    /// fresh window first if the previous one has passed. A clock
    /// that steps backwards keeps the current window rather than
    /// handing out a second budget.
    fn spent_in_window(&mut self, now: u64) -> u32 {
        if now.saturating_sub(self.window_start) >= LOGIN_BUDGET_WINDOW_SECS {
            self.window_start = now;
            self.spent = 0;
        }
        self.last_seen = now;
        self.spent
    }

    fn charge(&mut self, now: u64) {
        self.spent = self.spent.saturating_add(1);
        self.last_seen = now;
    }

    /// Seconds until this window rolls over and the budget is
    /// restored.
    fn retry_after(&self, now: u64) -> u64 {
        LOGIN_BUDGET_WINDOW_SECS
            .saturating_sub(now.saturating_sub(self.window_start))
            .max(1)
    }
}

#[derive(Default)]
pub(crate) struct ThrottleState {
    clients: HashMap<String, LoginBudget>,
    global: LoginBudget,
}

/// What the pre-comparison login gate decided about one attempt.
#[derive(Clone, Copy, PartialEq, Eq)]
enum LoginGate {
    /// The attempt may be compared against the passcode. Carries the
    /// escalating delay step.
    Compare(u32),
    /// Refused without comparing; `retry_after` seconds until the
    /// budget is restored.
    Refused { retry_after: u64 },
}

impl ThrottleState {
    /// Decide whether one login attempt may be compared, charging it
    /// to both the per-client and the global budget when it may.
    /// Runs before the passcode is read, so a refused attempt costs
    /// the caller nothing and reveals nothing about the guess.
    fn admit(&mut self, client_key: &str, now: u64) -> LoginGate {
        let global = self.global.spent_in_window(now);
        let global_retry = self.global.retry_after(now);
        let (spent, client_retry) = match self.clients.get_mut(client_key) {
            Some(client) => {
                let spent = client.spent_in_window(now);
                (spent, client.retry_after(now))
            }
            None => (0, LOGIN_BUDGET_WINDOW_SECS),
        };
        if spent >= LOGIN_FAILURE_BUDGET {
            return LoginGate::Refused {
                retry_after: client_retry,
            };
        }
        if global >= LOGIN_GLOBAL_FAILURE_BUDGET {
            return LoginGate::Refused {
                retry_after: global_retry,
            };
        }
        self.charge_client(client_key, now);
        self.global.charge(now);
        LoginGate::Compare(spent.saturating_add(1).max(global.saturating_add(1)))
    }

    /// Charge an admitted attempt, keeping the client map bounded by
    /// evicting the least recently seen client.
    fn charge_client(&mut self, client_key: &str, now: u64) {
        if self.clients.len() >= MAX_TRACKED_CLIENTS && !self.clients.contains_key(client_key) {
            if let Some(oldest) = self
                .clients
                .iter()
                .min_by_key(|(_, budget)| budget.last_seen)
                .map(|(key, _)| key.clone())
            {
                self.clients.remove(&oldest);
            }
        }
        // A first attempt starts its window now, not at zero: an
        // entry left with a zeroed window start would look long
        // expired and its next attempt would be admitted as a free
        // budget.
        self.clients
            .entry(client_key.to_owned())
            .or_insert(LoginBudget {
                window_start: now,
                ..LoginBudget::default()
            })
            .charge(now);
    }

    /// A successful login restores that client's budget. The global
    /// budget is deliberately left alone: a success must not be able
    /// to hand an attacker a fresh global allowance.
    fn record_success(&mut self, client_key: &str) {
        self.clients.remove(client_key);
    }
}

/// Report an accounts-store failure to the client: 500 with the error
/// text. The store was validated at startup, so this only fires when
/// it breaks while the server runs; the error never contains a
/// password or hash.
fn store_error(err: accounts::AccountsError) -> Response {
    let mut response = (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()).into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// Refuse a login attempt without comparing it: 429 with `Retry-After`
/// and an empty body, so a refused attempt is indistinguishable from
/// any other.
fn login_refused(retry_after: u64) -> Response {
    let mut response = StatusCode::TOO_MANY_REQUESTS.into_response();
    if let Ok(value) = HeaderValue::from_str(&retry_after.to_string()) {
        response.headers_mut().insert(header::RETRY_AFTER, value);
    }
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// Build the read-only viewer router.
///
/// The accounts store is opened here; a store that cannot be read is
/// a panic because every caller that can report an error uses
/// [`router_from`] instead.
pub fn router<C: Catalog, P: PreviewProvider>(
    catalog: C,
    previews: P,
    config: ServeConfig,
) -> Router {
    router_from(Arc::new(catalog), Arc::new(previews), config)
        .expect("accounts store must be readable")
}

/// Build a router from trait objects for later CLI integration.
///
/// Fails when `accounts.json` is unreadable, malformed, or written by
/// an unknown schema version: the caller must exit non-zero rather
/// than start a viewer that would silently fall back to first-run
/// setup. With [`AuthMode::None`] the store is never read, so a
/// missing or broken one does not stop a no-login server.
pub fn router_from(
    catalog: Arc<dyn Catalog>,
    previews: Arc<dyn PreviewProvider>,
    config: ServeConfig,
) -> Result<Router, accounts::AccountsError> {
    let accounts = accounts::AccountsStore::new(&config.data_dir);
    if config.auth == AuthMode::Account {
        accounts.has_users()?;
    }
    let index_handle = match IndexHandle::open(catalog.root()) {
        Ok(handle) => handle,
        // A library whose index cannot be built is still worth
        // serving: sign-in, media and notes work, and every list page
        // says why it is empty rather than showing a library that
        // looks fine.
        Err(error) => {
            eprintln!(
                "WARNING: the library index is unavailable ({error}); list pages will report it"
            );
            IndexHandle::unavailable(catalog.root().to_path_buf(), error)
        }
    };
    let index = Arc::new(index_handle);
    index.ensure_background_rescan(config.rescan_interval);
    let state = AppState {
        catalog,
        previews,
        config,
        accounts,
        setup_lock: Arc::new(Mutex::new(())),
        sessions: Arc::new(Mutex::new(HashMap::new())),
        throttles: Arc::new(Mutex::new(ThrottleState::default())),
        admission: Arc::new(tokio::sync::Semaphore::new(ADMISSION_LIMIT)),
        login_slots: Arc::new(tokio::sync::Semaphore::new(LOGIN_CONCURRENCY_LIMIT)),
        index,
    };
    Ok(Router::new()
        .route("/login", get(login_page).post(login))
        .route("/logout", get(logout).post(logout))
        .route("/setup", get(setup_page).post(setup))
        // The library and its three siblings (spec §2).
        .route("/", get(library_page))
        .route("/folders", get(folders_page))
        .route("/collections", get(collections_page))
        .route("/search", get(search_page))
        .route("/image/*path", get(image_page))
        // Old links keep working, pointing at the new form (spec §2).
        .route("/folder/*path", get(folder_redirect))
        .route("/collection/*path", get(collection_redirect))
        .route("/media/*path", get(media::media))
        .route("/thumb/*path", get(media::media))
        .route("/raw/*path", get(media::media))
        // The hashed stylesheet and script (spec §0). Only the current
        // hash resolves: a stale URL is a 404, which a cached page
        // cannot hit.
        .route(&assets::css_route(), get(css_asset))
        .route(&assets::js_route(), get(js_asset))
        // The JSON the HTML pages read from.
        .route("/api/view", get(view_json))
        .route("/api/sidebar", get(sidebar_json))
        // The JSON agents read, unchanged.
        .route("/api/folder", get(folder_root_json))
        .route("/api/folder/*path", get(folder_json))
        .route("/api/collection/*path", get(collection_json))
        .route("/api/image/*path", get(image_json))
        .layer(middleware::from_fn_with_state(state.clone(), auth))
        .with_state(state))
}

/// What a request has to satisfy before it reaches a page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Gate {
    /// `--auth none`: every page is public and there are no accounts
    /// at all.
    Open,
    /// No account and no passcode: every page belongs to `/setup`,
    /// which creates the owner.
    Setup,
    /// Accounts exist (or a passcode does): a session is required.
    Login,
}

/// Decide the gate for this request.
///
/// A *missing* accounts file is the one shape that means "no users"
/// and an open setup. A store that cannot be read at request time is
/// not that: it is an error, and the callers answer 500 with the
/// store's own message. Startup refuses such a store outright, so the
/// error branch fires only when it breaks while the server runs — and
/// a read error must never read as "no owner: open setup".
fn gate(state: &AppState) -> Result<Gate, accounts::AccountsError> {
    if state.config.auth == AuthMode::None {
        return Ok(Gate::Open);
    }
    let has_users = state.accounts.has_users()?;
    Ok(if !has_users && state.config.passcode.is_none() {
        Gate::Setup
    } else {
        Gate::Login
    })
}

async fn auth(State(state): State<AppState>, request: Request<Body>, next: Next) -> Response {
    let path = request.uri().path();
    // The stylesheet and script carry no library data and the sign-in and
    // setup pages need them to render, so they are served before the gate.
    // Gating them would redirect the sign-in page's own stylesheet back to
    // the sign-in page, which then renders unstyled.
    if path.starts_with("/assets/") {
        return next.run(request).await;
    }
    let gate = match gate(&state) {
        Ok(gate) => gate,
        // The store was fine at startup and broke while running: fail
        // the request with the store's error, instead of guessing —
        // "no users" here would hand the first visitor an open setup.
        Err(err) => return store_error(err),
    };
    match gate {
        Gate::Open => {
            // Nothing to sign in to: `/login` and `/logout` land on
            // the library, and owner creation is not offered here at
            // all.
            if path == "/setup" {
                return error_page(
                    &state,
                    StatusCode::NOT_FOUND,
                    "There is no setup page on this server.",
                );
            }
            if path == "/login" || path == "/logout" {
                return Redirect::to("/").into_response();
            }
            return next.run(request).await;
        }
        Gate::Setup => {
            if path == "/setup" {
                return next.run(request).await;
            }
            return Redirect::to("/setup").into_response();
        }
        Gate::Login => {}
    }

    if path == "/setup" {
        return error_page(
            &state,
            StatusCode::NOT_FOUND,
            "There is no setup page on this server.",
        );
    }

    if path == "/login" || path == "/logout" {
        return next.run(request).await;
    }

    let token = request
        .headers()
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| cookie_value(v, &state.config.cookie_name));
    let authenticated = if let Some(token) = token {
        let now = now_seconds();
        let mut sessions = state.sessions.lock().unwrap();
        match sessions.get(&token).copied() {
            Some(expiry) if expiry > now => true,
            Some(_) => {
                sessions.remove(&token);
                false
            }
            None => false,
        }
    } else {
        false
    };
    if authenticated {
        let mut response = next.run(request).await;
        response
            .headers_mut()
            .entry(header::CACHE_CONTROL)
            .or_insert(HeaderValue::from_static("private, no-cache"));
        return response;
    }
    if path.starts_with("/api/") {
        return (StatusCode::UNAUTHORIZED, "authentication required").into_response();
    }
    Redirect::to("/login").into_response()
}

/// The setup form, optionally preceded by an error paragraph. This is
/// the single place that knows the form's fields, so the weak-password
/// warning and its checkbox live here and not in every error page.
fn setup_form_html(error: Option<&str>) -> String {
    let error = error
        .map(|message| format!("<p>{}</p>", crate::ui::escape_html(message)))
        .unwrap_or_default();
    format!(
        "{error}<form method=\"post\">\
         <h2>Welcome to dimagine</h2>\
         <p>Create the owner account to finish server setup.</p>\
         <p>Until this account exists, the first visitor can claim this server.</p>\
         <label>Email <input name=\"email\" type=\"email\" autocomplete=\"email\" autofocus required></label>\
         <label>Password <input name=\"password\" type=\"password\" autocomplete=\"new-password\" required></label>\
         <label>Confirm password <input name=\"confirm_password\" type=\"password\" autocomplete=\"new-password\" required></label>\
         <label>Use this weak password anyway <input name=\"allow_weak\" type=\"checkbox\" value=\"yes\"></label>\
         <p>Any password is accepted, but fewer than {WEAK_PASSWORD_LENGTH} characters is easy to guess: \
         tick the box to use one anyway.</p>\
         <button type=\"submit\">Create the owner account</button>\
         </form>"
    )
}

/// A setup error page, so every refusal renders the same form again.
fn setup_error(status: StatusCode, error: &str) -> Response {
    let html = layout("Initial Setup", &setup_form_html(Some(error)));
    let mut response = (status, Html(html)).into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// A setup response that is not an error page: the redirects of the
/// setup flow, with `no-store` like every other credential-bearing
/// response. Error pages carry their own `no-store`.
fn setup_response(mut response: Response) -> Response {
    response
        .headers_mut()
        .entry(header::CACHE_CONTROL)
        .or_insert(HeaderValue::from_static("no-store"));
    response
}

async fn setup_page(State(state): State<AppState>) -> Response {
    let has_users = match state.accounts.has_users() {
        Ok(has_users) => has_users,
        Err(err) => return store_error(err),
    };
    if has_users || state.config.passcode.is_some() {
        return error_page(
            &state,
            StatusCode::NOT_FOUND,
            "There is no setup page on this server.",
        );
    }
    let html = layout("Initial Setup", &setup_form_html(None));
    let mut response = Html(html).into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

#[derive(Deserialize)]
struct SetupForm {
    #[serde(default)]
    email: String,
    #[serde(default)]
    password: String,
    #[serde(default)]
    confirm_password: String,
    /// The "Use this weak password anyway" checkbox. A checkbox posts
    /// its `value` when ticked and nothing when not, so any non-empty
    /// value here is an explicit confirmation.
    #[serde(default)]
    allow_weak: String,
}

async fn setup(
    State(state): State<AppState>,
    client: Option<axum::extract::ConnectInfo<SocketAddr>>,
    headers: HeaderMap,
    Form(form): Form<SetupForm>,
) -> Response {
    let has_users = match state.accounts.has_users() {
        Ok(has_users) => has_users,
        Err(err) => return store_error(err),
    };
    if has_users || state.config.passcode.is_some() {
        return error_page(
            &state,
            StatusCode::NOT_FOUND,
            "There is no setup page on this server.",
        );
    }

    // Refuse everything that can be refused without waiting on the
    // lock, so a request that is going to fail never queues behind a
    // slow argon2 hash.
    let trimmed_email = form.email.trim();
    if trimmed_email.is_empty() || !trimmed_email.contains('@') {
        return setup_error(
            StatusCode::BAD_REQUEST,
            "Please enter a valid email address.",
        );
    }

    if form.password != form.confirm_password {
        return setup_error(StatusCode::BAD_REQUEST, "Passwords do not match.");
    }

    if form.password.is_empty() {
        return setup_error(StatusCode::BAD_REQUEST, "Password cannot be empty.");
    }

    // No length minimum: a short password is accepted, but only when
    // the operator ticked the box saying they know it is weak.
    if is_weak_password(&form.password) && form.allow_weak.trim().is_empty() {
        return setup_error(
            StatusCode::BAD_REQUEST,
            &format!(
                "This password is fewer than {WEAK_PASSWORD_LENGTH} characters and is easy to guess. \
                 Tick \"Use this weak password anyway\" to use it."
            ),
        );
    }

    // One critical section spans the re-check and the account creation.
    // A second concurrent request blocks on this lock and then finds
    // an owner already exists, so it is sent to the login page
    // instead of creating a second account (RW25 F-2).
    let setup_guard = state.setup_lock.lock().unwrap();
    match state.accounts.has_users() {
        Ok(true) => return setup_response(Redirect::to("/login").into_response()),
        Ok(false) => {}
        Err(err) => return store_error(err),
    }

    if let Err(err) = state
        .accounts
        .create_user(trimmed_email, &form.password, "owner")
    {
        // Setup stays open: a transient store failure must not close
        // it, or the operator would have to restart the server.
        let html = layout(
            "Initial Setup",
            &format!(
                "<p>Failed to create account: {}</p>",
                crate::ui::escape_html(&err.to_string())
            ),
        );
        let mut response = (StatusCode::INTERNAL_SERVER_ERROR, Html(html)).into_response();
        response
            .headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        return response;
    }
    drop(setup_guard);

    let token = uuid::Uuid::new_v4().to_string();
    store_session(
        &mut state.sessions.lock().unwrap(),
        token.clone(),
        now_seconds(),
    );
    let mut response = Redirect::to("/").into_response();
    let cookie = format!(
        "{}={}; Path=/; HttpOnly; SameSite=Strict; Max-Age={}{}",
        state.config.cookie_name,
        token,
        SESSION_SECONDS,
        if cookie_secure(&state, client.map(|c| c.0), &headers) {
            "; Secure"
        } else {
            ""
        }
    );
    if let Ok(value) = HeaderValue::from_str(&cookie) {
        response.headers_mut().insert(header::SET_COOKIE, value);
    }
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

async fn login_page(State(state): State<AppState>) -> Response {
    let gate = match gate(&state) {
        Ok(gate) => gate,
        Err(err) => return store_error(err),
    };
    match gate {
        // No login exists in this mode: send the visitor to the
        // library.
        Gate::Open => return Redirect::to("/").into_response(),
        // No owner and no passcode: the visitor creates the owner.
        Gate::Setup => return Redirect::to("/setup").into_response(),
        Gate::Login => {}
    }
    let has_users = match state.accounts.has_users() {
        Ok(has_users) => has_users,
        Err(err) => return store_error(err),
    };

    let form_html = if has_users {
        "<form method=\"post\">\
         <label>Email <input name=\"email\" type=\"email\" autocomplete=\"email\" autofocus required></label>\
         <label>Password <input name=\"password\" type=\"password\" autocomplete=\"current-password\" required></label>\
         <button>Sign in</button>\
         </form>"
            .to_string()
    } else {
        "<form method=\"post\">\
         <label>Passcode <input name=\"passcode\" type=\"password\" autocomplete=\"current-password\" autofocus></label>\
         <button>Sign in</button>\
         </form>"
            .to_string()
    };

    let mut response = Html(layout("Sign in", &form_html)).into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

#[derive(Deserialize, Default)]
struct LoginForm {
    #[serde(default)]
    email: String,
    #[serde(default)]
    password: String,
    #[serde(default)]
    passcode: String,
}

async fn login(
    State(state): State<AppState>,
    client: Option<axum::extract::ConnectInfo<SocketAddr>>,
    headers: HeaderMap,
    Form(form): Form<LoginForm>,
) -> Response {
    // Fail closed, as in the auth middleware: a store that cannot be
    // read fails the attempt with 500, because an unreadable store
    // must never read as "no users" and reopen setup.
    match gate(&state) {
        Err(err) => return store_error(err),
        Ok(Gate::Open) => return Redirect::to("/").into_response(),
        Ok(Gate::Setup) => return Redirect::to("/setup").into_response(),
        Ok(Gate::Login) => {}
    }
    let has_users = match state.accounts.has_users() {
        Ok(has_users) => has_users,
        Err(err) => return store_error(err),
    };

    let client_key =
        resolve_client_ip(client.map(|c| c.0), &headers, &state.config.trusted_proxies);
    // Bound how many guesses are in flight before anything else: a
    // parallel wave of guesses is refused without the passcode ever
    // being compared. The hint says how long a busy slot can still
    // hold, not a best case — and the slot outlives the delay: it is
    // held through the argon2 check that follows, so the hint rounds
    // the hold up to the next whole second (RW54 review Low #4).
    let _slot = match state.login_slots.clone().try_acquire_owned() {
        Ok(slot) => slot,
        Err(_) => return login_refused(LOGIN_MAX_DELAY_SECS + 1),
    };
    // Then charge the attempt to the per-client and the global budget,
    // still before comparing, so an exhausted budget refuses without
    // comparing and response timing never reveals whether a guess was
    // correct: every attempt that is compared pays the same escalating
    // delay.
    let delay_step = {
        let mut throttle = state.throttles.lock().unwrap();
        match throttle.admit(&client_key, now_seconds()) {
            LoginGate::Compare(step) => step,
            LoginGate::Refused { retry_after } => return login_refused(retry_after),
        }
    };
    let millis = (100u64.saturating_mul(1u64.checked_shl(delay_step.min(6)).unwrap_or(u64::MAX)))
        .min(LOGIN_MAX_DELAY_SECS * 1000);
    tokio::time::sleep(std::time::Duration::from_millis(millis)).await;

    let authenticated = if has_users {
        let user_opt = state
            .accounts
            .find_user_by_email(&form.email)
            .ok()
            .flatten();
        if let Some(user) = user_opt {
            accounts::verify_password(&form.password, &user.password_hash)
        } else {
            const DUMMY_HASH: &str = "$argon2id$v=19$m=19456,t=2,p=1$eW91cnNhbHQxMjM0NTY3OA$AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
            let _ = accounts::verify_password(&form.password, DUMMY_HASH);
            false
        }
    } else if let Some(ref expected_passcode) = state.config.passcode {
        form.passcode
            .as_bytes()
            .ct_eq(expected_passcode.as_bytes())
            .unwrap_u8()
            == 1
    } else {
        false
    };

    if !authenticated {
        let error_body = if has_users {
            "<p>Incorrect email or password.</p>\
             <form method=\"post\">\
             <label>Email <input name=\"email\" type=\"email\" autofocus required></label>\
             <label>Password <input name=\"password\" type=\"password\" required></label>\
             <button>Sign in</button>\
             </form>"
                .to_string()
        } else {
            "<p>Incorrect passcode.</p>\
             <form method=\"post\">\
             <label>Passcode <input name=\"passcode\" type=\"password\" autofocus></label>\
             <button>Sign in</button>\
             </form>"
                .to_string()
        };
        let mut response = (
            StatusCode::UNAUTHORIZED,
            Html(layout("Sign in", &error_body)),
        )
            .into_response();
        response
            .headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        return response;
    }

    {
        let mut throttles = state.throttles.lock().unwrap();
        throttles.record_success(&client_key);
    }
    let token = uuid::Uuid::new_v4().to_string();
    store_session(
        &mut state.sessions.lock().unwrap(),
        token.clone(),
        now_seconds(),
    );
    let mut response = Redirect::to("/").into_response();
    let cookie = format!(
        "{}={}; Path=/; HttpOnly; SameSite=Strict; Max-Age={}{}",
        state.config.cookie_name,
        token,
        SESSION_SECONDS,
        if cookie_secure(&state, client.map(|c| c.0), &headers) {
            "; Secure"
        } else {
            ""
        }
    );
    if let Ok(value) = HeaderValue::from_str(&cookie) {
        response.headers_mut().insert(header::SET_COOKIE, value);
    }
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

async fn logout(
    State(state): State<AppState>,
    client: Option<axum::extract::ConnectInfo<SocketAddr>>,
    headers: HeaderMap,
) -> Response {
    let token = headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| cookie_value(v, &state.config.cookie_name));
    if let Some(token) = token {
        state.sessions.lock().unwrap().remove(&token);
    }
    let mut response = Redirect::to("/login").into_response();
    let cookie = format!(
        "{}=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0{}",
        state.config.cookie_name,
        if cookie_secure(&state, client.map(|c| c.0), &headers) {
            "; Secure"
        } else {
            ""
        }
    );
    if let Ok(value) = HeaderValue::from_str(&cookie) {
        response.headers_mut().insert(header::SET_COOKIE, value);
    }
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// Decide whether a session cookie must carry `Secure`.
///
/// `Secure` is set when the viewer is served directly over HTTPS
/// (`config.https`, raised by the `--secure-cookies` flag), or when
/// the request arrived through a trusted reverse proxy that terminated
/// TLS: the TCP peer is a configured trusted proxy and its
/// `X-Forwarded-Proto` header says `https`. The header is honoured
/// only from a trusted peer, so a direct client can never influence
/// the flag.
fn cookie_secure(
    state: &AppState,
    peer: Option<std::net::SocketAddr>,
    headers: &HeaderMap,
) -> bool {
    if state.config.https {
        return true;
    }
    let Some(peer) = peer else {
        return false;
    };
    if !state.config.trusted_proxies.contains(&peer.ip()) {
        return false;
    }
    headers
        .get("x-forwarded-proto")
        .and_then(|value| value.to_str().ok())
        .and_then(|proto| proto.split(',').next())
        .is_some_and(|proto| proto.trim().eq_ignore_ascii_case("https"))
}

/// `GET /assets/app-<hash>.css` — the stylesheet, immutable until it
/// changes.
async fn css_asset(uri: axum::http::Uri) -> Response {
    asset(uri.path(), "text/css; charset=utf-8", assets::css_at)
}

/// `GET /assets/app-<hash>.js` — the enhancement script.
async fn js_asset(uri: axum::http::Uri) -> Response {
    asset(uri.path(), "text/javascript; charset=utf-8", assets::js_at)
}

/// One asset response. The URL carries a hash of the body, so it can
/// be cached for a year: a different body is a different URL.
fn asset(
    path: &str,
    content_type: &'static str,
    body: fn(&str) -> Option<&'static str>,
) -> Response {
    let Some(body) = body(path) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let mut response = Response::new(Body::from(body));
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=31536000, immutable"),
    );
    response
}

fn store_session(sessions: &mut HashMap<String, u64>, token: String, now: u64) {
    sessions.retain(|_, expiry| *expiry > now);
    if sessions.len() >= 10_000 {
        if let Some(oldest) = sessions
            .iter()
            .min_by_key(|(_, expiry)| *expiry)
            .map(|(token, _)| token.clone())
        {
            sessions.remove(&oldest);
        }
    }
    sessions.insert(token, now + SESSION_SECONDS);
}

fn cookie_value(cookies: &str, name: &str) -> Option<String> {
    cookies
        .split(';')
        .filter_map(|p| p.trim().split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v.to_string())
}

fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Run the viewer on a pre-bound TCP listener. Callers choose the bind
/// address.
pub async fn serve(
    listener: tokio::net::TcpListener,
    app: Router,
    config: &ServeConfig,
) -> std::io::Result<()> {
    let address = listener.local_addr()?;
    if default_passcode_warning(config, address) {
        eprintln!("WARNING: the default passcode is active on a non-loopback address");
    }
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
}

fn default_passcode_warning(config: &ServeConfig, address: SocketAddr) -> bool {
    config.passcode.as_deref() == Some("2333") && !address.ip().is_loopback()
}

#[cfg(test)]
mod regression_unit_tests {
    use super::*;

    #[test]
    fn session_storage_prunes_expired_tokens_and_stays_bounded() {
        let mut sessions = (0..10_000)
            .map(|index| (format!("session-{index}"), 100 + index))
            .collect::<HashMap<_, _>>();
        store_session(&mut sessions, "fresh".to_owned(), 50);
        assert_eq!(sessions.len(), 10_000);
        assert!(sessions.contains_key("fresh"));
        assert!(!sessions.contains_key("session-0"));

        sessions.insert("expired".to_owned(), 1);
        store_session(&mut sessions, "after-expiry".to_owned(), 200);
        assert!(!sessions.contains_key("expired"));
        assert!(sessions.contains_key("after-expiry"));
        assert!(sessions.len() <= 10_000);
    }

    #[test]
    fn login_budget_bounds_comparisons_per_client_and_globally() {
        // One client cannot spend more than its per-window budget,
        // however many guesses it fires: only the first ones are ever
        // compared.
        let mut throttle = ThrottleState::default();
        let mut compared = 0;
        for attempt in 0..1_000u64 {
            // Two guesses per second: the whole wave is inside one
            // window.
            if matches!(throttle.admit("one", attempt / 2), LoginGate::Compare(_)) {
                compared += 1;
            }
        }
        assert_eq!(compared, LOGIN_FAILURE_BUDGET);
        // A refused attempt says when the budget comes back...
        assert!(matches!(
            throttle.admit("one", LOGIN_BUDGET_WINDOW_SECS / 2),
            LoginGate::Refused { retry_after } if retry_after > 0
        ));
        // ...and a window that has passed restores it: no permanent
        // lockout.
        let mut compared = 0;
        for attempt in 0..1_000u64 {
            let now = LOGIN_BUDGET_WINDOW_SECS + attempt / 2;
            if matches!(throttle.admit("one", now), LoginGate::Compare(_)) {
                compared += 1;
            }
        }
        assert_eq!(compared, LOGIN_FAILURE_BUDGET);

        // Spreading the guesses over many clients cannot beat the
        // global budget either.
        let mut throttle = ThrottleState::default();
        let mut compared = 0;
        for attempt in 0..1_000u64 {
            if matches!(
                throttle.admit(&format!("client-{attempt}"), 0),
                LoginGate::Compare(_)
            ) {
                compared += 1;
            }
        }
        assert_eq!(compared, LOGIN_GLOBAL_FAILURE_BUDGET);
        assert!(matches!(
            throttle.admit("client-new", 0),
            LoginGate::Refused { .. }
        ));

        // A success restores that client's own budget and only that
        // client's: the global allowance is not handed back.
        let mut throttle = ThrottleState::default();
        for attempt in 0..LOGIN_FAILURE_BUDGET as u64 {
            throttle.admit("one", attempt);
        }
        throttle.record_success("one");
        assert!(matches!(
            throttle.admit("one", LOGIN_FAILURE_BUDGET as u64),
            LoginGate::Compare(_)
        ));
        assert_eq!(throttle.clients["one"].spent, 1);
        assert_eq!(
            throttle.global.spent,
            LOGIN_FAILURE_BUDGET + 1,
            "a success must not hand back the global allowance"
        );
    }

    #[test]
    fn login_budget_keeps_tracked_clients_bounded_and_saturates() {
        // One window per global budget's worth of guesses, so the map
        // really does overflow and the eviction path runs.
        let mut throttle = ThrottleState::default();
        let mut admitted = 0;
        for index in 0..2_000u64 {
            let now = index / LOGIN_GLOBAL_FAILURE_BUDGET as u64 * LOGIN_BUDGET_WINDOW_SECS;
            if matches!(
                throttle.admit(&format!("client-{index}"), now),
                LoginGate::Compare(_)
            ) {
                admitted += 1;
            }
        }
        assert_eq!(admitted, 2_000);
        assert!(throttle.clients.len() <= MAX_TRACKED_CLIENTS);
        // Counters saturate instead of wrapping, and a refused attempt
        // is not charged at all.
        let mut only = ThrottleState::default();
        only.global.spent = u32::MAX;
        only.admit("one", 0);
        assert_eq!(only.global.spent, u32::MAX);
        assert!(only.clients.is_empty());
    }

    #[test]
    fn configured_budget_pushes_a_four_digit_keyspace_past_an_hour() {
        // Projection, not a wall-clock measurement: at the configured
        // budget an attacker needs at least this long to try every
        // four-digit passcode, whichever bound binds first.
        const FOUR_DIGIT_KEYSPACE: u64 = 10_000;
        const ONE_HOUR: u64 = 60 * 60;
        let per_client_windows = FOUR_DIGIT_KEYSPACE.div_ceil(LOGIN_FAILURE_BUDGET as u64);
        let global_windows = FOUR_DIGIT_KEYSPACE.div_ceil(LOGIN_GLOBAL_FAILURE_BUDGET as u64);
        let per_client = per_client_windows * LOGIN_BUDGET_WINDOW_SECS;
        let global = global_windows * LOGIN_BUDGET_WINDOW_SECS;
        let worst_case = per_client.min(global);
        assert!(
            worst_case >= ONE_HOUR,
            "full keyspace reachable in {worst_case}s, under the {ONE_HOUR}s floor \
             (per-client {per_client}s, global {global}s)"
        );
    }

    #[test]
    fn default_passcode_warning_only_applies_to_non_loopback_binds() {
        let config = ServeConfig::default();
        assert!(!default_passcode_warning(
            &config,
            "127.0.0.1:3000".parse().unwrap()
        ));
        assert!(default_passcode_warning(
            &config,
            "0.0.0.0:3000".parse().unwrap()
        ));
        let custom = ServeConfig {
            passcode: Some("different".to_owned()),
            ..config
        };
        assert!(!default_passcode_warning(
            &custom,
            "0.0.0.0:3000".parse().unwrap()
        ));
    }

    #[test]
    fn trusted_proxy_client_ip_resolution() {
        let trusted_ip: std::net::IpAddr = "127.0.0.1".parse().unwrap();
        let untrusted_peer: std::net::SocketAddr = "198.51.100.7:40000".parse().unwrap();
        let trusted_peer: std::net::SocketAddr = "127.0.0.1:40000".parse().unwrap();

        // Untrusted peer with X-Forwarded-For is ignored
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", "203.0.113.195".parse().unwrap());
        assert_eq!(
            resolve_client_ip(Some(untrusted_peer), &headers, &[trusted_ip]),
            "198.51.100.7"
        );

        // Trusted proxy with single hop X-Forwarded-For
        assert_eq!(
            resolve_client_ip(Some(trusted_peer), &headers, &[trusted_ip]),
            "203.0.113.195"
        );

        // Trusted proxy with multi-hop X-Forwarded-For extracts LAST
        // hop
        let mut multi_headers = HeaderMap::new();
        multi_headers.insert(
            "x-forwarded-for",
            "10.0.0.1, 192.168.1.1, 203.0.113.50".parse().unwrap(),
        );
        assert_eq!(
            resolve_client_ip(Some(trusted_peer), &multi_headers, &[trusted_ip]),
            "203.0.113.50"
        );

        // Trusted proxy with invalid last hop falls back to peer IP
        let mut invalid_headers = HeaderMap::new();
        invalid_headers.insert("x-forwarded-for", "invalid-ip-string".parse().unwrap());
        assert_eq!(
            resolve_client_ip(Some(trusted_peer), &invalid_headers, &[trusted_ip]),
            "127.0.0.1"
        );

        // Trusted proxy with missing header returns peer IP
        let empty_headers = HeaderMap::new();
        assert_eq!(
            resolve_client_ip(Some(trusted_peer), &empty_headers, &[trusted_ip]),
            "127.0.0.1"
        );

        // No peer addr returns "unknown"
        assert_eq!(resolve_client_ip(None, &headers, &[trusted_ip]), "unknown");
    }

    /// W27f copy nit: the weak-password hint reads "guess: tick", with the
    /// space a Rust line continuation would otherwise swallow.
    #[test]
    fn the_setup_hint_spells_guess_tick_with_a_space() {
        let html = setup_form_html(None);
        assert!(html.contains("easy to guess: tick the box"), "{html}");
        assert!(!html.contains("guess:tick"), "{html}");
    }
}
