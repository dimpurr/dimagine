---
name: dimagine
description: Create and manage a dimagine image library — a plain folder of images with optional Markdown notes, collections and JSON Canvas boards, following Obsidian conventions. Use when the user asks to build, organise, tag, search or curate a reference image library, a moodboard, a storyboard, or an "Obsidian for images", or mentions dimagine.
---

# dimagine library skill

A dimagine library is a folder of images. Everything else is optional and lives
in plain files next to the images. You need no program to work with it: use
ordinary file operations and the rules below. The full specification is
`docs/FORMAT.md` in the dimagine repository; this skill is the working summary.

## Golden rules

1. **Never modify an original image.** Copy, move and rename are fine; editing
   pixels or embedded metadata is not.
2. **Keep an image and its companions together.** `girl.jpg`, `girl.jpg.md` and
   `girl.jpg.<source>.json` always move and rename as a group.
3. **Paths are identity.** After any move or rename, update links that point to
   the old path.
4. **Unknown is not empty.** If you could not read or find something, say so;
   do not write an empty value.
5. **Do not write inside `.dimagine/`.** It belongs to tools and may be deleted.
6. **Never invent an `id`.** Leave it out, or generate a real ULID.

## Create a library

Any folder works. Nothing has to be created up front:

```bash
mkdir -p ~/Pictures/refs
```

The user chooses the folder structure. If they have no preference, put new
items in `inbox/` and suggest topic folders later.

## Add an image

1. Copy the image into the chosen folder. Keep a meaningful file name if it has
   one.
2. Avoid `[ ] # ^ |` in file names; they break links. Replace them with `-`.
3. If the image has no meaningful name (`image.png`, `download.jpg`,
   `IMG_1234.jpg`, a pasted image) and has a source URL, name it
   `<site>-<id>`: take the URL path only (ignore `?query` and `#fragment`),
   skip path segments containing `=`, split the rest on non-alphanumeric
   characters, and use the first ID-shaped token: all digits (5–20 long)
   preferred, otherwise letters mixed with digits (6–24 long). Ignore longer
   tokens; they are hashes. `<site>` is the host's main label (`twitter`,
   `amazon`). Without a usable ID, name it after the import time plus four
   characters: `20261004-143012-q3f7.jpg`. The full URL stays in the note.
4. If a file with that name already exists in the folder, add `-2`, `-3`, ...
   before the extension.
5. Only if there is something to record (source, author, tags, a note), write an
   image note next to it.

## Write an image note

File name: the image's full name plus `.md`. Every property is optional;
include only what you know.

```markdown
---
title: Girl sinking into deep water
tags: [underwater, sketch, monochrome]
rating: 4
source: https://example.com/artwork/12345
author: Example Artist
added: 2023-07-12T20:54:07+01:00
imported: 2026-10-04T14:30:12+01:00
---

Why this image is here, what to notice, related ideas.

![[girl-underwater.jpg]]
```

- End the note with an embed of its own image, so the picture shows when the
  note is opened. Use the bare file name if it is unique in the library,
  otherwise the path. This preview does not make the note a collection.
- Tags are a YAML list without `#`. `rating` is an integer 0-5; anything
  else reads as no rating.
- Datetimes are ISO 8601 with a UTC offset.
- Quote strings YAML could misread: `"no"`, `"on"`, `"Note: draft"`.
- Preserve properties you do not recognise, and a byte-order mark if the note
  has one.
- Do not edit `*.<source>.json` files; they are verbatim records of an import.

## Make a collection

A collection is a note that embeds images. Add `kind: collection` so tools can
list it.

```markdown
---
kind: collection
title: Sleep PV — shot 1
---

Shot 1: a girl sinks into the deep sea.

![[refs/deep-sea/girl-underwater.jpg]]
Keep the bubbles, darker water.

![[girl-curled-up.jpg]]
```

- Order of embeds is the order of the collection.
- The line right after an embed is that member's note. The plain-text lines that
  follow, up to a blank line or a heading, list, quote, table or embed, are all
  that member's caption; anything that is not plain text ends the block.
- To reorder, move only the embed (and its caption lines) and leave the blank
  lines, headings and other prose where they are. Prefer
  `dimagine collect --reorder <note> <ordinals…>`.
- Use a bare file name only if it is unique in the library; otherwise use the
  path. Check with `find . -name 'girl.jpg' -not -path './.*'`.
- For a project with shots, either use one note with a heading per shot, or one
  note per shot linked from a project note.
- An image can be in any number of collections. Never copy an image to put it in
  a second collection; embed it again instead.

## Make a board

Use a JSON Canvas file (`*.canvas`) with `file` nodes pointing at library paths.
Use a board when spatial arrangement matters; otherwise use a collection.

## Move or rename

```bash
dimagine mv refs/girl.jpg refs/deep-sea/girl.jpg
```

One command renames the whole group — the image, its note, its raw sidecars —
and rewrites every link that pointed at it, in one operation `dimagine undo`
can reverse. It adds the note and the `id` if either is missing, refuses when
the destination is taken, and reports links that were already ambiguous
instead of guessing. `--dry-run` prints the plan without touching a library
byte (opening the library may still finish a crashed operation's recovery,
and reports it).

Without the CLI, move the group yourself:

```bash
mv refs/girl.jpg refs/deep-sea/girl.jpg
mv refs/girl.jpg.md refs/deep-sea/girl.jpg.md 2>/dev/null
mv refs/girl.jpg.*.json refs/deep-sea/ 2>/dev/null
```

Then find and fix links to the old location:

```bash
grep -rn --include='*.md' --include='*.canvas' 'refs/girl.jpg' .
```

If the bare name was used in links and is still unique, nothing needs changing.
This includes the self-embed at the end of the image's own note. When you are
done, run `dimagine check`: it reports any image link left pointing at nothing
(the `dangling_link` item names each one and where it sits).

## Merge a near-duplicate

A channel often posts a compressed copy of a picture and then the original, so
the library holds the same image twice at different sizes. Find them first:

```bash
dimagine check near_duplicate          # read-only; needs `dimagine previews` run first
```

It lists each same-picture pair with its hash distance and pixel difference and
groups the ones safe to merge; it changes nothing. Then merge one pair, keeping
the better copy:

```bash
dimagine merge big.jpg small.jpg        # keep big.jpg, fold small.jpg into it
dimagine merge a.jpg b.jpg --auto       # keep whichever holds the bigger picture
```

`dimagine merge <keep> <drop>` folds the drop into the keep in one undoable
operation: the drop's tags are added to the keep, `added` takes the earlier date,
and the drop's provenance is recorded in the keep's `sources` as a `type: merged`
entry — the keep's own `source` is never overwritten. Every link that pointed at
the drop is repointed to the keep, and the drop's whole group moves to `.trash/`,
so `dimagine undo` can bring it all back. By default the first argument is kept;
`--auto` keeps the file with more pixels (then a lossless format, then a larger
file, then the lexicographically later library path) whichever order you name
them. A note too large to read refuses the merge instead of counting as absent.
`--dry-run` prints the plan without touching a byte.

## Search

Without tools, search the notes:

```bash
grep -rl --include='*.md' -i 'underwater' .          # text and tags
grep -rl --include='*.md' -E '^rating: [45]' .       # rated 4 or 5
```

Images without notes can only be found by name or by looking at them. If a
dimagine tool is installed, prefer it for search by meaning or by similar image.

## Large libraries

Folders with thousands of files work, but file browsers slow down. When a folder
passes a few thousand images, suggest splitting it by topic or by time, and say
what you plan before moving anything.

## Delete

Nothing is deleted in place. Move the whole group into `.trash/` at the library
root, keeping its relative path, and write the record beside it:

```bash
mkdir -p .trash/refs
mv refs/girl.jpg refs/girl.jpg.md .trash/refs/
```

Then write `.trash/refs/girl.jpg.trash.json` naming each file's original path,
its path in `.trash/`, its modification time, and the time you trashed it. A
restore is the move back. A permanent delete happens only past the retention
period, or when the user asks to empty the trash, and only when the user asked
for it. Prefer `dimagine trash <img>` and `dimagine restore <img>`, which do
this and keep the audit log.

An older version of the format put deleted files in `.dimagine/trash/`. Nothing
writes there now. If you find one, leave it alone and tell the user: there is no
record of where those files came from, so recovering them is a manual move.

## With the dimagine CLI

`dimagine tag`, `rate`, `set`, `edit`, `collect`, `mv`, `merge`, `upload`, `trash`,
`restore`, `undo` and `config` make the changes above through one journaled
operation each; a
command that changes a file prints that operation's id, and `dimagine undo <id>`
reverses it. `dimagine upload <file>…` brings files in from outside as image
groups (FORMAT §7.2): a meaningful name is kept, a meaningless camera or
screenshot name lands as `Upload <time>` with the original in the note's title,
bytes the library already holds are not copied twice, and every new image gets
a note carrying its provenance. `--to <folder>` places it, `--to-collection
<note>` lands it in the default folder and joins that collection, and
`--dry-run` prints what would land without changing a byte. A web server
started with `--write` has the same upload behind `POST /api/write/upload`
(multipart, one file per `file` field, optional `folder` and `collection`
fields). `dimagine edit <note> --body-file <path>` (or `--stdin`) replaces a
note's whole body — every byte after its front matter — with the text you give,
leaving the front matter and the body's own line endings, BOM and trailing newline
exactly as they are; `--expect <sha256>` carries the note's hash as you read it so
a note changed since then is refused, not clobbered. Editing the body never drops
an image note's self-embed (it is restored if the new text left it out).
`dimagine collect --reorder <note> <ordinals…>` permutes a
collection's member blocks and `dimagine collect --caption <note> <block> <line>
<text|->` edits, deletes (`-`) or appends one caption line; all three carry the
note's hash as a precondition, so a note changed since you read it is refused
rather than overwritten. `dimagine trash purge` deletes the groups past the
retention period and `dimagine trash empty` deletes every trashed group now;
both need `--yes` and neither can be undone. Every command takes `--library
<dir>` (default: the current folder) and `--json` for one machine-readable
document. Exit codes:
`0` done, `1` a finding or a failed operation, `2` a usage error, `3` the
library could not be read fully, so a "not found" proves nothing. Write through
the CLI when it is installed: it holds the library lock, checks hashes before it
overwrites, and can undo what it did.

## What not to do

- Do not rename images to IDs or hashes.
- Do not create notes for images when there is nothing to record.
- Do not create files in `.dimagine/`.
- Do not delete; move to `.trash/` as above, through a tool, or ask the user.
