# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added
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

### Removed
- The 2019 prototype (an Express/Pug Pinterest for illustrators). Its code is
  kept at tag `v0-2019`.
