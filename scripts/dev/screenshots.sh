#!/usr/bin/env bash
# scripts/dev/screenshots.sh <out-dir> [base-url]
#
# Capture headless Chrome/Chromium screenshots across viewports, themes, and routes.
#
# If [base-url] is provided:
#   Skips build and server startup; shoots against that base URL directly.
# If [base-url] is omitted:
#   1. Builds the release binary (cargo build --release -p dimagine).
#   2. Checks if binary supports '--auth none' (exits 3 if missing).
#   3. Generates the demo library in a temp dir.
#   4. Runs dimagine scan on the library.
#   5. Starts dimagine serve --auth none on 127.0.0.1:<free port>.
#   6. Waits for server readiness.
#   7. Captures screenshots to <out-dir>.
#   8. Cleans up server and temp dir on exit.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

usage() {
    echo "Usage: $0 <out-dir> [base-url]" >&2
    echo "  <out-dir>   Directory where screenshots will be saved." >&2
    echo "  [base-url]  Optional existing server URL (e.g. http://127.0.0.1:8917)." >&2
    exit 2
}

if [ "$#" -lt 1 ]; then
    usage
fi

OUT_DIR="$1"
BASE_URL_ARG="${2:-}"
mkdir -p "$OUT_DIR"

find_chrome() {
    if [ -n "${CHROME_BIN:-}" ] && [ -x "$CHROME_BIN" ]; then
        echo "$CHROME_BIN"
        return 0
    fi
    for candidate in \
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" \
        "/Applications/Chromium.app/Contents/MacOS/Chromium" \
        "/Applications/Brave Browser.app/Contents/MacOS/Brave Browser" \
        "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge" \
        "$HOME/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" \
        google-chrome \
        google-chrome-stable \
        chromium \
        chromium-browser; do
        if [ -x "$candidate" ]; then
            echo "$candidate"
            return 0
        elif command -v "$candidate" >/dev/null 2>&1; then
            command -v "$candidate"
            return 0
        fi
    done
    return 1
}

CHROME_BIN="$(find_chrome || true)"
if [ -z "$CHROME_BIN" ]; then
    echo "ERROR: Chrome or Chromium executable not found." >&2
    echo "Set CHROME_BIN=/path/to/chrome or install Google Chrome/Chromium." >&2
    exit 1
fi

locate_binary() {
    local target_dir=""
    if [ -n "${CARGO_TARGET_DIR:-}" ]; then
        target_dir="$CARGO_TARGET_DIR"
    elif command -v cargo >/dev/null 2>&1; then
        target_dir="$(cargo metadata --format-version 1 --no-deps 2>/dev/null | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p' || true)"
    fi
    if [ -z "$target_dir" ]; then
        target_dir="$REPO_ROOT/target"
    fi
    echo "$target_dir/release/dimagine"
}

TEMP_DIR=""
SERVER_PID=""

cleanup() {
    local exit_code=$?
    if [ -n "$SERVER_PID" ]; then
        kill "$SERVER_PID" 2>/dev/null || true
        wait "$SERVER_PID" 2>/dev/null || true
        SERVER_PID=""
    fi
    if [ -n "$TEMP_DIR" ] && [ -d "$TEMP_DIR" ]; then
        rm -rf "$TEMP_DIR"
        TEMP_DIR=""
    fi
    exit "$exit_code"
}
trap cleanup EXIT INT TERM HUP

BASE_URL=""
SAMPLE_IMAGE_PATH="refs/ui/alpine-glow-001.png"

if [ -n "$BASE_URL_ARG" ]; then
    BASE_URL="${BASE_URL_ARG%/}"
else
    echo "==> Building release binary: cargo build --release -p dimagine"
    (cd "$REPO_ROOT" && cargo build --release -p dimagine)

    BINARY="$(locate_binary)"
    if [ ! -x "$BINARY" ]; then
        echo "ERROR: Compiled dimagine binary not found at $BINARY" >&2
        exit 1
    fi

    echo "==> Checking for '--auth none' support in dimagine serve..."
    # The --auth none flag is being added in parallel; if lacking, exit 3.
    if ! "$BINARY" serve --help 2>&1 | grep -q -- '--auth'; then
        echo "dimagine binary lacks '--auth none' support (being added in parallel)." >&2
        exit 3
    fi

    TEMP_DIR="$(mktemp -d -t dimagine-demo-XXXXXX)"
    echo "==> Generating demo library at $TEMP_DIR..."
    python3 "$SCRIPT_DIR/demo-library.py" "$TEMP_DIR"

    echo "==> Running dimagine scan..."
    "$BINARY" scan --library "$TEMP_DIR"

    # Pick sample image path from the generated library
    first_img="$(find "$TEMP_DIR/refs/ui" -name "*.png" 2>/dev/null | head -n 1 || true)"
    if [ -n "$first_img" ]; then
        SAMPLE_IMAGE_PATH="${first_img#"$TEMP_DIR"/}"
    fi

    # Find free port on 127.0.0.1
    PORT="$(python3 -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1]); s.close()')"
    echo "==> Starting dimagine serve --auth none on 127.0.0.1:$PORT..."
    "$BINARY" serve --auth none --bind 127.0.0.1 --port "$PORT" --library "$TEMP_DIR" &
    SERVER_PID=$!

    # Wait for server readiness
    echo "==> Waiting for server readiness..."
    ready=0
    for _ in $(seq 1 40); do
        if ! kill -0 "$SERVER_PID" 2>/dev/null; then
            echo "ERROR: Server process exited unexpectedly." >&2
            exit 1
        fi
        if curl -s -f "http://127.0.0.1:$PORT/" >/dev/null 2>&1 || curl -s "http://127.0.0.1:$PORT/login" >/dev/null 2>&1; then
            ready=1
            break
        fi
        sleep 0.25
    done

    if [ "$ready" -ne 1 ]; then
        echo "ERROR: Timed out waiting for dimagine serve to become ready." >&2
        exit 1
    fi

    BASE_URL="http://127.0.0.1:$PORT"
fi

echo "==> Shooting screenshots against $BASE_URL using $CHROME_BIN"

# Resolutions: WxH
RESOLUTIONS=("390x844" "834x1194" "1440x900")

# Themes: name and Chrome flags
# dark: --force-dark-mode + --blink-settings=preferredColorScheme=0
# light: --blink-settings=preferredColorScheme=1
THEMES=("dark" "light")

# Paths and slugs:
# / , /?in=<a nested folder> , /folders , /collections , /search , one /image/<path> , /login , /setup
declare -a ROUTES=(
    "/|library"
    "/?in=refs/ui|folder-in"
    "/folders|folders"
    "/collections|collections"
    "/search|search"
    "/image/$SAMPLE_IMAGE_PATH|image"
    "/login|login"
    "/setup|setup"
)

total_shots=0
for res in "${RESOLUTIONS[@]}"; do
    width="${res%x*}"
    height="${res#*x}"

    for theme in "${THEMES[@]}"; do
        theme_flags=()
        if [ "$theme" = "dark" ]; then
            theme_flags=("--force-dark-mode" "--blink-settings=preferredColorScheme=0")
        else
            theme_flags=("--blink-settings=preferredColorScheme=1")
        fi

        for route_spec in "${ROUTES[@]}"; do
            path="${route_spec%|*}"
            slug="${route_spec#*|}"
            out_file="$OUT_DIR/${width}-${theme}-${slug}.png"
            target_url="$BASE_URL$path"

            echo "  [$width | $theme] $path -> ${width}-${theme}-${slug}.png"
            "$CHROME_BIN" \
                --headless=new \
                --screenshot="$out_file" \
                --window-size="$width,$height" \
                --hide-scrollbars \
                --disable-gpu \
                "${theme_flags[@]}" \
                "$target_url" >/dev/null 2>&1 || true

            total_shots=$((total_shots + 1))
        done
    done
done

echo "==> Completed $total_shots screenshots in $OUT_DIR"
