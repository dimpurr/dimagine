# Contributing

dimagine is pre-alpha and not yet accepting outside contributions; issues are
welcome once the first usable version ships.

## Checks

Run all three; each must exit 0. CI runs the same set on ubuntu-latest and
macos-latest.

```
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test
```

## Commits

- English, imperative mood, one logical change per commit.
- Update `CHANGELOG.md` under `Unreleased` for any user-visible change.
- Update the owning document from `docs/INDEX.md` when behaviour or format changes.
