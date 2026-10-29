#!/bin/sh
# Install a published Kae binary. No sudo, source compilation, or silent overwrite.
set -eu

die() { printf 'kae: %s\n' "$*" >&2; exit 1; }

[ "$#" -le 1 ] || die 'Usage: sh install.sh [vVERSION]'
version=${1:-latest}
case "$version" in
    latest) release=latest/download ;;
    v[0-9]*)
        case "$version" in *[!a-zA-Z0-9.+-]*) die 'Invalid version tag' ;; esac
        release=download/$version ;;
    *) die 'Version must be a tag such as v0.1.0' ;;
esac

os=$(uname -s)
arch=$(uname -m)
case "$os/$arch" in
    Darwin/arm64|Darwin/aarch64) target=aarch64-apple-darwin ;;
    Linux/x86_64|Linux/amd64) target=x86_64-unknown-linux-musl ;;
    Linux/aarch64|Linux/arm64) target=aarch64-unknown-linux-musl ;;
    *) die "Unsupported platform: $os/$arch (supported: macOS ARM64, Linux AMD64/ARM64)" ;;
esac

for tool in curl tar awk mktemp git; do
    command -v "$tool" >/dev/null 2>&1 || die "Required tool not found: $tool"
done
digest() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | awk '{print $1}'
    else
        die 'SHA-256 verification requires sha256sum or shasum'
    fi
}

install_dir=${KAE_INSTALL_DIR:-${HOME:?HOME must be set}/.local/bin}
mkdir -p "$install_dir"
install_dir=$(cd "$install_dir" && pwd -P)
destination=$install_dir/kae
[ ! -L "$destination" ] || die 'Refusing to replace a symbolic link; choose another KAE_INSTALL_DIR'
[ ! -d "$destination" ] || die 'The installation destination is a directory'
previous=
if [ -e "$destination" ]; then
    [ "${KAE_NONINTERACTIVE:-0}" != 1 ] || die 'Replacement requires an interactive confirmation; non-interactive installation will not overwrite files'
    [ -f "$destination" ] || die 'The existing destination is not a regular file'
    previous=$(digest "$destination")
    if ! printf 'Replace the installed binary at %s? [y/N] ' "$destination" >/dev/tty 2>/dev/null; then
        die 'Replacement requires an interactive confirmation. Run the saved installer in a terminal.'
    fi
    answer=
    IFS= read -r answer </dev/tty || die 'Could not read confirmation'
    case "$answer" in y|Y|yes|YES) ;; *) printf 'Installation cancelled.\n'; exit 0 ;; esac
fi

temporary=$(mktemp -d "${TMPDIR:-/tmp}/kae-install.XXXXXX")
staged=
cleanup() {
    if [ -n "$staged" ]; then rm -f -- "$staged"; fi
    rm -f -- "$temporary/archive.tar.gz" "$temporary/checksum" "$temporary/kae"
    rmdir "$temporary" 2>/dev/null || :
}
trap cleanup 0
trap 'exit 130' 2
trap 'exit 143' 1 15

asset=kae-$target.tar.gz
base=https://github.com/achainarong/kae/releases/$release
curl --fail --location --silent --show-error --proto '=https' --proto-redir '=https' --tlsv1.2 "$base/$asset" -o "$temporary/archive.tar.gz"
curl --fail --location --silent --show-error --proto '=https' --proto-redir '=https' --tlsv1.2 "$base/$asset.sha256" -o "$temporary/checksum"
expected=$(awk -v name="$asset" '$2 == name || $2 == "*" name {print $1}' "$temporary/checksum")
[ "${#expected}" -eq 64 ] || die 'Release checksum is missing, duplicated or malformed'
case "$expected" in *[!0-9a-fA-F]*) die 'Invalid SHA-256 checksum' ;; esac
[ "$(digest "$temporary/archive.tar.gz")" = "$expected" ] || die 'Checksum mismatch; nothing was installed'
[ "$(tar -tzf "$temporary/archive.tar.gz")" = kae ] || die 'Unexpected archive contents'
case "$(tar -tvzf "$temporary/archive.tar.gz")" in -*) ;; *) die 'Archive binary is not a regular file' ;; esac
tar -xzf "$temporary/archive.tar.gz" -C "$temporary" kae
[ -f "$temporary/kae" ] && [ ! -L "$temporary/kae" ] || die 'Invalid release binary'

# Stage on the destination filesystem, then atomically install. Recheck an
# approved replacement so a second installer / editor cannot silently win.
staged=$(mktemp "$install_dir/.kae.XXXXXX")
cat "$temporary/kae" > "$staged"
chmod 755 "$staged"
if [ -n "$previous" ]; then
    [ -f "$destination" ] && [ ! -L "$destination" ] || die 'Destination changed after confirmation'
    [ "$(digest "$destination")" = "$previous" ] || die 'Installed binary changed after confirmation; retry'
    mv -f -- "$staged" "$destination"
    staged=
else
    # A hard link is atomic and fails rather than overwriting a new destination.
    ln "$staged" "$destination" || die 'Destination appeared during installation; retry to confirm replacement'
fi
printf 'Installed kae (%s) to %s\n' "$target" "$destination"
case ":${PATH:-}:" in
    *":$install_dir:"*) ;;
    *) printf 'Add %s to your PATH, then run kae -C /path/to/repository\n' "$install_dir" ;;
esac
