# Contributing to Bree

[English](CONTRIBUTING.md) · [中文](CONTRIBUTING.zh-CN.md)

Contributions are welcome: report problems, improve documentation, submit fixes, or discuss features. Bree is a free macOS CLI for local memory inspection and explanation. It has no persistent state. Its terminal interface and direct commands share collection, query, and output logic.

## Reporting problems

Include the following in an [issue](https://github.com/yuyongyan29-dev/Bree/issues):

- Bree version, macOS version, and CPU architecture.
- Installation method, command or interface actions, and expected and actual results.
- Minimal steps to reproduce and any relevant error output.

Run `bree --version` and `bree doctor` to help with diagnosis. Review application names, project names, and local paths before sharing snapshots. Do not submit secrets, full environment variables, chat content, or personal data.

Report security vulnerabilities privately using the [security policy](SECURITY.md), not a public issue.

## Development environment

The currently tested environment is Apple Silicon with macOS 27.0.1. Building from source requires Rust 1.96.0 and Xcode Command Line Tools. Python 3 is used only for installer tests, script regression tests, and performance and terminal verification scripts; it is not a Bree runtime dependency.

```sh
git clone https://github.com/yuyongyan29-dev/Bree.git
cd Bree
git switch -c feat/your-change
cargo build --locked --release
./target/release/bree
```

The toolchain is pinned in `rust-toolchain.toml` and dependencies in `Cargo.lock`. For dependency updates, check `Cargo.lock` and `THIRD-PARTY-NOTICES.txt`; generate the notices with `distribution/notices.py`.

## Repository layout and checks

| Directory | Contents |
|---|---|
| `src/` | CLI, TUI, collection, queries, and output |
| `src/platform/` | macOS adapters and capability information for other platforms |
| `tests/` | Command behavior, live collection, and terminal tests |
| `scripts/` | Performance and terminal verification |
| `distribution/` | curl installer, release preparation, and installer tests |
| `docs/` | User and installation guides |

Run the relevant checks before submitting code:

```sh
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
```

For installer or distribution tooling changes, also run:

```sh
sh -n distribution/install.sh distribution/package.sh distribution/notarize.sh
python3 distribution/tests/test_install.py
python3 distribution/tests/test_release.py
```

Record the checks you actually ran and their results. Installer and release tooling tests use simulated downloads, builds, signing tools, and notarization responses; they do not access remote servers, real keychains, or user configuration. A mock Accepted response is not evidence of notarization.

For no-write checks, run the commands with an empty temporary `HOME` and verify that it stays empty. Tests that launch Bree must use their own temporary `HOME` and explicitly set terminal color variables rather than inheriting developer settings.

The stability runner provides a single entry point for read-only stability and performance checks. `--lock` must be an absolute path shared by all concurrent worktrees, and its parent directory must already exist. Do not create a separate lock for each worktree or delete the lock file. `--output` must be a new directory under this worktree's `target/` or `.artifacts/` to preserve existing evidence.

```sh
python3 scripts/stability-check.py \
  --lock /absolute/shared/native-experiment.lock \
  --output "$PWD/.artifacts/stability/run-001"
```

The runner rebuilds the release binary and runs the existing benchmark, terminal, theme, and signal checks, plus extended Home and Resources observations. It holds the exclusive lock during live measurements until its child processes are reaped. Home stays idle for 5 minutes; the explicitly opened TUI Resources page and a separate watch session each run for 10 minutes. The summary preserves the source manifest, binary SHA, commands, exit codes, and raw logs, distinguishing `failed`, `not-run`, and `unknown`. Raw logs can contain local application names and must not be committed. Do not run measurements alongside builds, other performance sampling, or native UI experiments.

Run script regression tests with `python3 -m unittest discover -s scripts/tests`; CI runs the same command. These tests use simulated child processes and temporary directories, without a prebuilt `target/release/bree`, developer terminal settings, or real user data. PTY checks do not replace visual checks in a real terminal for light and dark themes, fonts, small windows, or resizing. The runner marks that check as `not-run`; record actual visual checks separately. Measurements from one machine describe only that build on that system. They do not extend release compatibility claims.

Keep builds and local experiment output in the ignored `target/` or `.artifacts/` directories. Do not commit personal paths or raw process snapshots.

## Preparing a signed release

The published Alpha remains ad-hoc signed and unnotarized. The following maintainer tools prepare future releases; adding them does not change the published assets or installation guarantees.

With concurrent worktrees, take the same absolute `fcntl.flock` exclusive lock used by the stability runner **before** any build, packaging command, or live measurement, and hold it until its child processes exit. Packaging does not acquire that shared lock itself; its output-directory lock only prevents two writers from packaging into the same directory.

`distribution/package.sh` builds the locked release, checks the arm64 executable, version, and system-library dependencies, then prepares local assets. Without a signing option it preserves the linker's ad-hoc signature and existing output set. To sign the staged copy using a certificate SHA-1 or full keychain identity name:

```sh
sh distribution/package.sh \
  --sign-identity 'CERTIFICATE_SHA1_OR_FULL_NAME' \
  --output "$PWD/.artifacts/distribution/signed-candidate"
```

The script requires hardened runtime and an online secure timestamp, verifies the signature strictly, prints the Authority chain, TeamIdentifier, and runtime flags, and repeats the executable checks after signing. Signing or timestamp failure stops preparation without an unsigned fallback. Binary SHA-256 and the generated Formula refer to the final signed bytes; `bree-version.txt` matches their checked version. Existing output files are never replaced.

Public distribution needs a **Developer ID Application** certificate with its private key in the local keychain. An Apple Development certificate can validate the local signing flow, but its output must not be published. The script classifies the certificate using the signature's Authority, not the supplied identity string. If macOS asks to allow `codesign` access to the private key, the user must respond; do not change keychain access settings or export keys to bypass the prompt.

For notarization, provision a profile interactively with `xcrun notarytool store-credentials 'bree-notary'`. Enter credentials only at its secure prompts. Do not put passwords or API private-key contents in command arguments, environment variables, repository files, or logs. Once the Developer ID candidate and profile are ready, this **separate upload to Apple** is available:

```sh
sh distribution/notarize.sh \
  --binary "$PWD/.artifacts/distribution/signed-candidate/bree-aarch64-apple-darwin" \
  --keychain-profile 'bree-notary' \
  --output "$PWD/.artifacts/distribution/notary-candidate"
```

The output directory must be new. The script freezes a copy, rejects non-Developer ID signatures, missing hardened runtime, or missing secure timestamps, creates a zip with `ditto -c -k --keepParent`, and runs `notarytool submit --wait`. It retains the binary digest, signature, submission ID/status, command exit codes, and notary log in a private evidence directory. Any error or status other than Accepted fails; even Accepted submissions require log retrieval and warning review. A 30-minute timeout does not cancel processing at Apple: inspect the saved submission before retrying. Never commit these local artifacts.

Verify **signing**, **submission**, **Accepted status**, and **Gatekeeper acceptance** separately. Standalone binaries and zip archives cannot be stapled; an Accepted response does not establish clean offline launch. Check the actual curl download and Homebrew bottle with `codesign -dvvv`, `codesign --verify --strict`, `spctl -a -t exec -vv`, and `xattr -l`, then test fresh installation, upgrade, failure preservation, and uninstall under a clean standard account. Record online/offline behavior without removing quarantine or bypassing system checks. See Apple's [notarization requirements](https://developer.apple.com/documentation/security/notarizing-macos-software-before-distribution) and [custom workflow](https://developer.apple.com/documentation/security/customizing-the-notarization-workflow).

On a disposable Homebrew installation, use `brew install --build-bottle <tap>/bree` and `brew bottle <tap>/bree`. Compare SHA-256 and signature details of the signed candidate, installed binary, and extracted bottle binary; after a test pour compare the installed binary again. Homebrew can modify and re-sign relocated binaries, so do not infer signature preservation from successful bottling. Stop if bytes or Authority change. Do not use a user's existing global installation for this experiment. See [Homebrew bottles](https://docs.brew.sh/Bottles).

Packaging, notarization submission, publishing a GitHub Release, updating the tap, and changing `distribution/latest-version.txt` are separate actions. The package version comes from `Cargo.toml`; the default curl version is chosen separately by `distribution/latest-version.txt`. Prepare matching source and license assets and verify the final distribution before an authorized publication.

## Behavior contracts

- Keep missing metrics as `null` with a reason; do not substitute zero. Use a consistent process memory metric and do not count groups twice.
- Object IDs describe the current instance for inspection; they are not stopping credentials that can be reused across launches.
- Bree is a read-only viewer with no persistent state. It does not stop applications, run background cleanup, or delete data left by older versions.
- JSON stdout contains only results; diagnostics go to stderr. Preserve the declared schemas and validity fields.

See the [user guide](docs/cli.md) for metric definitions and capability scope.

## Submitting a pull request

Use a separate branch and keep the scope clear. Describe the concrete problem, resulting behavior, reproduction or verification steps, and anything still unverified. Reference the relevant issue for a bug fix and update related documentation for feature changes.

Do not add unrelated product designs, research materials, or unused build output to the CLI repository.

## Contribution license

Contributions are released under Bree's GPL-3.0 license. Do not submit secrets, personal configuration, internal development documents, experiment output, or design assets.
