# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added
- Near-duplicate merge (W77): `dimagine check near_duplicate` reports images that
  are the same picture at different sizes. It reads a 64-bit perceptual hash and a
  small grayscale signature that `dimagine previews` now stores alongside each
  rendition — two images whose hashes are close and whose aspect ratios match are a
  candidate pair; a small measured pixel difference makes it a `same` pair (one
  picture, re-encoded) and joins it into a mergeable group, a larger one a `variant`
  (similar but different — reported, never grouped for merge). It reports only and
  changes nothing. `dimagine merge <keep> <drop>` folds one image into the other as
  a single undoable operation: the drop's tags merge into the keep, `added` takes
  the earlier date, and the drop's provenance is appended to the keep's `sources` as
  a `type: merged` entry (its path and SHA-256, plus its own source when it had one)
  without touching the keep's own `source`; every link that named the drop is
  repointed to the keep, and the drop's whole group (image, note, raw sidecars)
  moves to `.trash/`. By default the first argument names the keeper; `--auto` keeps
  the file with more pixels, then a lossless format at equal pixels, then a larger
  file, then the lexicographically later library path. A note too large to read
  refuses the merge rather than being treated as no note. `--dry-run` prints the
  plan and writes no library byte.
- Viewer desktop layout (W76): the sidebar is now the single filter —
  one set of Views / Folders / Collections / Tags on screen, with counts
  scoped to the current selection and zero-count rows dimmed; row-end `+`
  (Alt-click does the same) stacks a row onto the current filters, all in
  the URL. The desktop filter column is gone, so the grid runs full width
  at 1440 px. The info bar only renders when used: no empty "select an
  image" column — selecting a photo (`?sel=<path>`, in the URL, Esc or
  clear to dismiss) opens a top-aligned bar with preview, title, path,
  source, tags and note. No-script, keyboard and phone (drawer, ≤768 px)
  behaviour unchanged. Rating / file-type chips need index support first
  and are not faked in.
- Viewer: phone drag-select in the grid (Phase 3b). A long press
  (500 ms) on a tile opens selection mode, a drag across the grid
  adds or removes every thumbnail the finger passes — the pressed
  tile's own state decides which, so a drag never flickers a tile
  it crosses twice — and release keeps the selection open under a
  bottom bar with the count, Open (the first selected tile's page)
  and Clear. Esc, Clear or an emptied selection stands down. The
  selected tiles wear the single selection's own accent border; a
  tap while the mode is open toggles that tile on every width.
  Tap-to-open, tap-to-select on a wide screen and scrolling are
  unchanged: a press that drifts 10 px before the long press fires
  is a scroll (or a tap) and cancels itself, and nothing is
  prevented until the press says otherwise. Enhancement only — no
  new route, no new state, and without the script the grid is
  exactly what it was.
- CLI write commands (Phase 3a): `dimagine tag` (add and remove tags), `rate`
  (0–5, or `none` to clear) and `set` (scalar properties — and `KEY=` with no
  value clears a property by deleting its key, never by writing `key: ""`)
  edit one image's note
  through the front-matter editor, print the resulting front matter, and write
  nothing at all when the change would not change the file. `collect --new
  <title>` writes a collection note (in the folder `.obsidian/app.json` asks
  for, at the next free name) and gives every image it lists an `id`; `collect
  --add` and `--remove` edit an existing collection's embed list. `trash` moves
  an image group — image, note, raw sidecars — into `.trash/` keeping its
  relative path and writes the record beside it; `restore` moves it back to the
  recorded original paths and their recorded times, refusing when a path is
  taken. `undo [op-id]` reverses the last operation, or the one named (a command
  that changes a file prints its `op` id, in `--json` too): all of it or none, unless
  `--force` takes what still matches its recorded hash.   `config list|get|set|reset`
  reads and changes `.dimagine/settings.json` against the settings schema.
  `trash` is the group that manages the trash: `trash <img>…` moves a group in,
  `trash purge --dry-run|--yes` deletes the groups past `trash.retentionDays`
  (default 180 days; `0` never purges), and `trash empty --yes` deletes every
  trashed group now — both permanently, so their undo refuses, and both leaving
  any file without a record alone.
  Each command is one journaled operation through the write core: the library
  lock held for the whole operation, every file hashed before it is replaced and
  a mismatch refused, a `.dimagine.tmp.<nonce>` write fsynced and renamed into
  place, each step journaled so a crash recovers, and each step audited in
  `.dimagine/log/ops.jsonl` with its operation's id, intent and the hash of the
  file before and after. A created file undone comes back as a
  trash move, never a delete. `dimagine check` now also reports the state of
  past writes: an unrecovered journal, a stray temporary file, a `.trash` that
  is a symbolic link, a trash record that does not parse or names a file
   that is not there, and any files left in the old `.dimagine/trash/` — a
   location nothing writes to any more, but which stays readable so a library
   that has one is reported rather than stranded.
- `dimagine mv <img> <new-path>` (Phase 3b): the move that FORMAT §7 promises.
  The plan fixes the `id` before anything moves — creating the group's
  note if it has none, the `id` landing with the note's edit — renames
  the whole group, refuses when any destination is taken by a different
  file (a file, a directory, or a name that differs only by case or
  Unicode form — a rename onto the file's own case-shifted spelling
  remains the routine way to fix display case), and
  then rewrites in
  place every link that resolved to the group: wikilinks, Markdown images and
  canvas `nodes[].file` values, each keeping its syntax and display parts
  (alias, heading, block reference, alt, title), code blocks and inline code
  left alone. A link that was already ambiguous is reported and untouched; a
  destination whose bare name would come out ambiguous is refused. `--dry-run`
  prints the renames and every link edit without changing a byte. The whole
  move is one journaled operation, so one `dimagine undo <op>` puts every byte
  back; the write core carries a modify step's before-state when the operation
  itself is what moves the file into place, and undo's all-or-none check reads
  each file's final state, not one journal line at a time.
- `dimagine collect --reorder <note> <ordinals…>` and `collect --caption <note>
  <block> <line> <text|->` (Phase 3b): the collection writes FORMAT §5.2
  promises. A member block is a member line plus the plain-text caption lines
  that directly follow it; reordering permutes only the blocks among the byte
  ranges they already occupy and leaves every other byte — blank lines,
  headings, prose, front matter — exactly where it was, refusing anything that
  is not a permutation of the blocks whole. A caption edit replaces, deletes
  (`-`) or appends one plain-text line and never touches the member line
  (removing a member is `--remove`). Both are whole-replace operations that
  carry the note's hash as the caller read it: a note changed since is a
  conflict, never an overwrite, and both are reversible with `undo`. `POST
  /api/write/collections/reorder` and `/caption` expose the same two behind the
  write API's gates, answering a stale precondition with a `409` and the note's
  current ETag.
- `dimagine edit <note> --body-file <path>|--stdin [--expect <sha256>]` (Phase
  3b): the whole-body edit FORMAT §5.3 promises. It replaces every byte after
  the note's front matter with the text given and touches nothing before it —
  the front matter is carried over untouched, never re-serialized, and the body's
  own line endings and trailing newline (or absence), and the note's leading
  byte-order mark, survive byte for byte. A note whose opening `---` fence is never closed is refused,
  because nothing says where its body starts, and a note that ends right at its
  closing fence is given the line break its first body line needs. Collection and
  other plain notes are edited as text the same way. The result keeps the
  self-embed rule: an image note whose new body no longer embeds its own picture
  gets the embed restored at the end. It is a whole-replace operation, so it
  carries the note's hash as the caller read it (`--expect`, or the command's own
  read) and a mismatch is a conflict that writes nothing; the new hash is printed
  for the caller to carry onward, and `undo` puts the prior bytes back. `POST
  /api/write/note/body` exposes the same behind the write API's gates. The
  image/note page now carries a plain textarea over the body with an explicit
  Save (source mode, no autosave, no WYSIWYG, no new dependency): it posts the
  body with the open-time hash the page read, and on a `409` offers to load the
  latest or use your version anyway (a retry carrying the hash the conflict
  returned). The `409` a whole-replace edit answers now names the file's current
  hash next to its ETag, so an overwrite-anyway has that hash to send back.
- `dimagine check` gained `dangling_link`: image links that resolve to no file,
  reported once per note with each link's line and body byte span — the exact
  damage a hand move in another tool leaves behind. The per-link `missing_link`
  and `ambiguous_link` warnings are unchanged.
- Uploads land as image groups (Phase 3b, FORMAT §7.2): `dimagine upload
  <file>…` refuses a file whose bytes are not the format its name claims and
  SVG/HTML/XML on both sides, keeps a meaningful name (sanitized as ever) and
  renames a meaningless camera, screenshot or share name to `Upload
  <YYYYMMDDHHMMSS>.<ext>` with the original kept in the note's `title`, takes
  ` 2` on a library-wide NFC, case-insensitive collision, and does not copy
  twice what the library already holds: the bytes' SHA-256 is looked up among
  the live images in the index, and a hit names the existing image instead of
  writing a second copy. Each new image lands with a note carrying a fresh
  `id`, `added` and `imported` at the upload time, and a `sources` entry
  (`type: upload`, `imported`, `importer`, `original_name`). Placement is
  ADR-017 §11.1: `--to <folder>`, or `--to-collection <note>` for the default
  upload folder plus the join, or the default folder itself —
  `.obsidian/app.json`'s `attachmentFolderPath`, else `inbox/`. `--dry-run`
  reports the placement, names and dedupe hits and changes nothing; a batch
  over 1000 files is refused before anything is opened; the whole request is
  one journaled operation, so one `dimagine undo <op>` removes the whole
  uploaded group.
- The write API uploads too (Phase 3b): `POST /api/write/upload` takes
  `multipart/form-data` — the one route ADR-018 §3 excepts from the JSON-body
  rule, and multipart is accepted nowhere else — with one `file` field per
  file and optional `folder` and `collection` fields mirroring the CLI. A file
  over the per-file cap (`--max-upload-bytes`, default 100 MiB) is refused
  while it streams: 413 the moment the field crosses the cap, the connection
  dropped with the rest of the body unread and nothing buffered first. Owner,
  origin, CSRF, audit and `--write` gating are every other write's; the
  response carries the per-file outcomes and the `op` id.
- `dimagine check` reports an image left in an upload landing zone with no
  note (`orphaned_upload`, info): the state an interrupted upload whose
  journal was lost leaves behind. The scan is read-only.
- The write API can move too (Phase 3b): `POST /api/write/move`
  (`{"from", "to", "dry_run"?}`) runs the same plan as the CLI and answers with
  it, `dimagine.move/0.1`, the `op` id null on a dry run — behind the same
  auth, CSRF, origin and `--write` gates as every other write.
- `dimagine serve` can write, off by default (Phase 3a). `--write` opens the
  write API — front-matter edit, undo, the trash view, `POST
  /api/write/collections` (new/add/remove), and settings save — each
  a journaled operation through the write core, the same the CLI runs. Without
  `--write` none of those routes exist, so upgrading never quietly opens a write
  door. `--write` requires `--public-origin`, against which every write checks
  its `Origin` (or `Referer`); a write must be `POST`/`PATCH`,
  `application/json`, capped at 1 MiB as it streams, and carry the session's
  server-side CSRF token in `x-csrf-token`. Writing is an owner action: a
  passcode-only or account-less server stays read-only even with `--write`, and
  `--allow-anonymous-write` — no-login writes with no session — is confined to a
  loopback bind and refuses to start on any other. Each operation records the
  peer address in the audit log, trusting `X-Forwarded-For` only from a
  `--trusted-proxy` peer. Deployment steps (make the library group-writable with
  a setgid + default ACL, the one-time ownership change, the `check` item for
  files the service cannot write) are in `docs/HLD.md`.
- Viewer settings page at `/settings`, from the same schema the CLI's `config`
  reads: owner-only, a read-only display without `--write` and an editable,
  searchable form with it, saving through the write core under the CSRF rules
  above. `X-Content-Type-Options: nosniff` is now on every image and rendition
  response.
- Viewer password change: `/settings` carries the signed-in account's own
  password change — the current password, the new one twice, and the
  weak-password confirmation the setup form asks for. It follows the write
  rules above (present only with `--write`, JSON body, exact origin, the
  session's CSRF token in `x-csrf-token`), it edits only the account the
  session belongs to, and on success every other session of that account is
  signed out, so setting a new password because the old one leaked ends the
  sessions it leaked to.
- The front-matter editor (Phase 3a): `dimagine-core::fm_edit` edits a note's
  YAML properties as raw bytes, splicing exactly the range an edit intends
  and leaving every other byte identical. `set_scalar`, `delete_key`,
  `set_rating`, `add_tags`, `remove_tags`, `ensure_id` and
  `ensure_self_embed` work on a closed subset of YAML — a plain or
  double-quoted top-level key that occurs once, whose value is a single-line
  scalar, a single-line flow list, or a block list of single-line items —
  and refuse everything else with typed errors that never quote note
  contents: duplicate keys, merge keys, anchors and aliases, multiline
  values, nested structures, single-quoted keys, unterminated front matter
  and anything the reader already reports broken. Every splice is re-parsed
  with saphyr before it is returned and must read back as exactly the
  intended value, with all other properties unchanged. Tags dedupe and match
  case-insensitively keeping the first spelling, allow hierarchies, keep the
  list's existing style (flow or block), and removing the last tag deletes
  the key; ratings are always written unquoted so `rating: "4"` becomes
  `rating: 5`; a `tags` key created from nothing is flow-style
  (`tags: [a, b]`); new keys go as the last line inside the fences, and a
  note without front matter grows a block in its own line-ending style with
  its BOM kept. A checked-in fixture corpus asserts each edit's byte diff
  equals exactly its intended range (quoted ratings, flow lists with quoted
  commas, block lists, comments beside and on the edited line, CRLF + BOM,
  bodies that begin with `---`, add-and-remove round trips, hierarchical and
  case-variant tags), and an opt-in harness
  (`DIMAGINE_ROUNDTRIP_LIBRARY`, run only against a scratch copy) walks every
  note of a library copy, sets and restores one scalar key and reports byte
  identity with refusal counts by reason. The `---` delimiter rule is now one shared rule for reading
  and editing (`fm_edit::front_matter_range`): both fences accept
  `---` followed by whitespace, where previously only the closing fence
  did.
- Viewer: previews are addressed by their content. `/p/<sha256>/thumb.g2` and
  `/p/<sha256>/view.g2` serve the rendition of one content, answered from the
  URL and cacheable for a year (`private, max-age=31536000, immutable`, an
  ETag of the digest, the size class and the generation) — because a different
  image is a different digest and a different URL, so a rename or a move keeps
  the address and its cached bytes, and changed bytes are a new address. The
  generation rides in the variant, so a URL from an older rendition policy is
  a `302` to the current one rather than a year of stale pixels. Every page and
  every JSON body emits these URLs: tiles, collection covers and members, the
  image page's stage, `/api/view` and `/api/image` (each row carrying the
  `sha256` its URL is built from). An unknown digest, a digest that names no
  image, an unknown size class, an unknown variant spelling and a digest that is
  not 64 hexadecimal characters are all `404`, and an unknown digest never
  triggers a render. `dimagine
  check` reports the renditions the cache is missing ("missing previews: N
  thumb, M view of K indexed images") — counted from the index, not a finding,
  and absent when there is no index to count from — and the viewer works the
  same gap in the background, without waiting for a request.
- Viewer Phase 2a, the filter panel: the facet list beside the grid (K27 motion
  3), where every tag, folder, collection and lens carries the number of images
  it would show *beside the filters already on* — a folder row counts what the
  tags leave, and a tag row counts what the folders and the tags already on
  leave, so a reader who has narrowed three thousand pictures to fourteen can
  still see where the rest went. Every row is a link to the same view with that
  one filter toggled, so the panel works with scripting off and every row is a
  real tab stop. A value the filters leave nothing for keeps its place, dimmed
  and unlinked, because a value missing from the list would read as the library
  having changed; the value that is on is never dimmed, since its row is the
  way to turn it off. A list longer than eight rows opens through `more=<facet>`
  rather than a click handler, and names `/search`, `/folders` or `/collections`
  when even the opened list has to stop. On a phone and a tablet the panel is
  one 44 px row above the grid, folded until it is tapped, with the number of
  filters on riding on the button; at ≥ 1200 px the same lists stand open in a
  sticky 208 px column and the grid keeps the tile sizes its three settings were
  chosen for. `Index::view_facet_counts` answers the five groups in five grouped
  queries — the Recent lens capping every count at its own window, and a
  `sub=0` view counting a folder's direct members — read once per HTML page and
  never for `/api/view` or `/api/sidebar`, whose shapes are untouched.
- Viewer Phase 2b: justified rows and the "Taken" sort. The grid lays
  images out in rows of equal height that fill the column, by the
  width and height the index read from each picture's header — the
  layout no longer jumps while thumbnails load, and a header nobody
  could read keeps the square cell the grid always had. The justified
  layout is progressive enhancement: without a script the grid is the
  plain grid of links, and the keyboard's focus order follows the rows
  the eye sees. The sort menu gains "Taken" (newest first and oldest
  first): the order is the EXIF time the index holds, and an image
  with no taken time goes last in both directions, marked "no taken
  time" — an unknown is never a date nobody recorded. The sort survives
  in the URL and combines with the folder, tag, text and size filters.
  The image page shows the picture's dimensions and its taken time,
  and "Unknown" for what the header did not say.
- Viewer Phase 2a, the image page: the picture and its facts side by side.
  Prev/next walk the view the picture was reached from — the sort, folder,
  tag, collection, search or Recent-lens `v=` a tile carries; the neighbours
  and the position ("3 / 124") are computed in SQL against the index, the
  arrows are plain links, and the step keys are ←/→ with Esc back to the
  tile the page left. On a phone the picture is full width and the info
  panel pulls up as a sheet (a plain section below the picture without the
  script) that a sideways stroke on the picture also walks. "Appears in"
  names every collection that embeds the picture by its own title and links
  to it, and says so in one line when there are none. At ≥ 1200 px the info
  panel is a pinned inspector beside the stage.
- Viewer (Phase 2a): `/?c=<note>` is the collection page. A collection is a
  note that embeds images (FORMAT §5), so the view renders as the note's own
  page: a header naming it, the note's own text (its body without the member
  rows the grid lists — the embeds and the caption lines directly after them
  — rendered by the Markdown sanitiser every rendered note goes through), the
  number of items the page lists, and an "Open note" link to the note's
  source. The members follow in the order the note embeds them, each with the
  caption line its embed gave it, and a duplicate embed shows once, at its
  first position. Narrowings beside the collection (`tag`, `q`, `in`,
  `untagged`, `recent`) pick which members show, in the note's order.
- Viewer: `/raw/<path>` serves a note as its source view — the bytes as
  written, `text/plain`, never a content type a browser could interpret as a
  page of the viewer's origin. Images on `/raw` are unchanged.
- Index: per-image width, height and the EXIF "taken" time
  (`DateTimeOriginal` with `OffsetTimeOriginal`, `SubSecTimeOriginal`
  refining below the second) in every image row, read from the image
  header and EXIF block during the refresh — no pixel decode. Schema v7
  migrates v6 databases in place, keeping every row; the first refresh
  backfills the new columns because it hashes each unread file once and
  after that reuses everything (dimensions, taken time and digest) while
  size and nanosecond mtime stand. Identical content is cached by its
  SHA-256 digest the way previews are, so a duplicate, a moved file or a
  touched file never parses twice. A header that cannot be read records
  `NULL` — an unknown, never a zero — and the dimensions that stay
  unknown are counted on every scan in `dimagine scan`'s human and JSON
  output (`index.images`, `index.with_dimensions`, `index.with_taken`,
  `index.unknown_dimensions`; the JSON key is absent when the refresh
  did not happen). The read API: `Index::image_meta` for one image's
  metadata, `Index::view_by_taken` for a page ordered by taken time
  (NULLs last in both directions, ties broken by path).
- Index: the reason an image has no taken time, recorded beside the missing
  value in every image row and in the digest cache (`TakenReason`: `no-exif`,
  `exif-without-date`, `exif-unreadable`, `date-unreadable`,
  `file-unreadable`). Schema v8 migrates a v7 database in place and keeps its
  rows, dropping their image digests and the header cache with them: a digest
  earned before the column existed would vouch for a row no refresh has any
  reason to revisit, so the first v8 scan reads each image once to learn the
  reason. `dimagine scan` then prints the images with no taken time broken
  down by reason (the human summary, and `index.taken_missing` in the JSON),
  because "0 with taken time" across a thousand images is something to fix
  only if the files carry a date the reader could not use — and the number
  alone cannot say which library this is.
- Index: the taken time is read from wherever a date actually is — a later
  IFD as readily as the first (a thumbnail-first file is how scanners write
  TIFF), `DateTimeDigitized` or the file's own `DateTime` when nothing says
  when the picture was taken, each refined by its own `SubSecTime*` and moved
  into UTC by its own `OffsetTime*`, an UNDEFINED date as readily as an ASCII
  one, and the XMP packet an exporter moved the date to (a JPEG APP1 segment,
  a WebP `XMP ` chunk, EXIF tag 700) when EXIF names none. Dates are read
  tolerantly (the ISO `2021-06-30T17:45:12+02:00` beside the EXIF
  `2023:07:12 20:54:07`, a time to the minute, a date with no time of day,
  `Z` or `±hh:mm` alike) and never invented: a date naming no moment stays an
  unknown, and says it was a date that failed rather than the absence of one.
  One entry whose offset runs past the block — the MakerNote cameras write —
  no longer costs the date written beside it, and a block that yields no entry
  at all is reported as unreadable rather than as holding no date.
- Viewer Phase 1: the library is the home. `/` shows every image with the
  scope in the query string (`in`, `sub`, `c`, `tag`, `q`, `sort`, `dir`,
  `size`, `p`, and the `untagged` and `recent` lenses), and `/folders`,
  `/collections` and `/search` are its siblings. Every view is a URL, and
  the same query drives `/api/view` and `/api/sidebar`.
- Viewer: one layout at three widths — a bottom tab bar on a phone, an icon
  rail on a tablet, a pinned sidebar and inspector on a desktop — with the
  stylesheet and one small script shipped inside the binary and served from a
  content-hashed URL. The script only enhances; every page works without it.
- Viewer: the index is opened at startup, built before traffic is accepted
  when it is missing or unusable, and refreshed every `--rescan-interval`
  seconds (default 300, `0` disables). Listings and counts come from the index.
- Index view API: `untagged` and `added_after_ns` filters (the Recent lens
  no longer uses the latter; see below).
- Accounts: the viewer has a single owner account (email + password, argon2id)
  that the first visitor creates on `/setup` — there is no setup code to copy,
  and while no account exists the server says so at startup and every 10
  minutes, because until then the first visitor can claim it. Setup is
  serialised, so a second concurrent request is sent to the login page instead
  of becoming a second owner. `dimagine user` creates, lists, changes passwords
  and deletes accounts: `user delete <email>` asks first (`--yes` skips the
  prompt), and deleting the last account puts the next `serve` start back into
  setup. Any non-empty password is accepted; one shorter than 8 characters is
  only used after the setup form's "Use this weak password anyway" checkbox or
  the CLI's `--allow-weak`. The account store lives outside the library
  (`--data-dir`), refuses to start when it is unreadable or malformed, and keeps
  unknown fields. Passcode mode still works until an account exists.
  `--trusted-proxy` takes the client address from `X-Forwarded-For` only from
  listed proxies; session cookies get `Secure` behind an HTTPS proxy or with
  `--secure-cookies`.
- Viewer without login: `dimagine serve --auth none` (default is `--auth
  account`) serves the library to anyone who can reach the address, banners
  every page with "No login: anyone who can reach this address can see this
  library", warns once when the bind address is not loopback — and starts
  anyway — and needs no account store at all.
- Note property `added` (FORMAT §3.1): when the image first entered
  the collection, possibly in another tool (e.g. Eagle's add time);
  "newest added" sorts by `added`, else `imported`.
- Eagle import: `added` written from the item's `btime` (ISO 8601,
  local UTC offset); `dimagine import eagle --backfill-added` inserts
  it into already-imported notes (dry run by default, `--apply`
  atomic and byte-preserving, idempotent).
- Index view API: read-only `view` (folder, recursive, collection,
  tags, text, sort, paging), `folder_counts`, `tag_counts`,
  `collections` and `appears_in`; `dimagine scan` populates the new
  `first_seen_ns` and `added_ns` columns (schema v3).
- Viewer hardening: login attempts are throttled before comparison, bounded
  in flight and charged against per-client and global budgets; expensive
  requests are admission-bounded (503 + Retry-After); ETags come from the
  streamed file handle; renditions revalidate instead of being immutable.
- Eagle import: IP-literal hosts get no site label; Rust and Python agree on
  meaningless names; YAML escapes every forbidden character; superscript
  Windows device names are cleaned; the Python selftest runs in CI.
- Viewer: links and collections resolved through dimagine-core (`../` embeds,
  collections without `kind`, ambiguous and missing members shown explicitly).
- Index: `finish_scan` without an active scan is rejected instead of pruning.
- Previews: on Windows, cache writes refuse reparse points and re-verify paths.
- One `dimagine` binary: `import eagle`, `previews` and `serve` subcommands as
  built-in plugins (Cargo features, on by default), switchable per library in
  `.dimagine/core-plugins.json`. `previews` reports images that fail to decode.
- Skill: images with meaningless names are named `<site>-<id>` from an
  ID-shaped token in their source URL path (query strings and tracking
  segments ignored) before falling back to the import time.
- Core checks: grouped duplicate-ID findings, incomplete-read exit reporting, bounded note reads, native path tracking, and stricter link/collection checks.
- Rust workspace with the first two read-only CLI commands, `dimagine scan` (library summary) and `dimagine check` (findings), plus CI.
- Read-only web viewer (`dimagine-serve`): sanitized Markdown,
  bounded login/session state, hidden-path checks, streaming media, and secure
  cache/cookie behavior.
- Viewer front matter parsing through `saphyr`, with explicit malformed-note diagnostics.
- Index scans reject writes outside an active transaction, validate required
  schema keys and FTS synchronization triggers, and use Unicode case folding
  for text search.
- Prototype scripts: an Eagle importer and a tool that adds missing
  self-embeds (`scripts/prototype/`).
- Format 0.1 draft revision: image notes end with an embed of their own image;
  file names avoid `[ ] # ^ |`.
- Library format 0.1 draft (`docs/FORMAT.md`) and the agent skill (`SKILL.md`).
- Project restart: README, MIT license, contributor and agent rules, vision and
  documentation index.

### Fixed
- Viewer: the sidebar Views counts — All, Recent, Untagged — now follow the
  active filter instead of reporting whole-library totals, so one screen no
  longer mixes two different populations.

### Changed
- Library format `docs/FORMAT.md` is now version **0.2** (its changelog records
  the bump): the whole 3b write contract has landed, so the minor version the
  ADR promised is owed. A short **Writing** index section maps each writing
  rule — front matter and `id`, the self-embed, link syntax, member blocks and
  captions, the body edit, move/upload/delete, the write log — to the section
  that owns it, so the contract is reachable without any rule appearing twice.
- Viewer: `/thumb/<path>` and `/media/<path>` now answer `302 Found` to the
  content URL when the index holds a digest for the bytes, with `private,
  no-cache` on the hop itself — a cached `301` would pin a path to one digest
  for as long as the browser kept it, and a path can be reused for other bytes.
  A path the index has no digest for keeps the answer it always had: the
  rendition itself, light when it is already on disk and heavy when it must be
  generated. `/raw/` is unchanged.
- Viewer: classifying a media request no longer reads and hashes the source
  image (W70 review M1). The digest a rendition is keyed by comes from the
  index, keyed by the file's own size and nanosecond mtime — a stat and one
  indexed lookup — and the file is read only when the index holds no digest for
  those facts. A grid's revalidations now cost no image I/O at all.
- Viewer: signing out also answers `Clear-Site-Data: "cache"`, beside the
  `no-store` and the cleared cookie it already sent. A preview is cached for a
  year, so the next person at this machine would otherwise read one back out of
  the browser's disk cache.
- Viewer: a collection view states its order instead of offering one it does
  not keep ("Note order", the counterpart of the Recent lens's arrow — a
  collection is ordered by the embeds in its note, FORMAT §5), and a `sort`
  in a collection URL says it was ignored rather than looking like it
  decided something. A narrowed view that is also a collection is named by
  the collection, not by the folder that merely narrows it; its empty state
  blames the filters beside it when they matched none of the members.
- Viewer: "Recent" is the 200 most recently added images, however old they
  are — labelled "Recent — last 200 added" — instead of a fixed 30-day
  window that covered 95% of a library imported inside it. The sidebar row,
  the view's own count and `/?recent=1` all state the lens's size: 200 or
  fewer. The URL is unchanged, and the lens keeps its newest-first order:
  like a collection's embed order, a sort picked beside Recent cannot
  decide which images the lens holds.
- Viewer: a narrowed view names itself, in the tab title and in a heading
  above the grid — "Tag: x" (every tag named, in the order they narrow),
  "Folder: path", "Collection: name" (the note's `title`, else the file), or
  "Search: q" — instead of the generic "Search" that never said what was on
  screen (W34 audit #12). The whole library stays quietly "Library".
- `dimagine serve` stops cleanly: Ctrl-C (SIGINT) and SIGTERM now close the
  listening socket, answer the requests already in flight, and exit with status
  `0`. The wait is bounded at 5 seconds, so a client that has stalled cannot
  hold the viewer open forever; whatever is still running when the window
  closes is dropped with a line on stderr.
- CLI: `dimagine import eagle` answers exit code `3` — "did not finish reading",
  the code `scan`, `check` and `previews` already use — when the folder it was
  pointed at cannot be read at all (missing, or locked) or when an I/O failure
  stops the import partway through. Exit code `1` stays for a reading that
  finished and found something: a folder that is not an Eagle library, a
  destination that is not empty, overlapping paths. The `--json` document for an
  incomplete read carries `"read_complete": false` and, when the run got that
  far, the partial import report. `import eagle --backfill-added` follows the
  same rule, so a missing library folder is now `3` where it used to be `1`.
- Viewer: the collection list — the sidebar section, the `/collections` page
  and the `/api/sidebar` `collections` array — carries only the notes that
  mean to collect: `kind: collection`, or a non-image note that embeds at
  least one image. FORMAT §5 is unchanged about what a collection IS: an
  image note that embeds its siblings is still a collection — `/?c=<note>`
  shows its members and "Appears in" still names it — but it is not listed
  (on a real library, 1,170 of 1,175 collections were such image notes showing
  sibling previews). The index API stays backward compatible: `collections()`
  still names every collection, now with a `listed` flag, and `/api/sidebar`
  collection items consistently carry `path`, `title` and `count`, with
  `count` a number.
- Documented that the account `role` field is stored but not yet enforced
  (reserved for the future multi-user surface); there is no behavior change.
- Viewer: tiles and the image page follow the design system. A tile is square
  and shows the whole picture; only an extreme aspect ratio is cropped — a very
  tall image is top-aligned and carries a "Tall" badge, a very wide one is
  centre-cropped and carries a "Wide" badge — and the ratio is measured from
  the loaded thumbnail, so an unmeasured image is never cropped on a guess.
  Names are visible on a phone and on hover or selection elsewhere, a pending
  thumbnail holds the grid's shape as a skeleton, and a picture that cannot be
  loaded names its file and says "Unavailable" instead of rendering an empty
  square (W34 audit #1 display side and #3). The image page's stage takes the
  picture's shape: a tall screenshot fills the column and scrolls inside a
  capped well instead of painting a sliver in an oversized empty box (#2), and
  the properties and the rendered note are drawn as the design system's field
  list and its wikilink / embed styles (#8, #9).

### Removed
- The 2019 prototype (an Express/Pug Pinterest for illustrators). Its code is
  kept at tag `v0-2019`.

### Changed
- Viewer: `/folder/<path>` and `/collection/<path>` answer 301 to the library
  view they now mean (`/?in=`, `/?c=`), so a link shared from an older version
  still lands on the images it meant. A path that is not in the library is not
  redirected: an empty grid would read as "this is empty" rather than "this is
  not here".
- Viewer: the image page carries "Back to view" (the view it was reached
  from), "Appears in" (the collections that embed it), the path with a copy
  button, and the note's `source` link.
- Index view queries answer from SQLite: folder, tags, text, sort and paging
  are pushed into SQL over derived columns (`folder`, `name_key`, `rating`,
  `added_ns`) and a normalised `note_tags` table, and the counts are grouped
  in SQL (schema v6). On a 20 000-image synthetic library a folder view is
  ~16x faster, a short-text view ~14x, `collections()` ~2x, and no query reads
  and parses every note any more.

### Performance
- The viewer's list queries no longer wait for a background rescan, and a
  repeated query no longer re-counts its whole result on every request. The
  number of committed scans is now stored in the index and raised in the same
  transaction as the scan's content, so a read snapshot and the number it was
  counted against always move together. A read-only reader connection answers
  `/api/view` — the page, each tile's header and digest, and the sidebar — while
  the rescan writes on the index connection, so no live request blocks behind one;
  view totals and the sidebar are cached per query and per committed generation,
  so each is counted once per scan rather than once per request. On a 20k-image
  library a request that once waited several seconds for an ongoing rescan now
  answers in tens of milliseconds.

### Fixed
- Upload: a duplicate inside one batch now names the note its
  group lands with (RW74 Low-1). The duplicate outcome looked the
  note up in the pre-op index, so the second of two identical
  files in one upload reported `note: ""` while the note the
  first file planned landed with the operation; the outcome now
  asks the batch's own planned steps as well. Only `--json`
  consumers see the field.
- Viewer: the tiles a "Load more" appends now open on double
  click (RW74 Low-3). Only the server-rendered tiles carried the
  double-click handler, so an appended tile never opened on a
  double click — outside selection mode it was single-click
  only, the asymmetry the drag-select review noted. Appended
  tiles now carry the same handler: the same `selecting` guard
  and the same open as the page's own tiles.
- Upload: the serve endpoint is bounded and two naming edges are closed (RW66c
  follow-ups). A whole request may buffer at most four times the per-file cap in
  file bytes, and each `folder`/`collection` field is capped at 4 KiB — both
  enforced while the body streams, so a batch of many large files or an oversized
  placement field can no longer hold RAM without bound. The paired note's name is
  reserved alongside the image leaf, so a pre-existing orphan `X.jpg.md` lands the
  upload as `X 2.jpg` (with `X 2.jpg.md`) instead of splitting the group after the
  image has landed, and a leading-dot stem is sanitized rather than failing the
  whole upload as a hidden path. An `IMG_20261017_093012`-style camera name is now
  recognized as meaningless and renamed, and a duplicate outcome names its note
  only when the existing image really has one. `folder` and `collection` in one
  request is a typed refusal, as the CLI's `--to`/`--to-collection` are, and the
  upload cap's docs say MiB, matching the value.
- Viewer: the body editor's conflict prompt (RW68 follow-up). A save whose
  precondition is stale now shows what the note holds now — the 409 carries the
  current body beside the hash — behind a disclosure, with "Load the latest"
  (that body into the editor) and "Use my version anyway" (retry with the hash
  the server just returned) as the two ways out, so a change made elsewhere is
  neither silently overwritten nor left unexplained. The editor's hint says the
  browser sends line endings as LF, and FORMAT §5.3 records that a CRLF body
  saved through a textarea comes back with LF. Replacing a bare-closing-fence
  note's empty body with an empty one leaves the file untouched instead of
  gaining a newline, and the note is no longer reported as changed. A leading
  byte-order mark is now described as the file's, not the body's.
- Viewer: three drag-select corners (RW69 follow-up). A double tap on a
  tile while selection mode is open no longer opens the image and drops the
  whole selection: the double click carries the same `selecting` guard as
  the single click. A swipe on a phone with the bottom bar open scrolls the
  grid again — the `touchmove` block now covers only the finger a long press
  already claimed, instead of the whole `.selecting` phase, which used to
  leave a dead zone where a swipe neither selected nor scrolled. The arrow
  keys and Enter wait while the mode is open rather than refilling the
  inspector and adding a tile the bar never counted.
- CLI: `collect --new`, `--add` and `--remove` refuse at the argument parser
  (exit `2`) when given no images again, as they did before `--reorder` and
  `--caption` relaxed the operand into a list that could be empty. An empty
  `--new` no longer creates an empty collection and an empty `--add`/`--remove`
  no longer reports "nothing to change"; `--reorder` and `--caption` keep the
  operands they need (RW67 D2).
- `dimagine serve` now runs its periodic background rescan. The router — which
  starts the rescan task, and needs a Tokio runtime to spawn on — was built
  before that runtime existed, so the task never started and the viewer served
  its startup index forever, never seeing files added or changed after launch.
  The app is now built inside the runtime, and a CLI regression test drops a file
  into a served library and requires the next rescan to pick it up live.
- A move whose destination is a symlink to the source — or any fold-equal
  alias sitting beside it — is refused whole instead of being torn apart
  (RW65c R1). The planner's same-file test now reads the destination entry's
  own identity (device + inode, without following the final symlink), not the
  path a symlink resolves to, so an alias is its own entry and the group move
  stops before it renames anything.
- Reordering a collection whose last line has no final newline no longer glues
  that line onto the next one: a block's content moves, but the terminator that
  closes a byte range stays with the range, so every line boundary and the
  note's trailing-newline shape survive the permutation. Before this, moving the
  unterminated block into the middle could swallow the following embed into a
  caption and merge two member blocks into one.
- A collection edit that would change nothing still answers a stale open hash
  with a conflict instead of a silent `changed: false`: the precondition is
  judged before the no-op shortcut, so a caller whose version is no longer the
  note's learns that rather than believing its edit is in place.
- Viewer: a busy server no longer breaks random thumbnails for good (W70). A
  grid page asks for dozens of thumbnails at once, and every one of them —
  revalidated on every reload, since rendition URLs cannot cache immutably —
  used to take one of eight admission permits as its very first step and hold
  it until the last byte, with the ninth request onward answered 503
  immediately; `<img>` does not retry, so those tiles became "Unavailable"
  permanently, a different set on every reload, while every preview sat intact
  on disk. Media admission is now two classes, decided before either permit is
  taken: serving an existing preview (or a note's source view) is light — 64
  permits, waited for up to 2 s, held only around the file open — and anything
  that may generate a preview, stream an original as a fallback, or answer
  `/raw` of an image is heavy — the same 8 permits, but queued: up to 256
  waiters may wait up to 10 s for their turn, and the next request past that,
  or one that waits its 10 s out, is refused with an honest `Retry-After: 10`
  (the whole wait a fresh attempt could still face; a refused light request
  says 2). A missing thumb preview is never answered by streaming the multi-MB
  original outside the heavy class. The client retries with it: a tile image
  that fails to load tries again up to three times — 1 s, 2 s, 4 s plus
  0–500 ms of jitter, each attempt a fresh `?r=N` the routes ignore — and only
  the last failure takes the tile's "Unavailable" form; a 404 is caught by a
  cheap probe before it can waste the schedule, since a missing image is the
  one answer a retry cannot change. Directory listings keep the immediate
  refusal they always had.
- Viewer: the review follow-ups on the polish round (W55), from RW54. The order
  bubble — the sentence a one-word order label stands for — now opens inside
  the viewport at every width it can open in: below ~550 px it anchors at the
  label's left edge, because a bubble grown leftwards from the label's right
  one ran ~178 px off the left edge of a 390 px window with the row wrapped,
  and clipped until ~550 px unwrapped. Nothing hides it on touch either: a tap
  that focuses the label reveals it for no row width, where the old
  `(hover: none)` rule's `display: none` left a sighted keyboard user on a
  touch device with no visible sentence and took the text out of the screen
  reader's tree with it. And a login refused because both in-flight slots are
  busy now answers `Retry-After: 6` — the whole hold a busy slot can impose,
  the escalating delay plus the argon2 check after it, rounded up to the next
  whole second — where `5` could still send a client back into one refused
  attempt.
- Viewer: the image page's Properties list decides the note's `width`/`height`
  pair together (RW53 review Low-1). A note whose two numbers both agree with
  the index stays silent — the Image section has just said those exact numbers
  — but a note that disagrees in one number of the pair now keeps both rows:
  half a match used to drop only the agreeing row and leave a lone `height`,
  which read as a note that carried a height and no width at all, beside the
  pair it disagreed with two lines up.
- Viewer: the review follow-ups on the older-Low round (W54), serve side. A login
  refused because both in-flight slots are busy now answers `Retry-After: 5` —
  the longest a held slot can last — instead of `1`, so a client is not sent
  straight back into a refused attempt (RW19b). `/api/folder` lists collections
  by the one shared list rule, so an image note that embeds its siblings is no
  longer enumerated beside the deliberate collections, while its own
  `/api/collection/` endpoint and `/?c=<note>` still answer for it (RW27f L-2).
  And the sentence a stated sort order stands for — "Recent is always the last
  200 added, newest first." on the lens, the collection's on `/?c=` — is now
  real text in a bubble the label shows on hover or focus, so a sighted
  keyboard user reaches it and a screen reader reads it, not only a native
  `title` tooltip (RW39b L1). The cross-process setup race on a shared
  `--data-dir` is documented rather than locked: the setup critical section is
  in-process, and two servers on one live state directory are operator error
  (RW25b).
- CLI: `dimagine --help` and `--version` no longer warn about a malformed or
  unknown-key `core-plugins.json`. The switch file is read before clap because
  it decides which subcommands exist, but an invocation that only asks how the
  command works is not a library operation, so the complaint now rides along
  only when the invocation acts on the library (RW16 L7).
- Viewer: "Back to view" out of a collection no longer asks the collection for a
  page number (RW51). Which page holds a tile is worked out from the image's
  place in the view — right for every view that pages — but a collection lists
  all of its members at once and tells the reader a `p` beside `c` was ignored,
  so on a collection longer than one page the way back arrived with a
  status line announcing a page nobody asked for. A collection's own view now
  travels back exactly as it was written, anchor included; every view that
  really does page still gets the page its position says the tile is on.
- Index: the review follow-ups on the taken-time round (RW50). A date property
  that a file's XMP merely *mentions* — a caption spelling
  `exif:DateTimeOriginal='2019-…'` inside a `dc:description` — no longer speaks
  for the file: only a real property position counts, so the date the file
  actually sets wins and a sentence inside a caption stays a sentence. And the
  "taken time missing" line and `index.taken_missing` now account for the whole
  count they name: an image whose reason nobody recorded is counted under
  `reason not recorded` rather than left out of a sum that disagreed with its
  own total.
- Viewer: the review follow-ups on the justified-rows and taken-sort round
  (RW49). A tile "Load more" appends carries the "no taken time" mark the page
  gives its own tiles: the script asked the tile — which was still unappended, so
  it had no grid above it to ask — whether the view was ordered by the taken
  time, the answer was always no, and every dateless image past the first page of
  a library went unmarked and read as the oldest picture there. That mark's own
  words no longer claim a cause the index did not record, either: the index keeps
  five kinds of missing taken time, and the tooltip now states the one thing true
  of all five. The image page and `/api/view` say *which* kind a row is — the
  image page in words, the JSON in the same one word `dimagine scan` prints
  (`no-exif`, `date-unreadable`, …) — so an unknown stays the specific unknown the
  scan wrote down all the way to the reader and to the agent (invariant 4). And a
  note that records the picture's `width`/`height` is no longer asked to state
  them a second time under Properties once the Image section has printed that
  exact pair from the index; a pair that disagrees with the index is still shown,
  because two readings of one picture are two facts.
- Index: the review follow-ups on the image-metadata round (RW48). An
  `OffsetTimeOriginal` that parses but names no zone — `+25:00`, `+99:99`, an
  hour past 23 or a minute past 59 — is refused rather than applied: a corrupt
  or hand-edited field used to move a photo by up to ±6.7 days, where the
  documented rule keeps the date in the zone-less rule instead (a companion
  tag that is refused) or names no moment at all (a value that carries its
  own bad zone). The images whose dimensions are unknown are counted under a
  name that says so, `index.unknown_dimensions`, rather than as "unreadable
  image headers" — a format this build has no reader for (AVIF, HEIF) lands
  there beside a header that failed to parse, and the count now claims only
  what it knows. The content-keyed header cache is pruned with the content it
  describes instead of only growing, and two properties that held by
  construction now hold under test: an upgrade interrupted before its `COMMIT`
  rolls back to the old schema and completes on the next open, and an
  unchanged file is re-read by nothing.
- Viewer: the review follow-ups on the media and collection routes (RW46).
  `/raw` is the source view of a *note*, and now gates on the library's own
  class test instead of on "not an image": what a note's page links as
  "Open note" comes back as text, while a raw source JSON, a canvas, or
  anything else an importer left in the library stays unreachable through it.
  Answering one such click no longer reads the whole file either — a note's
  ETag is its own size and modification time rather than a SHA-256 of its
  bytes, which is what a file the viewer never writes needs to revalidate on;
  images and renditions keep the content hash. A collection page says it
  ignored a page number, the way it already says it ignored a sort: its members
  are the whole of the note's embed list, listed at once, so `p` asks for a
  page of a list that has no pages. And the collection grid stops carrying a
  class no rule of the stylesheet or line of the script has ever read.
  `/raw` and `/media` are named beside the pages in the passcode test, the
  source view is pinned against traversal and the outside-symlink, and a
  collection's own hostile prose is pinned arriving escaped at the route a
  reader reaches rather than only where the sanitiser is unit-tested.
- Viewer: the image page's three review follow-ups (RW45). "Back to view" asks
  for the page the tile is actually on: the arrows walk the whole view while
  the `p` the tile carried stays where the reader started, so after a walk past
  a page boundary Esc was anchoring a tile that is not in the page it opened
  and landed at the top of the grid. "Appears in" no longer reads an index that
  did not answer as "Not in any collection." — the section says the collections
  could not be read, because an unknown is not an empty row. And the pull-up
  sheet's `aria-expanded` (with its "Show details"/"Hide details" label) moves
  onto the handle that changes it, where a screen reader reads it, instead of
  the panel it opens, where one is read as nothing.
- CLI: `dimagine previews` decides "generated" from "cached" by what the cache
  held when the run started — a rendition is named after the content hash of
  the image it came from — instead of by the file's timestamp. On a filesystem
  with coarse (one-second) mtimes a first run could report "0 generated, N
  cached" for work it had just done, and a cached rendition carrying a newer
  timestamp than the run was reported as generated. A rendition the run found
  unusable and rewrote now counts as cached rather than generated: the counts
  say how much of the cache had to be built from the images, not how many bytes
  reached the disk.
- Viewer: an accounts store that becomes unreadable while the server runs
  fails every gated request with 500 and the store's own error message, and
  the auth gate's comment says what the code does. Read errors were read as
  "no users", which pointed every page back at `/setup` — the first visitor
  could try to claim a server whose store had broken under it. A *missing*
  accounts file is still the one shape that opens setup; startup still
  refuses an unreadable one.
- Viewer: error pages (404, 403, 500 and the index-unavailable 503) are
  HTML pages with the frame and navigation, so under `--auth none` they
  carry the no-login banner like every other page, instead of answering
  with a bare status and an empty body.
- Viewer: the setup form's weak-password hint reads "easy to guess: tick the
  box"; the space was missing.
- Viewer: the `--auth none` banner is one full-width bar above the whole shell
  at every width, instead of becoming a column beside the rail or sidebar on
  tablet and desktop. The stylesheet and script are served without a session,
  so `/login` and `/setup` render styled instead of having their own assets
  redirected back to the page that asked for them.
- Eagle import: note writes are fsynced before the rename and the directory is
  fsynced after it, so an imported or backfilled note survives a crash.
- Index: a `rating` outside 0-5, or one that is not an integer, reads as no
  rating instead of sorting above five (FORMAT §3.1).
- Index: `tag_counts` no longer counts a note whose image is gone, so the
  sidebar count matches the list.
- CLI: `dimagine scan` always says when the index could not be refreshed,
  including when the library read was incomplete, instead of leaving a stale
  index unmentioned.
- Index: the module documentation says where the `first_seen` fallback is
  applied (in SQL, by the index) and the unused `Index::first_seen` accessor is
  gone, instead of claiming the scan reads the column back.
- Index: removing the `added` (or `imported`) property from a note clears the
  derived added position again instead of keeping the old value until the index
  is rebuilt (schema v5).
- Eagle import: `--backfill-added` reports `unreadable` for a note or raw file
  it could not read, separately from `no btime`.
- Eagle import: `--backfill-added` reports `no front matter` for a note it
  cannot insert into, instead of counting it as "already had added" and
  claiming the library is up to date.
- Index and viewer: a collection is a note that embeds images or carries
  `kind: collection`, decided by one shared rule that `dimagine serve` also
  uses; an image note's self-embed is no longer counted as a membership.
- Eagle import: an integral float `btime` (which Eagle writes) becomes `added`
  again, and a `btime` whose year falls outside 0-9999 is omitted instead of
  writing an unreadable five-digit year. The year is decided on the UTC
  instant, so `10000-01-01T00:00:00Z` is left out in every timezone, and both
  importers are now checked at fixed UTC offsets rather than at whatever
  timezone the machine running the tests is in.
- Eagle import: `--backfill-added` keeps a note's leading UTF-8 BOM (FORMAT
  §3.1), which it used to delete while claiming byte preservation.
- Index: an `added` property that does not parse falls through to `imported`
  instead of ending the "newest added" chain.
- Index: a move keeps the image's `first_seen_ns` in the real `dimagine scan`
  path, paired only when the match between the vanished and the appeared rows is
  unambiguous 1:1 (by note `id`, else by size and mtime) and the appeared image
  is one this scan brought in, so a duplicate copy in the same scan as a move is
  dated as the new file it is.
- Previews: grid thumbnails render through a crop-then-scale pipeline, so a
  tall screenshot (e.g. 1440x3000) gets its thumbnail in milliseconds and
  concurrent `/thumb` requests no longer queue behind the four generation
  slots.
- Previews: a thumbnail keeps a readable shape (FORMAT §8.2): long edge ≤ 400
  px, aspect ≤ 5:2. An extreme-aspect source (e.g. a 780x48000 full-page
  screenshot) is cropped to 5:2 — from the top for tall sources, from the
  left for wide ones — a real crop, never a squeeze or a sliver on the grid.
- Viewer: `/collection/<note>` resolves for every collection the index knows,
  including the image-note collections the list leaves off. Reading the list
  there made "unlisted" mean "not a collection", so a legacy link to an image
  note answered 404 while `/?c=<note>` answered 200.
- Viewer: the Recent lens's order is stated, not offered — the lens is the last
  200 added, newest first, so a sort beside it cannot decide the order; before
  this it showed the picked option while the grid stayed Added-descending. The
  order is a focusable arrow beside the count, so the sentence that explains it
  is reachable on hover and focus and by a screen reader, and the phone toolbar
  at 390px keeps all five controls visible (a disabled control with a wide
  visible hint did neither). Over a collection the toolbar claims no order at
  all, because a collection keeps its own member order. A page past the end of
  a view now says so and offers the way back, instead of claiming the library
  is empty beside a count of 200.
- Viewer: `/collection/<note>` answers from that note's own row instead of
  listing every collection on every request, so a burst of legacy links no
  longer holds the index lock and slows unrelated pages down.
- Viewer: a legacy `/collection/<note>` that also carries a `c` keeps the note
  the pretty URL named — the carried `c` used to win and point the page at a
  different collection.
- Viewer: `/collection/<note>` reports an index it cannot read as 503 with the
  reason, instead of answering 404 "not a collection" for a note that may well
  be one.
