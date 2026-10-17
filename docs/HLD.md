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
dimagine mv              # move an image group and repair the links pointing at it (FORMAT §7)
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
  before/after hash of the file it touched, which is what an undo reads. A
  step may name a file its own operation moves in — that is how `mv` renames
  a note and then edits it at its new path inside one operation — and it
  carries the before-state its caller read; the writer re-reads the real file
  before writing a byte, so the journal still records fact.
- `undo <op-id>` reverses one operation from that log, all of it or none: a
  participant whose bytes moved since is a refusal, unless `--force` says to
  undo what still matches. The check is per file in the operation's final
  state: a renamed-and-repaired note sits at its new path with the repair's
  bytes, and counts. A file an operation created comes back as its own
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
| `serve` | axum server: grid, folder and collection views, image detail, previews; a login by owner account, by shared passcode until an account exists, or none at all (`--auth none`). Accounts live outside the library in `--data-dir` and are managed by `dimagine user`. Read-only by default; `--write` opens the write API (front-matter, move, undo, trash view, settings, password change) behind the CSRF, origin and owner-only rules of *Serving with writes (deployment)* below. |

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

## Serving with writes (deployment)

By default `dimagine serve` is read-only: with no `--write`, none of the write
routes exist, so upgrading the binary never quietly opens a door (ADR-018 §1).
Turning it on is a deliberate, reviewable act with preconditions the server
refuses to start without:

- `--write` also requires `--public-origin <scheme://host[:port]>`. Every write
  is checked against this exact origin (its `Origin`, or its `Referer` when
  there is none); a mismatch, a `null`, or a look-alike prefix is refused. There
  is no path or trailing slash in the origin, because a request's `Origin`
  carries neither.
- A write is an **owner** action. A passcode-only server or a first-run server
  with no account yet stays read-only even with `--write`; a session that is not
  an owner account's is refused.
- Each session carries a server-side CSRF token that the owner's own pages echo
  back in the `x-csrf-token` header; it is never in a URL or a client-set
   cookie. Writes are `POST`/`PATCH` only, must be `application/json`, and are
   body-capped at 1 MiB as they stream. Uploads (multipart, the 100 MB cap,
   magic-vs-extension with SVG/HTML/XML refusal) have no endpoint in 3a and are
   deferred to 3b (W66).
- `--allow-anonymous-write` exists only for a no-login server (`--auth none`) on
  a loopback bind; on any other bind the server refuses to start rather than
  accept writes with no session.

The settings page's password change has the same shape: the signed-in account
gives its current password and the new one twice, and on success every *other*
session of that account is signed out — a new password set because the old one
leaked has to end the sessions it leaked to. The account is the session's own,
so a session can never change another account's password, and a passcode
session (which signs in to the server, not to a person) is offered no form at
all.

The write itself is the same journaled operation the CLI runs through
`dimagine-write`, so the two agree about what a change means; the peer address is
recorded in `.dimagine/log/ops.jsonl` (`X-Forwarded-For` is trusted only from a
`--trusted-proxy` peer).

Deployment changes the filesystem the service writes through, and the operator
does these steps themselves:

1. Make the library writable by the service user (the unit that mounts it
   read-only stops mounting it so).
2. Put the library's files under the service group, set the setgid bit on
   directories, and set a **default ACL** (`setfacl -d -m g:<group>:rwX`) on the
   library, rather than relying on `umask` — files written in over `ssh` or by
   root would not honour a umask.
3. Record the one-time ownership change (this step changes the group and mode of
   existing files).
4. `dimagine check` then reports the fourth deployment item: files the service
   user cannot write (alongside an unrecovered journal, a stray temporary file,
   a `.trash` that is a symlink, and trash/record mismatches).

Enabling `--write` on a real server is gated on the Phase 3a review passing;
until then it is exercised against test libraries only.

## `check` findings (0.1)

Unreadable or unsupported images · extension/content mismatch · notes without
an image · invalid YAML · duplicate `id` · ambiguous links · missing links ·
dangling image links — one finding per note, naming every broken link with its
line and body byte span, what a hand move leaves behind — ·
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
