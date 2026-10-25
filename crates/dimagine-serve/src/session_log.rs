//! Persistent session log (W81): the live sessions of one `--data-dir`
//! survive a restart of `dimagine serve`.
//!
//! The log is `<data-dir>/sessions.jsonl`, beside `accounts.json`, one JSON
//! object per line: `put` records a session, `drop` forgets one. The cookie
//! token is never written — a record's `token_hash` is the SHA-256 of the
//! token salted with the record's own random `salt` (W81b, the shape a
//! password record keeps: the salt sits beside the hash), so the file cannot
//! hand anyone a usable session, one token hashes one way per session, and a
//! record whose salt is wrong verifies nothing. The file is mode 0600 like
//! `accounts.json`, and its state directory is created 0700.
//!
//! Crash consistency: one record is one `write` of one line, flushed before
//! the operation returns. A crash can only tear the last line, and a torn
//! line is skipped on load — earlier records are never lost or misread. Any
//! other damage is corruption, not crash recovery: the log is quarantined
//! whole (`sessions.jsonl.corrupt.<timestamp>`) and never overwritten in
//! place (W81b, see [`SessionLog::load`]).
//! Startup replays the log, forgets expired sessions (an expiry passes while
//! the server is down the same way it passes while it runs), and rewrites the
//! file with only the live records through a temp file and a `rename`.
//!
//! Concurrency follows the write core's one lock (ADR-016 §6): an exclusive
//! `flock` on a stable sidecar (`sessions.lock`, never renamed) held across
//! every append and across the load-time rewrite, so two servers sharing one
//! `--data-dir` cannot interleave records or lose one to a compaction. A
//! process-wide mutex inside this type serialises its own callers first.

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

/// The log file's name inside the state directory.
pub const SESSION_LOG_FILE: &str = "sessions.jsonl";

/// The sidecar the `flock` of every log operation is taken on. It is never
/// renamed away, so the lock always guards the current log file (the write
/// core's `write.lock` is the same shape).
const LOCK_FILE: &str = "sessions.lock";

/// The most sessions one server keeps. The in-memory maps hold the same
/// number; a log that names more is capped here (oldest expiry first) when it
/// is replayed, so a hostile or broken file cannot grow the process without
/// bound.
pub const MAX_SESSIONS: usize = 10_000;

/// One live session, as the log records it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRecord {
    /// The SHA-256 (hex) of the session token the cookie carries, hashed
    /// under this record's [`salt`](Self::salt). The token itself is never
    /// stored.
    pub token_hash: String,
    /// The session's own random hash salt (hex), kept beside the hash the
    /// way a password record keeps its own (W81b). It is not secret: its job
    /// is that every session's hash is its own, so a leaked hash is good on
    /// no other record and no other installation. Records written before
    /// W81b carry none and hash as if salted with nothing.
    pub salt: String,
    /// Unix seconds: after this the session is over, logged in or not.
    pub expiry: u64,
    /// The session's server-side CSRF token (ADR-018 §3). It is server state
    /// like the rest of this file, is handed only to the owner's own pages,
    /// and must outlive a restart with its session or a page held open across
    /// one could not write again.
    pub csrf: String,
    /// The account the session was opened for; `None` for a passcode session.
    pub account: Option<String>,
}

/// One line of the log.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op")]
enum Line {
    #[serde(rename = "put")]
    Put {
        token_hash: String,
        /// Absent on records written before W81b: they replay as if salted
        /// with nothing, which is how their hash was written.
        #[serde(default)]
        salt: String,
        expiry: u64,
        csrf: String,
        account: Option<String>,
    },
    #[serde(rename = "drop")]
    Drop { token_hash: String },
}

/// Thread-safe manager for the session log.
#[derive(Debug)]
pub struct SessionLog {
    path: PathBuf,
    lock_path: PathBuf,
    /// Serialises this process's log operations before the `flock` does
    /// across processes, and keeps the two from interleaving in one server.
    guard: Mutex<()>,
}

impl SessionLog {
    /// The log under `data_dir`, the same state directory `accounts.json`
    /// uses. Nothing is read or written until [`load`](Self::load) or a
    /// mutation.
    pub fn new(data_dir: &Path) -> Self {
        Self {
            path: data_dir.join(SESSION_LOG_FILE),
            lock_path: data_dir.join(LOCK_FILE),
            guard: Mutex::new(()),
        }
    }

    /// The log file's path — `<data-dir>/sessions.jsonl`.
    pub fn file_path(&self) -> &Path {
        &self.path
    }

    /// Replay the log into the live sessions at `now` and rewrite it down to
    /// those records. Expired sessions are gone (their expiry passed while the
    /// server was down the same way it would while it runs).
    ///
    /// A missing file is simply "no sessions". An unreadable one is an error
    /// the caller may report: it is not "no sessions", and dropping every
    /// session on a broken disk is the caller's decision, not this function's.
    ///
    /// Corruption never truncates the log (W81b). The one line a crash may
    /// tear is the final one, unterminated at EOF: it is skipped, crash
    /// recovery as ever, and everything before it is intact. Any *other*
    /// unparseable line — damage in the middle, binary junk, a whole file of
    /// garbage — is not crash recovery: the log is quarantined whole to
    /// `sessions.jsonl.corrupt.<timestamp>`, nothing is restored, and a loud
    /// warning names the moved file. The corrupt bytes are never overwritten
    /// in place: an operator gets them back intact.
    pub fn load(&self, now: u64) -> std::io::Result<Vec<SessionRecord>> {
        self.load_with_warnings(now, &mut |message| eprintln!("{message}"))
    }

    /// [`load`](Self::load) with its warnings routed through `warn`, so a
    /// test can hear what stderr hears.
    fn load_with_warnings(
        &self,
        now: u64,
        warn: &mut dyn FnMut(String),
    ) -> std::io::Result<Vec<SessionRecord>> {
        self.with_lock(|| {
            if !self.path.exists() {
                return Ok(Vec::new());
            }
            let bytes = fs::read(&self.path)?;
            let text = String::from_utf8_lossy(&bytes);
            // A final line with no terminating newline is the torn tail of a
            // crash mid-append (every complete write ends its line).
            let torn_tail = !bytes.is_empty() && bytes.last() != Some(&b'\n');
            let lines: Vec<&str> = text.lines().collect();
            let mut live: HashMap<String, SessionRecord> = HashMap::new();
            let mut parsed_any = false;
            for (index, line) in lines.iter().enumerate() {
                let is_torn_tail = torn_tail && index + 1 == lines.len();
                let Ok(parsed) = serde_json::from_str::<Line>(line) else {
                    if is_torn_tail {
                        continue;
                    }
                    return self.quarantine("an unparseable line", warn);
                };
                parsed_any = true;
                match parsed {
                    Line::Put {
                        token_hash,
                        salt,
                        expiry,
                        csrf,
                        account,
                    } => {
                        live.insert(
                            token_hash.clone(),
                            SessionRecord {
                                token_hash,
                                salt,
                                expiry,
                                csrf,
                                account,
                            },
                        );
                    }
                    Line::Drop { token_hash } => {
                        live.remove(&token_hash);
                    }
                }
            }
            if !parsed_any && !bytes.is_empty() {
                return self.quarantine("not one parseable record", warn);
            }
            let mut records: Vec<SessionRecord> = live
                .into_values()
                .filter(|record| record.expiry > now)
                .collect();
            records = capped(records, MAX_SESSIONS);
            if let Err(error) = self.rewrite(&records) {
                // Compaction is hygiene: the records above are already the
                // truth. A failed rewrite leaves the old log for the next
                // start and must not fail the start.
                warn(format!(
                    "WARNING: could not compact the session log: {error}"
                ));
            }
            Ok(records)
        })
    }

    /// Move a corrupt log aside whole — `sessions.jsonl.corrupt.<timestamp>`
    /// — and start fresh with no sessions, warning loudly. The rename is the
    /// only write: the corrupt bytes are preserved for an operator, never
    /// overwritten in place. When even the rename fails, the error goes to
    /// the caller and the log stays exactly as it is.
    fn quarantine(
        &self,
        reason: &str,
        warn: &mut dyn FnMut(String),
    ) -> std::io::Result<Vec<SessionRecord>> {
        let parent = self.path.parent().unwrap_or_else(|| Path::new("."));
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.as_nanos())
            .unwrap_or(0);
        let moved = parent.join(format!("{SESSION_LOG_FILE}.corrupt.{stamp}"));
        fs::rename(&self.path, &moved)?;
        warn(format!(
            "WARNING: the session log was corrupt ({reason}); it was moved to {} and no session was restored",
            moved.display()
        ));
        Ok(Vec::new())
    }

    /// Record one session. Durable when this returns: the line is written and
    /// flushed before the caller's session counts as open.
    pub fn put(&self, record: &SessionRecord) -> std::io::Result<()> {
        let line = serde_json::to_string(&Line::Put {
            token_hash: record.token_hash.clone(),
            salt: record.salt.clone(),
            expiry: record.expiry,
            csrf: record.csrf.clone(),
            account: record.account.clone(),
        })
        .map_err(json_error)?;
        self.append(&[line])
    }

    /// Forget sessions by their token hashes: one `drop` line each, one
    /// write, one flush. An empty list writes nothing at all.
    pub fn drop_keys(&self, token_hashes: &[String]) -> std::io::Result<()> {
        if token_hashes.is_empty() {
            return Ok(());
        }
        let mut lines = Vec::with_capacity(token_hashes.len());
        for token_hash in token_hashes {
            lines.push(
                serde_json::to_string(&Line::Drop {
                    token_hash: token_hash.clone(),
                })
                .map_err(json_error)?,
            );
        }
        self.append(&lines)
    }

    /// Append whole lines to the log in one write, flushed, with the lock
    /// held. Every line ends in `\n` and is written complete.
    fn append(&self, lines: &[String]) -> std::io::Result<()> {
        if lines.is_empty() {
            return Ok(());
        }
        let mut payload = String::new();
        for line in lines {
            payload.push_str(line);
            payload.push('\n');
        }
        self.with_lock(|| {
            let mut options = fs::OpenOptions::new();
            options.create(true).append(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(&self.path)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                // Appending keeps an existing file's mode: repair one left
                // loose the way `accounts.json`'s save repairs its own.
                let _ = fs::set_permissions(&self.path, fs::Permissions::from_mode(0o600));
            }
            file.write_all(payload.as_bytes())?;
            file.sync_all()?;
            Ok(())
        })
    }

    /// Rewrite the log to exactly these records through a 0600 temp file and
    /// a `rename`, the way `accounts.json` is saved. The caller holds the
    /// lock.
    fn rewrite(&self, records: &[SessionRecord]) -> std::io::Result<()> {
        let parent = self.path.parent().unwrap_or_else(|| Path::new("."));
        let random_suffix: u64 = rand::random();
        let tmp_path = parent.join(format!(".sessions.jsonl.tmp.{random_suffix}"));
        let write_result = (|| -> std::io::Result<()> {
            let mut options = fs::OpenOptions::new();
            options.write(true).create(true).truncate(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(&tmp_path)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = fs::set_permissions(&tmp_path, fs::Permissions::from_mode(0o600));
            }
            for record in records {
                let line = serde_json::to_string(&Line::Put {
                    token_hash: record.token_hash.clone(),
                    salt: record.salt.clone(),
                    expiry: record.expiry,
                    csrf: record.csrf.clone(),
                    account: record.account.clone(),
                })
                .map_err(json_error)?;
                file.write_all(line.as_bytes())?;
                file.write_all(b"\n")?;
            }
            file.sync_all()?;
            drop(file);
            fs::rename(&tmp_path, &self.path)?;
            Ok(())
        })();
        if let Err(error) = write_result {
            let _ = fs::remove_file(&tmp_path);
            return Err(error);
        }
        Ok(())
    }

    /// Run `f` with this process's mutex and the cross-process `flock` held,
    /// creating the state directory 0700 and the lock file on the way.
    fn with_lock<R>(&self, f: impl FnOnce() -> std::io::Result<R>) -> std::io::Result<R> {
        let _guard = self
            .guard
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let parent = self.path.parent().unwrap_or_else(|| Path::new("."));
        if !parent.exists() {
            // The directory holds session state: owner-only from birth (W81b,
            // S3), the way the accounts save creates it. `DirBuilder` with a
            // mode creates 0700 outright — masked by the umask only toward
            // tighter — so there is no window where the state is briefly
            // group/world-readable before a chmod lands.
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                fs::DirBuilder::new()
                    .recursive(true)
                    .mode(0o700)
                    .create(parent)?;
            }
            #[cfg(not(unix))]
            {
                fs::create_dir_all(parent)?;
            }
        }
        let mut options = fs::OpenOptions::new();
        options.create(true).read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let lock_file = options.open(&self.lock_path)?;
        #[cfg(unix)]
        {
            // A signal can interrupt the wait; `flock` is then retried, the
            // way the write core's lock is taken.
            loop {
                let rc = unsafe {
                    libc::flock(std::os::fd::AsRawFd::as_raw_fd(&lock_file), libc::LOCK_EX)
                };
                if rc == 0 {
                    break;
                }
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::EINTR) {
                    return Err(error);
                }
            }
        }
        let result = f();
        drop(lock_file);
        result
    }
}

/// Keep the `max` sessions with the latest expiries, the same boundary the
/// in-memory maps hold: a session stored when the maps are full evicts the
/// oldest-expiry one.
fn capped(mut records: Vec<SessionRecord>, max: usize) -> Vec<SessionRecord> {
    if records.len() > max {
        records.sort_by_key(|record| record.expiry);
        records.drain(..records.len() - max);
    }
    records
}

/// JSON is only refused here when a record cannot be serialised at all,
/// which cannot happen for these fields; it becomes an I/O error so every
/// caller has one error type.
fn json_error(error: serde_json::Error) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, error)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(token_hash: &str, expiry: u64) -> SessionRecord {
        SessionRecord {
            token_hash: token_hash.to_owned(),
            salt: format!("salt-{token_hash}"),
            expiry,
            csrf: format!("csrf-{token_hash}"),
            account: Some("owner@example.com".to_owned()),
        }
    }

    /// The quarantine files in the state directory: the corrupt logs kept for
    /// an operator (`sessions.jsonl.corrupt.<timestamp>`).
    fn quarantined(dir: &tempfile::TempDir) -> Vec<std::path::PathBuf> {
        fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.file_name().is_some_and(|name| {
                    name.to_string_lossy()
                        .starts_with(&format!("{SESSION_LOG_FILE}.corrupt."))
                })
            })
            .collect()
    }

    #[test]
    fn put_and_drop_replay_to_the_live_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let log = SessionLog::new(dir.path());
        log.put(&record("a", 200)).unwrap();
        log.put(&record("b", 300)).unwrap();
        log.put(&record("a", 250)).unwrap();
        log.drop_keys(&["b".to_owned()]).unwrap();
        let mut live = log.load(100).unwrap();
        live.sort_by(|left, right| left.token_hash.cmp(&right.token_hash));
        assert_eq!(
            live,
            vec![record("a", 250)],
            "the last write of a wins, drops go"
        );
    }

    #[test]
    fn a_torn_last_line_is_skipped_and_earlier_records_survive() {
        let dir = tempfile::tempdir().unwrap();
        let log = SessionLog::new(dir.path());
        log.put(&record("a", 200)).unwrap();
        // A crash mid-append: the tail line is half-written.
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(log.file_path())
            .unwrap();
        file.write_all(b"{\"op\":\"put\",\"token_hash\":\"b\",\"exp")
            .unwrap();
        drop(file);
        let live = log.load(100).unwrap();
        assert_eq!(
            live,
            vec![record("a", 200)],
            "the torn tail is not a session"
        );
        assert!(
            quarantined(&dir).is_empty(),
            "a torn final line is crash recovery, not corruption: no quarantine"
        );
    }

    /// W81b: damage in the middle is corruption, not the crash-recovery tail.
    /// The log is quarantined whole and never truncated in place; the start
    /// is fresh and the warning says so.
    #[test]
    fn mid_file_corruption_is_quarantined_never_truncated() {
        let dir = tempfile::tempdir().unwrap();
        let log = SessionLog::new(dir.path());
        log.put(&record("a", 200)).unwrap();
        log.put(&record("b", 300)).unwrap();
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(log.file_path())
            .unwrap();
        file.write_all(b"{\"op\":\"put\",\"token_hash\":\"brok\n")
            .unwrap();
        file.write_all(
            b"{\"op\":\"put\",\"token_hash\":\"c\",\"salt\":\"s\",\"expiry\":400,\"csrf\":\"x\",\"account\":null}\n",
        )
        .unwrap();
        drop(file);
        let before = fs::read(log.file_path()).unwrap();

        let mut warnings = Vec::new();
        let live = log
            .load_with_warnings(100, &mut |message| warnings.push(message))
            .unwrap();
        assert!(live.is_empty(), "a corrupt log restores nothing: {live:?}");
        assert_eq!(
            warnings.len(),
            1,
            "the corruption is reported loudly: {warnings:?}"
        );
        assert!(
            warnings[0].contains("WARNING"),
            "the warning is a warning: {:?}",
            warnings[0]
        );
        let moved = quarantined(&dir);
        assert_eq!(moved.len(), 1, "the corrupt log is quarantined: {moved:?}");
        assert_eq!(
            fs::read(&moved[0]).unwrap(),
            before,
            "every corrupt byte is preserved, never overwritten in place"
        );

        // A fresh log works from here.
        log.put(&record("fresh", 500)).unwrap();
        assert_eq!(log.load(100).unwrap(), vec![record("fresh", 500)]);
    }

    /// W81b: a file that parses to nothing is not "no sessions" — it is
    /// corrupt, and it too is quarantined instead of rewritten away.
    #[test]
    fn a_file_with_no_parseable_record_is_quarantined() {
        let dir = tempfile::tempdir().unwrap();
        let log = SessionLog::new(dir.path());
        fs::write(log.file_path(), "not a session log at all").unwrap();
        let mut warnings = Vec::new();
        let live = log
            .load_with_warnings(100, &mut |message| warnings.push(message))
            .unwrap();
        assert!(live.is_empty());
        assert_eq!(warnings.len(), 1, "the corruption is reported loudly");
        assert_eq!(
            quarantined(&dir).len(),
            1,
            "the garbage is kept for an operator, not wiped"
        );
    }

    #[test]
    fn an_expired_session_is_not_restored_and_leaves_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let log = SessionLog::new(dir.path());
        log.put(&record("old", 50)).unwrap();
        log.put(&record("live", 200)).unwrap();
        assert_eq!(log.load(100).unwrap(), vec![record("live", 200)]);
        let on_disk = fs::read_to_string(log.file_path()).unwrap();
        assert!(
            !on_disk.contains("old"),
            "compaction rewrites only live records: {on_disk}"
        );
    }

    #[test]
    fn the_cap_keeps_the_latest_expiries() {
        let records: Vec<SessionRecord> =
            (0..5).map(|i| record(&format!("k{i}"), 100 + i)).collect();
        let kept = capped(records, 3);
        let mut hashes: Vec<String> = kept.into_iter().map(|record| record.token_hash).collect();
        hashes.sort();
        assert_eq!(hashes, vec!["k2", "k3", "k4"]);
    }

    #[test]
    fn the_log_file_is_0600() {
        let dir = tempfile::tempdir().unwrap();
        let log = SessionLog::new(dir.path());
        log.put(&record("a", 200)).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(log.file_path()).unwrap().permissions().mode() & 0o777;
            assert_eq!(
                mode, 0o600,
                "session state must not be group/world-readable"
            );
        }
    }

    /// W81b (S3): the state directory is created 0700 outright, not created
    /// loose and chmodded afterwards.
    #[test]
    fn the_state_directory_is_0700_from_birth() {
        let dir = tempfile::tempdir().unwrap();
        let log = SessionLog::new(&dir.path().join("state"));
        log.put(&record("a", 200)).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(dir.path().join("state"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(
                mode, 0o700,
                "session state must not be group/world-accessible"
            );
        }
    }

    #[test]
    fn two_logs_on_one_data_dir_interleave_without_losing_records() {
        let dir = tempfile::tempdir().unwrap();
        let one = SessionLog::new(dir.path());
        let two = SessionLog::new(dir.path());
        for i in 0..20 {
            let log = if i % 2 == 0 { &one } else { &two };
            log.put(&record(&format!("k{i}"), 200 + i)).unwrap();
        }
        one.drop_keys(&["k0".to_owned()]).unwrap();
        let live = two.load(100).unwrap();
        assert_eq!(live.len(), 19, "every record but the dropped one is live");
    }
}
