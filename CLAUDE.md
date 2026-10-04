# Working on dimagine

## What belongs in this file

This file is loaded into every agent's context automatically, so anything written
here is followed without being questioned. It holds only invariants: properties
that do not change when a version ships or a dependency moves. Anything of the
form "what we are working on now" or "which approach is currently best" belongs
in the owning document listed in `docs/INDEX.md`. Before adding a line, ask
whether it will still be true in six months; if unsure, write a pointer.

## Invariants

1. **The library is a folder, and the files are the source of truth.** Images,
   their notes, collections and boards are plain files. Any cache, index or
   database is derived and must be rebuildable from the files alone.
2. **Tools are optional.** Code in this repository must work on a library that no
   dimagine tool has touched, must tolerate files edited by hand or by agents,
   and must never make a library depend on it.
3. **Originals are never modified.** Tools may copy, move or rename images only
   when asked; they never rewrite image bytes.
4. **An unknown is never recorded as empty.** "Not found", "unreadable" and
   "not present" are different states and stay different all the way to what
   the user or agent sees.
5. **Provenance is kept.** Every imported image records where it came from, and
   the source's original metadata is stored verbatim beside it.

## Repository rules

- English only in this repository: code, comments, docs, commit messages.
- No personal data, machine names, local paths, credentials or tokens.
- One home per fact: update the owning document named in `docs/INDEX.md` and
  link to it from elsewhere; do not restate it.
- `docs/FORMAT.md` is a public contract. Changes must keep existing libraries
  readable and must update `SKILL.md` and the format changelog in the same commit.

## Checks

`CONTRIBUTING.md` lists the commands that must pass. This file does not repeat
them.
