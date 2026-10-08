# Installing Bree

[English](installation.md) · [中文](installation.zh-CN.md)

Bree is free and can be installed with Homebrew or curl. Both use the same precompiled program. No account, Rust, Cargo, Python, or Node.js is required.

## Supported platforms

| Item | Current scope |
|---|---|
| Program version | `0.4.0-alpha.1` |
| Official support | macOS 27 on native Apple Silicon (arm64), tested on the maintainer's Mac |
| Tested machine | Mac17,3, Apple M5, macOS 27.0.1 |
| Homebrew bottle | Apple Silicon, macOS 27 only |
| Other macOS versions | Not tested on a maintainer machine and outside the supported scope |
| CI reference | Builds and automated tests pass on macOS 15; this is reference evidence, not a supported platform |
| Intel | Unsupported; the curl installer rejects it |
| Linux, Windows | No installation packages |

Use a native terminal on Apple Silicon. The curl installer also rejects an x86_64 environment under Rosetta. It checks macOS and architecture but does not check the macOS version; passing that check does not extend the supported scope. Installation and upgrades need access to GitHub; everyday local memory inspection needs no network.

Bree does not quit applications. Releases have only an ad-hoc signature, with no Developer ID signature or notarization. Installation under a clean user account remains unverified. The installer does not remove quarantine or bypass system checks.

## Install with Homebrew

Install and configure [Homebrew](https://brew.sh/) first, then run:

```sh
brew install yuyongyan29-dev/tap/bree
bree --version
bree
```

The full tap name automatically selects [Bree's Formula](https://github.com/yuyongyan29-dev/homebrew-tap); there is no need to add the tap separately. The Formula pins the version URL and SHA-256 and downloads a matching precompiled bottle by default to install the `bree` command.

A standard installation using the matching bottle needs neither Xcode Command Line Tools nor Rust. The tap provides only a macOS 27 arm64 bottle and requires at least macOS 27. Other macOS versions are outside Bree's supported scope. Homebrew's own requirements are in its [installation documentation](https://docs.brew.sh/Installation).

The briefly available cask was withdrawn because Gatekeeper blocked the unnotarized program when installed with quarantine. If you installed that old cask, run `brew uninstall --cask bree` before using the Formula command above. The installer does not change system security settings.

Upgrade:

```sh
brew update
brew upgrade yuyongyan29-dev/tap/bree
```

Uninstall:

```sh
brew uninstall bree
```

## Install with curl

Use the shell, curl, and SHA-256 tools included with macOS:

```sh
curl -fsSL https://raw.githubusercontent.com/yuyongyan29-dev/Bree/main/distribution/install.sh | sh
```

The default location is `~/.local/bin/bree`; sudo is not needed. The installer reads the default release version from the repository's `distribution/latest-version.txt`, then downloads the binary and checksum from the fixed `vVERSION` tag. This also lets the same command install Alpha releases. It replaces an existing binary atomically only after SHA-256 and `bree --version` checks succeed. Download, checksum, or version-check failures preserve the old file.

The installer does not edit shell configuration. If `~/.local/bin` is not in PATH, run this in the current terminal:

```sh
export PATH="$HOME/.local/bin:$PATH"
bree --version
bree
```

Save the same `export` line in `~/.zshrc` to use Bree in new terminals. Use the appropriate configuration file for other shells.

### Use a custom directory

Choose a writable directory you manage:

```sh
curl -fsSL https://raw.githubusercontent.com/yuyongyan29-dev/Bree/main/distribution/install.sh \
  | sh -s -- --bin-dir "$HOME/bin"
```

If the directory is not in PATH, the installer prints the configuration line to add. It does not overwrite symbolic links. Upgrade a Homebrew-managed installation through Homebrew, or choose another directory.

### Pin a version and upgrade

Pin the current version:

```sh
curl -fsSL https://raw.githubusercontent.com/yuyongyan29-dev/Bree/main/distribution/install.sh \
  | sh -s -- --version 0.4.0-alpha.1
```

Run the default installation command again to upgrade to the installer's selected release, or explicitly use `--version latest`. Set `BREE_RELEASE_BASE_URL` to use your own HTTPS release mirror with the same asset layout. `BREE_VERSION_URL` selects an HTTPS version metadata URL.

### Uninstall

Remove the curl-installed file, adjusting the path for a custom directory:

```sh
rm "$HOME/.local/bin/bree"
```

Bree's read-only viewer saves no data. You may manually remove `~/Library/Application Support/Bree` left by version 0.3 or earlier. Neither the program nor uninstalling automatically deletes or migrates that directory.

## Check the installation

```sh
command -v bree
bree --version
bree doctor
bree status --json
```

`command -v` identifies the binary currently used. If you installed through both channels, the earlier directory in PATH determines the running version. Use that installation channel for later upgrades.

Missing process metrics do not mean installation failed. `doctor` and the validity fields describe data availability. For other problems, include the macOS version, architecture, installation method, and error output in an [issue](https://github.com/yuyongyan29-dev/Bree/issues). Review application names and local paths before sharing data.
