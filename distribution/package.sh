#!/bin/sh
# Prepare local release assets. This script does not upload or publish them.
set -eu

usage() {
    cat <<'USAGE'
Usage: sh distribution/package.sh [--output DIR] [--release-base-url HTTPS_URL]

Builds the locked Apple Silicon macOS release and prepares:
  bree-aarch64-apple-darwin
  bree-aarch64-apple-darwin.sha256
  bree-version.txt
  install.sh
  bree.rb
  LICENSE
  THIRD-PARTY-NOTICES.txt

Default output: .artifacts/distribution/v<crate version>
Default release base: https://github.com/yuyongyan29-dev/Bree/releases
These assets are local preparation only; publication is a separate action.
Use the releases base URL of the download host to generate its cask.
Existing output files are never replaced. Requires Cargo and macOS build tools.
USAGE
}

fail() {
    printf 'bree package: %s\n' "$*" >&2
    exit 1
}

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
project_dir=$(CDPATH= cd -- "$script_dir/.." && pwd)
cargo_dir=$project_dir
manifest=$cargo_dir/Cargo.toml
[ -f "$manifest" ] || fail "Cargo.toml is missing from the project"
version=$(awk '
    /^\[package\]$/ { package = 1; next }
    /^\[/ { package = 0 }
    package && /^version[[:space:]]*=/ {
        sub(/^[^"]*"/, ""); sub(/".*$/, ""); print; exit
    }
' "$manifest")
case "$version" in
    ''|*[!A-Za-z0-9.+-]*) fail "invalid or missing crate version" ;;
esac

output_dir=$project_dir/.artifacts/distribution/v$version
release_base=https://github.com/yuyongyan29-dev/Bree/releases
while [ "$#" -gt 0 ]; do
    case "$1" in
        --output)
            [ "$#" -ge 2 ] || fail "--output requires a directory"
            [ -n "$2" ] || fail "--output requires a nonempty directory"
            output_dir=$2
            shift 2
            ;;
        --release-base-url)
            [ "$#" -ge 2 ] || fail "--release-base-url requires a URL"
            release_base=$2
            shift 2
            ;;
        -h|--help) usage; exit 0 ;;
        *) fail "unknown option: $1" ;;
    esac
done

case "$release_base" in
    https://?*) ;;
    *) fail "release base must be an HTTPS URL" ;;
esac
case "${release_base#https://}" in
    ''|/*) fail "release base is missing its host" ;;
esac
# Keep generated Ruby strings literal and reject queries/fragments on a base URL.
case "$release_base" in
    *[!A-Za-z0-9:/._~-]*) fail "unsupported character in release base URL" ;;
esac
while [ "${release_base%/}" != "$release_base" ]; do
    release_base=${release_base%/}
done
[ "$release_base" != https: ] || fail "release base is missing its host"

[ "$(uname -s)" = Darwin ] || fail "build host must be macOS"
[ "$(uname -m)" = arm64 ] || fail "build host must be Apple Silicon (arm64)"
for tool in cargo file otool shasum mktemp; do
    command -v "$tool" >/dev/null 2>&1 || fail "required build tool not found: $tool"
done
[ -f "$script_dir/install.sh" ] || fail "distribution/install.sh is missing"
for name in LICENSE THIRD-PARTY-NOTICES.txt; do
    [ -f "$project_dir/$name" ] || fail "$name is missing"
done
sh -n "$script_dir/install.sh" || fail "installer shell syntax is invalid"

asset=bree-aarch64-apple-darwin
mkdir -p "$output_dir"
output_dir=$(CDPATH= cd -- "$output_dir" && pwd)
lock_dir=$output_dir/.bree-package.lock
mkdir "$lock_dir" 2>/dev/null || fail "another packaging run holds $lock_dir"
stage_dir=
cleanup() {
    if [ -n "$stage_dir" ]; then rm -rf "$stage_dir"; fi
    rmdir "$lock_dir" 2>/dev/null || :
}
trap cleanup 0
trap 'exit 130' INT
trap 'exit 143' TERM
for name in "$asset" "$asset.sha256" bree-version.txt install.sh bree.rb LICENSE THIRD-PARTY-NOTICES.txt; do
    [ ! -e "$output_dir/$name" ] && [ ! -L "$output_dir/$name" ] ||
        fail "refusing to replace existing output: $output_dir/$name"
done
stage_dir=$(mktemp -d "$output_dir/.bree-package.XXXXXX")

cargo build --locked --release --target aarch64-apple-darwin --manifest-path "$manifest" --target-dir "$cargo_dir/target"
binary=$cargo_dir/target/aarch64-apple-darwin/release/bree
[ -x "$binary" ] || fail "release executable is missing"
case "$(file -b "$binary")" in
    'Mach-O 64-bit executable arm64'*) ;;
    *) fail "release executable is not an arm64 Mach-O" ;;
esac
[ "$("$binary" --version)" = "bree $version" ] || fail "binary version differs from Cargo.toml"
dependencies=$(otool -L "$binary") || fail "cannot inspect binary dependencies"
unexpected=$(printf '%s\n' "$dependencies" | awk '
    NR > 1 && $1 !~ /^\/usr\/lib\// && $1 !~ /^\/System\/Library\// { print $1 }
')
[ -z "$unexpected" ] || fail "binary has non-system dependencies: $unexpected"

cp "$binary" "$stage_dir/$asset"
cp "$project_dir/LICENSE" "$stage_dir/LICENSE"
cp "$project_dir/THIRD-PARTY-NOTICES.txt" "$stage_dir/THIRD-PARTY-NOTICES.txt"
chmod 755 "$stage_dir/$asset"
# Match the source default literally; never interpolate a URL into shell code,
# a regular expression, or a replacement string interpreted by sed.
awk -v base="$release_base" '
    $0 == "    release_base=${BREE_RELEASE_BASE_URL:-https://github.com/yuyongyan29-dev/Bree/releases}" {
        print "    release_base=${BREE_RELEASE_BASE_URL:-" base "}"
        matched++
        next
    }
    { print }
    END { if (matched != 1) exit 1 }
' "$script_dir/install.sh" > "$stage_dir/install.sh" ||
    fail "installer release default changed; update the packaging template match"
sh -n "$stage_dir/install.sh" || fail "generated installer shell syntax is invalid"
chmod 755 "$stage_dir/install.sh"
checksum_result=$(shasum -a 256 "$stage_dir/$asset") || fail "cannot calculate binary checksum"
digest=${checksum_result%% *}
printf '%s  %s\n' "$digest" "$asset" > "$stage_dir/$asset.sha256"
printf '%s\n' "$version" > "$stage_dir/bree-version.txt"
cat > "$stage_dir/bree.rb" <<CASK
# Generated by distribution/package.sh. Publish only with the matching asset.
# Source repository visibility and binary download visibility are independent.
cask "bree" do
  version "$version"
  sha256 "$digest"

  url "$release_base/download/v$version/$asset"
  name "Bree"
  desc "Local memory inspection for macOS"
  homepage "https://github.com/yuyongyan29-dev/Bree"

  depends_on arch: :arm64
  depends_on :macos
  container type: :naked

  binary "$asset", target: "bree"
end
CASK

(cd "$stage_dir" && shasum -a 256 -c "$asset.sha256")
if command -v ruby >/dev/null 2>&1; then
    ruby -c "$stage_dir/bree.rb"
fi
for name in "$asset" "$asset.sha256" bree-version.txt install.sh bree.rb LICENSE THIRD-PARTY-NOTICES.txt; do
    mv "$stage_dir/$name" "$output_dir/$name"
done
printf 'Prepared Bree %s in %s\n' "$version" "$output_dir"
printf 'Binary SHA-256: %s\n' "$digest"
printf '%s\n' 'No release was uploaded; signing, notarization, and remote installation are not verified.'
