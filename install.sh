#!/bin/sh
# Installs nyq: curl -fsSL https://nyq.zalthar.com/install.sh | sh
#
# Downloads the release archive for this machine from GitHub, verifies it
# against the release's SHA256SUMS and copies the binary to ~/.local/bin.
#   NYQ_VERSION=0.0.1     install that version instead of the newest
#   NYQ_INSTALL_DIR=DIR   install somewhere other than ~/.local/bin
set -eu

REPO="Znskr/nyq"

err() {
    printf 'install.sh: error: %s\n' "$*" >&2
    exit 1
}

have() {
    command -v "$1" >/dev/null 2>&1
}

case "$(uname -s)" in
    Linux) os_part=unknown-linux-musl ;;
    Darwin) os_part=apple-darwin ;;
    *) err "unsupported operating system '$(uname -s)': this installer ships nyq for Linux and macOS (Windows: see https://github.com/$REPO/releases)" ;;
esac

case "$(uname -m)" in
    x86_64|amd64) arch_part=x86_64 ;;
    aarch64|arm64) arch_part=aarch64 ;;
    *) err "unsupported CPU architecture '$(uname -m)': nyq ships x86_64 and aarch64 binaries" ;;
esac

target="$arch_part-$os_part"
archive="nyq-$target.tar.gz"

if [ -n "${NYQ_VERSION:-}" ]; then
    base="https://github.com/$REPO/releases/download/v${NYQ_VERSION#v}"
    wanted="version ${NYQ_VERSION#v}"
else
    base="https://github.com/$REPO/releases/latest/download"
    wanted="the latest release"
fi

if have curl; then
    download() { curl -fsSL "$1"; }
elif have wget; then
    download() { wget -qO- "$1"; }
else
    err "neither curl nor wget is installed: one of them is needed to download nyq"
fi

if have sha256sum; then
    sha256() { sha256sum "$1" | awk '{ print $1 }'; }
elif have shasum; then
    sha256() { shasum -a 256 "$1" | awk '{ print $1 }'; }
else
    err "neither sha256sum nor shasum is installed: one of them is needed to verify the download"
fi

if [ -z "${HOME:-}" ] && [ -z "${NYQ_INSTALL_DIR:-}" ]; then
    err "neither HOME nor NYQ_INSTALL_DIR is set: cannot tell where to install nyq"
fi
dest_dir=${NYQ_INSTALL_DIR:-$HOME/.local/bin}

tmpdir=$(mktemp -d) || err "failed to create a temporary directory"
trap 'rm -rf "$tmpdir"' EXIT

printf 'Downloading nyq (%s) for %s...\n' "$wanted" "$target"
download "$base/$archive" > "$tmpdir/$archive" ||
    err "failed to download $base/$archive (does $wanted exist?)"
download "$base/SHA256SUMS" > "$tmpdir/SHA256SUMS" ||
    err "failed to download SHA256SUMS: refusing to install without checksum verification"

expected=$(awk -v f="$archive" '$2 == f || $2 == "*" f { print $1; exit }' "$tmpdir/SHA256SUMS")
[ -n "$expected" ] || err "SHA256SUMS has no entry for $archive"
actual=$(sha256 "$tmpdir/$archive")
[ "$actual" = "$expected" ] || err "checksum mismatch for $archive
  expected: $expected
  actual:   $actual
The download may be incomplete or tampered with; not installing it."

tar -xzf "$tmpdir/$archive" -C "$tmpdir" || err "failed to extract $archive"
[ -f "$tmpdir/nyq" ] || err "$archive does not contain a 'nyq' binary"
version=$("$tmpdir/nyq" --version 2>/dev/null) || err "the downloaded nyq does not run on this machine"

mkdir -p "$dest_dir" 2>/dev/null ||
    err "cannot create install directory '$dest_dir': set NYQ_INSTALL_DIR to a writable location"
rm -f "$dest_dir/nyq"
cp "$tmpdir/nyq" "$dest_dir/nyq" 2>/dev/null ||
    err "cannot write to '$dest_dir/nyq': set NYQ_INSTALL_DIR to a writable location"
chmod 0755 "$dest_dir/nyq"

printf 'Installed %s to %s\n' "$version" "$dest_dir/nyq"

case ":$PATH:" in
    *":$dest_dir:"*) ;;
    *)
        printf 'hint: %s is not on your PATH\n' "$dest_dir"
        printf 'Add this line to your shell profile (~/.profile, ~/.bashrc, ...) and restart your shell:\n'
        printf '  export PATH="%s:$PATH"\n' "$dest_dir"
        ;;
esac
printf 'Run nyq to get started. Docs: https://nyq.zalthar.com/docs\n'
