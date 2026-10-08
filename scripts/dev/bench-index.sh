#!/usr/bin/env bash
# scripts/dev/bench-index.sh
#
# Runs the index timing test with its timing output. The test is
# part of the normal suite: its two sizes run interleaved and the
# medians are compared, so a busy machine slows both runs the same
# way instead of failing the ratio. Run this for the --nocapture
# view when touching the FTS refresh path.
set -euo pipefail

exec cargo test -p dimagine-index ten_thousand_notes_refresh_fts_roughly_linearly -- --nocapture
