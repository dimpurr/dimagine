//! Accounts store for dimagine serve (ADR-014).
//!
//! Accounts live outside the library in a state directory: `--data-dir <dir>`
//! (default `$XDG_STATE_HOME/dimagine`, else `~/.local/state/dimagine`).
//! The file `accounts.json` stores user records with argon2id password hashes,
//! written atomically with file mode 0600 on Unix. Unknown JSON fields are
//! preserved across reads and writes.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use argon2::{
    password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Argon2,
};
use rand_core::OsRng;
use serde::{Deserialize, Serialize};

/// Default schema version for accounts.json.
pub const CURRENT_SCHEMA: u32 = 1;

/// Passwords shorter than this are *weak*: too short to resist guessing.
///
/// There is no minimum length any more — any non-empty password is accepted —
/// but a weak one must be confirmed explicitly, both on the setup form
/// (the "Use this weak password anyway" checkbox) and by the user CLI
/// (`--allow-weak`).
pub const WEAK_PASSWORD_LENGTH: usize = 8;

/// Whether a password is weak: shorter than [`WEAK_PASSWORD_LENGTH`].
///
/// Counted in characters, not bytes, so a non-ASCII password is not judged
/// weak because it happens to be multibyte.
pub fn is_weak_password(password: &str) -> bool {
    password.chars().count() < WEAK_PASSWORD_LENGTH
}

/// In-memory representation of `accounts.json`.
///
/// `users` is required, never defaulted: a store that exists but has no
/// `users` key is a renamed or hand-edited file, not a first run, and
/// parsing it as "no users" would silently reopen public setup. Unknown
/// fields still round-trip through `extra` (ADR-014).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct AccountsFile {
    pub schema: u32,
    pub users: Vec<UserRecord>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

impl Default for AccountsFile {
    fn default() -> Self {
        Self {
            schema: CURRENT_SCHEMA,
            users: Vec::new(),
            extra: BTreeMap::new(),
        }
    }
}

/// A single user account record.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct UserRecord {
    pub id: String,
    pub email: String,
    pub password_hash: String,
    /// Account role. Stored, not yet enforced: today every authenticated
    /// account reaches the same read-only viewer, and `role` is reserved
    /// for the future multi-user surface (see the CHANGELOG note).
    pub role: String,
    pub created: String,
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug)]
pub enum AccountsError {
    Io(std::io::Error),
    Json(serde_json::Error),
    Hash(String),
    /// The file parsed, but its schema version is not one this build knows.
    Schema {
        found: u32,
    },
    /// No account has that email address.
    NotFound {
        email: String,
    },
}

impl fmt::Display for AccountsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AccountsError::Io(err) => write!(f, "account storage I/O error: {err}"),
            AccountsError::Json(err) => write!(f, "malformed accounts.json: {err}"),
            AccountsError::Hash(err) => write!(f, "password hashing error: {err}"),
            AccountsError::Schema { found } => write!(
                f,
                "unsupported accounts.json schema {found}; expected {CURRENT_SCHEMA}"
            ),
            AccountsError::NotFound { email } => {
                write!(f, "no account with email '{email}'")
            }
        }
    }
}

impl std::error::Error for AccountsError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            AccountsError::Io(err) => Some(err),
            AccountsError::Json(err) => Some(err),
            AccountsError::Hash(_) | AccountsError::Schema { .. } => None,
            AccountsError::NotFound { .. } => None,
        }
    }
}

impl From<std::io::Error> for AccountsError {
    fn from(err: std::io::Error) -> Self {
        AccountsError::Io(err)
    }
}

impl From<serde_json::Error> for AccountsError {
    fn from(err: serde_json::Error) -> Self {
        AccountsError::Json(err)
    }
}

/// Resolve the default state data directory according to XDG base directory specification.
///
/// Returns `$XDG_STATE_HOME/dimagine` if set and non-empty, otherwise
/// `$HOME/.local/state/dimagine`. Falls back to `.local/state/dimagine` if `$HOME` is unset.
pub fn default_data_dir() -> PathBuf {
    if let Some(xdg) = std::env::var_os("XDG_STATE_HOME") {
        if !xdg.is_empty() {
            return PathBuf::from(xdg).join("dimagine");
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        if !home.is_empty() {
            return PathBuf::from(home).join(".local/state/dimagine");
        }
    }
    PathBuf::from(".local/state/dimagine")
}

/// Hash a password using Argon2id with default parameters and random salt,
/// returning the standard PHC string.
pub fn hash_password(password: &str) -> Result<String, AccountsError> {
    let salt = SaltString::generate(&mut OsRng);
    let argon2 = Argon2::default();
    argon2
        .hash_password(password.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|e| AccountsError::Hash(e.to_string()))
}

/// Verify a password against an Argon2id PHC string.
pub fn verify_password(password: &str, hash_str: &str) -> bool {
    let Ok(parsed_hash) = PasswordHash::new(hash_str) else {
        return false;
    };
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed_hash)
        .is_ok()
}

/// Process-wide guard so the loose-permissions warning is printed once per
/// process (at startup, when the first load happens) and not again on every
/// request-time load or for every store instance.
static WARNED_PERMISSIONS: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Thread-safe manager for reading and atomically writing `accounts.json`.
#[derive(Clone, Debug)]
pub struct AccountsStore {
    file_path: PathBuf,
}

impl AccountsStore {
    pub fn new(data_dir: &Path) -> Self {
        Self {
            file_path: data_dir.join("accounts.json"),
        }
    }

    pub fn file_path(&self) -> &Path {
        &self.file_path
    }

    /// Read `accounts.json` from disk. If the file does not exist, returns a default
    /// empty structure.
    ///
    /// Any other problem — unreadable file, malformed JSON, a missing required
    /// key such as `users`, or a schema version this build does not know — is an
    /// error, never an empty store: treating a broken store as "no users" would
    /// silently reopen first-run setup.
    pub fn load(&self) -> Result<AccountsFile, AccountsError> {
        if !self.file_path.exists() {
            return Ok(AccountsFile::default());
        }
        self.warn_if_loose_permissions();
        let bytes = fs::read(&self.file_path)?;
        let accounts: AccountsFile = serde_json::from_slice(&bytes)?;
        if accounts.schema != CURRENT_SCHEMA {
            return Err(AccountsError::Schema {
                found: accounts.schema,
            });
        }
        Ok(accounts)
    }

    /// Return a warning message when `accounts.json` exists with permissions
    /// looser than 0600 (group- or world-readable), or `None` when the mode is
    /// acceptable or the file is absent. The store still loads: the warning
    /// is an operator signal, not a failure.
    pub fn permissions_warning(&self) -> Option<String> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let metadata = fs::metadata(&self.file_path).ok()?;
            let mode = metadata.permissions().mode() & 0o777;
            (mode & 0o077 != 0).then(|| {
                format!(
                    "{} is mode {mode:04o}; expected 0600 (argon2id password hashes must not be group/world-readable)",
                    self.file_path.display()
                )
            })
        }
        #[cfg(not(unix))]
        {
            None
        }
    }

    /// Print the loose-permissions warning once per process. The first load
    /// happens at startup, so in practice this fires there; a store made
    /// loose later (by a backup extraction, say) warns on the next read.
    fn warn_if_loose_permissions(&self) {
        let Some(warning) = self.permissions_warning() else {
            return;
        };
        if WARNED_PERMISSIONS
            .compare_exchange(
                false,
                true,
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
            )
            .is_ok()
        {
            eprintln!("WARNING: {warning}");
        }
    }

    /// Write `AccountsFile` atomically to `accounts.json` with permissions 0600 on Unix.
    ///
    /// The state directory is created 0700 when missing, and the fresh 0600
    /// temp file is renamed over the target, so a write also repairs an
    /// `accounts.json` left world-readable by a backup extraction.
    pub fn save(&self, accounts: &AccountsFile) -> Result<(), AccountsError> {
        let parent = self.file_path.parent().unwrap_or_else(|| Path::new("."));
        if !parent.exists() {
            fs::create_dir_all(parent)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                // The directory holds password hashes: owner-only.
                let _ = fs::set_permissions(parent, fs::Permissions::from_mode(0o700));
            }
        }

        let random_suffix: u64 = rand::random();
        let tmp_path = parent.join(format!(".accounts.json.tmp.{random_suffix}"));

        let write_result = (|| -> Result<(), std::io::Error> {
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
            let payload = serde_json::to_vec_pretty(accounts)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
            file.write_all(&payload)?;
            file.sync_all()?;
            drop(file);
            fs::rename(&tmp_path, &self.file_path)?;
            Ok(())
        })();

        if let Err(err) = write_result {
            let _ = fs::remove_file(&tmp_path);
            return Err(AccountsError::Io(err));
        }

        Ok(())
    }

    /// Check if at least one user exists in `accounts.json`.
    ///
    /// A store that cannot be read is an error, not "no users": callers must
    /// fail closed (refuse to start, or require authentication) instead of
    /// falling back to first-run setup.
    pub fn has_users(&self) -> Result<bool, AccountsError> {
        let doc = self.load()?;
        Ok(!doc.users.is_empty())
    }

    /// Find a user by email address (case-insensitive comparison).
    pub fn find_user_by_email(&self, email: &str) -> Result<Option<UserRecord>, AccountsError> {
        let doc = self.load()?;
        let target = email.trim();
        for user in doc.users {
            if user.email.eq_ignore_ascii_case(target) {
                return Ok(Some(user));
            }
        }
        Ok(None)
    }

    /// Create a new user with the given email, plain password, and role.
    ///
    /// Fails if a user with that email already exists.
    pub fn create_user(
        &self,
        email: &str,
        password: &str,
        role: &str,
    ) -> Result<UserRecord, AccountsError> {
        let mut doc = self.load()?;
        let trimmed_email = email.trim();
        if doc
            .users
            .iter()
            .any(|u| u.email.eq_ignore_ascii_case(trimmed_email))
        {
            return Err(AccountsError::Io(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                format!("user with email '{trimmed_email}' already exists"),
            )));
        }

        let password_hash = hash_password(password)?;
        let id = ulid::Ulid::new().to_string();
        let created = chrono::Utc::now().to_rfc3339();

        let new_user = UserRecord {
            id,
            email: trimmed_email.to_string(),
            password_hash,
            role: role.to_string(),
            created,
            extra: BTreeMap::new(),
        };

        doc.users.push(new_user.clone());
        self.save(&doc)?;
        Ok(new_user)
    }

    /// Update password for an existing user.
    pub fn set_password(&self, email: &str, new_password: &str) -> Result<(), AccountsError> {
        let mut doc = self.load()?;
        let trimmed_email = email.trim();
        let user = doc
            .users
            .iter_mut()
            .find(|u| u.email.eq_ignore_ascii_case(trimmed_email))
            .ok_or_else(|| AccountsError::NotFound {
                email: trimmed_email.to_string(),
            })?;

        user.password_hash = hash_password(new_password)?;
        self.save(&doc)?;
        Ok(())
    }

    /// Remove an account by email, returning the record that was removed.
    ///
    /// The write is the same atomic [`AccountsStore::save`] every other
    /// mutation uses, and unknown fields survive it. Deleting the last
    /// account leaves a store with no users, which is what puts the next
    /// `serve` start back into first-run setup.
    pub fn delete_user(&self, email: &str) -> Result<UserRecord, AccountsError> {
        let mut doc = self.load()?;
        let trimmed_email = email.trim();
        let index = doc
            .users
            .iter()
            .position(|u| u.email.eq_ignore_ascii_case(trimmed_email))
            .ok_or_else(|| AccountsError::NotFound {
                email: trimmed_email.to_string(),
            })?;
        let removed = doc.users.remove(index);
        self.save(&doc)?;
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_hash_and_verify() {
        let password = "correct-horse-battery-staple";
        let hash = hash_password(password).expect("hash password");
        assert!(hash.starts_with("$argon2id$"));
        assert!(verify_password(password, &hash));
        assert!(!verify_password("wrong-password", &hash));
    }

    #[test]
    fn accounts_file_preserves_unknown_fields_and_mode_0600() {
        let dir = tempfile::tempdir().unwrap();
        let store = AccountsStore::new(dir.path());

        let raw_json = r#"{
  "schema": 1,
  "custom_server_setting": "keep_me",
  "users": [
    {
      "id": "01J9XEXAMPLEULID0000000000",
      "email": "owner@example.com",
      "password_hash": "$argon2id$v=19$dummy",
      "role": "owner",
      "created": "2026-10-05T12:00:00Z",
      "custom_user_note": "do_not_drop"
    }
  ]
}"#;
        fs::write(store.file_path(), raw_json).unwrap();

        let mut loaded = store.load().unwrap();
        assert_eq!(loaded.schema, 1);
        assert_eq!(loaded.users.len(), 1);
        assert_eq!(
            loaded.extra.get("custom_server_setting").unwrap(),
            &serde_json::Value::String("keep_me".into())
        );
        assert_eq!(
            loaded.users[0].extra.get("custom_user_note").unwrap(),
            &serde_json::Value::String("do_not_drop".into())
        );

        // Add a second user and save
        let second_hash = hash_password("secret123").unwrap();
        loaded.users.push(UserRecord {
            id: ulid::Ulid::new().to_string(),
            email: "second@example.com".into(),
            password_hash: second_hash,
            role: "user".into(),
            created: chrono::Utc::now().to_rfc3339(),
            extra: BTreeMap::new(),
        });
        store.save(&loaded).unwrap();

        // Reload raw JSON string and ensure custom fields are still there
        let updated_raw = fs::read_to_string(store.file_path()).unwrap();
        assert!(updated_raw.contains("custom_server_setting"));
        assert!(updated_raw.contains("custom_user_note"));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let meta = fs::metadata(store.file_path()).unwrap();
            let mode = meta.permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "file mode must be 0600, got {:o}", mode);
        }
    }

    #[test]
    fn missing_users_key_is_an_error_not_an_empty_store() {
        let dir = tempfile::tempdir().unwrap();
        let store = AccountsStore::new(dir.path());

        // The `users` key renamed by another build: present file, so not a
        // first run, but it must not read as "no users".
        fs::write(
            store.file_path(),
            r#"{"schema":1,"accts":[{"id":"01J9XEXAMPLEULID0000000000"}]}"#,
        )
        .unwrap();
        let err = store.load().unwrap_err();
        assert!(err.to_string().contains("malformed accounts.json"), "{err}");
        assert!(
            err.to_string().contains("users"),
            "the error must name the missing key: {err}"
        );
        assert!(store.has_users().is_err());

        // No `users` key at all, beside a current schema.
        fs::write(store.file_path(), r#"{"schema":1}"#).unwrap();
        assert!(store.load().is_err());
        assert!(store.has_users().is_err());

        // A genuinely absent file is still a first run.
        let empty = tempfile::tempdir().unwrap();
        let first_run = AccountsStore::new(empty.path());
        assert!(!first_run.has_users().unwrap());
        assert!(first_run.load().unwrap().users.is_empty());
    }

    #[test]
    fn weak_passwords_are_judged_by_characters() {
        // There is no minimum any more: only a warning threshold.
        assert!(is_weak_password(""));
        assert!(is_weak_password("short"));
        assert!(is_weak_password("1234567"));
        assert!(!is_weak_password("12345678"));
        assert!(!is_weak_password("a-very-long-password"));
        // Eight multibyte characters are eight characters, not eight bytes.
        assert!(!is_weak_password("パスワード八个字符です"));
        assert!(is_weak_password("パスワード7"));
    }

    #[test]
    fn delete_user_removes_the_account_and_can_empty_the_store() {
        let dir = tempfile::tempdir().unwrap();
        let store = AccountsStore::new(dir.path());

        let owner = store
            .create_user("Owner@example.com", "secret123456", "owner")
            .unwrap();
        store
            .create_user("second@example.com", "secret123456", "user")
            .unwrap();

        // Unknown addresses are an error, not a silent no-op.
        let err = store.delete_user("nobody@example.com").unwrap_err();
        assert!(err.to_string().contains("nobody@example.com"), "{err}");
        assert_eq!(store.load().unwrap().users.len(), 2);

        // Case-insensitive, like every other lookup.
        let removed = store.delete_user("OWNER@EXAMPLE.COM").unwrap();
        assert_eq!(removed.id, owner.id);
        assert_eq!(removed.role, "owner");
        let doc = store.load().unwrap();
        assert_eq!(doc.users.len(), 1);
        assert_eq!(doc.users[0].email, "second@example.com");

        // Deleting the last account leaves a readable, empty store: that is
        // the shape that puts the next start back into setup.
        store.delete_user("second@example.com").unwrap();
        assert!(!store.has_users().unwrap());
        assert!(store.load().unwrap().users.is_empty());
        assert!(store.file_path().exists(), "the store file itself stays");

        // Unknown fields survive a delete, like any other write.
        let dir = tempfile::tempdir().unwrap();
        let store = AccountsStore::new(dir.path());
        std::fs::write(
            store.file_path(),
            r#"{"schema":1,"server_note":"keep me","users":[{"id":"01J9XEXAMPLEULID0000000000","email":"owner@example.com","password_hash":"$argon2id$v=19$dummy","role":"owner","created":"2026-10-05T12:00:00Z","user_note":"do_not_drop"}]}"#,
        )
        .unwrap();
        store.delete_user("owner@example.com").unwrap();
        let raw = std::fs::read_to_string(store.file_path()).unwrap();
        assert!(raw.contains("server_note"), "{raw}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(store.file_path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600, "delete must write 0600, got {mode:o}");
        }
    }

    #[test]
    fn create_user_and_set_password() {
        let dir = tempfile::tempdir().unwrap();
        let store = AccountsStore::new(dir.path());

        assert!(!store.has_users().unwrap());

        let user = store
            .create_user("Alice@example.com", "pass123", "owner")
            .unwrap();
        assert_eq!(user.email, "Alice@example.com");
        assert_eq!(user.role, "owner");
        assert!(verify_password("pass123", &user.password_hash));
        assert!(store.has_users().unwrap());

        // Duplicate email creation fails
        let dup = store.create_user("alice@example.com", "pass456", "owner");
        assert!(dup.is_err());

        // Find user case-insensitive
        let found = store.find_user_by_email("ALICE@EXAMPLE.COM").unwrap();
        assert!(found.is_some());
        assert_eq!(found.unwrap().id, user.id);

        // Update password
        store
            .set_password("alice@example.com", "new-pass789")
            .unwrap();
        let updated = store
            .find_user_by_email("alice@example.com")
            .unwrap()
            .unwrap();
        assert!(verify_password("new-pass789", &updated.password_hash));
        assert!(!verify_password("pass123", &updated.password_hash));
    }
}
