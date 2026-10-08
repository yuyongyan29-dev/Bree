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

**Bree is a read-only viewer with no persistent state. Official support is macOS 27 on Apple Silicon, tested on the maintainer's Mac (Mac17,3, Apple M5, macOS 27.0.1). The package version is `0.4.0-alpha.1`.**

Version 0.4 includes breaking changes: `clean` and `history` are removed, and JSON uses schema 2. Bree does not read, migrate, or delete data left by earlier 0.3 or older builds; you may manually remove `~/Library/Application Support/Bree` if no longer needed.

<!-- screenshot: TUI Home and Resources -->

## Installation

Other macOS versions have not been tested on a maintainer machine and are outside the supported scope. CI builds and automated tests pass on macOS 15 as reference evidence only. Intel is unsupported and rejected by the curl installer.

Both methods install precompiled binaries. **You do not need Rust, Cargo, Python, or Node.js.** Installation and updates require internet access; everyday memory inspection runs locally.

### Homebrew

With Homebrew installed, install Bree and start it:

```sh
brew install yuyongyan29-dev/tap/bree
bree
```

Bree's tap provides a precompiled Homebrew bottle only for native Apple Silicon on macOS 27.

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

See the [installation guide](docs/installation.md) for updates, uninstalling, custom directories, and compatibility details. Releases have no Developer ID signature or notarization; installation under a clean user account remains unverified.

## Features

| What you want to know | What Bree shows |
|---|---|
| How is my Mac's memory doing? | Total and used memory, compression, swap, and memory pressure |
| Which application or process is using memory? | Application groups supported by evidence, process details, and search by name, bundle ID, or PID |
| Which AI or developer tool does a process belong to? | Labels for ChatGPT.app's bundled Codex CLI and native Claude Code, matched by [documented executable layouts](docs/cli.md#development-labels) without a pinned version; labels explain installation source, not task completion or whether memory can be safely reclaimed |
| Is usage still changing? | Foreground monitoring and text, JSON, or JSONL output |

Missing or inaccessible data is explicitly marked instead of being reported as zero.

Unattributed processes are grouped by executable path and UID, or by process name and UID when the path is unreadable. Processes with unreadable UIDs remain separate. Developer labels name these groups; native Claude Code includes the version from its installation path, such as `Claude Code 2.1.294`, without executing the program. Application grouping is unchanged, and aggregation does not establish application ownership.

Home's static pixel mascot adds no animation, background process, or extra dependency, and stays out of direct commands and JSON output.

Bree does not install a background service or launch at login. Force quitting and automatic background cleanup are unavailable.

## Usage

```sh
bree                         # Open the terminal interface
bree status                  # Show system memory
bree list --limit 20          # List application and process usage
bree list --search Safari --sort name  # Find matching groups and sort by name
bree inspect 12345            # Inspect the process currently using this PID
bree inspect '<short ID>'     # Inspect a group returned by list
bree watch                   # Monitor in the foreground
bree doctor                  # Check capabilities and data availability
bree license                  # Display the GPL-3.0 license
```

Text output identifies processes by PID and groups by a stable short ID, also shown in the TUI. `inspect` accepts a PID, group short ID, or full ID. PID input does not detect reuse between commands; full process IDs remain bound to the sampled instance. Short IDs start at 8 hexadecimal characters and lengthen on collisions within the full sample; filtering does not recalculate them. A missing or ambiguous target returns exit code 1 and a structured error with `--json`. Unattributed group IDs remain stable while their grouping key is unchanged; inspection shows current members.

Text samples use local `HH:MM:SS` time. Memory formulas are explained by `doctor`. A group inspection starts with its name, instance count, and total RSS. Example excerpt (illustrative values):

```text
Sampled at: 14:32:08 · Collection 20 ms
Memory       Instances  Category      Name                 ID
128.0 MiB    2          Unattributed  Claude Code 2.1.294   8c73a1de
```

Lists are sorted by memory by default; use `--sort name` to sort by group name. Search ignores case and surrounding whitespace. Non-numeric queries match substrings of group names, member process names, or member bundle IDs. Purely numeric queries match only a complete PID, not name substrings or PID prefixes. It does not search paths, full command lines, or environment variables.

In the **Memory** page, press **/** to edit a search, **Enter** to apply it, or **Esc** to discard edits and keep the previous search. Outside the editor, **Esc** first clears an applied search, then returns home. **Ctrl+U** clears the input and **Backspace** deletes one character. **Q** is text while editing; **Ctrl+C** always quits. Applied searches work with filters, sorting, and refresh. Search changes only the displayed list and preserves the underlying classifications.

Home has one entry, **1. Memory**; press **Enter** to open it. **Up/Down** select · **Enter** open details · **R** refresh · **Q** quit outside search editing. The minimum interactive window size is 48 columns by 16 rows.

For scripts:

```sh
bree status --json
bree list --search Safari --sort name --json
bree watch --json --count 3
```

JSON keeps `schema_version: 2`. Metrics contain `value`, `status`, and `reason`; sources are documented centrally in the user guide. Group objects add `short_id`, while full IDs in `id` and `process_ids` and the Unix-millisecond `sampled_at_unix_ms` field retain their meanings. For `list`, `groups` reflects search, sorting, and `--limit`, while `processes` and `coverage` retain the full sample. The `view` metadata reports `search`, `sort`, `total_groups`, `matched_groups`, and `shown_groups`. Unknown values are `null`. Results go to stdout; diagnostics go to stderr. Command options, metric definitions, and the schema changes are covered in the [user guide](docs/cli.md#output-contract).

## Known limitations

- Bree is read-only: it does not quit applications or save data, and has no background service.
- Releases are unsigned by Developer ID and unnotarized; the binary has only an ad-hoc signature. Installation under a clean user account remains unverified.
- Official support is limited to macOS 27 on Apple Silicon, tested on the maintainer's Mac. Other macOS versions are untested on a maintainer machine and unsupported; macOS 15 CI results are reference evidence only. Intel is unsupported and rejected by the curl installer; Homebrew bottles are provided only for macOS 27.
- `inspect <pid>` does not detect PID reuse between commands. Use a full process ID to bind inspection to the sampled instance.
- `inspect --json` can contain project names and paths outside the current `HOME` prefix. Review output before sharing it.

## Feedback and contributions

Found a problem or have a suggestion? [Open an issue](https://github.com/yuyongyan29-dev/Bree/issues) with your Bree version, macOS version, architecture, and steps to reproduce. Check application names and local paths before sharing diagnostics.

See [CONTRIBUTING.md](CONTRIBUTING.md) for development and verification guidance.

Report security vulnerabilities privately using [SECURITY.md](SECURITY.md), not a public issue.

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
