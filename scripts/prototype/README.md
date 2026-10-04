# Prototype scripts

Small, dependency-free Python scripts (3.8+) used to try the library format
before the `dimagine` tools exist. They follow [FORMAT.md](../../docs/FORMAT.md)
and will be replaced by the corresponding tool commands.

| Script | What it does | Replaced by |
| --- | --- | --- |
| `eagle-import.py <Name.library> <target>` | Copies an Eagle library into a new dimagine library: images, image notes (with self-embed), verbatim `.eagle.json`, one collection per Eagle library, and an import report. The Eagle library is only read. Skips items in Eagle's trash, items without `metadata.json`, and non-images, and lists them in the report. | `dimagine import eagle` |
| `add-self-embeds.py <library> [--write]` | Appends the missing self-embed (FORMAT §3.2) to image notes. Dry run unless `--write`. | `dimagine check --fix` (planned) |
