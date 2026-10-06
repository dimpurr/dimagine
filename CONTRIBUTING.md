# Contributing

dimagine is pre-alpha and not yet accepting outside contributions; issues are
welcome once the first usable version ships.

## Toolchain

`rust-toolchain.toml` pins the Rust stable that CI and dev machines use
(currently 1.99.0, with `rustfmt` and `clippy`), so every machine checks the
same code with the same lints: a floating toolchain let clippy 1.99 reject
`chunks_exact` with a constant chunk size that clippy 1.92 accepted, and the
same class of drift breaks CI for code a developer saw pass. rustup installs
the pinned toolchain automatically on the first cargo command in the checkout.
To move the pin, update `rust-toolchain.toml` and the toolchain refs in
`.github/workflows/ci.yml` and `.github/workflows/release.yml` in the same
change, then run the checks in this document on the new pin.

## Checks

Run all three; each must exit 0, with the pinned toolchain above. CI runs the
same set on ubuntu-latest and macos-latest.

```
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test
```

## Commits

- English, imperative mood, one logical change per commit.
- Update `CHANGELOG.md` under `Unreleased` for any user-visible change.
- Update the owning document from `docs/INDEX.md` when behaviour or format changes.
