<p align="center">
  <img src="assets/bree-brand.png" alt="Bree: a sleeping mascot on an orange icon and the bree wordmark" width="600">
</p>

<p align="center">
  <strong>See where your memory goes. Make informed choices.</strong><br>
  A free macOS terminal tool · Runs locally · No accounts or subscriptions
</p>

<p align="center">
  <a href="README.md">English</a> · <a href="README.zh-CN.md">中文</a>
</p>

<p align="center">
  <a href="https://github.com/yuyongyan29-dev/Bree/actions/workflows/ci.yml"><img src="https://github.com/yuyongyan29-dev/Bree/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
  <a href="https://github.com/yuyongyan29-dev/Bree/releases"><img src="https://img.shields.io/github/v/release/yuyongyan29-dev/Bree?include_prereleases" alt="Release"></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-GPL--3.0-blue" alt="GPL-3.0"></a>
</p>

<p align="center">
  <a href="#installation">Install</a> ·
  <a href="#usage">Usage</a> ·
  <a href="docs/cli.md">User guide</a> ·
  <a href="https://github.com/yuyongyan29-dev/Bree/issues">Report an issue</a>
</p>

Bree brings system memory pressure, application usage, and process details into a terminal interface you can navigate with your keyboard. Find what is using memory, inspect the evidence behind each application's grouping and metrics, or follow changes with direct commands and JSON output.

**Current version: `0.3.0-alpha.3`. Tested on native Apple Silicon with macOS 27.0.1; other configurations remain unverified. Application quitting is disabled: this version does not stop applications. Rules and cleanup commands provide previews and session summaries.**

## Installation

Both methods install precompiled binaries. **You do not need Rust, Cargo, Python, or Node.js.** Installation and updates require internet access; everyday memory inspection runs locally.

### Homebrew

With Homebrew installed, install Bree and start it:

```sh
brew install yuyongyan29-dev/tap/bree
bree
```

Bree's tap provides a precompiled Homebrew bottle for native Apple Silicon on macOS 27. Other macOS versions have not been verified.

### curl

Homebrew is optional. You can install using tools included with macOS:

```sh
curl -fsSL https://raw.githubusercontent.com/yuyongyan29-dev/Bree/main/distribution/install.sh | sh
```

The default location is `~/.local/bin/bree`. If that directory is already in your PATH, run `bree`. Otherwise, run:

```sh
export PATH="$HOME/.local/bin:$PATH"
bree
```

Add the same `export` line to `~/.zshrc` to make the command available in new terminal sessions. The installer checks SHA-256 and the binary's version before replacing an existing installation. Failed checks leave the old binary in place. It does not use sudo or modify your shell configuration.

See the [installation guide](docs/installation.md) for updates, uninstalling, custom directories, and compatibility details. This guide is currently in Chinese. Developer ID signing, notarization, and installation under a clean user account remain unverified.

## Features

| What you want to know | What Bree shows |
|---|---|
| How is my Mac's memory doing? | Total and used memory, compression, swap, and memory pressure |
| Which application or process is using memory? | Application groups supported by evidence, process details, and search by name, bundle ID, or PID |
| Is usage still changing? | Foreground monitoring and text, JSON, or JSONL output |
| Why is an item protected or skipped? | Allow and protect rules, classification reasons, read-only previews, and session history |

Missing or inaccessible data is explicitly marked instead of being reported as zero. Bree inherits your terminal's background and text colors, supports light and dark themes, and adapts to narrow windows without extra fonts. It does not install a background service or launch at login. Force quitting and automatic background cleanup are unavailable.

## Usage

```sh
bree                         # Open the terminal interface
bree status                  # Show system memory
bree list --limit 20          # List application and process usage
bree list --search Safari --sort name  # Find matching groups and sort by name
bree inspect '<object ID>'    # Inspect an ID returned by list
bree watch                   # Monitor in the foreground
bree doctor                  # Check capabilities and data availability
bree clean --dry-run --json   # Preview without taking action
bree history --json           # Read session history
bree license                  # Display the GPL-3.0 license
```

Lists are sorted by memory by default; use `--sort name` to sort by group name. Search ignores case and surrounding whitespace. Non-numeric queries match substrings of group names, member process names, or member bundle IDs. Purely numeric queries match only a complete PID, not name substrings or PID prefixes. It does not search paths, full command lines, or environment variables.

In the **Memory** page, press **/** to edit a search, **Enter** to apply it, or **Esc** to discard edits and keep the previous search. Outside the editor, **Esc** first clears an applied search, then returns home. **Ctrl+U** clears the input and **Backspace** deletes one character. **Q** is text while editing; **Ctrl+C** always quits. Applied searches work with filters, sorting, and refresh. Search changes only the displayed list, not rules, classifications, or cleanup candidates.

**Up/Down** select · **Enter** open · **R** refresh · **S** settings · **Q** quit outside search editing. The minimum interactive window size is 48 columns by 16 rows.

For scripts:

```sh
bree status --json
bree list --search Safari --sort name --json
bree watch --json --count 3
```

JSON includes `schema_version: 1` and data validity fields. For `list`, `groups` reflects search, sorting, and `--limit`, while `processes`, `coverage`, and `policy` retain the full sample. The `view` metadata reports `search`, `sort`, `total_groups`, `matched_groups`, and `shown_groups`. Unknown values are `null`. Results go to stdout; diagnostics go to stderr. Command options, rule scopes, and local data behavior are covered in the [user guide](docs/cli.md), currently in Chinese.

## Feedback and contributions

Found a problem or have a suggestion? [Open an issue](https://github.com/yuyongyan29-dev/Bree/issues) with your Bree version, macOS version, architecture, and steps to reproduce. Check application names and local paths before sharing diagnostics.

See [CONTRIBUTING.md](CONTRIBUTING.md) for development and verification guidance.

<details>
<summary>Build from source</summary>

Development requires Rust 1.96.0 and Xcode Command Line Tools. The Rust toolchain and dependencies are pinned by `rust-toolchain.toml` and `Cargo.lock`.

```sh
git clone https://github.com/yuyongyan29-dev/Bree.git
cd Bree
cargo build --locked --release
./target/release/bree
```

</details>

## License

Bree is available for free under [GPL-3.0](LICENSE). Commercial use is allowed. If you redistribute a modified version, comply with the GPL and provide its corresponding source code. Third-party components retain their own licenses; see [THIRD-PARTY-NOTICES.txt](THIRD-PARTY-NOTICES.txt).
