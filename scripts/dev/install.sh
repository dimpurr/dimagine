#!/bin/sh
set -eu

usage() {
    cat <<'USAGE'
Usage: install.sh [--uninstall]

Install the latest dimagine release into ~/.local/bin, or into
$DIMAGINE_INSTALL_DIR when set. Set DIMAGINE_VERSION to select a tag.
USAGE
}

if [ "$#" -gt 1 ]; then
    usage >&2
    exit 2
fi

uninstall=0
if [ "$#" -eq 1 ]; then
    case "$1" in
        --uninstall) uninstall=1 ;;
        -h|--help) usage; exit 0 ;;
        *) usage >&2; exit 2 ;;
    esac
fi

if [ -n "${DIMAGINE_INSTALL_DIR:-}" ]; then
    install_dir=$DIMAGINE_INSTALL_DIR
else
    install_dir=${HOME:?HOME must be set}/.local/bin
fi

if [ "$uninstall" -eq 1 ]; then
    rm -f "$install_dir/dimagine" "$install_dir/dimg"
    printf 'Uninstalled dimagine from %s\n' "$install_dir"
    exit 0
fi

version=${DIMAGINE_VERSION:-latest}
os_name=$(uname -s)
machine=$(uname -m)
case "$os_name/$machine" in
    Darwin/arm64|Darwin/aarch64) platform=darwin-arm64 ;;
    Darwin/x86_64|Darwin/amd64) platform=darwin-x86_64 ;;
    Linux/x86_64|Linux/amd64) platform=linux-x86_64 ;;
    Linux/aarch64|Linux/arm64) platform=linux-arm64 ;;
    *) printf 'Unsupported platform: %s/%s\n' "$os_name" "$machine" >&2; exit 1 ;;
esac

archive_name="dimagine-${platform}.tar.gz"
if [ -n "${DIMAGINE_RELEASE_BASE_URL:-}" ]; then
    release_base=${DIMAGINE_RELEASE_BASE_URL%/}
else
    if [ "$version" = latest ]; then
        release_base=https://github.com/dimpurr/dimagine/releases/latest/download
    else
        release_base="https://github.com/dimpurr/dimagine/releases/download/$version"
    fi
fi

command -v curl >/dev/null 2>&1 || { printf 'curl is required\n' >&2; exit 1; }
if command -v shasum >/dev/null 2>&1; then
    sha256() { shasum -a 256 "$1" | awk '{print $1}'; }
elif command -v sha256sum >/dev/null 2>&1; then
    sha256() { sha256sum "$1" | awk '{print $1}'; }
else
    printf 'shasum or sha256sum is required\n' >&2
    exit 1
fi

umask 077
tmp_dir=$(mktemp -d "${TMPDIR:-/tmp}/dimagine-install.XXXXXX")
trap 'rm -rf "$tmp_dir"' EXIT HUP INT TERM
archive_path=$tmp_dir/$archive_name
sums_path=$tmp_dir/SHA256SUMS
curl -fsSL "$release_base/$archive_name" -o "$archive_path"
curl -fsSL "$release_base/SHA256SUMS" -o "$sums_path"
expected=$(awk -v file="$archive_name" '$2 == file { value = $1; count++ } END { if (count != 1) exit 1; print value }' "$sums_path") || {
    printf 'No unique SHA256SUMS entry for %s\n' "$archive_name" >&2
    exit 1
}
actual=$(sha256 "$archive_path")
if [ "$actual" != "$expected" ]; then
    printf 'SHA256 mismatch for %s\n' "$archive_name" >&2
    exit 1
fi

mkdir -p "$tmp_dir/unpacked" "$install_dir"
tar -xzf "$archive_path" -C "$tmp_dir/unpacked" dimagine
if [ ! -f "$tmp_dir/unpacked/dimagine" ] || [ ! -x "$tmp_dir/unpacked/dimagine" ]; then
    printf 'Archive does not contain an executable dimagine binary\n' >&2
    exit 1
fi
cp "$tmp_dir/unpacked/dimagine" "$install_dir/.dimagine.$$"
chmod 755 "$install_dir/.dimagine.$$"
mv -f "$install_dir/.dimagine.$$" "$install_dir/dimagine"
ln -sfn dimagine "$install_dir/dimg"
printf 'Installed dimagine (%s) into %s\n' "$platform" "$install_dir"
