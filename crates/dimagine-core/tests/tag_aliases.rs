use dimagine_core::tag_aliases::{TagAliases, ALIAS_PATH};

#[test]
fn resolves_chains_without_changing_unmapped_inputs() {
    let table =
        TagAliases::parse(br#"{"version":1,"aliases":{"Old Name":"middle","middle":"Final/Tag"}}"#)
            .unwrap();
    assert_eq!(table.resolve("OLD NAME").unwrap(), "Final/Tag");
    assert_eq!(table.resolve("Other").unwrap(), "Other");
}

#[test]
fn rejects_ambiguous_invalid_and_cyclic_documents() {
    for json in [
        r#"{"version":1,"aliases":{"A":"x","a":"y"}}"#,
        r#"{"version":1,"aliases":{"a":"x","a":"y"}}"#,
        r#"{"version":1,"aliases":{"a":"bad target"}}"#,
        r#"{"version":1,"aliases":{"a":"a//b"}}"#,
        r#"{"version":1,"aliases":{"a":5}}"#,
        r#"{"version":1,"aliases":{"a":"b","b":"A"}}"#,
        r#"{"version":2,"aliases":{}}"#,
        r#"{"version":1,"aliases":{},"extra":true}"#,
        "{",
    ] {
        assert!(TagAliases::parse(json.as_bytes()).is_err(), "{json}");
    }
}

#[test]
fn missing_is_distinct_from_unreadable_or_invalid() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(
        TagAliases::load(dir.path()).unwrap().resolve("a").unwrap(),
        "a"
    );
    std::fs::create_dir_all(dir.path().join(ALIAS_PATH)).unwrap();
    assert!(TagAliases::load(dir.path()).is_err());
    std::fs::remove_dir(dir.path().join(ALIAS_PATH)).unwrap();
    std::fs::write(dir.path().join(ALIAS_PATH), b"invalid").unwrap();
    assert!(TagAliases::load(dir.path()).is_err());
}
