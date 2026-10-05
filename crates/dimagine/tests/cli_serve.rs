#![cfg(feature = "serve")]
//! Compiled only when the matching built-in plugin feature is on; with
//! --no-default-features the plugin subcommands do not exist.

use std::io::{BufRead, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

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
    setup_code: Option<String>,
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
        let setup_code = document["setup_code"].as_str().map(ToString::to_string);
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
            setup_code,
            state: None,
            stdout,
        }
    }

    fn url(&self) -> &str {
        &self.address
    }

    fn setup_code(&self) -> Option<&str> {
        self.setup_code.as_deref()
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
    // Never falls back to setup: no startup document, no setup code.
    assert!(!output.stdout.windows(12).any(|w| w == b"setup_code"));
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
    assert!(!output.stdout.windows(12).any(|w| w == b"setup_code"));
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
    assert!(!output.stdout.windows(12).any(|w| w == b"setup_code"));
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
    assert!(!output.stdout.windows(12).any(|w| w == b"setup_code"));
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
        "secret123\n",
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
        "secret456\n",
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
        "other\n",
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
        "newpass456\n",
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
        "email=owner%40example.com&password=newpass456",
    );
    assert_eq!(status, "303");
    let cookie = cookie.expect("login sets a session cookie");
    assert_eq!(status_of(&http_get(&address, "/", Some(&cookie))), "200");
    server.kill();
}

#[test]
fn setup_flow_over_http_and_restart() {
    let dir = library("setup-http");
    let state = state_dir();
    let server = Server::start_with_data_dir(&dir.root, &["--port", "0"], &[], &state.0);
    let address = server.url().to_string();
    let code = server
        .setup_code()
        .expect("first run prints a one-time setup code")
        .to_string();

    let redirect = http_get(&address, "/", None);
    assert_eq!(status_of(&redirect), "303");
    assert!(header_of(&redirect, "location")
        .unwrap()
        .starts_with("/setup"));
    assert!(http_get(&address, "/setup", None).contains("Initial Setup"));

    // A wrong one-time code is refused and creates nothing.
    let (status, _) = post_form(
        &address,
        "/setup",
        "email=owner%40example.com&password=secret123&confirm_password=secret123&setup_code=wrong",
    );
    assert_eq!(status, "400");
    assert!(!state.0.join("accounts.json").exists());

    // The right code creates the owner and signs in.
    let body = format!(
        "email=owner%40example.com&password=secret123&confirm_password=secret123&setup_code={code}"
    );
    let (status, cookie) = post_form(&address, "/setup", &body);
    assert_eq!(status, "303");
    let cookie = cookie.expect("setup signs the owner in");
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

    server.kill();

    // Restart on the same state: no code is printed and /setup is a 404.
    let server = Server::start_with_data_dir(&dir.root, &["--port", "0"], &[], &state.0);
    assert!(server.setup_code().is_none());
    let address = server.url().to_string();
    assert_eq!(status_of(&http_get(&address, "/setup", None)), "404");
    let redirect = http_get(&address, "/", None);
    assert_eq!(status_of(&redirect), "303");
    assert!(header_of(&redirect, "location")
        .unwrap()
        .starts_with("/login"));
    server.kill();
}
