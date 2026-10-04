//! Front matter parsing and `id` / `kind` extraction (FORMAT §3).

mod support;

use dimagine_core::note::{parse_note, IdProperty};

#[test]
fn parses_properties_and_ignores_unknown_ones() {
    let text = "---\nid: 01JA8X3Q7K2M9V4T6R1B5N0C3Q\ntitle: Girl sinking\nrating: 4\ncompletely_unknown_property: {deep: [1, 2, 3]}\nkind: collection\n---\n\nFree text.\n\n![[girl.jpg]]\n";
    let note = parse_note(text);
    assert!(note.has_front_matter);
    assert!(note.error.is_none());
    assert_eq!(
        note.id,
        Some(IdProperty::Text("01JA8X3Q7K2M9V4T6R1B5N0C3Q".to_string()))
    );
    assert_eq!(note.kind.as_deref(), Some("collection"));
    assert!(note.body.contains("![[girl.jpg]]"));
    assert_eq!(note.body_line, 8);
}

#[test]
fn body_line_counts_front_matter_lines() {
    let text = "---\na: 1\nb: 2\nc: 3\n---\nline after\n";
    let note = parse_note(text);
    assert_eq!(note.body_line, 6);
    let note = parse_note("no front matter\nhere");
    assert_eq!(note.body_line, 1);
}

#[test]
fn no_front_matter() {
    let note = parse_note("# Just a note\n\n![[a.png]]\n");
    assert!(!note.has_front_matter);
    assert!(note.error.is_none());
    assert!(note.id.is_none());
    assert!(note.kind.is_none());
    assert_eq!(note.body, "# Just a note\n\n![[a.png]]\n");
}

#[test]
fn empty_front_matter_block_is_not_an_error() {
    let note = parse_note("---\n---\nbody\n");
    assert!(note.has_front_matter);
    assert!(note.error.is_none());
    assert!(note.id.is_none());
}

#[test]
fn unterminated_front_matter_is_reported() {
    let text = "---\nid: whatever\nthis block never closes\n";
    let note = parse_note(text);
    let error = note.error.expect("front matter error");
    assert!(error.message.contains("never closed"));
    assert!(error.line.is_none());
}

#[test]
fn invalid_yaml_reports_line_and_column() {
    // `a: b: c` is not valid YAML: "mapping values are not allowed".
    let text = "---\ntitle: Lost at Sea\na: b: c\n---\nbody\n";
    let note = parse_note(text);
    let error = note.error.expect("yaml error");
    // `a: b: c` sits on file line 3 (front matter text begins after line 1).
    assert_eq!(error.line, Some(3));
    assert!(error.column.is_some(), "column is reported when available");
    assert!(!error.message.is_empty());
}

#[test]
fn non_mapping_front_matter_is_an_error() {
    let note = parse_note("---\njust a scalar\n---\nbody\n");
    let error = note.error.expect("non-mapping front matter");
    assert!(error.message.contains("not a mapping"));
}

#[test]
fn handles_bom_and_crlf() {
    let crlf = "---\r\nkind: collection\r\n---\r\nbody\r\n".to_string();
    let note = parse_note(&format!("\u{feff}{crlf}"));
    assert_eq!(note.kind.as_deref(), Some("collection"));
    assert!(note.error.is_none());
}

#[test]
fn id_forms() {
    let note = parse_note("---\nid: 42\n---\n");
    assert_eq!(note.id, Some(IdProperty::NotAString));
    let note = parse_note("---\nid: [1, 2]\n---\n");
    assert_eq!(note.id, Some(IdProperty::NotAString));
    let note = parse_note("---\nid:\n---\n");
    assert_eq!(note.id, None);
    let note = parse_note("---\nid: 01JA8X3Q7K2M9V4T6R1B5N0C3Q\n---\n");
    assert_eq!(
        note.id,
        Some(IdProperty::Text("01JA8X3Q7K2M9V4T6R1B5N0C3Q".into()))
    );
}

#[test]
fn front_matter_error_still_yields_body_for_links() {
    let text = "---\nbroken: [\n---\n![[girl.jpg]]\n";
    let note = parse_note(text);
    assert!(note.error.is_some());
    assert!(note.body.contains("![[girl.jpg]]"));
}
