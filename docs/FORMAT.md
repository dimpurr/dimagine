# dimagine library format

- **Version:** 0.1 (draft)
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
| `imported` | datetime with offset | When it entered this library. |
| `copied_from` | string (`id`) | Set on a copy that received a new `id`. |
| `sources` | list of maps | One entry per import (§3.4). |

Dates and times use ISO 8601. Datetimes must carry a UTC offset.

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
  a collection, tagged, or imported with metadata. Images nobody has touched stay
  without a note.
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

## 4. Folders

Folders mean whatever the user wants them to mean. dimagine assigns no meaning
to any folder name and requires no folder.

Tools that import or create files need a default place. They should use
`inbox/` for items with no better home, and should mirror the source's own
structure when there is one (for example an Eagle folder path).

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

## 6. Boards

A board is a [JSON Canvas](https://jsoncanvas.org/) file (`*.canvas`). Image
nodes are `file` nodes whose `file` is a library path. A board is a collection
whose members also carry positions.

## 7. Moving, renaming, copying and deleting

These are plain file operations. To keep a library consistent:

- Move or rename an image together with its note and raw files.
- After a move or rename, update links that used the old path or name. A tool
  may do this automatically using `id` (§3.3).
- A copied image is a new image. If its note was copied too, the duplicate `id`
  is resolved as in §3.3.
- Tools that delete should move files to `.dimagine/trash/` rather than remove
  them.

## 8. The `.dimagine/` folder

`.dimagine/` holds only things that are meaningless to other tools and safe to
lose: settings, plugin state, previews, indexes and embeddings. Deleting it must
not lose any user data. It should be excluded from backups and sync.

### 8.1 Caches

Derived data is keyed by the SHA-256 of the image bytes, never by path. Moving or
renaming an image therefore never invalidates its caches, and identical images
share them.

### 8.2 Previews

Tools that generate previews should produce:

| Name | Size rule | Use |
| --- | --- | --- |
| `thumb` | long edge ≤ 400 px | grids, phones |
| `view` | long edge ≤ 1568 px **and** area ≤ 1,150,000 px | detail view, and images sent to vision models |

- Format: JPEG at quality about 88; PNG when the image has transparency. These two
  formats are accepted by every major vision-model API.
- Never upscale. Apply EXIF orientation. Convert to sRGB. Use the first frame of
  animated images.
- Larger renditions are produced on demand from the original, not stored by
  default.

## 9. Versioning

This document carries the format version. Libraries do not record a version;
files are expected to stay readable across versions, and a future version that
cannot keep that promise will describe a migration.

## Changelog

| Version | Date | Change |
| --- | --- | --- |
| 0.1 | 2026-10-04 | First draft. |
| 0.1 | 2026-10-04 | Draft revision: image notes end with a self-embed (§3.2); file names avoid `[ ] # ^ \|` (§2.1). |
