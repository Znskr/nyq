#!/bin/sh
set -eu

REPO="sharqlabs/nyq"

err() {
    printf 'install.sh: error: %s\n' "$*" >&2
    exit 1
}

have() {
    command -v "$1" >/dev/null 2>&1
}

os=$(uname -s)
case "$os" in
    Linux) os_part=unknown-linux-gnu ;;
    Darwin) os_part=apple-darwin ;;
    *) err "unsupported operating system '$os': this installer ships nyq binaries for Linux and macOS only" ;;
esac

arch=$(uname -m)
case "$arch" in
    x86_64|amd64) arch_part=x86_64 ;;
    aarch64|arm64) arch_part=aarch64 ;;
    *) err "unsupported CPU architecture '$arch': this installer ships x86_64 and aarch64 binaries only" ;;
esac

target="$arch_part-$os_part"
archive="nyq-$target.tar.gz"

if [ -n "${NYQ_VERSION:-}" ]; then
    tag="v${NYQ_VERSION#v}"
    api_url="https://api.github.com/repos/$REPO/releases/tags/$tag"
else
    tag="latest"
    api_url="https://api.github.com/repos/$REPO/releases/latest"
fi

auth=""
if [ -n "${GITHUB_TOKEN:-}" ]; then
    auth="Authorization: Bearer $GITHUB_TOKEN"
fi

if have curl; then
    fetcher=curl
elif have wget; then
    fetcher=wget
else
    err "neither curl nor wget is installed: one of them is needed to download nyq"
fi

download() {
    _url=$1
    if [ "$fetcher" = curl ]; then
        if [ -n "$auth" ]; then
            curl -fsSL -H "$auth" "$_url"
        else
            curl -fsSL "$_url"
        fi
    else
        if [ -n "$auth" ]; then
            wget -qO- --header="$auth" "$_url"
        else
            wget -qO- "$_url"
        fi
    fi
}

release_json=$(download "$api_url") || err "failed to fetch release information for '$tag' from $REPO
  $api_url
Check that the release exists. If the repository is private, set GITHUB_TOKEN
to a token with read access to it."

asset_url() {
    if [ -n "$auth" ]; then
        # Private repositories: browser download links ignore tokens, so use the
        # API address of the asset (downloaded with Accept: application/octet-stream).
        printf '%s\n' "$release_json" | tr ',{}' '\n\n\n' | awk -v want="$1" '
            /"url": *"https:\/\/api\.github\.com\/repos\/.*\/releases\/assets\/[0-9]+"/ {
                u = $0; sub(/^.*"url": *"/, "", u); sub(/".*$/, "", u)
            }
            /"name": *"/ {
                n = $0; sub(/^.*"name": *"/, "", n); sub(/".*$/, "", n)
                if (n == want && u != "") { print u; exit }
            }'
    else
        printf '%s\n' "$release_json" |
            grep -oE '"browser_download_url": *"[^"]*"' |
            sed -e 's/^"browser_download_url": *"//' -e 's/"$//' |
            grep "/$1\$" |
            head -n 1
    fi
}

download_asset() {
    if [ -z "$auth" ]; then
        download "$1"
    elif [ "$fetcher" = curl ]; then
        curl -fsSL -H "$auth" -H "Accept: application/octet-stream" "$1"
    else
        wget -qO- --header="$auth" --header="Accept: application/octet-stream" "$1"
    fi
}

archive_url=$(asset_url "$archive")
[ -n "$archive_url" ] || err "release '$tag' of $REPO has no asset named '$archive'"

sums_url=$(asset_url SHA256SUMS)
[ -n "$sums_url" ] || err "release '$tag' of $REPO has no SHA256SUMS asset: refusing to install without checksum verification"

version=$(printf '%s\n' "$release_json" |
    grep -oE '"tag_name": *"[^"]*"' |
    sed -e 's/^"tag_name": *"//' -e 's/"$//' |
    head -n 1)
[ -n "$version" ] || version="$tag"

if have sha256sum; then
    sum_tool=sha256sum
elif have shasum; then
    sum_tool=shasum
else
    err "neither sha256sum nor shasum is installed: one of them is needed to verify the download"
fi

tmpdir=$(mktemp -d) || err "failed to create a temporary directory"
trap 'rm -rf "$tmpdir"' EXIT

printf 'Downloading nyq %s for %s...\n' "$version" "$target"

download_asset "$archive_url" > "$tmpdir/$archive" || err "failed to download $archive from $archive_url"
download_asset "$sums_url" > "$tmpdir/SHA256SUMS" || err "failed to download SHA256SUMS from $sums_url"

expected=$(awk -v f="$archive" '$2 == f { print $1; exit }' "$tmpdir/SHA256SUMS")
[ -n "$expected" ] || err "SHA256SUMS of release '$tag' has no entry for $archive"

if [ "$sum_tool" = sha256sum ]; then
    actual=$(sha256sum "$tmpdir/$archive" | awk '{ print $1 }')
else
    actual=$(shasum -a 256 "$tmpdir/$archive" | awk '{ print $1 }')
fi

[ "$actual" = "$expected" ] || err "checksum mismatch for $archive
  expected: $expected
  actual:   $actual
The download may be incomplete or tampered with; not installing it."

tar -xzf "$tmpdir/$archive" -C "$tmpdir" || err "failed to extract $archive"
[ -f "$tmpdir/nyq" ] || err "$archive does not contain a 'nyq' binary"

if [ -z "${HOME:-}" ] && [ -z "${NYQ_INSTALL_DIR:-}" ]; then
    err "neither HOME nor NYQ_INSTALL_DIR is set: cannot tell where to install nyq"
fi
dest_dir=${NYQ_INSTALL_DIR:-$HOME/.local/bin}

if ! mkdir -p "$dest_dir" 2>/dev/null; then
    err "cannot create install directory '$dest_dir': set NYQ_INSTALL_DIR to a writable location"
fi

rm -f "$dest_dir/nyq"
if ! cp "$tmpdir/nyq" "$dest_dir/nyq" 2>/dev/null; then
    err "cannot write to '$dest_dir/nyq': set NYQ_INSTALL_DIR to a writable location"
fi
chmod 0755 "$dest_dir/nyq"

printf 'Installed nyq %s to %s\n' "$version" "$dest_dir/nyq"

case ":$PATH:" in
    *":$dest_dir:"*) ;;
    *)
        printf 'hint: %s is not on your PATH\n' "$dest_dir"
        printf 'Add this line to your shell profile (~/.profile, ~/.bashrc, ...) and restart your shell:\n'
        printf '  export PATH="%s:$PATH"\n' "$dest_dir"
        ;;
esac
