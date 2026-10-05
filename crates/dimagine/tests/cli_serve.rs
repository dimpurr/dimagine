//! End-to-end CLI tests for `dimagine serve`: a real process on a real
//! socket, hand-rolled HTTP so no client dependency is added. Every server
//! here is a child this test started and later kills.

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

struct Server {
    child: Child,
    address: String,
    stdout: std::process::ChildStdout,
}

impl Server {
    fn start(library: &Path, extra: &[&str], envs: &[(&str, &str)]) -> Server {
        let mut command = Command::new(BIN);
        let mut all: Vec<String> = vec![
            "--json".into(),
            "serve".into(),
            "--library".into(),
            library.to_string_lossy().into_owned(),
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
        let address = document["address"]
            .as_str()
            .expect("address field")
            .trim_start_matches("http://")
            .to_string();
        let stdout = reader.into_inner();
        wait_listening(&address);
        Server {
            child,
            address,
            stdout,
        }
    }

    fn url(&self) -> &str {
        &self.address
    }

    fn kill(mut self) -> String {
        // Kill first so the pipes reach EOF and the reads finish.
        let _ = self.child.kill();
        let mut stderr = String::new();
        if let Some(mut pipe) = self.child.stderr.take() {
            let _ = pipe.read_to_string(&mut stderr);
        }
        let _ = self.child.wait();
        let mut rest = String::new();
        let _ = self.stdout.read_to_string(&mut rest);
        stderr
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
    let server = Server::start(&dir.root, &["--port", "0"], &[]);
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
    let server = Server::start(&dir.root, &["--port", "0"], &[]);
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
    let server = Server::start(&dir.root, &["--port", "0"], &[]);
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
fn serve_empty_passcode_env_falls_back_to_the_default() {
    let dir = library("empty-pass");
    let server = Server::start(&dir.root, &["--port", "0"], &[("DIMAGINE_PASSCODE", "")]);
    let address = server.url().to_string();

    let (status, cookie) = post_login(&address, "2333");
    assert_eq!(status, "303");
    let page = http_get(&address, "/", Some(&cookie.unwrap()));
    assert_eq!(status_of(&page), "200");

    server.kill();
}

#[test]
fn serve_default_passcode_on_non_loopback_bind_warns() {
    let dir = library("warn");
    // Bind to all local IPv4 interfaces while reaching it on loopback; the
    // serve crate must warn that the default passcode is active.
    let server = Server::start(&dir.root, &["--port", "0", "--bind", "0.0.0.0"], &[]);
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
