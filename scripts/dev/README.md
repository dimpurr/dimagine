# Developer Tooling: Demo Library & Screenshots

This directory contains developer utilities for generating realistic test libraries and capturing viewer screenshots for design and implementation reviews (W27/W28).

## 1. Demo Library Generator (`demo-library.py`)

`scripts/dev/demo-library.py` generates a small but realistic, deterministic dimagine library conforming to `docs/FORMAT.md`.

### Usage

```bash
# Generate default library (120 images) to an output directory
python3 scripts/dev/demo-library.py /tmp/demo-lib

# Custom seed and image count
python3 scripts/dev/demo-library.py /tmp/demo-lib --seed 42 --images 120

# Run self-test validation against FORMAT rules
python3 scripts/dev/demo-library.py --selftest
```

### Library Characteristics

- **Folder Hierarchy**:
  - `refs/` and `refs2/` (tests prefix boundary matching)
  - Names with spaces: `Architecture & Design`, `Projects [2026]/Concept Art`
  - Names with Unicode: `Café & Life ☕/Interior`
  - Names with brackets: `Projects [2026]`, `refs/studies [wip]`
  - Nested folders: e.g. `refs/ui`, `Architecture & Design/Modern`
- **Images (~120 items)**:
  - Varied aspect ratios: portrait (e.g. 600×900, 800×1200), panoramic (e.g. 1800×600, 2400×800), square (600×600), landscape (1200×800, 1600×900).
  - Varied color palettes, geometric patterns, and borders.
  - Generated using PIL/Pillow when available, with an automatic pure-Python stdlib fallback.
- **Notes (`docs/FORMAT.md`)**:
  - Some notes missing (~25% of images have no `.md` note).
  - Some notes contain only `imported` in front matter (~20%).
  - Full notes include `id` (valid 26-character Crockford ULID), `title`, `tags`, `rating` (1–5), `source`, and timestamps (`imported` and `added`).
  - All image notes end with a self-embed `![[<path>]]` per FORMAT §3.2.
- **Collections**:
  - `collections/featured-picks.md`: Explicit collection note with `kind: collection` embedding multiple images.
  - `notes/project-moodboard.md`: Plain note embedding multiple images (implicit embedded collection).
  - Image notes embedding their own paired image: Verified that their self-embeds do *not* count as collections per FORMAT §3.2.
- **Non-Image Files**:
  - `docs/styleguide.pdf`, `notes/palette.txt`, and `README.txt` (FORMAT §2.1 non-image files).

---

## 2. Screenshot Automation (`screenshots.sh` + `capture.mjs`)

`scripts/dev/screenshots.sh` automates capturing screenshots across all required viewports, themes, and application routes. It builds a job list and hands it to `scripts/dev/capture.mjs`, which drives Chrome over the DevTools Protocol.

`capture.mjs` uses `Emulation.setDeviceMetricsOverride` rather than `--window-size`: headless Chrome will not lay a window out below 500 CSS pixels wide, so `--window-size=390,844` produces a 500px layout cropped to 390, not a phone layout. The override sets the viewport the page actually uses, and the same page is then measured — every 390px job fails the run when `document.documentElement.scrollWidth` exceeds `window.innerWidth`, so a horizontal overflow is caught without looking at the images.

### Usage

```bash
# Standard mode: builds release binary, creates temp demo library,
# runs dimagine scan, starts two servers (--auth none for the library
# routes, --auth account with an empty --data-dir for /setup and /login),
# and captures screenshots.
./scripts/dev/screenshots.sh /path/to/screenshots

# External URL mode: shoot against an existing server (skips build and serve)
./scripts/dev/screenshots.sh /path/to/screenshots http://127.0.0.1:8917
```

`/setup` is shot on the account server while no owner exists; the script then creates an owner with `dimagine user create --password-stdin`, which turns that same server into the `/login` form. Both state directories are temporary and removed on exit. In external-URL mode there is no second server, so the auth routes are shot against the given URL (where they may not exist).

`capture.mjs` needs Node 22 or newer (for the built-in global `WebSocket`).

### Behavior & Exit Codes

- **Exit 0**: All screenshots captured, and no page overflowed at 390px.
- **Exit 1**: Missing Chrome/Chromium or Node 22+, a server failed to start, or a 390px page overflowed horizontally.
- **Exit 2**: Command-line usage error (missing `<out-dir>`).
- **Exit 3**: The binary lacks the `--auth none` flag (which is being added in parallel). When this flag is missing, the script prints an explanatory message and exits with status 3.

### Matrix

- **Viewports (W×H)**:
  - `390×844` (Mobile)
  - `834×1194` (Tablet)
  - `1440×900` (Desktop)
- **Themes**:
  - `dark`: `--force-dark-mode` and `--blink-settings=preferredColorScheme=0`
  - `light`: `--blink-settings=preferredColorScheme=1`
- **Routes & Slugs**:
  - `/` -> `library`
  - `/?in=refs/ui` -> `folder-in`
  - `/folders` -> `folders`
  - `/collections` -> `collections`
  - `/search` -> `search`
  - `/image/<path>` -> `image`
  - `/login` -> `login`
  - `/setup` -> `setup`

### Output Files

Files are named `<width>-<theme>-<slug>.png` (3 widths × 2 themes × 8 routes = 48 files total), e.g.:
- `390-dark-library.png`
- `390-light-library.png`
- `834-dark-folders.png`
- `1440-light-setup.png`

---

## 3. Browsers

Capture screenshots only through `screenshots.sh` / `capture.mjs`: they start Chrome, record its PID, and kill it on every exit path — normal exit, thrown error, and SIGINT/SIGTERM/SIGHUP. Any other headless browser must record its PID at launch and kill that exact PID in a trap/finally on every exit path; never kill browsers by name or command-line pattern.
