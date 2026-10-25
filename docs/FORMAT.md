# dimagine library format

- **Version:** 0.2 (draft)
- **Status:** Current

This document defines what a dimagine library is on disk. It is the contract
between people, agents and tools. Anything not written here is free for the
user to decide.

The key words "must", "should" and "may" are used as in RFC 2119.

## 1. Principles

1. **A library is a folder.** Any folder that contains images is a valid
   library. There is no required structure, no required file and no required
   tool.
2. **Files are the source of truth.** Every fact a user or agent records lives
   in a plain file next to the image it describes. Tools may build caches, but
   a cache is never the only copy of anything.
3. **The path is the identity.** An image is identified by its path inside the
   library. Folder layout, file names, numbering schemes and duplicates are the
   user's business; the file system already enforces that a name is unique
   within a folder.
4. **Tools are optional.** Viewers, indexers, search, importers and the CLI are
   conveniences. A library must stay fully usable, readable and editable when no
   dimagine tool has ever touched it, and after every tool is removed.
5. **Unknown is not empty.** A tool that cannot read something reports it as
   unreadable or missing. It never records it as an empty value.

dimagine deliberately follows the conventions of Obsidian vaults (Markdown,
wikilinks, YAML properties, JSON Canvas), so a library can also be opened as an
Obsidian vault. This is a description of compatibility, not an affiliation.

## 2. Files in a library

| File | Required | Purpose |
| --- | --- | --- |
| `<name>.<ext>` | yes | An image. The original is never modified by tools. |
| `<name>.<ext>.md` | no | The image note: properties and free text about one image. |
| `<name>.<ext>.<source>.json` | no | Original metadata from an import source, kept verbatim. |
| any other `*.md` | no | Notes, including collections (§5). |
| `*.canvas` | no | Boards in JSON Canvas format (§6). |
| `.dimagine/` | no | Tool settings and caches (§8). Safe to delete. |
| `.trash/` | no | Deleted groups waiting to be restored or purged (§7.1). Ignored like any dotted name; emptying it is the permanent delete. |

### 2.1 Images

A file is an image if its extension is one of `jpg`, `jpeg`, `png`, `gif`,
`webp`, `avif`, `heic`, `heif`, `tif`, `tiff`, `bmp` (case-insensitive). Tools
should detect the real format from the file content and report a mismatch with
the extension rather than rename the file.

Other files (video, PDF, archives) may live in a library. Version 0.1 does not
describe them; tools must leave them untouched.

File names should avoid `[`, `]`, `#`, `^` and `|`. These characters break
wikilinks (§5.1). Tools that create files must replace them; tools that find
them in existing names should report them, not rename.

### 2.2 Ignored paths

Tools must ignore: any file or folder whose name starts with `.` (including
`.dimagine/` for content purposes), `._*` resource-fork files, `Thumbs.db` and
`desktop.ini`. Tools must not follow symbolic links that leave the library and
should not follow symbolic links at all when scanning.

## 3. The image note

An image note sits in the same folder as its image and is named after the
image's full file name plus `.md`:

```
refs/deep-sea/girl-underwater.jpg
refs/deep-sea/girl-underwater.jpg.md
```

The pairing is by name only. If the image is moved or renamed, its note should
be moved or renamed with it (§7).

### 3.1 Properties

Properties are YAML front matter, as in Obsidian. Every property is optional.
Unknown properties must be preserved by tools.

```markdown
---
id: 01JA8X3Q7K2M9V4T6R1B5N0C3Q
title: Girl sinking into deep water
tags: [underwater, sketch, monochrome]
rating: 4
source: https://example.com/artwork/12345
author: Example Artist
license: unknown
created: 2021-10-15
added: 2023-07-12T20:54:07+01:00
imported: 2026-10-04T14:30:12+01:00
copied_from:
sources:
  - type: eagle
    library: References
    item: L8X2Q4M7Z1A9B
    imported: 2026-10-04T14:30:12+01:00
    importer: dimagine-import-eagle 0.1.0
    raw: girl-underwater.jpg.eagle.json
---

Free Markdown about the image. Links to other notes work as usual: [[sleep-pv]].

![[girl-underwater.jpg]]
```

| Property | Type | Meaning |
| --- | --- | --- |
| `id` | string (ULID) | Stable reference handle (§3.3). |
| `title` | string | Human title. |
| `tags` | list of strings | Tags, without `#`. |
| `rating` | integer 0–5 | User rating. |
| `source` | URL | Where the image was found. |
| `author` | string | Creator of the image, if known. |
| `license` | string | License or `unknown`. |
| `created` | date or datetime | When the image was made, if known. |
| `added` | datetime with offset | When the image first entered the user's collection, possibly in another tool (e.g. Eagle's add time). Tools sort "newest added" by `added`, else `imported`. |
| `imported` | datetime with offset | When it entered this library. |
| `copied_from` | string (`id`) | Set on a copy that received a new `id`. |
| `sources` | list of maps | One entry per import (§3.4). |

Dates and times use ISO 8601. Datetimes must carry a UTC offset.

`rating` is an integer 0–5. A value outside that range, or one that is not an
integer, is read as no rating rather than as a number that would sort above
five.

A note may begin with a UTF-8 byte-order mark. Tools must read such a note and
must keep the mark when they write one.

Write strings that YAML could misread (`no`, `yes`, `on`, `off`, values
starting with `@`, `*`, `&`, `!`, `%`, or containing `: `) in quotes.

### 3.2 The self-embed

An image note should end with an embed of its own image, so that any Markdown
viewer shows the picture together with its note:

```markdown
![[girl-underwater.jpg]]
```

- Use the bare file name when it is unique in the library, otherwise the path
  (§5.1), as for any other link.
- The self-embed is a preview, not a membership: tools must not treat an image
  note as a collection of its own image (§5).
- Tools that write a note add the self-embed if it is missing and keep it as the
  last block. A note without one is still valid.
- When the image is renamed or moved to a place where its bare name is no longer
  unique, the self-embed is updated like any other link (§7).

### 3.3 The `id` property

- `id` is optional. A person or agent may leave it out.
- If present it must be a ULID (26 characters, Crockford base32) that is unique
  within the library.
- Tools assign an `id` only when one is needed: when the image is first added to
  a collection, tagged, imported with metadata, or uploaded (§9). Images nobody
  has touched stay without a note.
- Once written, an `id` is never changed, except when two notes carry the same
  `id`. Then the one that is a copy receives a new `id` and records the original
  in `copied_from`.
- Never invent an `id` by hand. Leave it out, or generate a real ULID.

The `id` lets tools notice that an image moved (same `id`, new path) and repair
links that still point at the old path. Links themselves stay plain paths (§5).

### 3.4 Sources and raw metadata

Each import appends one entry to `sources`. When the source provides its own
metadata, the importer stores it verbatim next to the image as
`<name>.<ext>.<source>.json` (for example `girl.jpg.eagle.json`) and names that
file in the entry's `raw` field. Raw files are read-only by convention: tools
and agents do not edit them.

A file that arrives by upload (§7.2) records that in its note's `sources` with
one entry carrying `type: upload`, the upload `imported` time, the `importer`
(`dimagine` and its version) and the `original_name` the file arrived with. A
bare upload carries no `raw` file and no source URL; the arrival name and time
are its provenance.

A merge (§7.3) records the image it folded away with one `type: merged` entry:
the time of the merge (`merged_at`, RFC 3339 UTC), the dropped image's library
path (`path`) and its content `sha256`, and — when the dropped image had one —
its `source`. The dropped image's own `sources` entries are appended after it,
re-indented to fit, so nothing the drop recorded is lost. The kept image's own
`source` is never overwritten by a merge.

## 4. Folders

Folders mean whatever the user wants them to mean. dimagine assigns no meaning
to any folder name and requires no folder.

Tools that import or create files need a default place. They should use
`inbox/` for items with no better home, and should mirror the source's own
structure when there is one (for example an Eagle folder path). An upload's
default place is `attachmentFolderPath` in `.obsidian/app.json` when that
resolves to a place inside the library, and `inbox/` otherwise (§7.2); a tool
reads `.obsidian/` and never writes it.

## 5. Collections

A collection is a Markdown note that embeds images. Order of embeds is the order
of the collection. Any note can be a collection, except that an image note's
self-embed (§3.2) does not make it one. Adding `kind: collection` to its
properties lets tools list it as one.

```markdown
---
kind: collection
title: Sleep PV — shot 1
tags: [project]
---

Shot 1: a girl sinks into the deep sea.

![[refs/deep-sea/girl-underwater.jpg]]
Too bright; keep the bubbles.

![[girl-curled-up.jpg]]
```

- An image may appear in any number of collections.
- Text between embeds is free; a line directly after an embed is that member's
  note by convention.
- Nested collections are links or embeds of other collection notes, or headings
  inside one note.
- Both Obsidian embeds (`![[path]]`) and standard Markdown images
  (`![alt](path)`) are valid. Standard Markdown paths with spaces must be
  percent-encoded or wrapped in `<>`.

### 5.1 Link resolution

Links resolve like Obsidian wikilinks:

1. A link with a folder part (`refs/deep-sea/girl.jpg`) is a path relative to the
   library root, or else relative to the note's folder.
2. A bare file name (`girl.jpg`) resolves if exactly one file in the library has
   that name.
3. If a bare name matches several files, the link is ambiguous. Tools must report
   it and must not guess. Writers should use a path whenever a name is not unique.

A link that resolves to nothing is reported as missing; the line is kept.

### 5.2 Member blocks, captions and reordering

A **member block** is a member line — a line whose only content is one image
embed that resolves to an image — together with the lines of plain text directly
after it. Those following lines are the member's **caption lines**. A blank line
ends the block, and so does any line that is not plain text: a heading, a list
item, a blockquote, a table row, a code fence or an embed. A block's member line
and its caption lines are addressed together, by ordinal in document order,
because the same image may appear twice.

A tool that **reorders** a collection permutes the blocks among the byte ranges
they already occupy and changes nothing else: every byte outside the blocks —
blank lines, headings, prose, the front matter — is reproduced unchanged and in
order. The line terminator that closes a range belongs to the range, not to the
block in it, so no line boundary moves and a note that does not end in a newline
still does not after the reorder. Anything that is not a permutation of the
blocks is refused whole. A tool that moves a member **out** of a collection
leaves the caption text where it was, for the user to delete.

A tool that edits a **caption line** replaces, appends or deletes one line of
plain text in a block. The member line itself is not a caption: removing it is
removing the member.

### 5.3 Whole-body edit

A note's **body** is every byte after the line its front matter closes on — the
same delimiter rule the reader uses to find the body (§5.1, §3). A tool that
replaces the whole body overwrites exactly those bytes and nothing before them:
the front matter is carried over untouched and never re-serialized, and the
body's own line endings and trailing newline (or its absence) are kept as
given, as is the file's leading byte-order mark, if any (§3.1). A note with no
front matter has a body that is the whole file (minus a
BOM, §3.1); a note whose opening `---` fence is never closed has no defined body
boundary, so it is refused rather than guessed at. A note that ends *at* its
closing fence has an empty body and no line break after the fence; the break is
added, in the file's own line-ending style, because text spliced onto the fence
line would no longer be a fence and the front matter would stop parsing.

Replacing a body is a whole-file write, so it carries a precondition on the bytes
it replaces (§9): a tool must not clobber a change it never read. The self-embed
still applies to the result (§3.2): an image note whose new body no longer embeds
its own picture gets the embed added at the end. Collection notes and other plain
notes are edited as text the same way; only the self-embed rule is specific to
image notes.

One caveat belongs to the *client*, not the format: an HTML `<textarea>`
normalizes its value to LF, so a note with CRLF line endings that is edited
through a browser form comes back with LF in the body, even though the file's
line endings are otherwise kept as given. The CLI and the write endpoint splice
the bytes they are handed verbatim; only a browser textarea rewrites them.

## 6. Boards

A board is a [JSON Canvas](https://jsoncanvas.org/) file (`*.canvas`). Image
nodes are `file` nodes whose `file` is a library path. A board is a collection
whose members also carry positions.

## 7. Moving, renaming, copying, uploading and deleting

These are plain file operations. To keep a library consistent:

- Move or rename an image together with its note and raw files.
- After a move or rename, update links that used the old path or name. A tool
  may do this automatically using `id` (§3.3).
- A copied image is a new image. If its note was copied too, the duplicate `id`
  is resolved as in §3.3.

A tool that moves an image automatically works as follows. The move plan
fixes the `id` before anything moves; it lands with the note's edit. It
renames the whole group and refuses the move if any destination is
already taken. Then it rewrites, in place, every link that resolved to the
group: each keeps its syntax and its display parts — alias, heading, block
reference, alt text, title — and names the target by its full library path;
links inside code blocks and inline code are left alone. A `.canvas` is
updated by parsing its JSON and replacing only the `nodes[].file` values that
pointed at the group, never by replacing text. A link that was already
ambiguous before the move is reported and left as it is. A move that ends with
an ambiguous bare name — one the repaired links could not disambiguate — is
refused. After a move by any other tool, `dimagine check` reports the links it
left pointing at nothing.

### 7.1 Deleting

A tool that deletes moves a group — the image, its note, and every raw metadata
sidecar named after the image — into `.trash/` at the library root, keeping each
file's library-relative path, and writes a record beside them:

```
.trash/refs/girl.png
.trash/refs/girl.png.md
.trash/refs/girl.png.trash.json
```

The record is JSON: the operation's id, the time it trashed (`trashed_at`,
RFC 3339 UTC), and for each file its original library path, its path in
`.trash/` and its modification time. A group whose name is already taken in
`.trash/` gets the next bumped name (`girl 2.png`), and the record moves with
it. Nothing is deleted in place: a tool removes a file permanently only when it
is past the retention period and the user asked for the purge.

Because `.trash/` starts with a dot it is ignored (§2.2): a trashed image is
out of the library as far as every other tool is concerned, and a restore is a
plain move back to the recorded original path.

A group with no record is never deleted automatically, whoever put it there —
including a file Obsidian moved into `.trash/` itself.

An older version of this document named `.dimagine/trash/` as the place to
delete into. That location is no longer written: a tool puts new groups in
`.trash/`. It stays readable, so a library that already has one is not stranded
— a tool reports what is in it and leaves it alone. Recovering those files is a
manual move back into the library, because the format has no record of where
they came from.

Two operations remove a file for good, and both are irreversible: deleting the
groups past the retention period, and emptying the whole trash on request. A
tool that offers either asks for explicit confirmation, and neither has an
undo.

### 7.2 Uploading

Uploading brings a file that lives outside the library into it as an image
group: the image, its note, and — when the upload names a collection — one
embed line appended to that collection (§5). Upload never opens an existing
image or a `*.json` sidecar for writing (§2.1); it only creates new files, joins
one collection note, and refuses a duplicate rather than overwrite.

**Placement.** A named folder wins. Otherwise images land in the library's
default upload folder: `attachmentFolderPath` in `.obsidian/app.json` when it is
set and resolves to a place inside the library, `inbox/` otherwise (§4). An
upload that names an existing collection note lands its images in that default
folder and adds each one to the note (§5), including a duplicate (adding an
image that already existed is a plain join).

**Naming.** A meaningful arrival name is kept, sanitized as in §2.1. A name a
camera, a screenshot or a share panel gives — `IMG_1234`, `PXL_…`,
`Screenshot …`, `image`, `photo`, `download`, `paste`, a stem that is only
digits, with or without a ` (2)` copy suffix — is meaningless, and the image
lands as `Upload <YYYYMMDDHHMMSS>.<ext>` with the original name kept in the
note's `title` (§3.1) and in its `sources` (§3.4). A name the library already
holds — matching NFC and case-insensitively across the whole library, the rule
§5.1 resolves bare names by — takes the next free ` 2`, ` 3`, ... before the
extension, created exclusively.

**Type.** The arrival name carries an image extension (§2.1) and the file's
leading bytes must be the format that extension claims; a mismatch is refused.
SVG, HTML and XML are refused on both sides — by the final extension and by the
bytes.

**Deduplication.** Before copying, the tool compares the file's SHA-256 with the
images already in the library — the ones that are not in `.trash/` (§7.1).
Bytes the library already holds are not copied a second time: the upload reports
the image that already has them. When the library's index holds no digest to
compare against, the upload copies; a duplicate copy is always recoverable, a
refused upload never arrives.

**The note.** A new image lands with a note (§3): a fresh `id` (§3.3), `added`
and `imported` set to the upload time with a numeric offset (§3.1), the upload
`sources` entry (§3.4), and the self-embed (§3.2). An upload interrupted
mid-flight leaves its half-state in the journal (§8.3), which the next writer
recovers; an image left in a landing folder with no note is what a lost journal
can still leave behind, and `dimagine check` reports it (`orphaned_upload`).

### 7.3 Merging

Two images can be the same picture at different sizes — a channel that posts a
compressed copy and then the original. `dimagine check near_duplicate` reports
these read-only (§9); a merge resolves one into the other without inventing any
structure Obsidian cannot read (ADR-012): the result is an ordinary image and
note, and the loser sits in the trash like any other delete (§7.1).

A merge names one kept image and one dropped image. It is one operation, so
`undo` hands both images back exactly as they were.

- **Provenance is kept.** The drop's `tags` are unioned into the keep; `added`
  becomes the earlier of the two; the drop's provenance is recorded in the keep's
  `sources` as one `type: merged` entry, with the drop's own `sources` entries
  appended after it (§3.4). The keep's own `source` is never overwritten. A keep
  that had no note gains one carrying this provenance and a fresh `id` of its own
  (§3.3).
- **The whole drop group is trashed.** The dropped image, its note, and every raw
  sidecar named after it move into `.trash/` together, with the record §7.1
  describes — nothing is removed in place, and a restore brings the group back.
- **Links follow the keep.** Every link that resolved to the drop — a collection
  member, a note's embed or wikilink, a `.canvas` file node — is repointed to the
  keep by the same in-place rule a move uses (§7), each keeping its syntax and
  display parts. The drop's own note is never rewritten; it moves whole. A link
  that was already ambiguous before the merge is reported and left as it is.

A merge keeps one image and discards another, so a tool picks the keeper
deliberately. By default the caller's first argument is the keeper. With `--auto`
the keeper is chosen from the two files by the picture they hold — more pixels,
then (at equal pixels) a lossless format such as PNG over a lossy one, then a
larger file, then, if the two are still tied, the lexicographically later library
path — and the two arguments may be given in any order.

`dimagine merge` refuses a keep and drop that are the same image, and refuses to
guess a `sources` value it cannot safely append to (a flow-style `sources: []`
rather than a block of entries). It also refuses when either note is larger than
the byte limit a reader will take from one note: a note it cannot read is not an
absent one, and the merge is refused before any file moves. A `--dry-run` prints
exactly what the merge would change and writes no library byte.

## Writing

The rules a tool must follow to *change* a library live in the section that
owns each fact; this index is a map to them, not a second copy of any rule. It
exists because, from version 0.2 on, the format itself — not just the viewer —
is written by tools, so the writing contract is part of what the format
promises.

| Writing concern | Where the rule lives |
| --- | --- |
| Note front matter: tags, `rating`, scalar properties | §3.1 |
| The self-embed a written note must keep | §3.2 |
| Assigning the `id` a membership gives an image | §3.3 |
| Link syntax and resolution (bare name vs full library path) | §5.1 |
| Member blocks, caption lines, add/remove and reordering | §5.2 |
| Whole-body edit (front matter untouched, open-hash precondition) | §5.3 |
| Move, rename, copy, upload, and the group delete | §7 |
| Merge two near-duplicate images into one | §7.3 |
| The journal, write log and settings a writer keeps | §8.3 |

## 8. The `.dimagine/` folder

`.dimagine/` holds only things that are meaningless to other tools and safe to
lose: settings, plugin state, previews, indexes and embeddings. Deleting it must
not lose any user data. It should be excluded from backups and sync.

`.dimagine/settings.json` is the library's settings: one JSON object keyed by
setting, the key dotted and namespaced (`trash.retentionDays`). Only the
settings the user changed from their default are written; a tool that writes the
file leaves keys it does not know untouched, and never stores a secret in it. A
tool reads what it knows, treats a value that fails its own rule as unset, and
says so once rather than refusing to start.

### 8.1 Caches

Derived data is keyed by the SHA-256 of the image bytes, never by path. Moving or
renaming an image therefore never invalidates its caches, and identical images
share them.

### 8.2 Previews

Tools that generate previews should produce:

| Name | Size rule | Use |
| --- | --- | --- |
| `thumb` | long edge ≤ 400 px **and** aspect ≤ 5:2 — a more extreme aspect is cropped to 5:2 first, from the top for tall sources and from the left for wide ones; the short edge is ≥ 40 px unless the source's own short edge is smaller (previews never upscale) | grids, phones |
| `view` | long edge ≤ 1568 px **and** area ≤ 1,150,000 px | detail view, and images sent to vision models |

- Format: JPEG at quality about 88; PNG when the image has transparency. These two
  formats are accepted by every major vision-model API.
- Never upscale. Apply EXIF orientation. Convert to sRGB. Use the first frame of
  animated images.
- A rendition is valid only for the rendition policy that produced it. The cache
  key carries a policy generation, so a tool regenerates an entry written by an
  older policy instead of reusing it — even when its dimensions would still match.
- Larger renditions are produced on demand from the original, not stored by
  default.

### 8.3 The write log and journal

A tool that writes the library records what it did:

- `.dimagine/log/ops.jsonl` — one line per step, newest last: the operation's
  id and time, who asked (`source`: a CLI, the viewer, an import), the intent as
  JSON, the step's kind and path, and the SHA-256 each file had before and after.
  The bytes needed to put a step back ride along, which is what an undo reads.
  The log keeps its last 200 operations; older ones are dropped.
- `.dimagine/journal/<op-id>/` — the in-progress steps of an operation, removed
  when it finishes. A folder left here means an operation did not complete and
  needs recovery; `check` reports it.
- `.dimagine/write.lock` — held while an operation runs.
- `.dimagine.tmp.<nonce>` — a temporary file beside the one being replaced,
  renamed into place. It carries the dotted prefix so it is ignored (§2.2).

All of it is derived: an operation the log does not describe still left its
files on disk, and a library with the folder deleted is a normal library that
can no longer be undone.

## 9. The `dimagine` CLI

This document describes the files; these are the commands that write them. The
full behaviour is in [HLD.md](HLD.md). Every command takes `--library <path>`
(default: the current directory) and `--json`.

```
dimagine tag <img> [--add T] [--remove T]     # edit an image note's tags
dimagine rate <img> <0-5|none>                # set or clear its rating
dimagine set <img> KEY=VALUE …                # set scalar properties (KEY= clears)
dimagine edit <note> (--body-file <path>|--stdin) [--expect <sha256>]  # replace the body (§5.3)
dimagine collect --new <title> <img>…         # create a collection note (§5)
dimagine collect --add|--remove <note> <img>… # edit a collection's members
dimagine collect --reorder <note> <ordinal>…  # permute the member blocks (§5.2)
dimagine collect --caption <note> <block> <line> <text|->  # edit one caption line (§5.2)
dimagine mv <img> <new-path> [--dry-run]      # move a group and repair its links (§7)
dimagine merge <keep> <drop> [--auto] [--dry-run]  # fold a near-duplicate into the keep, trash the drop (§7.3)
dimagine upload <file>… [--to <folder>] [--to-collection <note>] [--dry-run]  # land files as image groups (§7.2)
dimagine trash <img>…                         # move groups into .trash/ (§7.1)
dimagine restore <img>…                       # move them back
dimagine undo [<op-id>] [--force]             # reverse one recorded operation
dimagine config list|get|set|reset …          # read or write .dimagine/settings.json (§8)
dimagine trash purge [--dry-run] [--yes]      # delete groups past the retention period
dimagine trash empty [--yes]                  # delete every trashed group now
dimagine check                                # report problems, including past writes
dimagine check near_duplicate                 # report same-picture images by fingerprint (§7.3), read-only
```

A command that changes a file prints the id of the operation it made, which is
what `undo` takes; `--json` prints one document per command carrying the same
id. `purge` and `empty` delete for good, so each needs an explicit `--yes`.

## Viewer queries

The web viewer and its JSON API select images by URL query strings. What a
query *means* is part of this contract — two tools answering the same URL must
show the same images — while how a tool stores the facts it filters on is its
own business.

| Param | Values | Default |
| --- | --- | --- |
| `in` | library-relative folder path | (all folders) |
| `sub` | `0` / `1`, include subfolders | `1` |
| `c` | collection note path (§5) | — |
| `sel` | library-relative image path (the picked-out photo) | — |
| `tag` | repeatable, AND | — |
| `q` | full-text query over note text, titles and tags | — |
| `rating` | `1`–`5`, or `unrated` | — |
| `type` | `jpg` (also `jpeg`), `png`, `gif`, `webp`, `avif`, `heic`, `svg`, `other` | — |
| `sort` | `added`, `taken`, `modified`, `name`, `size`, `rating` | `added` |
| `dir` | `asc`, `desc` | `desc`, `asc` for `name` |
| `size` | `s`, `m`, `l` (thumbnail size) | `m` |
| `p` | page number, 120 items per page | `1` |
| `untagged` | `0` / `1` — only images whose note carries no tag | `0` |
| `recent` | `0` / `1` — the most recently added images | `0` |
| `more` | `tags`, `folders`, `collections` (show a facet list whole) | — |

An unknown or malformed value is ignored and reported as a notice beside the
grid; it never breaks the page. Generated URLs carry only the values that
differ from the defaults, and one spelling each.

**`rating`** is one chip at a time. `1`–`5` are the `≥N` chips: only images
whose rating is at least `N`. A missing `rating` and a `0` are not ratings
(§3.1), so no threshold shows them: `rating=unrated` — spelled `0` in a
hand-typed URL, which reads back as `unrated` — lists those and only those. A
threshold and the unrated chip never ride together.

**`type`** is one family at a time, read from the path's extension and never
from the bytes. `jpg` covers both `jpg` and `jpeg`; the next six name
themselves; `other` is every extension those seven do not name — §2.1's `heif`,
`tif`, `tiff`, `bmp` and the rest. `svg` is a family the chips offer, and §2.1
admits no svg as an image, so an ordinary library shows nothing under it. The
match is case-insensitive.

## 10. Versioning

This document carries the format version. Libraries do not record a version;
files are expected to stay readable across versions, and a future version that
cannot keep that promise will describe a migration.

## Changelog

| Version | Date | Change |
| --- | --- | --- |
| 0.1 | 2026-10-04 | First draft. |
| 0.1 | 2026-10-04 | Draft revision: image notes end with a self-embed (§3.2); file names avoid `[ ] # ^ \|` (§2.1). |
| 0.1 | 2026-10-05 | Additive optional property `added` (§3.1): when the image first entered the collection, possibly in another tool. |
| 0.1 | 2026-10-06 | A leading UTF-8 BOM in an image note is read and preserved (§3.1). |
| 0.1 | 2026-10-06 | `thumb` previews keep a readable shape: long edge ≤ 400 px, aspect ≤ 5:2; an extreme-aspect source is cropped to 5:2 from the top (tall) or the left (wide) instead of shrinking to a sliver, and the short edge is ≥ 40 px unless the source itself is smaller (§8.2). |
| 0.1 | 2026-10-06 | A rendition is valid only for the rendition policy that made it: the cache key carries a policy generation, and older-policy entries are regenerated, not reused (§8.2). |
| 0.1 | 2026-10-06 | An out-of-range or non-integer `rating` reads as no rating (§3.1). |
| 0.1 | 2026-10-13 | Deletion is a group move into `.trash/` with a `<name>.trash.json` record beside it (§7.1); `.trash/` joins the ignored dotted names (§2.2). `.dimagine/` gains `settings.json`, `log/ops.jsonl`, `journal/`, `write.lock` and the `.dimagine.tmp.` temporary prefix (§8, §8.3). |
| 0.1 | 2026-10-13 | The old `.dimagine/trash/` is no longer written to but stays readable, so a library that still holds one reports its files instead of losing track of them (§7.1). A group with no record is never deleted automatically (§7.1). |
| 0.1 | 2026-10-13 | The CLI that writes the format is listed with what each command does (§9); Versioning moves to §10. |
| 0.1 | 2026-10-16 | The contract for an automatic move (§7): the `id` is fixed before anything moves and lands with the note's edit, the group renames together and refuses a taken destination, links are rewritten in place keeping their syntax and display parts, `.canvas` `nodes[].file` values follow, ambiguous links are reported and not touched. `dimagine mv` joins the CLI (§9). |
| 0.1 | 2026-10-17 | Member blocks, caption lines and reordering (§5.2): a member line plus the plain-text lines that follow it; reordering permutes only the blocks and leaves every other byte, line boundary and trailing-newline shape alone; a caption edit touches one line and never the member line. `dimagine collect --reorder` and `collect --caption` join the CLI (§9). |
| 0.1 | 2026-10-17 | The whole-body edit (§5.3): a note's body is the bytes after its front matter's closing line; a replacement keeps the front matter and every byte-order mark, line ending and trailing newline as given, carries a precondition on the bytes it replaces, and keeps the self-embed rule on the result; a note whose opening fence is never closed is refused, and a note that ends at its closing fence is given the line break its body needs. `dimagine edit` joins the CLI (§9). |
| 0.1 | 2026-10-17 | Upload (§7.2): placement is a named folder, else the default upload folder — `attachmentFolderPath`, else `inbox/` — plus a plain join when the upload names a collection; naming keeps a meaningful name and renames a meaningless camera, screenshot or share name to `Upload <YYYYMMDDHHMMSS>.<ext>` with the original kept in `title` (§3.1) and `sources` (§3.4), taking ` 2` on a library-wide NFC, case-insensitive collision; the bytes must match the extension, with SVG, HTML and XML refused on both sides; identical bytes already in the library (outside `.trash/`) are not copied again; a new image lands with a note carrying `id` (§3.3), `added`, `imported` and the upload `sources` entry (§3.4). `dimagine upload` joins the CLI (§9). |
| 0.2 | 2026-10-19 | Minor bump for the write contract (ADR-017 §12). Every 3b write operation now lands — whole-body edit, member-block reorder, caption edit, upload and the collection join. A new "Writing" index (before §8) maps each writing rule to the section that owns it, so the contract is reachable without a rule living in two places. |
| 0.2 | 2026-10-20 | Near-duplicate merge (§7.3): `dimagine merge <keep> <drop>` folds one image's provenance into another and moves the loser's whole group into `.trash/` as one undoable op — tags unioned, earliest `added`, a `type: merged` entry plus the drop's own `sources` appended to the keep's `sources` (§3.4) without touching the keep's own `source`, a noteless keep gaining a note with a fresh `id` (§3.3), and links that named the drop repointed to the keep by the move rule (§7); `--auto` keeps the file with more pixels, then a lossless format, then a larger file, then the lexicographically later library path; a note too large to read refuses the merge rather than reading as absent. `dimagine check near_duplicate` reports same-picture images from a stored fingerprint, read-only. Both join the CLI (§9). |
| 0.2 | 2026-10-25 | The viewer's URL queries join the contract (Viewer queries): `rating` is one `≥N` threshold chip or the unrated chip — a missing `rating` and a `0` are unrated (§3.1), so no threshold shows them — and `type` is one extension family read off the path (§2.1). |
