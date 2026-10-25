//! Session persistence across a `dimagine serve` restart (W81).
//!
//! A "restart" here is a second router over the same `--data-dir`: the maps
//! are the router's, and what the new one believes comes only from the
//! session log on disk. Every test drives real requests through the auth
//! middleware, so what is asserted is what a browser would see.

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use dimagine_serve::session_log::SESSION_LOG_FILE;
use dimagine_serve::{router, FsCatalog, OriginalPreview, ServeConfig};
use std::fs;
use tempfile::TempDir;
use tower::ServiceExt;

const PNG: &[u8] = b"\x89PNG\r\n\x1a\nsynthetic-png-data";

fn app_for(root: &TempDir) -> axum::Router {
    // Keep account and session state inside the test's own directory: no
    // test may read or write the real user state directory.
    let config = ServeConfig {
        data_dir: root.path().join("state"),
        ..ServeConfig::default()
    };
    router(
        FsCatalog::new(root.path()).unwrap(),
        OriginalPreview,
        config,
    )
}

fn request(uri: &str, cookie: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder().uri(uri);
    if let Some(cookie) = cookie {
        builder = builder.header("cookie", cookie);
    }
    builder.body(Body::empty()).unwrap()
}

async fn login(app: &axum::Router, passcode: &str) -> String {
    login_holding(app, passcode, None).await
}

/// Sign in while presenting an existing session cookie — a browser that
/// signs in again.
async fn login_holding(app: &axum::Router, passcode: &str, holding: Option<&str>) -> String {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/login")
        .header("content-type", "application/x-www-form-urlencoded");
    if let Some(cookie) = holding {
        builder = builder.header("cookie", cookie);
    }
    let response = app
        .clone()
        .oneshot(
            builder
                .body(Body::from(format!("passcode={passcode}")))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    response.headers()["set-cookie"]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned()
}

async fn logout(app: &axum::Router, cookie: &str) {
    let response = app
        .clone()
        .oneshot(request("/logout", Some(cookie)))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
}

/// `/raw/pic.png` answers 200 to a live session and redirects to the sign-in
/// page when the session is gone. It reads the library directly, so the
/// answer says nothing about any index.
async fn session_is_live(app: &axum::Router, cookie: &str) -> bool {
    let response = app
        .clone()
        .oneshot(request("/raw/pic.png", Some(cookie)))
        .await
        .unwrap();
    match response.status() {
        StatusCode::OK => true,
        StatusCode::SEE_OTHER => false,
        other => panic!("unexpected status {other}"),
    }
}

/// The session token inside a `dimagine_session=...` cookie pair.
fn token_of(cookie: &str) -> String {
    cookie.split_once('=').unwrap().1.to_owned()
}

/// What the log names a session by: the SHA-256 of its salt followed by its
/// token (W81b), the salt read from the record the way verification reads it.
fn session_key(salt: &str, token: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(salt.as_bytes());
    hasher.update(token.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// The log record that verifies this token: the one whose own salt re-derives
/// its hash (W81b). Its `token_hash` is the session's map key.
fn record_of(root: &TempDir, token: &str) -> serde_json::Value {
    fs::read_to_string(log_path(root))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .find(|record| {
            let salt = record["salt"].as_str().unwrap();
            record["token_hash"] == session_key(salt, token).as_str()
        })
        .expect("the session log holds a record for the token")
}

fn log_path(root: &TempDir) -> std::path::PathBuf {
    root.path().join("state").join(SESSION_LOG_FILE)
}

#[tokio::test]
async fn a_live_session_survives_a_restart() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("pic.png"), PNG).unwrap();
    let app = app_for(&root);
    let cookie = login(&app, "2333").await;
    assert!(session_is_live(&app, &cookie).await);
    drop(app);

    let restarted = app_for(&root);
    assert!(
        session_is_live(&restarted, &cookie).await,
        "a session that has not expired keeps working across the restart"
    );
}

#[tokio::test]
async fn an_expired_session_is_not_restored_by_a_restart() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("pic.png"), PNG).unwrap();
    let app = app_for(&root);
    let cookie = login(&app, "2333").await;
    let key = record_of(&root, &token_of(&cookie))["token_hash"]
        .as_str()
        .unwrap()
        .to_owned();

    // Let the expiry pass while the server is down, by writing what time
    // would write: the same record, with its expiry in the past.
    let path = log_path(&root);
    let rewritten: Vec<String> = fs::read_to_string(&path)
        .unwrap()
        .lines()
        .map(|line| {
            let mut record: serde_json::Value = serde_json::from_str(line).unwrap();
            if record["token_hash"] == key.as_str() {
                record["expiry"] = serde_json::json!(1);
            }
            record.to_string()
        })
        .collect();
    fs::write(&path, rewritten.join("\n") + "\n").unwrap();
    drop(app);

    let restarted = app_for(&root);
    assert!(
        !session_is_live(&restarted, &cookie).await,
        "an expiry that passed while the server was down stays passed"
    );
    let on_disk = fs::read_to_string(&path).unwrap();
    assert!(
        !on_disk.contains(&key),
        "the expired record is compacted away: {on_disk}"
    );
}

#[tokio::test]
async fn a_logged_out_session_does_not_come_back_after_a_restart() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("pic.png"), PNG).unwrap();
    let app = app_for(&root);
    let cookie = login(&app, "2333").await;
    let key = record_of(&root, &token_of(&cookie))["token_hash"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(session_is_live(&app, &cookie).await);
    logout(&app, &cookie).await;
    assert!(!session_is_live(&app, &cookie).await, "logout ends it now");
    drop(app);

    let restarted = app_for(&root);
    assert!(
        !session_is_live(&restarted, &cookie).await,
        "a logout must not be undone by a restart"
    );
    let on_disk = fs::read_to_string(log_path(&root)).unwrap();
    assert!(
        !on_disk.contains(&key),
        "the restart compacts the dropped record away: {on_disk}"
    );
}

#[tokio::test]
async fn the_session_file_is_0600_and_never_holds_the_token() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("pic.png"), PNG).unwrap();
    let app = app_for(&root);
    let cookie = login(&app, "2333").await;
    let token = token_of(&cookie);
    let record = record_of(&root, &token);
    let key = record["token_hash"].as_str().unwrap().to_owned();

    let path = log_path(&root);
    let contents = fs::read_to_string(&path).unwrap();
    assert!(
        contents.contains(&key),
        "the log names the session by its salted hash: {contents}"
    );
    assert!(
        contents.contains(record["salt"].as_str().unwrap()),
        "the salt sits beside the hash, the password-record shape: {contents}"
    );
    // The grep assertion: no file in the state directory may hold the token
    // the cookie carries.
    for entry in fs::read_dir(root.path().join("state")).unwrap() {
        let entry = entry.unwrap();
        let bytes = fs::read(entry.path()).unwrap();
        let text = String::from_utf8_lossy(&bytes);
        assert!(
            !text.contains(&token),
            "{} holds the session token in plaintext",
            entry.path().display()
        );
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "session state must not be group/world-readable"
        );
    }
}

/// Two servers on one state directory — the shape the flock of the session
/// log exists for — interleave their logins and logouts without losing or
/// resurrecting a record, and the next start sees exactly the truth.
#[tokio::test]
async fn concurrent_sessions_across_two_servers_all_survive_a_restart() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("pic.png"), PNG).unwrap();
    let one = app_for(&root);
    let two = app_for(&root);

    let mut cookies = Vec::new();
    for _ in 0..2 {
        // Four logins at once: two per server, which is each server's whole
        // login slot allowance, writing the log from both sides at the same
        // time.
        let (a1, a2, b1, b2) = tokio::join!(
            login(&one, "2333"),
            login(&one, "2333"),
            login(&two, "2333"),
            login(&two, "2333"),
        );
        cookies.extend([a1, a2, b1, b2]);
    }
    let signed_out = vec![cookies.remove(0), cookies.remove(2)];
    let (out_one, out_two) = (signed_out[0].clone(), signed_out[1].clone());
    tokio::join!(logout(&one, &out_one), logout(&two, &out_two));
    drop(one);
    drop(two);

    let restarted = app_for(&root);
    for cookie in &cookies {
        assert!(
            session_is_live(&restarted, cookie).await,
            "every session that was not signed out is live after the restart"
        );
    }
    for cookie in &signed_out {
        assert!(
            !session_is_live(&restarted, cookie).await,
            "every session that was signed out stays out"
        );
    }
}

/// W81b: verification reads the salt from the record. A record whose salt no
/// longer matches its hash — a tampered file, a record moved between
/// installations — verifies nothing: the token hashes another way under it.
#[tokio::test]
async fn a_record_whose_salt_is_wrong_verifies_nothing() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("pic.png"), PNG).unwrap();
    let app = app_for(&root);
    let cookie = login(&app, "2333").await;
    let token = token_of(&cookie);
    let target = record_of(&root, &token)["token_hash"]
        .as_str()
        .unwrap()
        .to_owned();

    // Swap the record's salt for another, leaving its hash alone. It parses
    // as cleanly as it did, and opens nothing.
    let path = log_path(&root);
    let rewritten: Vec<String> = fs::read_to_string(&path)
        .unwrap()
        .lines()
        .map(|line| {
            let mut record: serde_json::Value = serde_json::from_str(line).unwrap();
            if record["token_hash"] == target.as_str() {
                record["salt"] = serde_json::json!("33333333333333333333333333333333");
            }
            record.to_string()
        })
        .collect();
    fs::write(&path, rewritten.join("\n") + "\n").unwrap();
    drop(app);

    let restarted = app_for(&root);
    assert!(
        !session_is_live(&restarted, &cookie).await,
        "a wrong salt fails to verify the token"
    );
}

/// W81b (S4): signing in again rotates the session — the token the browser
/// held is dropped, in the maps and in the log, instead of lingering live
/// beside the new one.
#[tokio::test]
async fn signing_in_again_drops_the_session_the_browser_held() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("pic.png"), PNG).unwrap();
    let app = app_for(&root);
    let old = login(&app, "2333").await;
    let new = login_holding(&app, "2333", Some(&old)).await;
    assert!(
        !session_is_live(&app, &old).await,
        "the re-login ends the old session at once"
    );
    assert!(session_is_live(&app, &new).await, "the new session is live");
    drop(app);

    let restarted = app_for(&root);
    assert!(
        !session_is_live(&restarted, &old).await,
        "the dropped session stays out across a restart"
    );
    assert!(session_is_live(&restarted, &new).await);
}
