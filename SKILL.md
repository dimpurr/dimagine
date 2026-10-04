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
   `IMG_1234.jpg`, a pasted image), build one from its source URL when it has
   one: `twitter-<user>-<status id>-<photo n>`, `pixiv-<artwork id>` (or
   `pixiv-<id>-p<n>` for a page), `bilibili-<id>`, otherwise
   `<site>-<last path segment>`. Without a usable URL, name it after the
   import time plus four characters: `20261004-143012-q3f7.jpg`.
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
imported: 2026-10-04T14:30:12+01:00
---

Why this image is here, what to notice, related ideas.

![[girl-underwater.jpg]]
```

- End the note with an embed of its own image, so the picture shows when the
  note is opened. Use the bare file name if it is unique in the library,
  otherwise the path. This preview does not make the note a collection.
- Tags are a YAML list without `#`.
- Datetimes are ISO 8601 with a UTC offset.
- Quote strings YAML could misread: `"no"`, `"on"`, `"Note: draft"`.
- Preserve properties you do not recognise.
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
- The line right after an embed is that member's note.
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
mv refs/girl.jpg refs/deep-sea/girl.jpg
mv refs/girl.jpg.md refs/deep-sea/girl.jpg.md 2>/dev/null
mv refs/girl.jpg.*.json refs/deep-sea/ 2>/dev/null
```

Then find and fix links to the old location:

```bash
grep -rn --include='*.md' --include='*.canvas' 'refs/girl.jpg' .
```

If the bare name was used in links and is still unique, nothing needs changing.
This includes the self-embed at the end of the image's own note.

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

## What not to do

- Do not rename images to IDs or hashes.
- Do not create notes for images when there is nothing to record.
- Do not create files in `.dimagine/`.
- Do not delete; move to `.dimagine/trash/` only through a tool, or ask the user.
