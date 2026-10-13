# High-level design of the tools

- **Status:** Planned (0.1). Everything here is optional tooling on top of
  [FORMAT.md](FORMAT.md); the format wins on any conflict.

## Shape

One Rust binary, `dimagine` (short alias `dimg`), with subcommands. It reads a
library folder, keeps derived data in `.dimagine/`, and never requires that
folder to exist beforehand.

```
dimagine scan            # walk the library, refresh the index, print a summary
dimagine check           # report problems, including the state of past writes
dimagine tag             # add or remove tags in an image's note
dimagine rate            # set an image's rating, or clear it
dimagine set             # set scalar front-matter properties (KEY= clears one)
dimagine collect         # create a collection note, or add and remove members
dimagine trash           # move an image group into .trash/ (FORMAT §7.1)
dimagine restore         # move a trashed group back
dimagine undo            # reverse one recorded operation
dimagine config          # show or change .dimagine/settings.json
dimagine trash purge     # delete trashed groups past their retention period
dimagine trash empty     # delete every trashed group now (needs --yes)
dimagine previews        # build missing previews in .dimagine/cache/
dimagine import eagle    # one-time import of an Eagle library, with provenance
dimagine serve           # read-only web viewer for other devices
```

Every command takes `--library <path>` (default: current directory) and
`--json` for machine-readable output.

## Writing the library

`scan`, `check`, `previews` and `serve` only read. The commands above change
files, and every one of them goes through the same path: plan the steps, then
run them as one **operation** (FORMAT §8.3).

- One writer at a time: `.dimagine/write.lock` is held for the whole operation,
  so a second writer waits for the first to finish rather than interleaving with
  it.
- A file is read, hashed, changed in a `.dimagine.tmp.<nonce>` file beside it,
  fsynced and renamed into place. A modify or move whose recorded hash no longer
  matches refuses, so a note edited by hand in the meantime is never overwritten.
- Each step is journaled under `.dimagine/journal/<op-id>/` before it happens;
  a crash is recovered by the next writer. Each step that lands is appended to
  `.dimagine/log/ops.jsonl` with its operation's id and intent and the
  before/after hash of the file it touched, which is what an undo reads.
- `undo <op-id>` reverses one operation from that log, all of it or none: a
  participant whose bytes moved since is a refusal, unless `--force` says to
  undo what still matches. A file an operation created comes back as its own
  trash move, never a delete. `trash purge` and `trash empty` have no inverse
  and say so: they are the two irreversible operations.
- `--json` prints one document per command with the `op` id, so a caller can
  name the operation it just made.

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
| `write` | The one path that changes the library: the lock, the read-hash-temp-rename write, the per-operation journal and crash recovery, and the `.dimagine/log/ops.jsonl` audit log with its inverse (FORMAT §8.3). Every write command is a planner that builds steps for this module; none of them writes a file itself. Also `.dimagine/settings.json`: a schema of typed settings with a default and a validation rule each, applied to whatever file is there — unknown keys kept, invalid values read as unset plus a report line. |
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

Plus the state of past writes (FORMAT §8.3, §7.1): an operation journal left
unrecovered, a `.dimagine.tmp.` temporary file that outlived its rename, a
`.trash/` record that does not parse or names a file that is not there, two
records claiming the same trash file, a file under `.trash/` that no record
claims, and a `.trash` that is a symbolic link. Severities follow what each one
costs: an unrecovered journal, a record that does not parse or names a missing
file, two records claiming one trash file, and a `.trash` that is a symlink are
errors; a stray temporary file is a warning — it costs space, but nothing is
lost. An unclaimed `.trash/` file is info when it may be another tool's
trash, and a warning when it is an image, an image note or a raw sidecar, the
shape dimagine's own group move leaves behind: with no record, neither restore
nor purge can reach it. A library that still holds files in the old
`.dimagine/trash/` gets one more info finding: the location stays readable
(FORMAT §7.1) but nothing writes to it or restores from it any more.

`check` also reports how many renditions the preview cache is missing, when
there is an index to count from. It is a count rather than a finding: a preview
is derived data the viewer rebuilds, so it never makes `check` exit non-zero.

## Not in 0.1

Search by meaning or by similar image, MCP, tagging models, colour management
beyond EXIF orientation, watching the folder for changes.
