#!/bin/sh
# Install a precompiled Bree release. No sudo or shell-profile changes.
set -eu

usage() {
    cat <<'EOF'
Usage: sh install.sh [--version VERSION|latest] [--bin-dir ABSOLUTE_PATH]

Defaults: latest release, $HOME/.local/bin
Environment: BREE_RELEASE_BASE_URL (HTTPS .../releases), BREE_VERSION_URL,
             BREE_INSTALL_DIR
Only macOS Apple Silicon is currently packaged. No Rust or Homebrew required.
EOF
}

fail() { printf 'Bree installation failed: %s\n' "$*" >&2; exit 1; }

validate_version() {
    case "$1" in *'
'*) fail 'invalid version' ;; esac
    printf '%s\n' "$1" | /usr/bin/grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$' || fail 'invalid version'
}

download() {
    curl -q --fail --silent --show-error --location --proto '=https' --proto-redir '=https' \
        --tlsv1.2 --connect-timeout 15 --max-time 120 --retry 2 \
        --output "$2" "$1" || fail 'download unavailable; check the release URL and repository access'
}

main() {
    version=latest
    bin_dir=${BREE_INSTALL_DIR:-${HOME:?HOME must be set}/.local/bin}
    release_base=${BREE_RELEASE_BASE_URL:-https://github.com/yuyongyan29-dev/Bree/releases}
    version_url=${BREE_VERSION_URL:-}
    while [ "$#" -gt 0 ]; do
        case "$1" in
            --help|-h) usage; exit 0 ;;
            --version|--bin-dir)
                [ "$#" -ge 2 ] || fail "$1 requires a value"
                case "$1" in
                    --version) version=$2 ;;
                    --bin-dir) bin_dir=$2 ;;
                esac
                shift 2 ;;
            *) fail "unknown argument: $1" ;;
        esac
    done
    case "$bin_dir" in /*) ;; *) fail 'install directory must be absolute' ;; esac
    case "$bin_dir" in *'
'*) fail 'install directory must not contain newlines' ;; esac
    case "$release_base" in https://?*) ;; *) fail 'release URL must use HTTPS' ;; esac
    case "$release_base" in *[!A-Za-z0-9_./:~-]*) fail 'release URL contains unsupported characters' ;; esac
    release_base=${release_base%/}
    if [ "$version" = latest ]; then
        if [ -z "$version_url" ]; then
            if [ "$release_base" = https://github.com/yuyongyan29-dev/Bree/releases ]; then
                version_url=https://raw.githubusercontent.com/yuyongyan29-dev/Bree/main/distribution/latest-version.txt
            else
                version_url=$release_base/latest/download/bree-version.txt
            fi
        fi
        case "$version_url" in https://?*) ;; *) fail 'version URL must use HTTPS' ;; esac
        case "$version_url" in *[!A-Za-z0-9_./:~-]*) fail 'version URL contains unsupported characters' ;; esac
    fi
    if [ "$version" != latest ]; then
        version=${version#v}
        validate_version "$version"
    fi
    [ "$(uname -s)" = Darwin ] || fail 'this release only supports macOS'
    case "$(uname -m)" in arm64|aarch64) ;; *) fail 'this release requires an Apple Silicon terminal (arm64)' ;; esac
    for tool in curl shasum mktemp chmod mv awk wc; do
        command -v "$tool" >/dev/null 2>&1 || fail "missing macOS tool: $tool"
    done
    target=$bin_dir/bree
    [ ! -L "$target" ] || fail 'existing bree is a symlink; use another --bin-dir or its package manager'
    if [ -e "$target" ] && [ ! -f "$target" ]; then fail 'existing bree is not a regular file'; fi
    mkdir -p "$bin_dir" || fail "cannot create install directory: $bin_dir"
    [ -w "$bin_dir" ] || fail "install directory is not writable: $bin_dir"
    install_tmp=$(mktemp -d "$bin_dir/.bree-install.XXXXXX") || fail 'cannot create staging directory'
    trap 'rm -rf "$install_tmp"' 0
    trap 'exit 130' INT
    trap 'exit 143' TERM HUP
    asset=bree-aarch64-apple-darwin
    if [ "$version" = latest ]; then
        download "$version_url" "$install_tmp/bree-version.txt"
        [ "$(wc -c < "$install_tmp/bree-version.txt")" -le 64 ] || fail 'invalid release version file'
        LC_ALL=C tr -d '\n.0-9A-Za-z-' < "$install_tmp/bree-version.txt" > "$install_tmp/invalid-version-bytes"
        [ ! -s "$install_tmp/invalid-version-bytes" ] || fail 'invalid release version file'
        version=$(LC_ALL=C awk 'NR == 1 && $0 ~ /^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$/ { version = $0; valid = 1; next } { valid = 0 } END { if (valid) print version; else exit 1 }' "$install_tmp/bree-version.txt") || fail 'invalid release version file'
        validate_version "$version"
    fi
    download_base=$release_base/download/v$version
    printf 'Downloading Bree (%s)…\n' "$version"
    for filename in "$asset" "$asset.sha256"; do
        download "$download_base/$filename" "$install_tmp/$filename"
    done
    [ "$(wc -c < "$install_tmp/$asset.sha256")" -le 256 ] || fail 'invalid checksum file'
    expected=$(awk -v name="$asset" 'NR == 1 && NF == 2 && length($1) == 64 && $1 !~ /[^0-9a-fA-F]/ && $2 == name { hash = tolower($1); valid = 1; next } { valid = 0 } END { if (valid) print hash; else exit 1 }' "$install_tmp/$asset.sha256") || fail 'invalid checksum file'
    checksum_result=$(shasum -a 256 "$install_tmp/$asset") || fail 'cannot calculate checksum'
    actual=${checksum_result%% *}
    [ "$actual" = "$expected" ] || fail 'SHA-256 mismatch; existing installation was preserved'
    chmod 755 "$install_tmp/$asset"
    downloaded_version=$("$install_tmp/$asset" --version) || fail 'downloaded Bree cannot run on this Mac'
    case "$downloaded_version" in *'
'*) fail 'unexpected Bree version output' ;; esac
    printf '%s\n' "$downloaded_version" | /usr/bin/grep -Eq '^bree [0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$' || fail 'unexpected Bree version output'
    if [ "$downloaded_version" != "bree $version" ]; then fail 'downloaded version differs from requested version'; fi
    # Recheck before atomic replacement. Staging and destination share a filesystem.
    [ ! -L "$target" ] || fail 'destination became a symlink'
    if [ -e "$target" ] && [ ! -f "$target" ]; then fail 'destination is not a regular file'; fi
    mv -f "$install_tmp/$asset" "$target" || fail 'cannot replace installation'
    printf 'Installed %s → %s\n' "$downloaded_version" "$target"
    case ":$PATH:" in
        *":$bin_dir:"*) printf 'Run: bree\n' ;;
        *)
            printf 'Add this directory to PATH, then run bree:\n'
            escaped_bin=$(printf '%s' "$bin_dir" | sed "s/'/'\\\\''/g")
            printf "export PATH='%s':\"\$PATH\"\n" "$escaped_bin"
            printf 'Keep that line in your shell profile for future terminals.\n' ;;
    esac
}

main "$@"
