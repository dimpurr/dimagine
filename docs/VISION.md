# Vision

## What dimagine is

A reference image library built for agents first and people second: the kind of
collection a designer or artist keeps in Eagle or Pinterest, but stored as plain
files so that coding agents and scripts can search, tag, sort and assemble it in
bulk.

Typical requests it should make easy:

- "Find ten black-and-white pencil sketches of a girl sinking into deep water."
- "Tag these five hundred images by style and palette."
- "Start a project for a music video, with one sub-collection per shot, and put
  ten candidates in each."

## Principles

- **Files are the library.** Images, sidecar metadata and collections are plain
  files that can be backed up, synced and read without dimagine running. Any
  database is a rebuildable index.
- **Agent-first interface.** A CLI with machine-readable output is the primary
  interface; an MCP server and a small web UI sit on top of the same core.
- **Runs where the files are.** The same program serves a library on a laptop
  or on a small server; a phone reaches it through a browser.
- **Imports keep provenance.** Every item records its source and the source's
  original metadata.
- **One collection model.** A collection can nest and an image can belong to
  many collections. A board or canvas is a collection whose members also carry
  positions; it is a view, not a separate system.

## Out of scope

- Social features, feeds, likes or public sharing.
- Multi-user accounts and collaboration (single user for now).
- Image generation.
- Life-photo management (dates, places, faces); other tools do that well.

## Roadmap (intent, not commitment)

- **0.1** — library format, CLI, Eagle import with provenance, previews, a
  minimal web view.
- **0.2** — semantic search, search by image, MCP server.
- **0.3** — projects and shot-by-shot sub-collections; later, canvas views.
