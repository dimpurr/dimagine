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

The name was first used in 2019 for a one-day prototype; that code is kept at tag `v0-2019`.
