#!/bin/bash

set -e

TARGET="$1"

if [[ -n "$TARGET" ]] && [[ ! "$TARGET" =~ ^(latest|[0-9]+\.[0-9]+\.[0-9]+(-[^[:space:]]+)?)$ ]]; then
    echo "Usage: $0 [latest|VERSION]" >&2
    exit 1
fi

REPO="supermemoryai/smfs"
RELEASES_URL="https://github.com/$REPO/releases"
DOWNLOAD_DIR="$HOME/.supermemory/downloads"

DOWNLOADER=""
if command -v curl >/dev/null 2>&1; then
    DOWNLOADER="curl"
elif command -v wget >/dev/null 2>&1; then
    DOWNLOADER="wget"
else
    echo "Either curl or wget is required but neither is installed" >&2
    exit 1
fi

HAS_JQ=false
if command -v jq >/dev/null 2>&1; then
    HAS_JQ=true
fi

download_file() {
    local url="$1"
    local output="$2"

    if [ "$DOWNLOADER" = "curl" ]; then
        if [ -n "$output" ]; then
            curl -fsSL -o "$output" "$url"
        else
            curl -fsSL "$url"
        fi
    else
        if [ -n "$output" ]; then
            wget -q -O "$output" "$url"
        else
            wget -q -O - "$url"
        fi
    fi
}

resolve_latest_version() {
    if [ "$DOWNLOADER" = "curl" ]; then
        curl -fsI "$RELEASES_URL/latest" \
            | awk -F'/' 'tolower($1) ~ /^location:/ { sub(/\r$/, "", $NF); print $NF }' \
            | sed -n 's/^mount-v//p'
    else
        wget -q --method=HEAD --server-response "$RELEASES_URL/latest" 2>&1 \
            | awk -F'/' '/Location:/ { sub(/\r$/, "", $NF); print $NF }' \
            | sed -n 's/^mount-v//p'
    fi
}

get_checksum_from_manifest() {
    local json="$1"
    local platform="$2"
    json=$(echo "$json" | tr -d '\n\r\t' | sed 's/ \+/ /g')
    if [[ $json =~ \"$platform\"[^}]*\"checksum\"[[:space:]]*:[[:space:]]*\"([a-f0-9]{64})\" ]]; then
        echo "${BASH_REMATCH[1]}"
        return 0
    fi
    return 1
}

case "$(uname -s)" in
    Darwin) os="darwin" ;;
    Linux)  os="linux" ;;
    MINGW*|MSYS*|CYGWIN*) echo "Windows is not supported. See https://github.com/$REPO for platform support." >&2; exit 1 ;;
    *) echo "Unsupported operating system: $(uname -s)" >&2; exit 1 ;;
esac

case "$(uname -m)" in
    x86_64|amd64) arch="x64" ;;
    arm64|aarch64) arch="arm64" ;;
    *) echo "Unsupported architecture: $(uname -m)" >&2; exit 1 ;;
esac

if [ "$os" = "darwin" ] && [ "$arch" = "x64" ]; then
    if [ "$(sysctl -n sysctl.proc_translated 2>/dev/null)" = "1" ]; then
        arch="arm64"
    fi
fi

platform="${os}-${arch}"
mkdir -p "$DOWNLOAD_DIR"

if [ -z "$TARGET" ] || [ "$TARGET" = "latest" ]; then
    version=$(resolve_latest_version)
else
    version="$TARGET"
fi

if [ -z "$version" ]; then
    echo "Could not resolve latest version from $RELEASES_URL/latest" >&2
    exit 1
fi

asset_base="$RELEASES_URL/download/mount-v$version"
manifest_json=$(download_file "$asset_base/manifest.json")

if [ "$HAS_JQ" = true ]; then
    checksum=$(echo "$manifest_json" | jq -r ".platforms[\"$platform\"].checksum // empty")
else
    checksum=$(get_checksum_from_manifest "$manifest_json" "$platform")
fi

if [ -z "$checksum" ] || [[ ! "$checksum" =~ ^[a-f0-9]{64}$ ]]; then
    echo "Platform $platform not found in manifest for version $version" >&2
    exit 1
fi

binary_path="$DOWNLOAD_DIR/smfs-$version-$platform"
if ! download_file "$asset_base/smfs-$platform" "$binary_path"; then
    echo "Download failed" >&2
    rm -f "$binary_path"
    exit 1
fi

if [ "$os" = "darwin" ]; then
    actual=$(shasum -a 256 "$binary_path" | cut -d' ' -f1)
else
    actual=$(sha256sum "$binary_path" | cut -d' ' -f1)
fi

if [ "$actual" != "$checksum" ]; then
    echo "Checksum verification failed" >&2
    rm -f "$binary_path"
    exit 1
fi

chmod +x "$binary_path"

echo "Setting up smfs..."
"$binary_path" install ${TARGET:+"$TARGET"}

rm -f "$binary_path"

echo ""
echo "✅ Installation complete!"
echo ""
