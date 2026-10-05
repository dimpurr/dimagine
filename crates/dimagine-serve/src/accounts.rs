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

/// Minimum password length enforced by the setup form and the user CLI.
pub const MIN_PASSWORD_LENGTH: usize = 12;

/// In-memory representation of `accounts.json`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct AccountsFile {
    pub schema: u32,
    #[serde(default)]
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
        }
    }
}

impl std::error::Error for AccountsError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            AccountsError::Io(err) => Some(err),
            AccountsError::Json(err) => Some(err),
            AccountsError::Hash(_) | AccountsError::Schema { .. } => None,
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
    /// Any other problem — unreadable file, malformed JSON, or a schema version
    /// this build does not know — is an error, never an empty store: treating a
    /// broken store as "no users" would silently reopen first-run setup.
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
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("user with email '{trimmed_email}' not found"),
                )
            })?;

        user.password_hash = hash_password(new_password)?;
        self.save(&doc)?;
        Ok(())
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
