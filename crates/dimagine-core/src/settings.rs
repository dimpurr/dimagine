//! Library settings: `<library>/.dimagine/settings.json` (ADR-019).
//!
//! One flat dotted-key JSON object per library, read by the CLI and
//! serve alike. The schema below is the single source of truth: the
//! CLI, the settings page and the docs all generate from it
//! (ADR-019 §3). Reading tolerates comments and trailing commas
//! (JSONC, like VS Code); writing keeps only the keys that differ
//! from their default and leaves unknown keys untouched.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::LazyLock;

use serde_json::{Map, Value};

/// The directory inside the library that holds write state (ADR-016 §0).
pub const SETTINGS_DIR: &str = ".dimagine";
/// The settings file name (ADR-019 §1).
pub const SETTINGS_FILE: &str = "settings.json";

/// The type of a setting's value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettingKind {
    Int,
}

/// When a change to a setting takes effect (ADR-019 §3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Effect {
    Immediate,
    Restart,
}

/// One row of the settings table: key, type, default, range,
/// description and effect. Every setting the tools know lives here
/// and nowhere else (ADR-019 §3).
#[derive(Clone, Debug)]
pub struct SettingSpec {
    pub key: &'static str,
    pub kind: SettingKind,
    pub default: Value,
    pub min: Option<i64>,
    pub max: Option<i64>,
    pub description_en: &'static str,
    pub description_zh: &'static str,
    pub effect: Effect,
}

/// The settings table — the single source of truth
/// (ADR-019 §3). Built once and shared.
static SCHEMA: LazyLock<Vec<SettingSpec>> = LazyLock::new(|| {
    vec![SettingSpec {
        key: "trash.retentionDays",
        kind: SettingKind::Int,
        default: Value::from(180),
        min: Some(0),
        max: Some(3650),
        description_en: "Days before a trashed group is permanently deleted; 0 never deletes.",
        description_zh: "回收站组保留多少天后永久删除；0 表示永不自动删除。",
        effect: Effect::Immediate,
    }]
});

pub fn schema() -> &'static [SettingSpec] {
    &SCHEMA
}

/// What was wrong with a settings file, reported to `check` and the
/// settings page (ADR-019 §3).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SettingsReport {
    /// Schema keys whose file value had the wrong type or was out of
    /// range: the default applies instead.
    pub invalid: Vec<String>,
    /// Keys the schema does not know. They are kept in the file and
    /// written back untouched (ADR-019 §2).
    pub unknown: Vec<String>,
    /// The file exists but is not a parsable JSON object: every
    /// setting defaults, the file is reported, and the tools do not
    /// refuse to start (ADR-019 §3).
    pub parse_error: Option<String>,
}

/// The effective settings: one value per schema key — the file's when
/// valid, the default when absent or invalid — plus the report of
/// what was wrong with the file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    values: BTreeMap<String, Value>,
    pub report: SettingsReport,
}

impl Settings {
    /// Resolve the effective settings from the file's text, if it
    /// exists (`None` = no file: every default applies).
    pub fn resolve(file_text: Option<&str>) -> Self {
        let mut values = BTreeMap::new();
        let mut report = SettingsReport::default();
        let parsed: Option<Map<String, Value>> = match file_text {
            Some(text) => match parse_jsonc_object(text) {
                Ok(map) => Some(map),
                Err(message) => {
                    report.parse_error = Some(message);
                    None
                }
            },
            None => None,
        };
        let file_map = parsed.as_ref();
        for spec in schema() {
            let mut value = spec.default.clone();
            if let Some(map) = file_map {
                if let Some(found) = map.get(spec.key) {
                    if validate(spec, found).is_ok() {
                        value = found.clone();
                    } else {
                        report.invalid.push(spec.key.to_string());
                    }
                }
            }
            values.insert(spec.key.to_string(), value);
        }
        if let Some(map) = file_map {
            let known: BTreeSet<&str> = schema().iter().map(|s| s.key).collect();
            for key in map.keys() {
                if !known.contains(key.as_str()) {
                    report.unknown.push(key.clone());
                }
            }
        }
        Self { values, report }
    }

    /// The effective value of a schema key (its default when the
    /// file is silent), or `None` for a key the schema does not
    /// know.
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.values.get(key)
    }

    /// The effective value of an integer setting.
    pub fn get_int(&self, key: &str) -> Option<i64> {
        self.get(key).and_then(Value::as_i64)
    }

    /// `trash.retentionDays` (ADR-017 §7, ADR-019 §6): days before
    /// a trashed group is permanently deleted; 0 never deletes.
    pub fn trash_retention_days(&self) -> i64 {
        self.get_int("trash.retentionDays").unwrap_or(180)
    }
}

/// A change to apply to the settings file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Change {
    Set(Value),
    /// Remove the key so its default applies again.
    Reset,
}

/// Why a settings change was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApplyError {
    /// The file exists but is not a parsable JSON object. A change
    /// refuses rather than drop content the reader could not
    /// understand (ADR-019 §3: an unparseable file defaults and is
    /// reported, it is not silently rewritten).
    Unparsable { message: String },
    /// The key is not in the schema; the allowed keys are listed.
    UnknownKey { key: String, allowed: Vec<String> },
    /// The value does not fit the key's type or range.
    InvalidValue {
        key: String,
        value: String,
        message: String,
    },
}

/// Compute the new settings file content after applying `changes`:
/// unknown keys kept verbatim, changed keys written, keys at their
/// default removed, so the file only ever holds non-default keys
/// (ADR-019 §2). The result is pretty-printed JSON.
pub fn apply(file_text: Option<&str>, changes: &[(&str, Change)]) -> Result<String, ApplyError> {
    let mut map: Map<String, Value> = match file_text {
        None => Map::new(),
        Some(text) => {
            parse_jsonc_object(text).map_err(|message| ApplyError::Unparsable { message })?
        }
    };
    let specs: BTreeMap<&str, &SettingSpec> = schema().iter().map(|s| (s.key, s)).collect();
    for (key, change) in changes {
        let Some(spec) = specs.get(key).copied() else {
            return Err(ApplyError::UnknownKey {
                key: (*key).to_string(),
                allowed: specs.keys().map(|k| (*k).to_string()).collect(),
            });
        };
        match change {
            Change::Set(value) => {
                validate(spec, value).map_err(|message| ApplyError::InvalidValue {
                    key: (*key).to_string(),
                    value: value.to_string(),
                    message,
                })?;
                map.insert((*key).to_string(), value.clone());
            }
            Change::Reset => {
                map.remove(*key);
            }
        }
    }
    for spec in schema() {
        if map
            .get(spec.key)
            .map(|v| v == &spec.default)
            .unwrap_or(false)
        {
            map.remove(spec.key);
        }
    }
    serde_json::to_string_pretty(&map).map_err(|e| ApplyError::Unparsable {
        message: e.to_string(),
    })
}

/// Parse JSONC text (comments and trailing commas tolerated) and
/// require a JSON object.
fn parse_jsonc_object(text: &str) -> Result<Map<String, Value>, String> {
    let stripped = strip_trailing_commas(&strip_jsonc_comments(text));
    let value: Value = serde_json::from_str(&stripped).map_err(|e| e.to_string())?;
    match value {
        Value::Object(map) => Ok(map),
        other => Err(format!("expected a JSON object, found {}", kind_of(&other))),
    }
}

fn validate(spec: &SettingSpec, value: &Value) -> Result<(), String> {
    match spec.kind {
        SettingKind::Int => {
            let Some(number) = value.as_i64() else {
                return Err(format!("expected an integer, found {}", kind_of(value)));
            };
            if let Some(min) = spec.min {
                if number < min {
                    return Err(format!("must be at least {min}"));
                }
            }
            if let Some(max) = spec.max {
                if number > max {
                    return Err(format!("must be at most {max}"));
                }
            }
            Ok(())
        }
    }
}

fn kind_of(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(n) if n.is_i64() => "an integer",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

/// Strip `//` and `/* */` comments, leaving string contents
/// untouched (ADR-019 §2).
fn strip_jsonc_comments(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut in_string = false;
    let mut escaped = false;
    while let Some(c) = chars.next() {
        if in_string {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => {
                in_string = true;
                out.push(c);
            }
            '/' => match chars.peek() {
                Some('/') => {
                    chars.next();
                    for n in chars.by_ref() {
                        if n == '\n' {
                            break;
                        }
                    }
                }
                Some('*') => {
                    chars.next();
                    // An unterminated block comment runs to the end
                    // of the file, like VS Code's reader.
                    while let Some(n) = chars.next() {
                        if n == '*' && chars.peek() == Some(&'/') {
                            chars.next();
                            break;
                        }
                    }
                }
                _ => out.push(c),
            },
            _ => out.push(c),
        }
    }
    out
}

/// Drop commas followed (after whitespace) by `}` or `]`, the
/// trailing-comma extension VS Code's JSONC also accepts.
fn strip_trailing_commas(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == ',' {
            let mut rest = chars.clone();
            let mut next = rest.next();
            while let Some(w) = next {
                if !w.is_whitespace() {
                    break;
                }
                next = rest.next();
            }
            if matches!(next, Some('}') | Some(']')) {
                continue;
            }
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn effective(key: &str, text: Option<&str>) -> Value {
        Settings::resolve(text)
            .get(key)
            .cloned()
            .expect("schema key")
    }

    #[test]
    fn no_file_means_every_default() {
        let settings = Settings::resolve(None);
        assert_eq!(settings.trash_retention_days(), 180);
        assert_eq!(settings.report, SettingsReport::default());
        assert_eq!(effective("trash.retentionDays", None), json!(180));
    }

    #[test]
    fn reads_a_valid_value() {
        assert_eq!(
            effective(
                "trash.retentionDays",
                Some(r#"{"trash.retentionDays": 30}"#)
            ),
            json!(30)
        );
        assert_eq!(
            Settings::resolve(Some(r#"{"trash.retentionDays": 0}"#)).trash_retention_days(),
            0
        );
    }

    #[test]
    fn an_invalid_value_defaults_and_is_reported() {
        for text in [
            r#"{"trash.retentionDays": 3651}"#,
            r#"{"trash.retentionDays": -1}"#,
            r#"{"trash.retentionDays": "many"}"#,
            r#"{"trash.retentionDays": 1.5}"#,
        ] {
            let settings = Settings::resolve(Some(text));
            assert_eq!(settings.trash_retention_days(), 180, "{text}");
            assert_eq!(settings.report.invalid, ["trash.retentionDays"], "{text}");
            assert!(settings.report.parse_error.is_none());
        }
    }

    #[test]
    fn unknown_keys_are_kept_and_reported() {
        let settings = Settings::resolve(Some(
            r#"{"trash.retentionDays": 30, "future.setting": true}"#,
        ));
        assert_eq!(settings.trash_retention_days(), 30);
        assert_eq!(settings.report.unknown, ["future.setting"]);
        assert!(settings.report.invalid.is_empty());
    }

    #[test]
    fn jsonc_comments_and_trailing_commas() {
        let text = r#"{
            // how long groups stay in the trash
            "trash.retentionDays": 90, /* days */
            "future.setting": true,
        }"#;
        let settings = Settings::resolve(Some(text));
        assert_eq!(settings.trash_retention_days(), 90);
        assert_eq!(settings.report.unknown, ["future.setting"]);
    }

    #[test]
    fn an_unparsable_file_defaults_everything_and_is_reported() {
        for text in ["{oops", "[]", "\"settings\"", ""] {
            let settings = Settings::resolve(Some(text));
            assert_eq!(settings.trash_retention_days(), 180, "{text}");
            assert!(settings.report.parse_error.is_some(), "{text}");
        }
    }

    #[test]
    fn apply_writes_only_non_default_keys() {
        assert_eq!(
            apply(None, &[("trash.retentionDays", Change::Set(json!(30)))]).unwrap(),
            "{\n  \"trash.retentionDays\": 30\n}"
        );
        // A value at the default is not written at all.
        assert_eq!(
            apply(None, &[("trash.retentionDays", Change::Set(json!(180)))]).unwrap(),
            "{}"
        );
        // Reset removes the key.
        assert_eq!(
            apply(
                Some("{\n  \"trash.retentionDays\": 30\n}"),
                &[("trash.retentionDays", Change::Reset)]
            )
            .unwrap(),
            "{}"
        );
    }

    #[test]
    fn apply_keeps_unknown_keys_verbatim() {
        let text = apply(
            Some("{\n  \"future.setting\": true\n}"),
            &[("trash.retentionDays", Change::Set(json!(30)))],
        )
        .unwrap();
        let map: Map<String, Value> = serde_json::from_str(&text).unwrap();
        assert_eq!(map.get("trash.retentionDays"), Some(&json!(30)));
        assert_eq!(map.get("future.setting"), Some(&json!(true)));
    }

    #[test]
    fn apply_refuses_what_the_schema_does_not_allow() {
        assert!(matches!(
            apply(None, &[("nope.key", Change::Set(json!(1)))]),
            Err(ApplyError::UnknownKey { .. })
        ));
        assert!(matches!(
            apply(None, &[("trash.retentionDays", Change::Set(json!(3651)))]),
            Err(ApplyError::InvalidValue { .. })
        ));
        assert!(matches!(
            apply(None, &[("trash.retentionDays", Change::Set(json!("many")))]),
            Err(ApplyError::InvalidValue { .. })
        ));
        assert!(matches!(
            apply(
                Some("{oops"),
                &[("trash.retentionDays", Change::Set(json!(1)))]
            ),
            Err(ApplyError::Unparsable { .. })
        ));
        // The unknown-key error carries the allowed list.
        match apply(None, &[("nope.key", Change::Set(json!(1)))]) {
            Err(ApplyError::UnknownKey { allowed, .. }) => {
                assert_eq!(allowed, ["trash.retentionDays"]);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn apply_reads_jsonc_too() {
        // A change computes from the JSONC text, comments and all.
        let text = apply(
            Some("{\n  // keep it short\n  \"trash.retentionDays\": 30,\n}"),
            &[("trash.retentionDays", Change::Reset)],
        )
        .unwrap();
        assert_eq!(text, "{}");
    }
}
