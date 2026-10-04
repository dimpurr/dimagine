# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added
- Security and streaming fixes for the read-only serve viewer: sanitized Markdown,
  bounded login/session state, hidden-path checks, streaming media, and secure
  cache/cookie behavior.
- YAML front matter parsing through `saphyr`, with explicit malformed-note diagnostics.
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
