# dimagine

Obsidian for images: a reference image library that is just a folder.

> Status: pre-alpha. The library format is drafted; tools are not usable yet.
> License: MIT.

- **A library is a folder of images.** Optional Markdown notes sit next to the
  images; collections are notes that embed images; boards are JSON Canvas files.
- **Agents work on it directly.** Hand an agent [SKILL.md](SKILL.md) and it can
  create, tag, sort and curate a library with ordinary file operations.
- **Tools are optional.** A viewer, search by meaning, previews and importers
  (CLI `dimagine`, short alias `dimg`) make a library nicer to use, but nothing
  depends on them. A library also opens as an Obsidian vault.

Start with [docs/FORMAT.md](docs/FORMAT.md) for the format, [docs/VISION.md](docs/VISION.md)
for what this is and is not, and [docs/INDEX.md](docs/INDEX.md) for where each
kind of documentation lives.

## Sharing a library

`dimagine serve` starts a read-only viewer on `127.0.0.1`; `--bind` moves it to
another address and `--port` moves the port. On the first start every page
leads to `/setup`, where the visitor creates the owner account (email and
password, no code to copy) — until then the first visitor can claim the server,
so the server reminds you of this on every start and every 10 minutes while no
account exists.

**Create the owner account right after the first start, before sharing the
address. If someone else claimed it, run `dimagine user delete <their email>`
on the server and set up again.**

`DIMAGINE_PASSCODE` keeps the old shared-passcode mode for as long as no
account exists. `dimagine serve --auth none` removes the login altogether: the
library is public to anyone who can reach the address, every page says so, and
the server warns once if the bind address is not loopback. `dimagine user list`,
`create`, `passwd` and `delete` manage accounts from the command line, including
short passwords once you confirm them (`--allow-weak`).

The name was first used in 2019 for a one-day prototype; that code is kept at tag `v0-2019`.
