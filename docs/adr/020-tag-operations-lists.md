# ADR-020: Versioned tag operations lists

Status: Current.

Reviewed bulk tag changes need a portable input that preserves choices and
review context. Use a strict versioned JSON list, with explicit selection and
alternatives, and pass selected operations to the existing tag batch planner and
journaled writer. Require the preview digest on apply. Retained tags are not
renames, and the optional redundant-parent operation requires explicit selection. One operation
ID covers the batch and its alias changes. See [HLD](../HLD.md#tag-operations-lists)
for the input and workflow and [FORMAT](../FORMAT.md#viewer-queries) for tag
query semantics. The digest is a snapshot check, not protection against
non-cooperating writers or a substitute for human approval.
