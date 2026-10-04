# Vision

## What dimagine is

The conventions of an Obsidian vault, applied to images. A library is a plain
folder of images; anything you want to record about an image, a set of images or
a board is a plain file next to them. People and agents manage it with ordinary
file operations, and any tool, from a file browser to Obsidian to dimagine's own
viewer, can read it.

Typical requests it should make easy:

- "Find ten black-and-white pencil sketches of a girl sinking into deep water."
- "Tag these five hundred images by style and palette."
- "Start a project for a music video, with one section per shot, and put ten
  candidates in each."

## Principles

- **File over app.** The library is the files. Every tool is optional, and the
  library stays complete when no tool exists.
- **No imposed structure.** Folder layout, naming and numbering belong to the
  user, exactly as in a notes vault.
- **The format and the skill are the product.** `docs/FORMAT.md` defines the
  library; `SKILL.md` teaches an agent to work with it.
- **Tools are plugins.** A viewer that reaches the library from a phone, search
  by meaning or by similar image, previews, importers and maintenance commands
  add convenience on top of the same folder.
- **Imports keep provenance.** Every imported image records where it came from,
  and the source's original metadata is kept beside it.
- **One collection model.** A collection is a note that embeds images; an image
  can be in any number of them. A board is a collection with positions.

## Out of scope

- Social features, feeds, likes or public sharing.
- Multi-user accounts and collaboration (single user for now).
- Image generation.
- Life-photo management (dates, places, faces); other tools do that well.

## Roadmap (intent, not commitment)

- **0.1** — library format and agent skill; Eagle import with provenance;
  previews; a minimal web viewer that serves a library to other devices.
- **0.2** — search by meaning and by similar image; MCP server.
- **0.3** — project and storyboard workflows; board views.
