#!/usr/bin/env bash
# scripts/dev/screenshots.sh <out-dir> [base-url]
#
# Capture headless Chrome screenshots across viewports, themes, and routes, and
# assert that no page overflows horizontally at the phone width.
#
# The capture goes through scripts/dev/capture.mjs, which drives Chrome over
# the DevTools Protocol and calls Emulation.setDeviceMetricsOverride. Plain
# `--window-size=390,844` does not work: headless Chrome lays a window smaller
# than 500px out at 500 and clips the screenshot to 390, so the picture is a
# cropped desktop layout, not a phone layout.
#
# If [base-url] is provided:
#   Skips build and server startup; shoots the library and auth routes against
#   that base URL directly (the auth routes may not exist there).
# If [base-url] is omitted:
#   1. Builds the release binary (cargo build --release -p dimagine).
#   2. Checks if binary supports '--auth none' (exits 3 if missing).
#   3. Generates the demo library in a temp dir.
#   4. Runs dimagine scan on the library.
#   5. Starts dimagine serve --auth none on 127.0.0.1:<free port> for the
#      library routes.
#   6. Starts a second dimagine serve --auth account with an empty --data-dir:
#      /setup is shot there, then an account is created with
#      `dimagine user create --password-stdin`, and /login is shot there.
#   7. Captures screenshots to <out-dir> and checks the 390px layout.
#   8. Cleans up both servers and temp dirs on exit.

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

# capture.mjs needs Node 22+ for the built-in global WebSocket and fetch.
if ! command -v node >/dev/null 2>&1; then
    echo "ERROR: node is required to drive Chrome over the DevTools Protocol." >&2
    exit 1
fi
NODE_MAJOR="$(node -p 'process.versions.node.split(".")[0]' 2>/dev/null || echo 0)"
if [ "${NODE_MAJOR:-0}" -lt 22 ]; then
    echo "ERROR: node 22 or newer is required (found $(node --version 2>/dev/null || echo none))." >&2
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
DATA_DIR=""
SERVER_PID=""
AUTH_SERVER_PID=""
CAPTURE_PID=""
JOBS_FILE=""

cleanup() {
    local exit_code=$?
    # capture.mjs owns the Chrome process: stop it first so its
    # own signal handling tears Chrome and its profile down.
    if [ -n "$CAPTURE_PID" ]; then
        kill "$CAPTURE_PID" 2>/dev/null || true
        wait "$CAPTURE_PID" 2>/dev/null || true
        CAPTURE_PID=""
    fi
    if [ -n "$JOBS_FILE" ]; then
        rm -f "$JOBS_FILE"
        JOBS_FILE=""
    fi
    if [ -n "$SERVER_PID" ]; then
        kill "$SERVER_PID" 2>/dev/null || true
        wait "$SERVER_PID" 2>/dev/null || true
        SERVER_PID=""
    fi
    if [ -n "$AUTH_SERVER_PID" ]; then
        kill "$AUTH_SERVER_PID" 2>/dev/null || true
        wait "$AUTH_SERVER_PID" 2>/dev/null || true
        AUTH_SERVER_PID=""
    fi
    if [ -n "$TEMP_DIR" ] && [ -d "$TEMP_DIR" ]; then
        rm -rf "$TEMP_DIR"
        TEMP_DIR=""
    fi
    if [ -n "$DATA_DIR" ] && [ -d "$DATA_DIR" ]; then
        rm -rf "$DATA_DIR"
        DATA_DIR=""
    fi
    exit "$exit_code"
}
trap cleanup EXIT INT TERM HUP

BASE_URL=""
SAMPLE_IMAGE_PATH="refs/ui/alpine-glow-001.png"
EXTERNAL=0
BINARY=""

if [ -n "$BASE_URL_ARG" ]; then
    EXTERNAL=1
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

    # Find free ports on 127.0.0.1
    PORT="$(python3 -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1]); s.close()')"
    echo "==> Starting dimagine serve --auth none on 127.0.0.1:$PORT..."
    "$BINARY" serve --auth none --bind 127.0.0.1 --port "$PORT" --library "$TEMP_DIR" \
        >"$TEMP_DIR/library-server.log" 2>&1 &
    SERVER_PID=$!

    # A second server in account mode with an empty state directory: it opens
    # on /setup until an owner exists, and on /login afterwards.
    DATA_DIR="$(mktemp -d -t dimagine-accounts-XXXXXX)"
    AUTH_PORT="$(python3 -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1]); s.close()')"
    echo "==> Starting dimagine serve --auth account on 127.0.0.1:$AUTH_PORT..."
    "$BINARY" serve --auth account --bind 127.0.0.1 --port "$AUTH_PORT" \
        --library "$TEMP_DIR" --data-dir "$DATA_DIR" \
        >"$DATA_DIR/account-server.log" 2>&1 &
    AUTH_SERVER_PID=$!

    # Wait for both servers
    echo "==> Waiting for server readiness..."
    for pair in "$SERVER_PID:$PORT:127.0.0.1" "$AUTH_SERVER_PID:$AUTH_PORT:127.0.0.1"; do
        pid="${pair%%:*}"
        rest="${pair#*:}"
        check_port="${rest%%:*}"
        ready=0
        for _ in $(seq 1 60); do
            if ! kill -0 "$pid" 2>/dev/null; then
                echo "ERROR: Server process $pid exited unexpectedly." >&2
                exit 1
            fi
            if curl -s -f "http://127.0.0.1:$check_port/setup" >/dev/null 2>&1 \
                || curl -s "http://127.0.0.1:$check_port/login" >/dev/null 2>&1 \
                || curl -s "http://127.0.0.1:$check_port/" >/dev/null 2>&1; then
                ready=1
                break
            fi
            sleep 0.25
        done
        if [ "$ready" -ne 1 ]; then
            echo "ERROR: Timed out waiting for dimagine serve on port $check_port." >&2
            exit 1
        fi
    done

    BASE_URL="http://127.0.0.1:$PORT"
    AUTH_BASE_URL="http://127.0.0.1:$AUTH_PORT"
fi

echo "==> Shooting screenshots against ${BASE_URL} using $CHROME_BIN"

# Resolutions: WxH
RESOLUTIONS=("390x844" "834x1194" "1440x900")

# Themes: name
THEMES=("dark" "light")

# Routes against the no-login library server, and against the account server.
declare -a LIBRARY_ROUTES=(
    "/|library"
    "/?in=refs/ui|folder-in"
    "/folders|folders"
    "/collections|collections"
    "/search|search"
    "/image/$(python3 -c 'import sys, urllib.parse; print(urllib.parse.quote(sys.argv[1]))' "$SAMPLE_IMAGE_PATH")|image"
)
declare -a AUTH_ROUTES=(
    "/setup|setup"
    "/login|login"
)

# The X's must end the template: BSD mktemp (macOS) only
# randomizes trailing X's, so a ".json" suffix would make every
# run reuse one literal filename and collide.
JOBS_FILE="$(mktemp "${TMPDIR:-/tmp}/dimagine-jobs-XXXXXX")"
JOBS_FIRST=1

begin_jobs() {
    printf '{"chrome":"%s","jobs":[' "$CHROME_BIN" > "$JOBS_FILE"
    JOBS_FIRST=1
}

add_job() {
    # url width height theme out check
    local url="$1" width="$2" height="$3" theme="$4" out="$5" check="$6"
    if [ "$JOBS_FIRST" = 1 ]; then
        JOBS_FIRST=0
    else
        printf ',' >> "$JOBS_FILE"
    fi
    printf '{"url":"%s","width":%s,"height":%s,"theme":"%s"' \
        "$url" "$width" "$height" "$theme" >> "$JOBS_FILE"
    if [ -n "$out" ]; then
        printf ',"out":"%s"' "$out" >> "$JOBS_FILE"
    fi
    if [ "$check" = "true" ]; then
        printf ',"check":true' >> "$JOBS_FILE"
    fi
    printf '}' >> "$JOBS_FILE"
}

end_jobs() {
    printf ']}' >> "$JOBS_FILE"
}

# Shoot one base URL across every route in the remaining arguments.
shoot_routes() {
    local base_url="$1"
    shift
    local -a specs=("$@")
    begin_jobs
    for res in "${RESOLUTIONS[@]}"; do
        width="${res%x*}"
        height="${res#*x}"
        for theme in "${THEMES[@]}"; do
            for route_spec in "${specs[@]}"; do
                path="${route_spec%|*}"
                slug="${route_spec#*|}"
                out_file="$OUT_DIR/${width}-${theme}-${slug}.png"
                check="false"
                if [ "$width" = "390" ]; then
                    check="true"
                fi
                echo "  [$width | $theme] $path -> ${width}-${theme}-${slug}.png"
                add_job "${base_url}${path}" "$width" "$height" "$theme" "$out_file" "$check"
            done
        done
    done
    end_jobs
    # Record capture.mjs's exact PID so the cleanup trap can stop
    # it on any exit path; capture.mjs then kills its own Chrome.
    node "$SCRIPT_DIR/capture.mjs" "$JOBS_FILE" &
    CAPTURE_PID=$!
    local capture_status=0
    wait "$CAPTURE_PID" || capture_status=$?
    CAPTURE_PID=""
    return "$capture_status"
}

shoot_routes "$BASE_URL" "${LIBRARY_ROUTES[@]}"

if [ "$EXTERNAL" = 1 ]; then
    shoot_routes "$BASE_URL" "${AUTH_ROUTES[@]}"
else
    # /setup exists only until the owner account is created.
    shoot_routes "$AUTH_BASE_URL" "/setup|setup"
    echo "==> Creating the owner account for /login..."
    printf '%s\n' "dimagine-demo-password" | \
        "$BINARY" user create --email demo@example.com --password-stdin --data-dir "$DATA_DIR" >/dev/null
    # The store now has an owner, so the same server serves the login form.
    shoot_routes "$AUTH_BASE_URL" "/login|login"
fi

rm -f "$JOBS_FILE"
echo "==> Done: screenshots in $OUT_DIR"
