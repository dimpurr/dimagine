#![cfg(feature = "serve")]
//! Compiled only when the matching built-in plugin feature is on; with
//! --no-default-features the plugin subcommands do not exist.

use std::io::{BufRead, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use dimagine_serve::NO_LOGIN_BANNER;

const BIN: &str = env!("CARGO_BIN_EXE_dimagine");

const PNG: &[u8] = include_bytes!("../../../tests/fixtures/pixel.png");
const JPG: &[u8] = include_bytes!("../../../tests/fixtures/pixel.jpg");

/// Holds the temp dir alive for the whole test and hands out the library
/// folder it contains; the guard must outlive every spawned server.
struct Library {
    _dir: DirGuard,
    root: PathBuf,
}

struct DirGuard(PathBuf);

impl Drop for DirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn library(tag: &str) -> Library {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let name = format!(
        "dimagine-cli-serve-{}-{}-{}.tmp",
        tag,
        std::process::id(),
        COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    let outer = std::env::var("CARGO_TARGET_TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir())
        .join(name);
    let root = outer.join("lab");
    std::fs::create_dir_all(root.join("sub")).expect("create serve library");
    std::fs::write(root.join("girl.jpg"), JPG).unwrap();
    std::fs::write(root.join("sub/cat.png"), PNG).unwrap();
    Library {
        _dir: DirGuard(outer),
        root,
    }
}

fn empty_dir(tag: &str) -> PathBuf {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let name = format!(
        "dimagine-cli-serve-empty-{}-{}-{}.tmp",
        tag,
        std::process::id(),
        COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    let dir = std::env::var("CARGO_TARGET_TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir())
        .join(name);
    std::fs::create_dir_all(&dir).expect("create dir");
    dir
}

/// A throwaway state directory for one spawned server; removed on drop so no
/// test ever reads or writes the real user state directory.
fn state_dir() -> DirGuard {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let name = format!(
        "dimagine-cli-serve-state-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    let dir = std::env::var("CARGO_TARGET_TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir())
        .join(name);
    std::fs::create_dir_all(&dir).expect("create state dir");
    DirGuard(dir)
}

struct Server {
    child: Option<Child>,
    address: String,
    auth: String,
    state: Option<DirGuard>,
    stdout: std::process::ChildStdout,
}

impl Server {
    fn start(library: &Path, extra: &[&str], envs: &[(&str, &str)]) -> Server {
        let state = state_dir();
        let mut server = Server::start_inner(library, extra, envs, &state.0);
        server.state = Some(state);
        server
    }

    /// Start a server against a caller-owned state directory, so a test can
    /// restart it and still see the same accounts.
    fn start_with_data_dir(
        library: &Path,
        extra: &[&str],
        envs: &[(&str, &str)],
        data_dir: &Path,
    ) -> Server {
        Server::start_inner(library, extra, envs, data_dir)
    }

    fn start_inner(
        library: &Path,
        extra: &[&str],
        envs: &[(&str, &str)],
        data_dir: &Path,
    ) -> Server {
        let mut command = Command::new(BIN);
        let mut all: Vec<String> = vec![
            "--json".into(),
            "serve".into(),
            "--library".into(),
            library.to_string_lossy().into_owned(),
            "--data-dir".into(),
            data_dir.to_string_lossy().into_owned(),
        ];
        for value in extra {
            all.push((*value).to_string());
        }
        let all: Vec<&str> = all.iter().map(String::as_str).collect();
        command
            .args(&all)
            .env_remove("DIMAGINE_PASSCODE")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(Stdio::null());
        for (key, value) in envs {
            command.env(key, value);
        }
        Server::spawn(command)
    }

    /// The isolated state directory this server was started with.
    fn state_dir(&self) -> &Path {
        &self.state.as_ref().expect("server state dir").0
    }

    fn spawn(mut command: Command) -> Server {
        let mut child = command.spawn().expect("spawn dimagine serve");
        let stdout = child.stdout.take().expect("piped stdout");
        let mut reader = std::io::BufReader::new(stdout);
        // The startup document carries the real address when --port 0 asks
        // the OS for a free port. It is flushed before the server blocks.
        let mut buffer = String::new();
        let document = loop {
            let mut line = String::new();
            let read = reader.read_line(&mut line).expect("read startup line");
            assert!(read > 0, "serve exited before its startup document");
            buffer.push_str(&line);
            // The startup document is one compact line, but tolerate
            // pretty-printed successors: keep reading until it parses.
            if let Ok(document) = serde_json::from_str::<serde_json::Value>(&buffer) {
                break document;
            }
        };
        assert_eq!(document["schema"], "dimagine.serve/0.1", "{document}");
        let auth = document["auth"].as_str().expect("auth field").to_string();
        let address = document["address"]
            .as_str()
            .expect("address field")
            .trim_start_matches("http://")
            .to_string();
        let stdout = reader.into_inner();
        wait_listening(&address);
        Server {
            child: Some(child),
            address,
            auth,
            state: None,
            stdout,
        }
    }

    fn url(&self) -> &str {
        &self.address
    }

    fn auth(&self) -> &str {
        &self.auth
    }

    /// Kill the server and hand back its stderr; the tests assert on the
    /// crate's warnings there.
    fn kill(mut self) -> String {
        // Drop runs shutdown again, which is now a no-op.
        self.shutdown()
    }

    /// Kill the child (if still there), wait for it, and drain its output
    /// pipes. Killing first guarantees both pipes reach EOF, so the reads
    /// finish.
    fn shutdown(&mut self) -> String {
        let mut stderr = String::new();
        if let Some(mut child) = self.child.take() {
            let pipe = child.stderr.take();
            let _ = child.kill();
            let _ = child.wait();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_string(&mut stderr);
            }
        }
        let mut drain = String::new();
        let _ = self.stdout.read_to_string(&mut drain);
        stderr
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        // A panicking test must not leak its server process.
        let _ = self.shutdown();
    }
}

fn wait_listening(address: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if TcpStream::connect(address).is_ok() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "server never listened on {address}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn connect(address: &str) -> TcpStream {
    let stream = TcpStream::connect(address).expect("connect to server");
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    stream
}

fn http_bytes(address: &str, request: &str) -> Vec<u8> {
    let mut stream = connect(address);
    stream.write_all(request.as_bytes()).expect("send request");
    let mut response = Vec::new();
    stream.read_to_end(&mut response).expect("read response");
    response
}

fn http_request(address: &str, request: &str) -> String {
    String::from_utf8_lossy(&http_bytes(address, request)).into_owned()
}

fn http_get(address: &str, path: &str, cookie: Option<&str>) -> String {
    let cookie_line = cookie
        .map(|value| format!("Cookie: {value}\r\n"))
        .unwrap_or_default();
    http_request(
        address,
        &format!("GET {path} HTTP/1.1\r\nHost: viewer\r\n{cookie_line}Connection: close\r\n\r\n"),
    )
}

fn post_login(address: &str, passcode: &str) -> (String, Option<String>) {
    let body = format!("passcode={passcode}");
    let response = http_request(
        address,
        &format!(
            "POST /login HTTP/1.1\r\nHost: viewer\r\n\
             Content-Type: application/x-www-form-urlencoded\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        ),
    );
    let status = status_of(&response).to_string();
    let cookie = header_of(&response, "set-cookie").map(str::to_string);
    (status, cookie)
}

fn status_of(response: &str) -> &str {
    response
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .expect("an HTTP status line")
}

/// Undo HTTP/1.1 chunked transfer coding: `SIZE\r\n BYTES \r\n` until a zero
/// chunk. Streaming handlers send bodies this way.
fn dechunk(mut body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let line_end = body
            .windows(2)
            .position(|window| window == b"\r\n")
            .expect("chunk size line");
        let size_text = std::str::from_utf8(&body[..line_end])
            .expect("hex size")
            .trim();
        let size = usize::from_str_radix(size_text, 16).expect("hex chunk size");
        body = &body[line_end + 2..];
        assert!(body.len() >= size, "truncated chunked body");
        if size == 0 {
            return out;
        }
        out.extend_from_slice(&body[..size]);
        body = &body[size + 2..];
    }
}

fn header_of<'a>(response: &'a str, name: &str) -> Option<&'a str> {
    let lower = name.to_ascii_lowercase();
    response.lines().skip(1).find_map(|line| {
        let (key, value) = line.split_once(':')?;
        (key.trim().to_ascii_lowercase() == lower).then_some(value.trim())
    })
}

#[test]
fn serve_login_page_and_redirect_for_anonymous() {
    let dir = library("anonymous");
    let server = Server::start(
        &dir.root,
        &["--port", "0"],
        &[("DIMAGINE_PASSCODE", "2333")],
    );
    let address = server.url().to_string();

    let response = http_get(&address, "/", None);
    assert_eq!(status_of(&response), "303");
    assert!(
        header_of(&response, "location")
            .unwrap()
            .starts_with("/login"),
        "{response}"
    );
    let login_page = http_get(&address, "/login", None);
    assert_eq!(status_of(&login_page), "200");
    assert!(login_page.contains("Passcode"), "{login_page}");

    // API paths answer 401 instead of redirecting.
    let api = http_get(&address, "/api/folder", None);
    assert_eq!(status_of(&api), "401", "{api}");

    server.kill();
}

#[test]
fn serve_login_with_default_passcode_shows_the_library() {
    let dir = library("default-pass");
    let server = Server::start(
        &dir.root,
        &["--port", "0"],
        &[("DIMAGINE_PASSCODE", "2333")],
    );
    let address = server.url().to_string();

    let (status, cookie) = post_login(&address, "2333");
    assert_eq!(status, "303");
    let cookie = cookie.expect("login sets a session cookie");
    assert!(cookie.starts_with("dimagine_session="), "{cookie}");

    let page = http_get(&address, "/", Some(&cookie));
    assert_eq!(status_of(&page), "200");
    assert!(page.contains("girl.jpg"), "{page}");
    assert!(page.contains("/folder/sub"), "{page}");
    let sub = http_get(&address, "/folder/sub", Some(&cookie));
    assert_eq!(status_of(&sub), "200");
    assert!(sub.contains("cat.png"), "{sub}");

    // Read-only viewer: originals are served byte-identical through /raw/
    // (the grid uses /thumb/ and the detail view /media/, which serve the
    // generated renditions).
    let media = http_bytes(
        &address,
        &format!(
            "GET /raw/girl.jpg HTTP/1.1\r\nHost: viewer\r\nCookie: {cookie}\r\nConnection: close\r\n\r\n"
        ),
    );
    let header_end = media
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("media response has headers");
    let headers = String::from_utf8_lossy(&media[..header_end]).into_owned();
    assert!(headers.contains("200"), "{headers}");
    let body = &media[header_end + 4..];
    let body = if headers
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked")
    {
        // Streaming handlers use chunked framing; undo it to compare bytes.
        dechunk(body)
    } else {
        body.to_vec()
    };
    assert_eq!(body, JPG);

    // A session cookie without a login does nothing for other clients.
    let stranger = http_get(&address, "/", None);
    assert_eq!(status_of(&stranger), "303", "{stranger}");

    server.kill();
}

#[test]
fn serve_wrong_passcode_is_rejected() {
    let dir = library("wrong-pass");
    let server = Server::start(
        &dir.root,
        &["--port", "0"],
        &[("DIMAGINE_PASSCODE", "2333")],
    );
    let address = server.url().to_string();

    let body = "passcode=wrong";
    let response = http_request(
        &address,
        &format!(
            "POST /login HTTP/1.1\r\nHost: viewer\r\n\
             Content-Type: application/x-www-form-urlencoded\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        ),
    );
    assert_eq!(
        status_of(&response),
        "401",
        "an incorrect passcode is refused: {response}"
    );

    server.kill();
}

#[test]
fn serve_passcode_from_env_overrides_the_default() {
    let dir = library("env-pass");
    let server = Server::start(
        &dir.root,
        &["--port", "0"],
        &[("DIMAGINE_PASSCODE", "open says me")],
    );
    let address = server.url().to_string();

    let (status, _) = post_login(&address, "2333");
    assert_eq!(status, "401", "the default passcode no longer works");

    let (status, cookie) = post_login(&address, "open says me");
    assert_eq!(status, "303");
    let cookie = cookie.expect("session cookie");
    let page = http_get(&address, "/", Some(&cookie));
    assert_eq!(status_of(&page), "200");
    assert!(page.contains("girl.jpg"), "{page}");

    server.kill();
}

#[test]
fn serve_empty_passcode_env_enters_setup_mode() {
    let dir = library("empty-pass");
    let server = Server::start(&dir.root, &["--port", "0"], &[("DIMAGINE_PASSCODE", "")]);
    let address = server.url().to_string();

    let response = http_get(&address, "/", None);
    assert_eq!(status_of(&response), "303");
    assert!(
        header_of(&response, "location")
            .unwrap()
            .starts_with("/setup"),
        "{response}"
    );
    let setup_page = http_get(&address, "/setup", None);
    assert_eq!(status_of(&setup_page), "200");
    assert!(setup_page.contains("Initial Setup"), "{setup_page}");
    // Merely visiting setup does not create an account file.
    assert!(!server.state_dir().join("accounts.json").exists());

    server.kill();
}

#[test]
fn serve_secure_cookies_flag_sets_secure_on_session_cookie() {
    let dir = library("secure-cookies");
    let server = Server::start(
        &dir.root,
        &["--port", "0", "--secure-cookies"],
        &[("DIMAGINE_PASSCODE", "2333")],
    );
    let address = server.url().to_string();

    let (status, cookie) = post_login(&address, "2333");
    assert_eq!(status, "303");
    let cookie = cookie.expect("login sets a session cookie");
    assert!(cookie.contains("Secure"), "{cookie}");

    server.kill();
}

#[test]
fn serve_session_cookie_is_not_secure_over_plain_http_by_default() {
    let dir = library("plain-cookies");
    let server = Server::start(
        &dir.root,
        &["--port", "0"],
        &[("DIMAGINE_PASSCODE", "2333")],
    );
    let address = server.url().to_string();

    let (status, cookie) = post_login(&address, "2333");
    assert_eq!(status, "303");
    let cookie = cookie.expect("login sets a session cookie");
    assert!(!cookie.contains("Secure"), "{cookie}");

    server.kill();
}

#[cfg(unix)]
#[test]
fn serve_warns_once_about_world_readable_store_at_startup() {
    use std::os::unix::fs::PermissionsExt;
    let dir = library("f6-serve-mode");
    let state = state_dir();
    std::fs::create_dir_all(&state.0).unwrap();
    let accounts = state.0.join("accounts.json");
    std::fs::write(
        &accounts,
        r#"{"schema":1,"users":[{"id":"01J9XEXAMPLEULID0000000000","email":"owner@example.com","password_hash":"$argon2id$v=19$dummy","role":"owner","created":"2026-10-05T12:00:00Z"}]}"#,
    )
    .unwrap();
    std::fs::set_permissions(&accounts, std::fs::Permissions::from_mode(0o644)).unwrap();

    // The server loads the store at startup and on every request; the
    // warning must appear exactly once, not once per request.
    let server = Server::start_with_data_dir(&dir.root, &["--port", "0"], &[], &state.0);
    let address = server.url().to_string();
    assert_eq!(status_of(&http_get(&address, "/", None)), "303");
    assert_eq!(status_of(&http_get(&address, "/login", None)), "200");
    let stderr = server.kill();
    let warnings = stderr
        .lines()
        .filter(|line| line.contains("WARNING"))
        .count();
    assert_eq!(warnings, 1, "stderr: {stderr}");
    assert!(stderr.contains("0644"), "{stderr}");
}

#[test]
fn serve_warns_when_trusted_proxy_family_cannot_match_bind() {
    let dir = library("f8-family");
    // ::1 can never be the TCP peer of a server bound to 127.0.0.1.
    let server = Server::start(
        &dir.root,
        &[
            "--port",
            "0",
            "--bind",
            "127.0.0.1",
            "--trusted-proxy",
            "::1",
        ],
        &[("DIMAGINE_PASSCODE", "2333")],
    );
    let address = server.url().to_string();
    // The server still starts and serves.
    assert_eq!(status_of(&http_get(&address, "/", None)), "303");
    let stderr = server.kill();
    assert!(
        stderr.contains("WARNING") && stderr.contains("::1"),
        "expected a family-mismatch warning: {stderr}"
    );
    assert!(
        stderr.contains("IPv6") && stderr.contains("IPv4"),
        "{stderr}"
    );
}

#[test]
fn serve_quiet_when_trusted_proxy_family_matches_bind() {
    let dir = library("f8-family-ok");
    let server = Server::start(
        &dir.root,
        &["--port", "0", "--trusted-proxy", "127.0.0.1"],
        &[("DIMAGINE_PASSCODE", "2333")],
    );
    let address = server.url().to_string();
    assert_eq!(status_of(&http_get(&address, "/", None)), "303");
    let stderr = server.kill();
    assert!(
        !stderr.contains("can never match"),
        "a matching proxy must not warn: {stderr}"
    );
}

#[test]
fn serve_default_passcode_on_non_loopback_bind_warns() {
    let dir = library("warn");
    // Bind to all local IPv4 interfaces while reaching it on loopback; the
    // serve crate must warn that the default passcode is active.
    let server = Server::start(
        &dir.root,
        &["--port", "0", "--bind", "0.0.0.0"],
        &[("DIMAGINE_PASSCODE", "2333")],
    );
    let address = server.url().to_string();
    let (status, cookie) = post_login(&address, "2333");
    assert_eq!(status, "303");
    let page = http_get(&address, "/", Some(&cookie.unwrap()));
    assert_eq!(status_of(&page), "200");
    let stderr = server.kill();
    assert!(
        stderr.contains("WARNING: the default passcode is active"),
        "expected the crate's default-passcode warning: stderr={stderr:?}"
    );
}

#[test]
fn serve_port_in_use_exits_one() {
    let dir = library("occupied");
    // Occupy a port ourselves, then ask the CLI to bind the same one.
    let squatter = TcpListener::bind("127.0.0.1:0").unwrap();
    let taken = squatter.local_addr().unwrap().port();
    let output = Command::new(BIN)
        .args([
            "--json",
            "serve",
            "--library",
            dir.root.to_str().unwrap(),
            "--port",
            &taken.to_string(),
        ])
        .env_remove("DIMAGINE_PASSCODE")
        .output()
        .expect("run dimagine serve");
    assert_eq!(
        output.status.code(),
        Some(1),
        "a taken port is a failed bind: {:?}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let error: serde_json::Value = serde_json::from_str(&stdout).expect("error JSON");
    assert_eq!(error["schema"], "dimagine.error/0.1");
    assert!(
        error["error"].as_str().unwrap().contains("cannot listen"),
        "{error}"
    );
}

/// Run `dimagine serve --json` against a caller-owned state directory and
/// hand back the raw output; used for startup-failure tests where the
/// process must exit before it ever binds a port.
fn run_serve(library: &Path, data_dir: &Path) -> std::process::Output {
    Command::new(BIN)
        .args([
            "--json",
            "serve",
            "--library",
            library.to_str().unwrap(),
            "--data-dir",
            data_dir.to_str().unwrap(),
            "--port",
            "0",
        ])
        .env_remove("DIMAGINE_PASSCODE")
        .output()
        .expect("run dimagine serve")
}

/// Assert the run exited 1 with an `dimagine.error/0.1` document on stdout
/// and return the error text.
fn serve_error_text(output: &std::process::Output) -> String {
    assert_eq!(
        output.status.code(),
        Some(1),
        "a broken accounts store must stop the server: {:?}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let error: serde_json::Value = serde_json::from_str(&stdout).expect("error JSON");
    assert_eq!(error["schema"], "dimagine.error/0.1");
    error["error"].as_str().unwrap().to_owned()
}

fn running_as_root() -> bool {
    std::process::Command::new("id")
        .arg("-u")
        .output()
        .map(|output| String::from_utf8_lossy(&output.stdout).trim() == "0")
        .unwrap_or(false)
}

#[cfg(unix)]
#[test]
fn serve_refuses_to_start_with_unreadable_accounts_file() {
    if running_as_root() {
        return;
    }
    use std::os::unix::fs::PermissionsExt;
    let dir = library("f1-unreadable");
    let state = state_dir();
    std::fs::create_dir_all(&state.0).unwrap();
    let accounts = state.0.join("accounts.json");
    std::fs::write(&accounts, r#"{"schema":1,"users":[]}"#).unwrap();
    std::fs::set_permissions(&accounts, std::fs::Permissions::from_mode(0o000)).unwrap();

    let output = run_serve(&dir.root, &state.0);
    let error = serve_error_text(&output);
    assert!(error.contains("account storage I/O error"), "{error}");
    assert!(error.contains("Permission denied"), "{error}");
    // No startup document: the process must not reach the serving stage.
    assert!(!output.stdout.windows(5).any(|w| w == b"serving"));
}

#[test]
fn serve_refuses_to_start_with_truncated_accounts_file() {
    let dir = library("f1-truncated");
    let state = state_dir();
    std::fs::create_dir_all(&state.0).unwrap();
    std::fs::write(
        state.0.join("accounts.json"),
        r#"{"schema":1,"users":[{"id":"01J9XEXAMPLEULID0000000000","#,
    )
    .unwrap();

    let output = run_serve(&dir.root, &state.0);
    let error = serve_error_text(&output);
    assert!(error.contains("malformed accounts.json"), "{error}");
    // No startup document: the process must not reach the serving stage.
    assert!(!output.stdout.windows(5).any(|w| w == b"serving"));
}

#[test]
fn serve_refuses_to_start_with_empty_accounts_file() {
    let dir = library("f1-empty");
    let state = state_dir();
    std::fs::create_dir_all(&state.0).unwrap();
    std::fs::write(state.0.join("accounts.json"), "").unwrap();

    let output = run_serve(&dir.root, &state.0);
    let error = serve_error_text(&output);
    assert!(error.contains("malformed accounts.json"), "{error}");
    // No startup document: the process must not reach the serving stage.
    assert!(!output.stdout.windows(5).any(|w| w == b"serving"));
}

#[test]
fn serve_refuses_to_start_with_unknown_schema() {
    let dir = library("f1-schema");
    let state = state_dir();
    std::fs::create_dir_all(&state.0).unwrap();
    std::fs::write(
        state.0.join("accounts.json"),
        r#"{"schema":999,"users":[]}"#,
    )
    .unwrap();

    let output = run_serve(&dir.root, &state.0);
    let error = serve_error_text(&output);
    assert!(error.contains("schema 999"), "{error}");
    // No startup document: the process must not reach the serving stage.
    assert!(!output.stdout.windows(5).any(|w| w == b"serving"));
}

#[test]
fn serve_refuses_to_start_when_the_users_key_is_missing() {
    let dir = library("f1-users-key");
    let state = state_dir();
    std::fs::create_dir_all(&state.0).unwrap();
    // The `users` key renamed by another build, and the same file with the key
    // dropped entirely: present, current-schema, but unreadable as a store.
    for contents in [
        r#"{"schema":1,"accounts":[{"id":"01J9XEXAMPLEULID0000000000","email":"owner@example.com","password_hash":"$argon2id$v=19$dummy","role":"owner","created":"2026-10-05T12:00:00Z"}]}"#,
        r#"{"schema":1}"#,
    ] {
        std::fs::write(state.0.join("accounts.json"), contents).unwrap();

        let output = run_serve(&dir.root, &state.0);
        let error = serve_error_text(&output);
        assert!(error.contains("malformed accounts.json"), "{error}");
        assert!(
            error.contains("users"),
            "the error must name the key: {error}"
        );
        // Never falls back to setup: the process must not reach serving.
        assert!(!output.stdout.windows(5).any(|w| w == b"serving"));
        assert!(
            std::fs::read_to_string(state.0.join("accounts.json")).unwrap() == contents,
            "a refused start must not rewrite the store"
        );
    }
}

#[test]
fn serve_first_run_without_an_accounts_file_opens_setup() {
    let dir = library("f1-first-run");
    let state = state_dir();
    std::fs::create_dir_all(&state.0).unwrap();
    assert!(!state.0.join("accounts.json").exists());

    // No store at all is the one shape that still means first run.
    let server = Server::start_with_data_dir(&dir.root, &["--port", "0"], &[], &state.0);
    let address = server.url().to_string();
    assert_eq!(server.auth(), "account");
    let redirect = http_get(&address, "/", None);
    assert_eq!(status_of(&redirect), "303");
    assert!(header_of(&redirect, "location")
        .unwrap()
        .starts_with("/setup"));
    assert!(http_get(&address, "/setup", None).contains("Initial Setup"));
    // Merely serving setup creates no store.
    assert!(!state.0.join("accounts.json").exists());

    // There is no code to hand out, and the reminder says so instead.
    let stderr = server.kill();
    assert!(
        !stderr.contains("setup code") && !stderr.contains("One-time"),
        "no setup code is printed any more: {stderr}"
    );
    assert!(
        stderr.contains("No owner account yet") && stderr.contains("/setup"),
        "the operator is reminded to create the owner: {stderr}"
    );
    // The reminder names the risk and carries nothing secret.
    assert!(stderr.contains("first visitor can claim"), "{stderr}");
    assert!(
        !state.0.join("accounts.json").exists(),
        "the reminder must not create an account"
    );
}

#[test]
fn serve_keeps_unknown_top_level_accounts_fields_across_a_write() {
    let state = state_dir();
    std::fs::create_dir_all(&state.0).unwrap();
    let accounts = state.0.join("accounts.json");
    std::fs::write(
        &accounts,
        r#"{"schema":1,"server_note":"keep me","users":[]}"#,
    )
    .unwrap();
    let data_dir = state.0.to_string_lossy().into_owned();

    // Requiring `users` must not reject or drop unknown fields (ADR-014).
    let output = run_user(
        &[
            "user",
            "create",
            "--data-dir",
            &data_dir,
            "--email",
            "owner@example.com",
            "--password-stdin",
        ],
        "secret123456\n",
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let raw = std::fs::read_to_string(&accounts).unwrap();
    assert!(raw.contains("server_note"), "{raw}");
    assert!(raw.contains("owner@example.com"), "{raw}");
}

#[test]
fn serve_missing_library_exits_one() {
    let outer = empty_dir("missing-lib");
    let output = Command::new(BIN)
        .args([
            "--json",
            "serve",
            "--library",
            outer.join("absent").to_str().unwrap(),
        ])
        .env_remove("DIMAGINE_PASSCODE")
        .output()
        .expect("run dimagine serve");
    assert_eq!(output.status.code(), Some(1));
    let stdout = String::from_utf8_lossy(&output.stdout);
    let error: serde_json::Value = serde_json::from_str(&stdout).expect("error JSON");
    assert_eq!(error["schema"], "dimagine.error/0.1");
    assert!(
        error["error"].as_str().unwrap().contains("not a folder"),
        "{error}"
    );
}

#[test]
fn serve_usage_errors_exit_two() {
    let outer = empty_dir("usage");
    let lib = outer.to_str().unwrap();
    let output = Command::new(BIN)
        .args(["serve", "--library", lib, "--port", "not-a-port"])
        .output()
        .expect("run dimagine serve");
    assert_eq!(output.status.code(), Some(2));
    let output = Command::new(BIN)
        .args(["serve", "--library", lib, "--bind", "no-ip"])
        .output()
        .expect("run dimagine serve");
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn serve_lists_in_help_with_default_bind_and_port() {
    let output = Command::new(BIN)
        .args(["serve", "--help"])
        .output()
        .expect("run dimagine serve --help");
    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("8917"), "{stdout}");
    assert!(stdout.contains("127.0.0.1"), "{stdout}");
    assert!(stdout.contains("DIMAGINE_PASSCODE"), "{stdout}");
}

fn post_form(address: &str, path: &str, body: &str) -> (String, Option<String>) {
    let response = http_request(
        address,
        &format!(
            "POST {path} HTTP/1.1\r\nHost: viewer\r\n\
             Content-Type: application/x-www-form-urlencoded\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        ),
    );
    (
        status_of(&response).to_string(),
        header_of(&response, "set-cookie").map(str::to_string),
    )
}

/// Run a `dimagine user ...` invocation with the given standard input.
fn run_user(args: &[&str], stdin_data: &str) -> std::process::Output {
    let mut child = Command::new(BIN)
        .args(args)
        .env_remove("DIMAGINE_PASSCODE")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn dimagine user");
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(stdin_data.as_bytes());
    }
    child.wait_with_output().expect("wait for dimagine user")
}

#[test]
fn user_cli_create_passwd_list_and_login_round_trip() {
    let dir = library("user-cli");
    let state = state_dir();
    let data_dir = state.0.to_string_lossy().into_owned();

    // An empty state lists nothing.
    let output = run_user(&["user", "list", "--data-dir", &data_dir], "");
    assert_eq!(output.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&output.stdout).trim().is_empty());

    // The first account becomes the owner; the second is a plain user.
    let output = run_user(
        &[
            "user",
            "create",
            "--data-dir",
            &data_dir,
            "--email",
            "owner@example.com",
            "--password-stdin",
        ],
        "secret123456\n",
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("owner@example.com"),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let output = run_user(
        &[
            "user",
            "create",
            "--data-dir",
            &data_dir,
            "--email",
            "second@example.com",
            "--password-stdin",
        ],
        "secret456789\n",
    );
    assert_eq!(output.status.code(), Some(0));

    let listed =
        String::from_utf8_lossy(&run_user(&["user", "list", "--data-dir", &data_dir], "").stdout)
            .into_owned();
    assert!(listed.contains("owner@example.com"), "{listed}");
    assert!(listed.contains("second@example.com"), "{listed}");
    assert!(listed.contains("owner"), "{listed}");
    assert!(listed.contains("user"), "{listed}");

    // A duplicate email is refused.
    let output = run_user(
        &[
            "user",
            "create",
            "--data-dir",
            &data_dir,
            "--email",
            "owner@example.com",
            "--password-stdin",
        ],
        "other1234567\n",
    );
    assert_eq!(output.status.code(), Some(1));

    // Change the owner's password.
    let output = run_user(
        &[
            "user",
            "passwd",
            "--data-dir",
            &data_dir,
            "--email",
            "owner@example.com",
            "--password-stdin",
        ],
        "newpass45678\n",
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    // The running viewer uses the same state directory: setup is gone, the
    // old password is refused, the new one signs in.
    let server = Server::start_with_data_dir(&dir.root, &["--port", "0"], &[], &state.0);
    let address = server.url().to_string();
    assert_eq!(status_of(&http_get(&address, "/setup", None)), "404");
    let redirect = http_get(&address, "/", None);
    assert_eq!(status_of(&redirect), "303");
    assert!(header_of(&redirect, "location")
        .unwrap()
        .starts_with("/login"));
    assert!(http_get(&address, "/login", None).contains("Email"));

    let (status, _) = post_form(
        &address,
        "/login",
        "email=owner%40example.com&password=secret123",
    );
    assert_eq!(status, "401");
    let (status, cookie) = post_form(
        &address,
        "/login",
        "email=owner%40example.com&password=newpass45678",
    );
    assert_eq!(status, "303");
    let cookie = cookie.expect("login sets a session cookie");
    assert_eq!(status_of(&http_get(&address, "/", Some(&cookie))), "200");
    server.kill();
}

/// Build a `script` command that runs `args` under a pseudo-terminal.
/// macOS takes the command after the output file; util-linux wants `-c`.
fn pty_command(args: &[&str]) -> Command {
    let mut command = Command::new("script");
    if cfg!(target_os = "macos") {
        command.arg("-q").arg("/dev/null").args(args);
    } else {
        command.arg("-qec").arg(args.join(" ")).arg("/dev/null");
    }
    command
}

/// Run a command under a pty, feeding `input` to it only after `delay`, and
/// return everything the pty printed. The delay lets the child start and (for
/// a password prompt) disable echo before the input arrives.
fn run_under_pty(args: &[&str], input: &str, delay: Duration) -> String {
    let mut child = pty_command(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn script");
    let mut stdin = child.stdin.take().expect("script stdin");
    let input = input.to_string();
    std::thread::spawn(move || {
        std::thread::sleep(delay);
        let _ = stdin.write_all(input.as_bytes());
        // Hold the pty master open until the child is done with it.
        std::thread::sleep(Duration::from_secs(10));
    });
    let output = child.wait_with_output().expect("wait for script");
    assert!(
        output.status.success(),
        "script failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn interactive_password_prompt_does_not_echo_the_password() {
    let state = state_dir();
    let data_dir = state.0.to_string_lossy().into_owned();
    let password = "secret123456";

    // No --password-stdin: the prompt path runs under a pty, and the password
    // is fed only after the prompt has been printed. With echo disabled by the
    // prompt, the password must not appear in the terminal output.
    let output = run_under_pty(
        &[
            BIN,
            "user",
            "create",
            "--data-dir",
            &data_dir,
            "--email",
            "pty@example.com",
        ],
        &format!("{password}\n"),
        Duration::from_secs(2),
    );

    assert!(output.contains("Password: "), "{output}");
    assert!(
        !output.contains(password),
        "the password was echoed to the terminal: {output}"
    );
    assert!(output.contains("created user pty@example.com"), "{output}");
    assert!(state.0.join("accounts.json").exists());
}

#[test]
fn interactive_prompt_reads_piped_stdin_without_the_flag() {
    let state = state_dir();
    let data_dir = state.0.to_string_lossy().into_owned();

    // No --password-stdin and no terminal: the prompt path falls back to
    // reading the piped line, so `echo <password> | dimagine user create`
    // keeps working.
    let output = run_user(
        &[
            "user",
            "create",
            "--data-dir",
            &data_dir,
            "--email",
            "owner@example.com",
        ],
        "secret123456\n",
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("Password: "),
        "the prompt is still printed: {:?}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(state.0.join("accounts.json").exists());
}

#[cfg(unix)]
#[test]
fn user_cli_warns_about_world_readable_store_and_repairs_it_on_write() {
    use std::os::unix::fs::PermissionsExt;
    let state = state_dir();
    std::fs::create_dir_all(&state.0).unwrap();
    let accounts = state.0.join("accounts.json");
    std::fs::write(&accounts, r#"{"schema":1,"users":[]}"#).unwrap();
    std::fs::set_permissions(&accounts, std::fs::Permissions::from_mode(0o644)).unwrap();
    let data_dir = state.0.to_string_lossy().into_owned();

    // list warns (and does not fail) about the loose mode.
    let output = run_user(&["user", "list", "--data-dir", &data_dir], "");
    assert_eq!(output.status.code(), Some(0));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("WARNING"), "{stderr}");
    assert!(stderr.contains("0644"), "{stderr}");

    // The next write repairs the mode to 0600.
    let output = run_user(
        &[
            "user",
            "create",
            "--data-dir",
            &data_dir,
            "--email",
            "owner@example.com",
            "--password-stdin",
        ],
        "secret123456\n",
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let mode = std::fs::metadata(&accounts).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "the write must repair the mode, got {mode:o}");
}

#[test]
fn user_cli_asks_before_using_a_weak_password() {
    let state = state_dir();
    let data_dir = state.0.to_string_lossy().into_owned();
    let create = |extra: &[&str], stdin_data: &str| {
        let mut args = vec!["user", "create", "--data-dir", &data_dir];
        args.extend_from_slice(extra);
        run_user(&args, stdin_data)
    };

    // With --password-stdin, standard input is carrying the password, so
    // there is nobody to ask: the flag is the confirmation.
    let output = create(
        &["--email", "owner@example.com", "--password-stdin"],
        "short\n",
    );
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("--allow-weak"), "{stderr}");
    assert!(stderr.contains("8 characters"), "{stderr}");
    assert!(!state.0.join("accounts.json").exists());

    // --allow-weak accepts it.
    let output = create(
        &[
            "--email",
            "owner@example.com",
            "--password-stdin",
            "--allow-weak",
        ],
        "short\n",
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(state.0.join("accounts.json").exists());

    // Without --password-stdin the answer comes from standard input: a
    // refusal changes nothing.
    let output = run_user(
        &[
            "user",
            "passwd",
            "--data-dir",
            &data_dir,
            "--email",
            "owner@example.com",
        ],
        "tiny\nn\n",
    );
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("fewer than 8 characters") && stderr.contains("[y/N]"),
        "the question must be asked: {stderr}"
    );
    assert!(stderr.contains("refused"), "{stderr}");

    // Saying yes goes through.
    let output = run_user(
        &[
            "user",
            "passwd",
            "--data-dir",
            &data_dir,
            "--email",
            "owner@example.com",
        ],
        "tiny\ny\n",
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let listed =
        String::from_utf8_lossy(&run_user(&["user", "list", "--data-dir", &data_dir], "").stdout)
            .into_owned();
    assert!(listed.contains("owner@example.com"), "{listed}");

    // A long-enough password is never questioned.
    let state = state_dir();
    let data_dir = state.0.to_string_lossy().into_owned();
    let output = run_user(
        &[
            "user",
            "create",
            "--data-dir",
            &data_dir,
            "--email",
            "owner@example.com",
            "--password-stdin",
        ],
        "12345678\n",
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!String::from_utf8_lossy(&output.stderr).contains("8 characters"));
}

#[test]
fn user_cli_delete_removes_accounts_and_reopens_setup_after_the_last_one() {
    let dir = library("user-delete");
    let state = state_dir();
    let data_dir = state.0.to_string_lossy().into_owned();
    for email in ["owner@example.com", "second@example.com"] {
        let output = run_user(
            &[
                "user",
                "create",
                "--data-dir",
                &data_dir,
                "--email",
                email,
                "--password-stdin",
            ],
            "secret123456\n",
        );
        assert_eq!(output.status.code(), Some(0));
    }

    // An unknown address is an error and deletes nothing.
    let output = run_user(&["user", "delete", "nobody@example.com", "--yes"], "");
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("nobody@example.com"), "{stderr}");
    assert!(String::from_utf8_lossy(
        &run_user(&["user", "list", "--data-dir", &data_dir], "").stdout
    )
    .contains("owner@example.com"));

    // Without --yes the answer is asked for, and "n" deletes nothing.
    let output = run_user(
        &[
            "user",
            "delete",
            "--data-dir",
            &data_dir,
            "second@example.com",
        ],
        "n\n",
    );
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(
        &run_user(&["user", "list", "--data-dir", &data_dir], "").stdout
    )
    .contains("second@example.com"));

    // Answering yes removes that account only.
    let output = run_user(
        &[
            "user",
            "delete",
            "--data-dir",
            &data_dir,
            "second@example.com",
        ],
        "y\n",
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(
        stdout.contains("deleted user second@example.com"),
        "{stdout}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(stderr.contains("second@example.com"), "prompt: {stderr}");
    let listed =
        String::from_utf8_lossy(&run_user(&["user", "list", "--data-dir", &data_dir], "").stdout)
            .into_owned();
    assert!(listed.contains("owner@example.com"), "{listed}");
    assert!(!listed.contains("second@example.com"), "{listed}");

    // A viewer started now still has an owner and asks for a login.
    let server = Server::start_with_data_dir(&dir.root, &["--port", "0"], &[], &state.0);
    let address = server.url().to_string();
    assert_eq!(status_of(&http_get(&address, "/setup", None)), "404");
    server.kill();

    // Deleting the last owner puts the next start back into setup.
    let output = run_user(
        &[
            "user",
            "delete",
            "--yes",
            "--data-dir",
            &data_dir,
            "owner@example.com",
        ],
        "",
    );
    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(
        stdout.contains("deleted user owner@example.com"),
        "{stdout}"
    );
    assert!(stdout.contains("/setup"), "{stdout}");

    let server = Server::start_with_data_dir(&dir.root, &["--port", "0"], &[], &state.0);
    let address = server.url().to_string();
    let redirect = http_get(&address, "/", None);
    assert_eq!(status_of(&redirect), "303");
    assert!(header_of(&redirect, "location")
        .unwrap()
        .starts_with("/setup"));
    // The first visitor can set up again, and the weak-password rule still
    // applies to the new owner.
    let (status, cookie) = post_form(
        &address,
        "/setup",
        "email=newowner%40example.com&password=brandnew&confirm_password=brandnew&allow_weak=yes",
    );
    assert_eq!(status, "303");
    let cookie = cookie.expect("the new owner is signed in");
    assert_eq!(status_of(&http_get(&address, "/", Some(&cookie))), "200");
    server.kill();
}

#[test]
fn user_cli_delete_needs_a_confirmation_or_yes() {
    let state = state_dir();
    let data_dir = state.0.to_string_lossy().into_owned();
    let output = run_user(
        &[
            "user",
            "create",
            "--data-dir",
            &data_dir,
            "--email",
            "owner@example.com",
            "--password-stdin",
        ],
        "secret123456\n",
    );
    assert_eq!(output.status.code(), Some(0));

    // No answer at all (standard input closed) is a no.
    let output = run_user(
        &[
            "user",
            "delete",
            "--data-dir",
            &data_dir,
            "owner@example.com",
        ],
        "",
    );
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(
        &run_user(&["user", "list", "--data-dir", &data_dir], "").stdout
    )
    .contains("owner@example.com"));

    // --yes skips the question and deletes, in JSON too.
    let output = run_user(
        &[
            "--json",
            "user",
            "delete",
            "--data-dir",
            &data_dir,
            "owner@example.com",
            "--yes",
        ],
        "",
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let document: serde_json::Value = serde_json::from_slice(&output.stdout).expect("delete JSON");
    assert_eq!(document["schema"], "dimagine.user/0.1");
    assert_eq!(document["deleted"]["email"], "owner@example.com");
    assert_eq!(document["accounts_left"], 0);
}

#[test]
fn serve_auth_none_serves_the_library_without_a_login() {
    let dir = library("auth-none");
    // A passcode in the environment does not bring a login back.
    let server = Server::start(
        &dir.root,
        &["--port", "0", "--auth", "none"],
        &[("DIMAGINE_PASSCODE", "2333")],
    );
    let address = server.url().to_string();
    assert_eq!(server.auth(), "none");

    // No login, no cookie, no redirect: the library answers straight away.
    let page = http_get(&address, "/", None);
    assert_eq!(status_of(&page), "200");
    assert!(page.contains("girl.jpg"), "{page}");
    assert!(page.contains(NO_LOGIN_BANNER), "{page}");
    assert_eq!(status_of(&http_get(&address, "/api/folder", None)), "200");

    // Every page carries the banner, not just the first one.
    let sub = http_get(&address, "/folder/sub", None);
    assert_eq!(status_of(&sub), "200");
    assert!(sub.contains(NO_LOGIN_BANNER), "{sub}");

    // /login leads to the library, /setup does not exist, and no account is
    // needed at all.
    let login = http_get(&address, "/login", None);
    assert_eq!(status_of(&login), "303");
    assert!(header_of(&login, "location").unwrap().starts_with('/'));
    assert_eq!(status_of(&http_get(&address, "/setup", None)), "404");
    assert!(!server.state_dir().join("accounts.json").exists());

    // Loopback is the common case, so it stays quiet.
    let stderr = server.kill();
    assert!(
        !stderr.contains("--auth none is serving"),
        "a loopback bind must not warn: {stderr}"
    );
}

#[test]
fn serve_auth_none_warns_once_on_a_non_loopback_bind() {
    let dir = library("auth-none-bind");
    // Binding to all local IPv4 interfaces while reaching it on loopback.
    let server = Server::start(
        &dir.root,
        &["--port", "0", "--auth", "none", "--bind", "0.0.0.0"],
        &[],
    );
    let address = server.url().to_string();

    // The warning is not a refusal: the server serves anyway.
    assert_eq!(status_of(&http_get(&address, "/", None)), "200");
    let stderr = server.kill();
    let warnings = stderr
        .lines()
        .filter(|line| line.contains("--auth none"))
        .count();
    assert_eq!(warnings, 1, "exactly one warning: {stderr}");
    assert!(stderr.contains("0.0.0.0"), "{stderr}");
}

#[test]
fn serve_auth_none_starts_on_a_broken_accounts_store() {
    let dir = library("auth-none-store");
    let state = state_dir();
    // A store that account mode must refuse.
    std::fs::write(state.0.join("accounts.json"), r#"{"schema":1}"#).unwrap();

    let server =
        Server::start_with_data_dir(&dir.root, &["--port", "0", "--auth", "none"], &[], &state.0);
    let address = server.url().to_string();
    assert_eq!(status_of(&http_get(&address, "/", None)), "200");
    let stderr = server.kill();
    assert!(
        !stderr.contains("malformed accounts.json"),
        "the store is not read at all: {stderr}"
    );
    assert_eq!(
        std::fs::read_to_string(state.0.join("accounts.json")).unwrap(),
        r#"{"schema":1}"#,
        "a no-login server must not rewrite the store"
    );
}

#[test]
fn serve_auth_account_is_the_default_and_keeps_passcode_mode() {
    let dir = library("auth-account");
    // No --auth: account mode, with the passcode protecting it as before.
    let server = Server::start(
        &dir.root,
        &["--port", "0"],
        &[("DIMAGINE_PASSCODE", "open-sesame")],
    );
    let address = server.url().to_string();
    assert_eq!(server.auth(), "account");

    let redirect = http_get(&address, "/", None);
    assert_eq!(status_of(&redirect), "303");
    assert!(header_of(&redirect, "location")
        .unwrap()
        .starts_with("/login"));
    let login_page = http_get(&address, "/login", None);
    assert!(login_page.contains("Passcode"), "{login_page}");
    assert!(!login_page.contains("No login:"), "{login_page}");
    // Passcode mode still wins over setup while no account exists.
    assert_eq!(status_of(&http_get(&address, "/setup", None)), "404");
    let (status, cookie) = post_login(&address, "open-sesame");
    assert_eq!(status, "303");
    let page = http_get(&address, "/", Some(&cookie.unwrap()));
    assert_eq!(status_of(&page), "200");
    assert!(!page.contains("No login:"), "{page}");
    let stderr = server.kill();
    assert!(!stderr.contains("No owner account yet"), "{stderr}");

    // An unknown mode is a usage error.
    let output = Command::new(BIN)
        .args([
            "serve",
            "--library",
            dir.root.to_str().unwrap(),
            "--auth",
            "sometimes",
        ])
        .env_remove("DIMAGINE_PASSCODE")
        .output()
        .expect("run dimagine serve");
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn setup_flow_over_http_and_restart() {
    let dir = library("setup-http");
    let state = state_dir();
    let server = Server::start_with_data_dir(&dir.root, &["--port", "0"], &[], &state.0);
    let address = server.url().to_string();

    let redirect = http_get(&address, "/", None);
    assert_eq!(status_of(&redirect), "303");
    assert!(header_of(&redirect, "location")
        .unwrap()
        .starts_with("/setup"));
    let setup_page = http_get(&address, "/setup", None);
    assert!(setup_page.contains("Initial Setup"), "{setup_page}");
    assert!(
        setup_page.contains("Use this weak password anyway"),
        "{setup_page}"
    );

    // A weak password without the checkbox is refused and creates nothing.
    let (status, _) = post_form(
        &address,
        "/setup",
        "email=owner%40example.com&password=tiny&confirm_password=tiny",
    );
    assert_eq!(status, "400");
    assert!(!state.0.join("accounts.json").exists());

    // With the checkbox it is accepted, creating the owner and signing them in.
    let (status, cookie) = post_form(
        &address,
        "/setup",
        "email=owner%40example.com&password=tiny&confirm_password=tiny&allow_weak=yes",
    );
    assert_eq!(status, "303");
    let cookie = cookie.expect("setup signs the owner in");
    assert!(!cookie.contains("setup_code"), "{cookie}");
    assert_eq!(status_of(&http_get(&address, "/", Some(&cookie))), "200");
    assert_eq!(status_of(&http_get(&address, "/setup", None)), "404");

    let accounts = state.0.join("accounts.json");
    assert!(accounts.exists());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&accounts).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "accounts.json must be 0600, got {mode:o}");
    }

    let stderr = server.kill();
    // This server started with no account, so it said so at startup; it stops
    // saying so once it next looks and finds the owner.
    assert!(stderr.contains("No owner account yet"), "{stderr}");

    // Restart on the same state: /setup is a 404 and login is required again.
    let server = Server::start_with_data_dir(&dir.root, &["--port", "0"], &[], &state.0);
    let address = server.url().to_string();
    assert_eq!(status_of(&http_get(&address, "/setup", None)), "404");
    let redirect = http_get(&address, "/", None);
    assert_eq!(status_of(&redirect), "303");
    assert!(header_of(&redirect, "location")
        .unwrap()
        .starts_with("/login"));
    let (status, cookie) = post_form(
        &address,
        "/login",
        "email=owner%40example.com&password=tiny",
    );
    assert_eq!(status, "303");
    let cookie = cookie.expect("the owner signs in with the short password");
    assert_eq!(status_of(&http_get(&address, "/", Some(&cookie))), "200");
    assert!(!server.kill().contains("No owner account yet"));
}
