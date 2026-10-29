# High-level design of the tools

- **Status:** Planned (0.1). Everything here is optional tooling on top of
  [FORMAT.md](FORMAT.md); the format wins on any conflict.

In write mode the inspector suggests full tag paths and aliases when adding a tag; the server resolves aliases on submission. The front-matter API accepts `fields.tags` only as a list of strings, resolving and validating its entries as tag inputs. The tags page reports unreadable inputs that block a batch and links to the affected notes. Web tag applies run in the background; the tags page reports completed write steps and the result, and can be refreshed without JavaScript. The writer lock serializes writers, while reads remain available during an apply.

The sidebar presents tag paths hierarchically, with links to manage individual tags. On `/tags` it shows only the tag count.

The viewer provides a read-only `/tags` hierarchy with indexed subtree counts,
server-side name filtering, and a review filter for invalid names, rare tags,
and matching leaf names. Write mode exposes CSRF-protected batch plan and apply
endpoints; apply rebuilds the preview and the Writer revalidates under its lock.
Write-enabled owner views also offer an initial JavaScript batch management interface with selection, preview, apply and undo. The preview reports changed notes and paired images, with up to twelve image thumbnails; browser-based management without JavaScript remains planned.

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
dimagine edit            # replace a note's whole body (everything after its front matter), with an open-hash precondition (FORMAT §5.3)
dimagine collect         # create a collection note, add and remove members, reorder blocks, edit a caption
dimagine mv              # move an image group and repair the links pointing at it (FORMAT §7)
dimagine upload          # land files from outside the library as image groups with notes (FORMAT §7.2)
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
- All step states are journaled together under `.dimagine/journal/<op-id>/` before any step happens; a later correction is recorded separately. Recovery also reads older individual step files.
  A crash is recovered by the next writer. Each step that lands is appended to
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
- A whole-replace operation — a body edit, a collection reorder, a caption
  change, a collection join, the `id` a join assigns to an image's own note —
  carries the note's hash as the caller read it, and does not recompute
  its intent on newer bytes: if the note has changed since, the operation
  refuses with a conflict and writes nothing. It is checked twice, because
  there are two moments the note can move: under the lock before the journal
  is written, where a mismatch refuses the whole operation, and again at the
  step that renames, which compares against the same read — so an edit made
  after the first check is a conflict on that step too, never a revert. There
  is no precondition-less overwrite; a conflicting request re-reads and tries
  again. Only the steps that will actually write carry one, so an operation
  that turns out to change nothing about a note is not refused for it.

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
| `serve` | axum server: grid, folder and collection views, image detail, previews; a login by owner account, by shared passcode until an account exists, or none at all (`--auth none`). Accounts live outside the library in `--data-dir` and are managed by `dimagine user`. Read-only by default; `--write` opens the write API (front-matter, note body edit, move, upload, collection reorder and caption, undo, trash view, settings, password change) behind the CSRF, origin and owner-only rules of *Serving with writes (deployment)* below. The image/note page carries a plain textarea that edits the body as source and posts it to `/api/write/note/body`, with the note's open-time hash as a precondition and a reload-or-overwrite prompt on a conflict. The browser normalizes a textarea's value to LF, so a note with CRLF line endings edited through that page comes back with LF in the body (FORMAT §5.3). |

### Tag batch planning (Current)

`dimagine-core::tag_batch::plan_tag_batch` is a read-only library planner. It
walks all non-ignored Markdown notes, retains their original and proposed bytes,
and snapshots the optional alias table. It validates a requested complete alias
replacement, reports unreadable or uneditable paths and rename collisions, and
produces a versioned deterministic SHA-256 digest of the plan. A missing alias
table is distinct from an unreadable one. Plans with issues are not ready.

`dimagine-write::Writer::apply_tag_batch` acquires the Writer lock, rejects an
existing `apply.pid` at the library root or under `.dimagine/`, rebuilds and
compares the complete plan, and refuses conflicts and unreadable files. It
uses per-file preconditions and journals all changes as one operation, with a
single undo and the standard partial and crash recovery behavior. Undo and
recovery move a newly created alias table to recorded trash and remove its parent `_dimagine/` if empty. The digest is not
an authorization or a cross-file atomicity guarantee. The lock cooperates with
other Writers; external editors remain subject to the existing per-file CAS
semantics. `dimagine tag rename SOURCE TARGET`, `merge SOURCE... TARGET`,
`move SOURCE TARGET`, and `rm SOURCE` plan by default; `--apply` journals
the change. Rename, merge, and move remember changed source spellings as aliases by default; `--no-aliases` opts out. Moves remember each changed descendant, following subsequent operations to the final name. The proposed table is validated and included in the same journal and undo as the notes. `--json` reports the proposed alias table, operations, counts, conflicts, the plan digest,
and the operation ID. `tag aliases` displays the validated alias table. Sources
can include legacy spaces; targets cannot. Single-image tag additions, web tag
additions, and Eagle-imported tags resolve through the same alias table. Tag
removals select the stored spelling. Upload intake has no tag input.

The viewer's grid also takes a phone drag-select: a long press on a
tile opens selection mode, a drag across the grid adds or removes
every thumbnail the finger passes — the pressed tile's own state is
the decision the whole drag applies — and release keeps the selection
open under a bottom bar with the count, Open (the first selected
tile's page) and Clear, with Esc, Clear or an emptied selection
standing down. It is an enhancement of the viewer script alone: no
new route, no new state on the server, and without the script the
grid is exactly what it was. The gestures keep the ones already
there — a press that drifts before the long press fires is a scroll
or a tap and cancels itself, so tap-to-open and scrolling are
unchanged.

### Hierarchical tag views (Current)

Tag filters match the case-folded tag and its slash-delimited descendants;
`a` includes `a/b` but not `ab`. Tag counts include descendants and count
an image once per tag even when it carries several tags in that subtree.
Facet counts retain the current view's filters and the Recent lens cap; the
Views rows retain their existing uncapped semantics.

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
   body-capped at 1 MiB as they stream. The one exception is the ADR's own
   (ADR-018 §3): `POST /api/write/upload` is `multipart/form-data`, and
   multipart is accepted on no other route (JSON at the upload path and
   multipart at a JSON path both answer 415). Each file field is capped per
   file — `--max-upload-bytes`, default 100 MiB — while it streams in: a field
   that would cross the cap is answered 413 and the connection is dropped with
   the rest of the body unread, nothing is buffered first. The whole request's
   file bytes are capped in total at four times the per-file cap, and each
   `folder`/`collection` field at 4 KiB, so a batch of many large files — or an
   oversized placement field — cannot buffer without bound. A file whose bytes
   are not the format its name claims is refused, SVG, HTML and XML on both
   sides (ADR-018 §5), and a batch over 1000 files before anything is opened.
   The optional `folder` and `collection` fields are the endpoint's `--to` and
   `--to-collection` (ADR-017 §11.1's folder-view and collection-page context
   is the page's to send) and are exclusive, as the CLI's two flags are: a
   request naming both is a typed refusal, neither silently winning; the upload
   is the same journaled operation the CLI
   runs, and the response carries the per-file outcomes and the `op` id that
   `POST /api/write/undo` takes.
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

The state directory (`--data-dir`) holds `accounts.json` and — since sessions
survive a restart — `sessions.jsonl`, the live sessions named by a salted hash
of each cookie token (every record carries its own random salt beside its
hash, the shape a password record keeps) and written 0600 like the accounts
file. Nothing here adds it to
a backup: it is convenience state, not library data, and losing it only signs
everyone out. An operator who does back it up should know what a **rollback**
does: restoring an older copy brings back the sessions that were signed out
after it was taken — including those a password change signed out — and they
stay back until their original expiry, no longer.

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

`check` also reports an image sitting in an upload landing zone — the resolved
default upload folder, or `inbox/` — with no note (FORMAT §7.2): nothing is
lost, but the provenance an upload would have recorded (`added`, `imported`,
`sources`) is missing, which is what an interrupted upload whose journal was
lost leaves behind, or a file copied in by hand. It is an info finding, and the
scan is read-only.

`check` also reports how many renditions the preview cache is missing, when
there is an index to count from. It is a count rather than a finding: a preview
is derived data the viewer rebuilds, so it never makes `check` exit non-zero.

## Not in 0.1

Search by meaning or by similar image, MCP, tagging models, colour management
beyond EXIF orientation, watching the folder for changes.


## Tag operations lists

Status: Current. `dimagine tag list FILE` previews a UTF-8 JSON document;
`--apply --expect-digest DIGEST` requires the digest from that preview and
applies through the existing tag batch journal as one undoable operation.
The writer revalidates the plan under its lock. Preview again after any change.
`--no-aliases` suppresses remembering changed names as input aliases; use the
same option for preview and apply. Conflicts and unreadable files block apply.

Version 1 requires `version`, `entries`, `notes`, and `optional_operations`.
Each entry requires `source`, `target`, `action` (`rename`, `merge`, `move`,
`remove`, or `keep`), `selected`, `image_count`, `reason`, and
`alternative_action` (an action or null). Counts and reasons are review context,
not current measurements or authorization. Select only reviewed actions; change
`action` to an alternative explicitly. `keep` requires identical paths and
does nothing. Duplicate sources and exact target-to-source operation chains are rejected. A rename
whose target exists is a conflict; choose merge after review. Optional entries
require `operation`, `selected`, and `description`; the only recognized option,
`remove-redundant-parent`, defaults to unselected in proposals and requires explicit
selection. It runs after selected entry actions, removing a parent only when a
case-sensitive, slash-delimited descendant remains on the same note. It does not
create aliases. Duplicate optional operations are rejected.
Unknown fields, action names, and versions are rejected.

A preview's digest covers the planned library snapshot and alias change, not
an authorization to write. External non-cooperating writers can still race;
review failures and the journal outcome rather than assuming atomicity.

The write-enabled Tags page also accepts version 1 proposal JSON for review,
including alternate actions and an explicitly selected redundant-parent cleanup.
The browser previews selected actions through the batch planner before applying
and offers undo. The Tags page also provides a server-rendered single-action
preview, apply and undo flow when JavaScript is disabled. Form submissions use
the same origin and session-token checks as the JSON write API.
