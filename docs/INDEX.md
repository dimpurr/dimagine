# Documentation index

Release history is in [CHANGELOG.md](../CHANGELOG.md).

## Source of truth

When documents disagree, use this order:

1. Running code and tests define what is actually shipped.
2. `FORMAT.md` (planned) defines the on-disk library format.
3. `HLD.md` (planned) defines architecture, indexing and the CLI/API surface.
4. `VISION.md` defines product intent and deliberate scope.
5. `CHANGELOG.md` records what shipped; it is not a specification.
6. `README.md` is the public orientation.
7. `CLAUDE.md` and `AGENTS.md` define durable contributor rules.

Do not copy a detailed rule into several documents. Put it in the owning
document and link to it from the others.

## Document ownership

| Question | Owner |
| --- | --- |
| Why the project exists and what is out of scope | `docs/VISION.md` |
| How a library is laid out on disk; sidecar and collection schema | `docs/FORMAT.md` (planned) |
| How indexing, search, import and the CLI/API work | `docs/HLD.md` (planned) |
| What shipped in a release | `CHANGELOG.md` |
| Checks and commit conventions | `CONTRIBUTING.md` |
| Durable contributor and agent rules | `CLAUDE.md` (`AGENTS.md` points to it) |
| Public orientation | `README.md` |

## Status labels

Every specification section states its status: `Current`, `Planned`,
`Superseded` or `Research`. Superseded material is kept with that label rather
than deleted; it never overrides running code or a current section.
