# Contributing to Bree

[English](CONTRIBUTING.md) · [中文](CONTRIBUTING.zh-CN.md)

Contributions are welcome: report problems, improve documentation, submit fixes, or discuss features. Bree is a free macOS CLI for local memory inspection and explanation. Its terminal interface and direct commands share collection, rule, and session logic.

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
| `src/` | CLI, TUI, collection, policy, storage, and processing sessions |
| `src/platform/` | macOS adapters and capability information for other platforms |
| `tests/` | Command behavior, live collection, and terminal tests |
| `scripts/` | Performance, terminal, and isolated application quit experiments |
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
sh -n distribution/install.sh distribution/package.sh
python3 distribution/tests/test_install.py
```

Record the checks you actually ran and their results. Installer tests use simulated downloads; they do not access remote servers or modify user configuration.

Use an isolated data directory for manual checks that write state:

```sh
BREE_DATA_DIR="$PWD/.artifacts/dev-data" ./target/release/bree
```

The stability runner provides a single entry point for read-only stability and performance checks. `--lock` must be an absolute path shared by all concurrent worktrees, and its parent directory must already exist. Do not create a separate lock for each worktree or delete the lock file. `--output` must be a new directory under this worktree's `target/` or `.artifacts/` to preserve existing evidence.

```sh
python3 scripts/stability-check.py \
  --lock /absolute/shared/native-experiment.lock \
  --output "$PWD/.artifacts/stability/run-001"
```

The runner rebuilds the release binary and runs the existing benchmark, terminal, theme, and signal checks, plus history reads and extended Home and Resources observations. It holds the exclusive lock during live measurements until its child processes are reaped. Home stays idle for 5 minutes; the explicitly opened TUI Resources page and a separate watch session each run for 10 minutes. A roughly 10 MiB history fixture stays in the run's output directory and is used only for history reads. The summary preserves the source manifest, binary SHA, commands, exit codes, and raw logs, distinguishing `failed`, `not-run`, and `unknown`. Raw logs can contain local application names and must not be committed. Do not run measurements alongside builds, other performance sampling, or native UI experiments.

Run script regression tests with `python3 -m unittest discover -s scripts/tests`; CI runs the same command. These tests use simulated child processes and temporary directories, without a prebuilt `target/release/bree`, developer terminal settings, or real user data. PTY checks do not replace visual checks in a real terminal for light and dark themes, fonts, small windows, or resizing. The runner marks that check as `not-run`; record actual visual checks separately. Measurements from one machine describe only that build on that system. They do not extend release compatibility claims or establish A1 application quitting capability.

Keep builds and local experiment output in the ignored `target/` or `.artifacts/` directories. Do not commit personal paths or raw process snapshots.

## Behavior contracts

- Keep missing metrics as `null` with a reason; do not substitute zero. Use a consistent process memory metric and do not count groups twice.
- Object IDs describe the current instance for inspection; they are not stopping credentials that can be reused across launches. Protect rules take priority over allow rules.
- Actual A1 normal application quitting is hard-disabled. Fake backend tests do not establish native capability. Do not add a force-quit fallback.
- Changes to quitting capability must provide evidence for exact instance control, foreground protection, document saving, cancellation, logging failures, and restart handling. Objects without reliable capability remain read-only.
- JSON stdout contains only results; diagnostics go to stderr. Preserve the declared schemas and validity fields.

See the [user guide](docs/cli.md) for metric definitions and capability scope. Isolated quit experiments must target only fixtures created for the experiment, never existing user applications as implicit test targets.

## Submitting a pull request

Use a separate branch and keep the scope clear. Describe the concrete problem, resulting behavior, reproduction or verification steps, and anything still unverified. Reference the relevant issue for a bug fix and update related documentation for feature changes.

Do not add unrelated product designs, research materials, or unused build output to the CLI repository.

## Contribution license

Contributions are released under Bree's GPL-3.0 license. Do not submit secrets, personal configuration, internal development documents, experiment output, or design assets.
