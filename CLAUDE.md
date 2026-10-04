# Working on dimagine

## What belongs in this file

This file is loaded into every agent's context automatically, so anything written
here is followed without being questioned. It holds only invariants: properties
that do not change when a version ships or a dependency moves. Anything of the
form "what we are working on now" or "which approach is currently best" belongs
in the owning document listed in `docs/INDEX.md`. Before adding a line, ask
whether it will still be true in six months; if unsure, write a pointer.

## Invariants

1. **Files are the source of truth.** Original images, one sidecar metadata file
   per image, and collection files are the library. Any database is a derived
   index and must be rebuildable from the files alone.
2. **Originals are never modified.** Imports copy; previews and other derived
   files are generated next to the library, never written over an original.
3. **An unknown is never recorded as empty.** "Not found", "unreadable" and
   "not present" are different states and stay different all the way to what
   the user or agent sees.
4. **Provenance is kept.** Every imported item records where it came from, when,
   and by which importer, and keeps the source's original metadata.

## Repository rules

- English only in this repository: code, comments, docs, commit messages.
- No personal data, machine names, local paths, credentials or tokens.
- One home per fact: update the owning document named in `docs/INDEX.md` and
  link to it from elsewhere; do not restate it.

## Checks

`CONTRIBUTING.md` lists the commands that must pass. This file does not repeat
them.
