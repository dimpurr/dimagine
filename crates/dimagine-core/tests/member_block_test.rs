//! Member-block model: reorder and caption editing (ADR-017 §9).
//!
//! The model is pure, so it is pinned here on in-memory libraries with the
//! block ranges and the exact bytes each edit produces. Nothing is written.

use std::ffi::OsString;
use std::path::PathBuf;

use dimagine_core::body_edit::{
    self, edit_caption, member_blocks, reorder_members, BodyCtx, CaptionChange, EditError,
};
use dimagine_core::library::{classify, FileEntry};
use dimagine_core::links::Resolver;

fn entries(rels: &[&str]) -> Vec<FileEntry> {
    rels.iter()
        .map(|rel| {
            let name = rel.rsplit('/').next().unwrap_or(rel).to_string();
            FileEntry {
                rel: rel.to_string(),
                name: name.clone(),
                path: PathBuf::from(rel),
                native_name: OsString::from(name),
                class: classify(rel),
            }
        })
        .collect()
}

fn with_ctx<R>(rels: &[&str], f: impl FnOnce(&BodyCtx) -> R) -> R {
    let files = entries(rels);
    let resolver = Resolver::new(&files);
    let ctx = BodyCtx::new(&files, &resolver);
    f(&ctx)
}

fn note_index(ctx: &BodyCtx, rel: &str) -> usize {
    ctx.index_of(rel).unwrap()
}

#[test]
fn a_block_is_a_member_line_and_the_caption_lines_after_it() {
    let body = b"# Heading\n\n![[a.jpg]]\nCap A\n\n![[b.jpg]]\nCap B1\nCap B2\n\nTail\n";
    with_ctx(&["a.jpg", "b.jpg", "c.md"], |ctx| {
        let blocks = member_blocks(body, note_index(ctx, "c.md"), ctx).unwrap();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].ordinal, 0);
        assert_eq!(blocks[0].member_line, 2);
        assert_eq!(blocks[0].image, "a.jpg");
        assert_eq!(blocks[0].line_ranges.len(), 2, "member line + one caption");
        assert_eq!(blocks[0].range, 11..28, "\"![[a.jpg]]\\nCap A\\n\"");
        assert_eq!(blocks[1].ordinal, 1);
        assert_eq!(blocks[1].member_line, 5);
        assert_eq!(blocks[1].image, "b.jpg");
        assert_eq!(blocks[1].line_ranges.len(), 3);
        assert_eq!(blocks[1].range, 29..54);
    });
}

#[test]
fn a_heading_or_blank_line_ends_the_caption_run() {
    // A heading, a blank line, a list item and a second member line all stop
    // the caption run (ADR-017 §9: a caption line is pure text).
    let body = b"![[a.jpg]]\nCaption\n# Not a caption\n![[a.jpg]]\n- list, not a caption\n";
    with_ctx(&["a.jpg", "c.md"], |ctx| {
        let blocks = member_blocks(body, note_index(ctx, "c.md"), ctx).unwrap();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].line_ranges.len(), 2, "member line + \"Caption\"");
        assert_eq!(
            blocks[1].line_ranges.len(),
            1,
            "the list line is not a caption"
        );
    });
}

#[test]
fn reorder_permutes_blocks_only_and_leaves_the_rest_byte_exact() {
    let body = b"# Heading\n\n![[a.jpg]]\nCap A\n\n![[b.jpg]]\nCap B1\nCap B2\n\nTail\n";
    with_ctx(&["a.jpg", "b.jpg", "c.md"], |ctx| {
        let reordered = reorder_members(body, note_index(ctx, "c.md"), ctx, &[1, 0]).unwrap();
        assert_eq!(
            reordered,
            b"# Heading\n\n![[b.jpg]]\nCap B1\nCap B2\n\n![[a.jpg]]\nCap A\n\nTail\n"
        );
        // The identity permutation changes nothing.
        let same = reorder_members(body, note_index(ctx, "c.md"), ctx, &[0, 1]).unwrap();
        assert_eq!(same, body);
    });
}

#[test]
fn the_same_image_twice_is_addressed_by_ordinal() {
    let body = b"![[a.jpg]]\nfirst\n![[a.jpg]]\nsecond\n";
    with_ctx(&["a.jpg", "c.md"], |ctx| {
        let blocks = member_blocks(body, note_index(ctx, "c.md"), ctx).unwrap();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].image, "a.jpg");
        assert_eq!(blocks[1].image, "a.jpg");
        let reordered = reorder_members(body, note_index(ctx, "c.md"), ctx, &[1, 0]).unwrap();
        assert_eq!(reordered, b"![[a.jpg]]\nsecond\n![[a.jpg]]\nfirst\n");
    });
}

#[test]
fn reorder_refuses_anything_that_is_not_a_permutation() {
    let body = b"![[a.jpg]]\n![[b.jpg]]\n";
    with_ctx(&["a.jpg", "b.jpg", "c.md"], |ctx| {
        let note = note_index(ctx, "c.md");
        for bad in [vec![0], vec![0, 2], vec![1, 1], vec![0, 1, 2]] {
            let err = reorder_members(body, note, ctx, &bad).unwrap_err();
            assert!(
                matches!(err, EditError::NotAPermutation { blocks: 2, .. }),
                "{bad:?} -> {err}"
            );
        }
    });
}

#[test]
fn caption_add_edit_and_delete_change_only_the_block() {
    let body = b"# H\n\n![[a.jpg]]\nold\n\nTail\n";
    with_ctx(&["a.jpg", "c.md"], |ctx| {
        let note = note_index(ctx, "c.md");
        let (edited, change) = edit_caption(body, note, ctx, 0, 1, Some("new")).unwrap();
        assert_eq!(change, CaptionChange::Edited);
        assert_eq!(edited, b"# H\n\n![[a.jpg]]\nnew\n\nTail\n");

        let (added, change) = edit_caption(body, note, ctx, 0, 2, Some("added")).unwrap();
        assert_eq!(change, CaptionChange::Added);
        assert_eq!(added, b"# H\n\n![[a.jpg]]\nold\nadded\n\nTail\n");

        let (deleted, change) = edit_caption(body, note, ctx, 0, 1, None).unwrap();
        assert_eq!(change, CaptionChange::Deleted);
        assert_eq!(deleted, b"# H\n\n![[a.jpg]]\n\nTail\n");
    });
}

#[test]
fn caption_can_be_added_to_a_member_with_none_yet() {
    let body = b"![[a.jpg]]\n";
    with_ctx(&["a.jpg", "c.md"], |ctx| {
        let note = note_index(ctx, "c.md");
        let (added, change) = edit_caption(body, note, ctx, 0, 1, Some("first caption")).unwrap();
        assert_eq!(change, CaptionChange::Added);
        assert_eq!(added, b"![[a.jpg]]\nfirst caption\n");
    });
}

#[test]
fn caption_keeps_the_notes_own_terminator() {
    let body = b"![[a.jpg]]\r\nold\r\n";
    with_ctx(&["a.jpg", "c.md"], |ctx| {
        let note = note_index(ctx, "c.md");
        let (edited, _) = edit_caption(body, note, ctx, 0, 1, Some("new")).unwrap();
        assert_eq!(edited, b"![[a.jpg]]\r\nnew\r\n");
        let (added, _) = edit_caption(body, note, ctx, 0, 2, Some("more")).unwrap();
        assert_eq!(added, b"![[a.jpg]]\r\nold\r\nmore\r\n");
    });
}

#[test]
fn caption_refuses_the_member_line_and_out_of_range_offsets() {
    let body = b"![[a.jpg]]\nold\n";
    with_ctx(&["a.jpg", "c.md"], |ctx| {
        let note = note_index(ctx, "c.md");
        assert!(matches!(
            edit_caption(body, note, ctx, 0, 0, None).unwrap_err(),
            EditError::MemberLineNotCaption { .. }
        ));
        assert!(matches!(
            edit_caption(body, note, ctx, 0, 3, Some("x")).unwrap_err(),
            EditError::NoSuchLine { .. }
        ));
        assert!(matches!(
            edit_caption(body, note, ctx, 0, 2, None).unwrap_err(),
            EditError::NothingToDelete { .. }
        ));
        assert!(matches!(
            edit_caption(body, note, ctx, 1, 1, Some("x")).unwrap_err(),
            EditError::NoSuchBlock { .. }
        ));
    });
}

#[test]
fn a_note_without_a_trailing_newline_keeps_its_line_boundaries() {
    // The final line has no terminator. That shape belongs to the range, not to
    // the block: an unterminated block dropped into the middle of the note
    // would otherwise glue its last line onto the following embed and merge two
    // blocks into one.
    let body = b"![[a.jpg]]\ncapa\n![[b.jpg]]\ncapb";
    with_ctx(&["a.jpg", "b.jpg", "c.md"], |ctx| {
        let note = note_index(ctx, "c.md");
        let reordered = reorder_members(body, note, ctx, &[1, 0]).unwrap();
        assert_eq!(reordered, b"![[b.jpg]]\ncapb\n![[a.jpg]]\ncapa");
        let blocks = member_blocks(&reordered, note, ctx).unwrap();
        assert_eq!(blocks.len(), 2, "both members are still addressable");
        assert_eq!(blocks[0].image, "b.jpg");
        assert_eq!(blocks[1].image, "a.jpg");
        assert_eq!(blocks[0].line_ranges.len(), 2, "caption travels with it");
        // Identity stays byte-exact, terminator shape included, and the
        // reorder round-trips.
        assert_eq!(reorder_members(body, note, ctx, &[0, 1]).unwrap(), body[..]);
        let back = reorder_members(&reordered, note, ctx, &[1, 0]).unwrap();
        assert_eq!(back, body[..]);
        // The slot's terminator keeps its own style, CRLF included.
        let crlf = b"![[a.jpg]]\r\ncapa\r\n![[b.jpg]]\r\ncapb";
        let reordered = reorder_members(crlf, note, ctx, &[1, 0]).unwrap();
        assert_eq!(reordered, b"![[b.jpg]]\r\ncapb\r\n![[a.jpg]]\r\ncapa");
    });
}

#[test]
fn a_note_without_a_trailing_newline_ends_without_one() {
    // The block moving into the final slot carries a terminator; the slot has
    // none, so the note still ends without a trailing newline.
    let body = b"# H\n\n![[a.jpg]]\n\n![[b.jpg]]\ncap\nx";
    with_ctx(&["a.jpg", "b.jpg", "c.md"], |ctx| {
        let note = note_index(ctx, "c.md");
        let reordered = reorder_members(body, note, ctx, &[1, 0]).unwrap();
        assert_eq!(reordered, b"# H\n\n![[b.jpg]]\ncap\nx\n\n![[a.jpg]]");
        assert_eq!(member_blocks(&reordered, note, ctx).unwrap().len(), 2);
    });
}

#[test]
fn caption_refuses_a_line_that_is_not_pure_text() {
    let body = b"![[a.jpg]]\nold\n";
    with_ctx(&["a.jpg", "b.jpg", "c.md"], |ctx| {
        let note = note_index(ctx, "c.md");
        for bad in [
            "# heading",
            "- list item",
            "1. ordered",
            "> quote",
            "| table |",
            "![[b.jpg]]",
            "```",
        ] {
            assert!(
                matches!(
                    edit_caption(body, note, ctx, 0, 1, Some(bad)).unwrap_err(),
                    EditError::NotCaptionText { .. }
                ),
                "{bad:?} should not be a caption"
            );
        }
        // A plain paragraph line, even one that starts with digits or `#`,
        // is a caption.
        assert!(body_edit::member_blocks(body, note, ctx).is_ok());
        assert!(edit_caption(body, note, ctx, 0, 1, Some("2020 was a year")).is_ok());
        assert!(edit_caption(body, note, ctx, 0, 1, Some("#hashtag")).is_ok());
    });
}

#[test]
fn caption_refuses_thematic_breaks_setext_and_table_delimiters() {
    // `---`, `***` and `___` are thematic breaks, `===` and `--` and `-` are
    // setext underlines (a run of `=` or `-` closes a setext heading at any
    // length) and `--- | ---` is a table delimiter row: all are block syntax,
    // not pure text, so none may caption a member (ADR-017 §9).
    let body = b"![[a.jpg]]\nold\n";
    with_ctx(&["a.jpg", "b.jpg", "c.md"], |ctx| {
        let note = note_index(ctx, "c.md");
        for bad in [
            "---",
            "***",
            "___",
            "----",
            "====",
            "=",
            "--",
            "-",
            "- - -",
            "* * *",
            "--- | ---",
            ":-- | --:",
            "---|---",
        ] {
            assert!(
                matches!(
                    edit_caption(body, note, ctx, 0, 1, Some(bad)).unwrap_err(),
                    EditError::NotCaptionText { .. }
                ),
                "{bad:?} should not be a caption"
            );
        }
        // A rule after an embed is not caption content, so it ends the run and
        // stays outside the block: a reorder must leave it in place. A two-mark
        // underline is block syntax too, so it ends the run the same way.
        let with_rule = b"![[a.jpg]]\n---\n![[b.jpg]]\n";
        let blocks = body_edit::member_blocks(with_rule, note, ctx).unwrap();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].line_ranges.len(), 1, "`---` is not a caption");
        assert_eq!(
            reorder_members(with_rule, note, ctx, &[1, 0]).unwrap(),
            b"![[b.jpg]]\n---\n![[a.jpg]]\n"
        );
        let with_short_rule = b"![[a.jpg]]\n--\n![[b.jpg]]\n";
        let blocks = body_edit::member_blocks(with_short_rule, note, ctx).unwrap();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].line_ranges.len(), 1, "`--` is not a caption");
        assert_eq!(
            reorder_members(with_short_rule, note, ctx, &[1, 0]).unwrap(),
            b"![[b.jpg]]\n--\n![[a.jpg]]\n"
        );
        // Ordinary prose around a dash or a pipe is still text. So is a spaced
        // `=` run: only a solid run of `=` or `-` is an underline, so `= =`
        // stays plain text.
        assert!(edit_caption(body, note, ctx, 0, 1, Some("a - b")).is_ok());
        assert!(edit_caption(body, note, ctx, 0, 1, Some("x | y")).is_ok());
        assert!(edit_caption(body, note, ctx, 0, 1, Some("plain words")).is_ok());
        assert!(edit_caption(body, note, ctx, 0, 1, Some("= =")).is_ok());
    });
}
