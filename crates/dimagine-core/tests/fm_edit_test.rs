//! The front-matter editor fixture corpus (fm_edit): each edit's byte diff
//! must equal exactly the intended range, and requests outside the closed
//! subset must refuse with a typed error.

use dimagine_core::fm_edit::{
    add_tags, delete_key, ensure_id, ensure_self_embed, remove_tags, set_rating, set_scalar,
    FmEditError, RatingEdit,
};

const DUPLICATE_KEY: &[u8] = include_bytes!("fixtures/fm/duplicate-key.md");
const QUOTED_RATING: &[u8] = include_bytes!("fixtures/fm/quoted-rating.md");
const FLOW_COMMA: &[u8] = include_bytes!("fixtures/fm/flow-comma.md");
const BLOCK_LIST: &[u8] = include_bytes!("fixtures/fm/block-list.md");
const NEIGHBOUR_COMMENT: &[u8] = include_bytes!("fixtures/fm/neighbour-comment.md");
const EDITED_LINE_COMMENT: &[u8] = include_bytes!("fixtures/fm/edited-line-comment.md");
const CRLF_BOM: &[u8] = include_bytes!("fixtures/fm/crlf-bom.md");
const NO_FRONT_MATTER: &[u8] = include_bytes!("fixtures/fm/no-front-matter.md");
const UNTERMINATED: &[u8] = include_bytes!("fixtures/fm/unterminated.md");
const BODY_DASHES: &[u8] = include_bytes!("fixtures/fm/body-dashes.md");
const COMMENTS_ONLY: &[u8] = include_bytes!("fixtures/fm/comments-only.md");
const HIERARCHY_CASE: &[u8] = include_bytes!("fixtures/fm/hierarchy-case.md");
const PLAIN_ROUNDTRIP: &[u8] = include_bytes!("fixtures/fm/plain-note-roundtrip.md");
const BLOCK_ROUNDTRIP: &[u8] = include_bytes!("fixtures/fm/block-note-roundtrip.md");
const SPACED_ROUNDTRIP: &[u8] = include_bytes!("fixtures/fm/spaced-flow-roundtrip.md");

/// The edit must replace exactly the one occurrence of `old` with `new`,
/// leaving every other byte identical; an empty `old` pins an insertion at
/// the expected position instead.
fn assert_only_splice(before: &[u8], after: &[u8], old: &[u8], new: &[u8]) {
    let mut positions = (0..=before.len().saturating_sub(old.len()))
        .filter(|start| before[*start..start + old.len()] == *old)
        .collect::<Vec<usize>>();
    if old.is_empty() {
        // An insertion: find where `new` landed in the after bytes and check
        // that everything around it is untouched.
        let at = after
            .windows(new.len().max(1))
            .position(|window| *window == *new)
            .expect("the inserted bytes to appear");
        assert_eq!(&after[..at], &before[..at], "the prefix must be identical");
        assert_eq!(
            &after[at + new.len()..],
            &before[at + old.len()..],
            "the suffix must be identical"
        );
        return;
    }
    assert_eq!(
        positions.len(),
        1,
        "the intended old range must occur exactly once"
    );
    let at = positions.remove(0);
    assert_eq!(&before[..at], &after[..at], "the prefix must be identical");
    assert_eq!(
        &after[at..at + new.len()],
        new,
        "the new bytes differ from the intended spelling"
    );
    assert_eq!(
        &before[at + old.len()..],
        &after[at + new.len()..],
        "the suffix must be identical"
    );
}

#[test]
fn duplicate_key_refuses_every_operation() {
    // (Keys with a typed operation of their own refuse first, before the
    // note is even scanned; the plain keys below prove the gate.)
    for result in [
        set_scalar(DUPLICATE_KEY, "title", "New"),
        delete_key(DUPLICATE_KEY, "title"),
        set_rating(DUPLICATE_KEY, RatingEdit::Set(5)),
        add_tags(DUPLICATE_KEY, &["x"]),
        remove_tags(DUPLICATE_KEY, &["x"]),
        ensure_id(DUPLICATE_KEY, "01JA8X3Q7K2M9V4T6R1B5N0C3Q"),
    ] {
        assert_eq!(result, Err(FmEditError::DuplicateTopLevelKey));
    }
    assert_eq!(
        set_scalar(DUPLICATE_KEY, "rating", "3"),
        Err(FmEditError::ReservedKey {
            key: "rating".to_string()
        })
    );
}

#[test]
fn quoted_rating_is_rewritten_unquoted_then_cleared() {
    let set = set_rating(QUOTED_RATING, RatingEdit::Set(5)).unwrap();
    assert_only_splice(QUOTED_RATING, &set, b"\"4\"", b"5");

    let cleared = set_rating(&set, RatingEdit::Clear).unwrap();
    assert_only_splice(&set, &cleared, b"rating: 5\n", b"");

    // Clearing again, or on the original with a missing rating, is a no-op
    // on bytes that do not carry the key line at all.
    let casing = b"---\ntitle: T\n---\nbody\n";
    assert_eq!(
        set_rating(casing, RatingEdit::Clear).unwrap(),
        casing.to_vec()
    );
    assert_eq!(set_rating(&cleared, RatingEdit::Clear).unwrap(), cleared);
}

#[test]
fn flow_list_with_quoted_comma_splices_before_the_closing_bracket() {
    let after = add_tags(FLOW_COMMA, &["added"]).unwrap();
    assert_only_splice(FLOW_COMMA, &after, b"sketch]", b"sketch, added]");
    assert!(after
        .windows(b"tags: [assoc, \"b,c\", sketch, added]".len())
        .any(|window| window == b"tags: [assoc, \"b,c\", sketch, added]"));

    // Tag inputs cannot contain a comma, so removing the quoted element is
    // refused by the tag rules; removing the others splices cleanly.
    assert_eq!(
        remove_tags(&after, &["b,c"]),
        Err(FmEditError::InvalidTag {
            tag: "b,c".to_string()
        })
    );
    let removed = remove_tags(&after, &["assoc"]).unwrap();
    assert_only_splice(&after, &removed, b"assoc, ", b"");
    assert!(removed
        .windows(b"tags: [\"b,c\", sketch, added]".len())
        .any(|window| window == b"tags: [\"b,c\", sketch, added]"));
}

#[test]
fn block_list_appends_and_removes_item_lines() {
    let after = add_tags(BLOCK_LIST, &["newtag"]).unwrap();
    assert_only_splice(
        BLOCK_LIST,
        &after,
        b"  - \"comma, tag\"\n",
        b"  - \"comma, tag\"\n  - newtag\n",
    );
    assert!(after
        .windows(b"  - newtag\n".len())
        .any(|w| w == b"  - newtag\n"));

    // The comma in the stored element keeps it out of tag inputs' reach;
    // removing the other items is byte-exact.
    assert_eq!(
        remove_tags(&after, &["comma, tag"]),
        Err(FmEditError::InvalidTag {
            tag: "comma, tag".to_string()
        })
    );
    let removed = remove_tags(&after, &["style/ink"]).unwrap();
    assert_only_splice(&after, &removed, b"  - style/ink\n", b"");
    let more = add_tags(&after, &["later"]).unwrap();
    let removed_again = remove_tags(&more, &["later"]).unwrap();
    assert_eq!(removed_again, after.to_vec());

    // The comma element cannot be removed through the tag rules, so the key
    // stays with it (the embedded unit tests cover whole-key deletion).
    let pinned = remove_tags(&removed, &["newtag"]).unwrap();
    assert_eq!(
        String::from_utf8_lossy(&pinned),
        "---\ntags:\n  - \"comma, tag\"\nrating: 2\n---\n\nBody.\n"
    );
}

#[test]
fn comments_on_neighbour_lines_stay_byte_identical() {
    let after = set_scalar(NEIGHBOUR_COMMENT, "title", "New title").unwrap();
    assert_only_splice(NEIGHBOUR_COMMENT, &after, b"Working title", b"New title");
    assert!(after.starts_with(b"---\n# created by hand\n"));
    assert!(after
        .windows(b"# leave this alone\n".len())
        .any(|window| window == b"# leave this alone\n"));
}

#[test]
fn comment_on_the_edited_line_is_kept() {
    let after = set_scalar(EDITED_LINE_COMMENT, "title", "New").unwrap();
    assert_only_splice(EDITED_LINE_COMMENT, &after, b"Old", b"New");
    assert!(after
        .windows(b"title: New # pencilled in".len())
        .any(|window| window == b"title: New # pencilled in"));

    let rated = set_rating(EDITED_LINE_COMMENT, RatingEdit::Set(1)).unwrap();
    assert_only_splice(EDITED_LINE_COMMENT, &rated, b"4", b"1");
}

#[test]
fn crlf_and_bom_edits_keep_the_style_and_the_mark() {
    assert!(CRLF_BOM.starts_with(b"\xEF\xBB\xBF---\r\n"));
    let after = set_scalar(CRLF_BOM, "title", "New girl").unwrap();
    assert_only_splice(CRLF_BOM, &after, b"CR girl", b"New girl");

    let rated = set_rating(CRLF_BOM, RatingEdit::Set(5)).unwrap();
    assert_only_splice(CRLF_BOM, &rated, b"4", b"5");
    assert!(rated.starts_with(b"\xEF\xBB\xBF"));

    let tagged = add_tags(CRLF_BOM, &["added"]).unwrap();
    assert_eq!(
        String::from_utf8_lossy(&tagged),
        "﻿---\r\ntitle: CR girl\r\nrating: 4\r\ntags: [crlf, added]\r\n---\r\n\r\n![[crlf.png]]\r\n"
    );
}

#[test]
fn a_note_without_front_matter_grows_a_block() {
    let after = add_tags(NO_FRONT_MATTER, &["ink"]).unwrap();
    assert_eq!(
        after,
        b"---\ntags: [ink]\n---\n# Just a body\n\n![[girl.jpg]]\n".to_vec()
    );

    let with_id = ensure_id(after.as_slice(), "01JA8X3Q7K2M9V4T6R1B5N0C3Q").unwrap();
    assert_eq!(
        with_id,
        b"---\ntags: [ink]\nid: 01JA8X3Q7K2M9V4T6R1B5N0C3Q\n---\n# Just a body\n\n![[girl.jpg]]\n"
            .to_vec()
    );

    let embed = ensure_self_embed(with_id.as_slice(), "![[girl.jpg]]").unwrap();
    assert_eq!(with_id, embed); // the body already embeds it
}

#[test]
fn unterminated_front_matter_refuses() {
    for result in [
        set_scalar(UNTERMINATED, "title", "New"),
        set_rating(UNTERMINATED, RatingEdit::Set(4)),
        add_tags(UNTERMINATED, &["x"]),
        ensure_id(UNTERMINATED, "01JA8X3Q7K2M9V4T6R1B5N0C3Q"),
    ] {
        assert_eq!(result, Err(FmEditError::UnterminatedFrontMatter));
    }
}

#[test]
fn a_body_starting_with_dashes_stays_put() {
    let after = set_scalar(BODY_DASHES, "title", "Notes").unwrap();
    assert_only_splice(BODY_DASHES, &after, b"Minutes", b"Notes");
    assert!(after.ends_with(b"---\n---\nA thematic break starts the body.\n"));
    let rated = set_rating(&after, RatingEdit::Set(2)).unwrap();
    assert!(rated.ends_with(b"---\n---\nA thematic break starts the body.\n"));
}

#[test]
fn comments_only_front_matter_takes_a_new_last_key_line() {
    let after = set_rating(COMMENTS_ONLY, RatingEdit::Set(4)).unwrap();
    assert_eq!(
        after,
        b"---\n# only comments\n# nothing else\nrating: 4\n---\ntext\n".to_vec()
    );
}

#[test]
fn add_tag_then_remove_tag_returns_the_original_bytes() {
    for before in [PLAIN_ROUNDTRIP, BLOCK_ROUNDTRIP, SPACED_ROUNDTRIP] {
        let after = add_tags(before, &["probe"]).unwrap();
        assert_ne!(after, before.to_vec());
        let restored = remove_tags(&after, &["probe"]).unwrap();
        assert_eq!(restored, before.to_vec());
    }
    // And a second round with a tag that needs quoting.
    for before in [PLAIN_ROUNDTRIP, BLOCK_ROUNDTRIP] {
        let after = add_tags(before, &["4"]).unwrap();
        let restored = remove_tags(&after, &["#4"]).unwrap();
        assert_eq!(restored, before.to_vec());
    }
}

#[test]
fn hierarchical_and_case_variant_tags_match_the_way_the_rules_say() {
    // Adding a case-variant duplicate adds nothing.
    assert_eq!(
        add_tags(HIERARCHY_CASE, &["Style/ink"]).unwrap(),
        HIERARCHY_CASE.to_vec()
    );
    // A genuinely new hierarchical tag lands before the bracket.
    let after = add_tags(HIERARCHY_CASE, &["Style/Charcoal"]).unwrap();
    assert_only_splice(
        HIERARCHY_CASE,
        &after,
        b"monochrome]",
        b"monochrome, Style/Charcoal]",
    );
    // Removing is case-insensitive and removes every variant...
    let removed = remove_tags(&after, &["style/INK"]).unwrap();
    assert_eq!(
        String::from_utf8_lossy(&removed),
        "---\ntags: [monochrome, Style/Charcoal]\ntitle: Case variants\n---\n\nBody.\n"
    );
    // ...but does not touch other tags by prefix: `style` leaves the
    // hierarchies alone.
    let kept = remove_tags(&removed, &["style"]).unwrap();
    assert_eq!(kept, removed.clone());
}
