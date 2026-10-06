#!/usr/bin/env bash
# scripts/dev/bench-index.sh
#
# Runs the #[ignore]d index timing test that `cargo test` skips by default:
# wall-clock scaling ratios are not reliable while the test binary, other
# crates' tests or a busy machine compete for the CPU. Run it on a quiet
# machine when touching the FTS refresh path.
set -euo pipefail

exec cargo test -p dimagine-index ten_thousand_notes_refresh_fts_roughly_linearly -- --ignored --nocapture
