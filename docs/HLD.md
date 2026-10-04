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
| `index` | `.dimagine/cache/index.sqlite`: paths, sizes, mtimes, SHA-256, notes, links. Rebuildable. Incremental: compare mtime and size, then hash. |
| `preview` | `thumb` and `view` renditions per FORMAT §8.2, keyed by SHA-256. |
| `import::eagle` | Read an Eagle library read-only and write images, notes and raw files into a target folder. |
| `serve` | axum server: grid, folder and collection views, image detail, previews; single passcode. |

## `check` findings (0.1)

Unreadable or unsupported images · extension/content mismatch · notes without
an image · invalid YAML · duplicate `id` · ambiguous links · missing links ·
images that moved (same `id`, new path) with links still at the old path.

## Not in 0.1

Search by meaning or by similar image, MCP, tagging models, colour management
beyond EXIF orientation, watching the folder for changes.
