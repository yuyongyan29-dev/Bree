#!/bin/sh
# Submit a signed, frozen copy to Apple. Never publishes or staples a bare CLI.
set -eu
umask 077

usage() {
    cat <<'USAGE'
Usage: sh distribution/notarize.sh --binary FILE --keychain-profile NAME --output NEW_DIR

Requires a verified Developer ID Application signature, hardened runtime, and a
secure timestamp. Uses only credentials already stored in a notarytool keychain
profile. Passwords, Apple IDs, and API keys are not accepted as script options or
read from environment variables. Provision the profile interactively beforehand.

Creates a private evidence directory with the binary SHA-256, signature, zip,
submission JSON/stderr, and notary log. Submits with --wait (timeout: 30 minutes).
Any failed command, missing submission ID, or status other than Accepted fails.
A timeout can leave a submission processing at Apple; inspect its ID before retrying.
Never staples the bare binary or zip. Acceptance by the notary service still
requires Gatekeeper testing of the actual distribution on a clean Mac, including
online/offline behavior. This command uploads to Apple, but does not publish a release.
USAGE
}

fail() { printf 'bree notarize: %s\n' "$*" >&2; exit 1; }

binary=
profile=
output_dir=
while [ "$#" -gt 0 ]; do
    case "$1" in
        --binary|--keychain-profile|--output)
            [ "$#" -ge 2 ] || fail "option requires a value"
            case "$2" in ''|-*|*'
'*) fail "option requires a nonempty value without newlines" ;; esac
            case "$1" in
                --binary) binary=$2 ;;
                --keychain-profile) profile=$2 ;;
                --output) output_dir=$2 ;;
            esac
            shift 2 ;;
        -h|--help) usage; exit 0 ;;
        # Do not echo an unrecognized argument, which could contain a credential.
        *) fail "unsupported option; use --help (credentials belong in the keychain)" ;;
    esac
done
[ -n "$profile" ] || fail "--keychain-profile is required; credentials must be stored in the keychain"
[ -n "$binary" ] && [ -n "$output_dir" ] || fail "--binary and --output are required"
[ "$(uname -s)" = Darwin ] || fail "notarization requires macOS tools"
[ -f "$binary" ] && [ ! -L "$binary" ] || fail "binary must be a regular file, not a symlink"
for tool in codesign ditto xcrun plutil shasum mktemp; do
    command -v "$tool" >/dev/null 2>&1 || fail "required tool not found: $tool"
done

mkdir -p "$(dirname -- "$output_dir")"
mkdir "$output_dir" || fail "output directory must be new; existing evidence is never replaced"
output_dir=$(CDPATH= cd -- "$output_dir" && pwd)
payload=$(mktemp -d "$output_dir/.payload.XXXXXX")
trap 'rm -rf "$payload"' 0
trap 'exit 130' INT
trap 'exit 143' TERM HUP
frozen=$payload/bree-aarch64-apple-darwin
cp "$binary" "$frozen"
codesign --verify --strict --verbose=2 "$frozen" > "$output_dir/verify.txt" 2>&1 ||
    fail "signature verification failed; see verify.txt"
codesign -dvvv "$frozen" > "$output_dir/signature.txt" 2>&1 || fail "cannot inspect signature"
authority=$(awk '/^Authority=/ { print; exit }' "$output_dir/signature.txt")
case "$authority" in
    'Authority=Developer ID Application: '*) ;;
    *) fail "Developer ID Application signing is required; Apple Development, ad-hoc, and other signatures cannot be submitted" ;;
esac
grep -Eq '^CodeDirectory .*flags=.*[=(,]runtime[,)]' "$output_dir/signature.txt" ||
    fail "hardened runtime is required"
grep -Eq '^Timestamp=.+$' "$output_dir/signature.txt" || fail "secure timestamp is required"
(cd "$payload" && shasum -a 256 bree-aarch64-apple-darwin) > "$output_dir/binary.sha256" ||
    fail "cannot calculate binary checksum"
ditto -c -k --keepParent "$frozen" "$output_dir/bree-notarization.zip" || fail "cannot create submission zip"

submit_exit=0
xcrun notarytool submit "$output_dir/bree-notarization.zip" \
    --keychain-profile "$profile" --wait --timeout 30m --output-format json \
    > "$output_dir/submission.json" 2> "$output_dir/submission.stderr" || submit_exit=$?
printf '%s\n' "$submit_exit" > "$output_dir/submission.exit-code"
submission_id=$(plutil -extract id raw -o - "$output_dir/submission.json" 2>/dev/null) || submission_id=
status=$(plutil -extract status raw -o - "$output_dir/submission.json" 2>/dev/null) || status=unknown
# Do not turn malformed output into a success or send it back as a log ID.
printf '%s\n' "$submission_id" | grep -Eq '^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$' ||
    fail "submission ID missing or invalid (exit $submit_exit); inspect submission.json and submission.stderr before retrying"
printf 'Submission ID: %s\nStatus: %s\n' "$submission_id" "$status"
# Retrieve warnings for Accepted submissions too. Never hide a failed log request.
log_exit=0
xcrun notarytool log "$submission_id" --keychain-profile "$profile" \
    > "$output_dir/notary-log.json" 2> "$output_dir/notary-log.stderr" || log_exit=$?
printf '%s\n' "$log_exit" > "$output_dir/notary-log.exit-code"
[ "$submit_exit" -eq 0 ] && [ "$status" = Accepted ] ||
    fail "notarization did not complete successfully (submit exit $submit_exit, log exit $log_exit); inspect evidence in $output_dir"
[ "$log_exit" -eq 0 ] || fail "notary log retrieval failed; inspect evidence before distribution"
printf '%s\n' 'Notary service status: Accepted. Review notary-log.json for warnings.'
printf '%s\n' 'The bare binary and zip are not stapled. Gatekeeper acceptance and clean online/offline installation remain unverified.'
