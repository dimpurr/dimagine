#!/bin/sh
set -eu

repo_dir=$(CDPATH='' cd -- "$(dirname -- "$0")/../.." && pwd)
test_root=$(mktemp -d "$repo_dir/.test-install.XXXXXX")
cleanup() {
    rm -rf "$test_root"
}
trap cleanup EXIT HUP INT TERM

case "$(uname -s)/$(uname -m)" in
    Darwin/arm64|Darwin/aarch64) platform=darwin-arm64 ;;
    Darwin/x86_64|Darwin/amd64) platform=darwin-x86_64 ;;
    Linux/x86_64|Linux/amd64) platform=linux-x86_64 ;;
    Linux/aarch64|Linux/arm64) platform=linux-arm64 ;;
    *) printf 'Unsupported test platform: %s/%s\n' "$(uname -s)" "$(uname -m)" >&2; exit 1 ;;
esac

release_dir=$test_root/release
install_dir=$test_root/bin
mkdir -p "$release_dir" "$test_root/archive"
cat > "$test_root/archive/dimagine" <<'BINARY'
#!/bin/sh
printf 'fake-dimagine\n'
BINARY
chmod 755 "$test_root/archive/dimagine"
archive_name="dimagine-${platform}.tar.gz"
tar -C "$test_root/archive" -czf "$release_dir/$archive_name" dimagine
if command -v shasum >/dev/null 2>&1; then
    (cd "$release_dir" && shasum -a 256 "$archive_name" > SHA256SUMS)
elif command -v sha256sum >/dev/null 2>&1; then
    (cd "$release_dir" && sha256sum "$archive_name" > SHA256SUMS)
else
    printf 'shasum or sha256sum is required\n' >&2
    exit 1
fi

DIMAGINE_RELEASE_BASE_URL="file://$release_dir" \
DIMAGINE_INSTALL_DIR="$install_dir" \
    "$repo_dir/scripts/dev/install.sh"
[ -x "$install_dir/dimagine" ]
[ -L "$install_dir/dimg" ]
[ "$("$install_dir/dimg")" = fake-dimagine ]

DIMAGINE_RELEASE_BASE_URL="file://$release_dir" \
DIMAGINE_INSTALL_DIR="$install_dir" \
    "$repo_dir/scripts/dev/install.sh"
[ "$("$install_dir/dimagine")" = fake-dimagine ]

cp "$install_dir/dimagine" "$test_root/installed-before-corruption"
printf 'corrupt archive\n' > "$release_dir/$archive_name"
if DIMAGINE_RELEASE_BASE_URL="file://$release_dir" \
    DIMAGINE_INSTALL_DIR="$install_dir" \
    "$repo_dir/scripts/dev/install.sh"; then
    printf 'Installer accepted an archive with a mismatched checksum\n' >&2
    exit 1
fi
cmp "$test_root/installed-before-corruption" "$install_dir/dimagine"

DIMAGINE_INSTALL_DIR="$install_dir" "$repo_dir/scripts/dev/install.sh" --uninstall
[ ! -e "$install_dir/dimagine" ]
[ ! -L "$install_dir/dimg" ]
printf 'install test passed (%s)\n' "$platform"
