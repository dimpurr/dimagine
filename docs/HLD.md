# High-level design of the tools

- **Status:** Planned (0.1). Everything here is optional tooling on top of
  [FORMAT.md](FORMAT.md); the format wins on any conflict.

## Shape

One Rust binary, `dimagine` (short alias `dimg`), with subcommands. It reads a
library folder, keeps derived data in `.dimagine/`, and never requires that
folder to exist beforehand.

```
dimagine scan            # walk the library, refresh the index, print a summary
dimagine check           # report problems; never changes files
dimagine previews        # build missing previews in .dimagine/cache/
dimagine import eagle    # one-time import of an Eagle library, with provenance
dimagine serve           # read-only web viewer for other devices
```

Every command takes `--library <path>` (default: current directory) and
`--json` for machine-readable output.

## Exit codes

| Code | Meaning |
| --- | --- |
| 0 | Finished; nothing to report. |
| 1 | Finished reading; problems found or an operation failed. |
| 2 | Usage error. |
| 3 | Did not finish reading; any "not found" in the output proves nothing. |

## Modules (0.1)

| Module | Responsibility |
| --- | --- |
| `library` | Walk the folder (FORMAT §2.2 ignore rules), classify images, notes, raw files, canvases. |
| `note` | Parse and minimally edit YAML front matter, preserving unknown keys and formatting. |
| `links` | Extract `![[...]]`, `![](...)` and canvas `file` nodes; resolve per FORMAT §5.1. |
| `index` | `.dimagine/cache/index.sqlite`: paths, sizes, mtimes, SHA-256, notes, links, per-image dimensions, the EXIF "taken" time and — where there is none — which kind of unknown that is (schema v8; a v7 database migrates in place, keeping its rows and re-reading each image once for the new column). Rebuildable. Incremental: compare mtime and size, then hash; header facts and digests are cached by content hash, so duplicates, moves and touches never re-read what the index already knows. |
| `preview` | `thumb` and `view` renditions per FORMAT §8.2, keyed by SHA-256. |
| `import::eagle` | Read an Eagle library read-only and write images, notes and raw files into a target folder. |
| `serve` | axum server: grid, folder and collection views, image detail, previews; a login by owner account, by shared passcode until an account exists, or none at all (`--auth none`). Accounts live outside the library in `--data-dir` and are managed by `dimagine user`. |

## Serving media (0.1)

`dimagine serve` answers a rendition request from the library, behind the same
gate as every page:

| Route | Answers |
| --- | --- |
| `/p/<sha256>/<variant>` | The rendition of one *content*. `<variant>` is a size class and the rendition generation (`thumb.g2`, `view.g2`). The URL is the content, so the answer is `private, max-age=31536000, immutable` and its ETag is the digest, the size class and the generation. A URL naming another generation redirects to the current one; a digest the index does not know, or one whose rows are no image, is a `404` and never renders anything. |
| `/thumb/<path>`, `/media/<path>` | `302 Found` to the content URL when the index holds a digest for the bytes, with `private, no-cache` on the hop. Otherwise the rendition itself, revalidated, because the content URL would be a `404` for a digest the index does not hold. |
| `/raw/<path>` | The file as written: an image's original, or a note's source view as `text/plain`. Never a redirect, never immutable. |

Renditions are keyed by content (FORMAT §8.2), which is what lets the first
route promise a year: a rename or a move keeps the address, and changed bytes
are a new one. Pages and the JSON they read state the digest, so a picture is
asked for by its content — and the script spells the generation it reads off
the page rather than keeping a copy of the policy.

## `check` findings (0.1)

Unreadable or unsupported images · extension/content mismatch · notes without
an image · invalid YAML · duplicate `id` · ambiguous links · missing links ·
images that moved (same `id`, new path) with links still at the old path.

`check` also reports how many renditions the preview cache is missing, when
there is an index to count from. It is a count rather than a finding: a preview
is derived data the viewer rebuilds, so it never makes `check` exit non-zero.

## Not in 0.1

Search by meaning or by similar image, MCP, tagging models, colour management
beyond EXIF orientation, watching the folder for changes.
