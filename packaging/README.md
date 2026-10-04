# Release packaging

The release workflow is defined in [`.github/workflows/release.yml`](../.github/workflows/release.yml). A maintainer prepares a release as follows:

1. Confirm the `dimagine` package builds on the four configured targets and that the version is ready.
2. Create and push a version tag such as `v0.1.0`. The workflow runs only for pushed tags matching `v*`; branch pushes and manual dispatches do not start it.
3. Wait for the `Release` workflow. It builds the `dimagine` binary for macOS arm64/x86_64 and Linux arm64/x86_64, creates one `dimagine-<platform>.tar.gz` archive per target, computes `SHA256SUMS`, and attaches those assets to the GitHub release for the tag.
4. Review the release assets and checksums, then smoke-test the installer using `DIMAGINE_VERSION=v0.1.0 scripts/dev/install.sh` on a supported machine. The installer verifies the archive against `SHA256SUMS` before extracting it.
5. For a Homebrew tap release, copy `homebrew/dimagine.rb.tmpl` into the tap as `Formula/dimagine.rb`. Replace `{{VERSION}}` with the tag version without its leading `v`, and replace each `{{SHA256_*}}` placeholder with the corresponding archive hash listed in `SHA256SUMS`. Verify the formula against the release before merging it into the tap.

This repository contains the formula template only; it does not publish a Homebrew tap. The installer can select a version with `DIMAGINE_VERSION`, use `DIMAGINE_INSTALL_DIR` instead of `~/.local/bin`, and remove its two files with `scripts/dev/install.sh --uninstall` (the same install directory setting applies).
