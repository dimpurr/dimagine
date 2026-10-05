//! Runtime switches for built-in plugin subcommands (ADR-013).
//!
//! `<library>/.dimagine/core-plugins.json` is an optional settings file that
//! decides which compiled-in plugin subcommands `dimagine` offers for that
//! library, e.g.:
//!
//! ```json
//! {"import-eagle": false}
//! ```
//!
//! Only explicit booleans change behaviour; a missing file, an unknown key
//! or a non-boolean value keeps the default (enabled). The file is settings
//! inside `.dimagine/`, so it is derived state: safe to lose, never required
//! (FORMAT §8). A file that exists but cannot be read or parsed is reported on
//! stderr; the CLI still runs with defaults rather than bricking itself over
//! settings.
//!
//! The switch is read before clap runs, because it changes which subcommands
//! exist; clap usage errors for a disabled subcommand are ordinary exit-2
//! errors.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

/// Which built-in plugin subcommands this binary offers. Defaults to every
/// compiled-in plugin (ADR-013); the switch file can only disable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CorePlugins {
    pub import_eagle: bool,
    pub previews: bool,
    pub serve: bool,
}

impl Default for CorePlugins {
    fn default() -> Self {
        Self {
            import_eagle: true,
            previews: true,
            serve: true,
        }
    }
}

impl CorePlugins {
    /// Load the switches for the library at `library`, falling back to the
    /// defaults whenever the file is missing, unreadable or malformed.
    pub fn load(library: &Path) -> Self {
        let mut plugins = Self::default();
        let path = library.join(".dimagine/core-plugins.json");
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return plugins,
            Err(error) => {
                warn(&format!("cannot read {}: {error}", path.display()));
                return plugins;
            }
        };
        let value: serde_json::Value = match serde_json::from_slice(&bytes) {
            Ok(value) => value,
            Err(error) => {
                warn(&format!("ignoring malformed {}: {error}", path.display()));
                return plugins;
            }
        };
        let Some(entries) = value.as_object() else {
            warn(&format!(
                "ignoring {}: expected a JSON object of plugin switches",
                path.display()
            ));
            return plugins;
        };
        for (key, setting) in entries {
            let Some(enabled) = setting.as_bool() else {
                warn(&format!(
                    "ignoring {}:{key}: expected a boolean",
                    path.display()
                ));
                continue;
            };
            match key.as_str() {
                "import-eagle" => plugins.import_eagle = enabled,
                "previews" => plugins.previews = enabled,
                "serve" => plugins.serve = enabled,
                other => warn(&format!(
                    "unknown plugin key {other:?} in {}",
                    path.display()
                )),
            }
        }
        plugins
    }
}

fn warn(message: &str) {
    eprintln!("dimagine: {message}");
}

/// Best-effort `--library` value from raw argv, before clap parses anything.
///
/// The switch file lives inside the library, so the offered subcommands
/// depend on the library before a single flag is validated. `--library` is a
/// clap global flag, so it may appear before or after the subcommand; a small
/// forward scan handles both positions and stops at `--`. When the scan finds
/// nothing it falls back to the current directory, which is also the flag's
/// default value, so a misread never changes which library is acted on.
pub fn library_hint(args: impl IntoIterator<Item = OsString>) -> PathBuf {
    let mut library: Option<PathBuf> = None;
    let mut expect_value = false;
    for arg in args.into_iter().skip(1) {
        if expect_value {
            expect_value = false;
            library.get_or_insert_with(|| PathBuf::from(arg));
            continue;
        }
        let arg = arg.as_os_str();
        if arg == OsStr::new("--library") {
            expect_value = true;
        } else if let Some(text) = arg.to_str() {
            if let Some(value) = text.strip_prefix("--library=") {
                library.get_or_insert_with(|| PathBuf::from(value));
            } else if text == "--" {
                break;
            }
        }
    }
    library.unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    fn args(values: &[&str]) -> Vec<OsString> {
        values.iter().map(|value| OsString::from(*value)).collect()
    }

    fn scratch(tag: &str) -> PathBuf {
        let root = std::env::var("CARGO_TARGET_TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| std::env::temp_dir());
        let dir = root.join(format!(
            "dimagine-plugins-unit-{}-{}",
            tag,
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn missing_file_means_all_defaults() {
        let dir = scratch("missing");
        assert_eq!(CorePlugins::load(&dir), CorePlugins::default());
    }

    #[test]
    fn explicit_false_disables_only_that_plugin() {
        let dir = scratch("false");
        let path = dir.join(".dimagine/core-plugins.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, br#"{"import-eagle": false}"#).unwrap();
        let plugins = CorePlugins::load(&dir);
        assert!(!plugins.import_eagle);
        assert!(plugins.previews);
        assert!(plugins.serve);
    }

    #[test]
    fn explicit_true_and_unknown_keys_keep_defaults() {
        let dir = scratch("true");
        let path = dir.join(".dimagine/core-plugins.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            br#"{"import-eagle": true, "serve": true, "vector-search": true}"#,
        )
        .unwrap();
        assert_eq!(CorePlugins::load(&dir), CorePlugins::default());
    }

    #[test]
    fn malformed_file_falls_back_to_defaults() {
        let dir = scratch("malformed");
        let path = dir.join(".dimagine/core-plugins.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"{nope").unwrap();
        assert_eq!(CorePlugins::load(&dir), CorePlugins::default());

        std::fs::write(&path, b"[\"import-eagle\"]").unwrap();
        assert_eq!(CorePlugins::load(&dir), CorePlugins::default());

        std::fs::write(&path, br#"{"previews": "off"}"#).unwrap();
        assert_eq!(CorePlugins::load(&dir), CorePlugins::default());
    }

    #[test]
    fn library_hint_reads_the_flag_in_any_position() {
        // The script name is skipped, like a real argv[0].
        let forward = library_hint(args(&["dimagine", "--library", "/data/lab", "scan"]));
        assert_eq!(forward, PathBuf::from("/data/lab"));

        let after_subcommand = library_hint(args(&[
            "dimagine",
            "import",
            "eagle",
            "s.library",
            "--library",
            "/data/lab",
        ]));
        assert_eq!(after_subcommand, PathBuf::from("/data/lab"));

        let inline = library_hint(args(&["dimagine", "--library=/data/lab", "check"]));
        assert_eq!(inline, PathBuf::from("/data/lab"));
    }

    #[test]
    fn library_hint_first_flag_wins_and_terminator_stops_the_scan() {
        let picky = library_hint(args(&["d", "--library", "/one", "--library", "/two"]));
        assert_eq!(picky, PathBuf::from("/one"));

        let stopped = library_hint(args(&["d", "--", "--library", "/nope"]));
        assert_eq!(stopped, std::env::current_dir().unwrap());
    }
}
