# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added
- Viewer Phase 1: the library is the home. `/` shows every image with the
  scope in the query string (`in`, `sub`, `c`, `tag`, `q`, `sort`, `dir`,
  `size`, `p`, and the `untagged` and `recent` lenses), and `/folders`,
  `/collections` and `/search` are its siblings. Every view is a URL, and the
  same query drives `/api/view` and `/api/sidebar`.
- Viewer: one layout at three widths — a bottom tab bar on a phone, an icon
  rail on a tablet, a pinned sidebar and inspector on a desktop — with the
  stylesheet and one small script shipped inside the binary and served from a
  content-hashed URL. The script only enhances; every page works without it.
- Viewer: the index is opened at startup, built before traffic is accepted
  when it is missing or unusable, and refreshed every `--rescan-interval`
  seconds (default 300, `0` disables). Listings and counts come from the index.
- Index view API: `untagged` and `added_after_ns` filters, for the sidebar's
  Untagged and Recent lenses.
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

### Changed
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

### Fixed
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
