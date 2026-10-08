# Using Bree

[English](cli.md) · [中文](cli.zh-CN.md)

Bree is a read-only viewer with no persistent state. It collects, displays, and exports local system memory and application/process usage in the terminal. The package version is `0.4.0-alpha.1`. The supported platform is macOS 27 on Apple Silicon, tested on the maintainer's Mac running macOS 27.0.1.

**Bree does not quit applications or save data.** Since 0.4, the only commands are `status`, `list`, `inspect`, `watch`, `doctor`, and `license`; `clean` and `history` are removed, and JSON uses schema 2. Bree provides no background service or launch-at-login feature.

See the [installation guide](installation.md) for installation and the [contribution guide](../CONTRIBUTING.md) for source builds and development.

## Terminal interface

Run `bree` to open the English interface. Home initially selects the Memory entry and stays idle after the first sample. The Memory page and watch refresh every 2 seconds by default.

| Key | Action |
|---|---|
| Up/Down, Enter | Select and open |
| R | Refresh the current page |
| Esc | Cancel search editing; outside the editor, clear an applied query before going back |
| Q / Ctrl+C | Q quits outside search editing and types text while editing; Ctrl+C always quits |

The minimum interactive size is 48 columns by 16 rows. Home places a compact static pixel mascot before the Bree wordmark, with the memory overview and menu aligned left below it and key hints immediately after the menu.

| Display conditions | Home branding |
|---|---|
| At least 60 columns by 26 rows, with RGB or 256-color support | A static 20-column by 10-row pixel mascot followed by a three-line Bree wordmark |
| At least 60 columns by 22 rows, without the conditions above | A three-line Bree wordmark |
| Smaller interactive windows | A compact single-line `bree` wordmark |

Terminals declaring RGB support such as truecolor/24bit/direct use the original pixel colors. A `TERM` value ending in `256color` alone uses a six-level color approximation. When `NO_COLOR` is present (even empty), `TERM` is empty or `dumb`, or no supported color capability is declared, the mascot is hidden and the size-appropriate wordmark is used. Here `NO_COLOR` controls only the mascot; interface accent colors remain.

Opaque pixels use their own colors. The surrounding background and text inherit terminal defaults for light and dark themes. The image is static, starts no background worker, and needs no extra font or runtime dependency. Direct commands and JSON output contain no image.

Home has one entry, `1. Memory`. Press Enter to open it or Q to quit. On the Memory page, Enter opens details for the selected object.

On the Memory page, press `/` to edit a search, Enter to apply it, or Esc to cancel editing and keep the previous query. Ctrl+U clears the editor and Backspace deletes one character. Q types text while editing; Ctrl+C always quits. Outside the editor, Esc first clears an applied query, then returns home on the next press. After applying a query, Tab still changes the category filter, O switches between memory and name sorting, and R refreshes. Search changes only the display, not classification.

Clearing or changing the editor still requires Enter to apply. In `N/M matching in filter`, N is the matching count and M is the number of groups in the current category before searching. An empty result shows `No matches in this filter`.

## Direct commands

```sh
bree status                  # One system memory snapshot
bree status --json
bree list --limit 20          # Application and unattributed process groups
bree list --search Safari --sort name  # Find matching groups and sort by name
bree list --json
bree inspect 12345            # Resample the current PID; no reuse detection between commands
bree inspect '<short ID>'     # Inspect current members of a group returned by list
bree inspect '<full ID>' --json
bree watch                   # Monitor in the foreground
bree watch --json --count 3
bree doctor                  # Sampling coverage and capabilities
bree license                 # Read the project license offline
```

Without an interactive terminal, bare `bree` prints usage. `watch` opens the TUI when both stdin and stdout are terminals and neither `--json` nor `--count` is supplied. Otherwise it outputs text, or one JSON snapshot per line with `--json`. `--count` limits the number of samples to a positive integer; `--interval` sets a 1–60 second interval (default: 2).

`list` defaults to `--sort memory`; `--sort name` orders groups by name. `--search` ignores surrounding whitespace and case. Non-numeric queries match substrings of group names, member process names, or member bundle IDs. Purely numeric queries match only a complete PID, not name substrings or PID prefixes. An empty query keeps all results. Search does not scan paths, full command lines, or environment variables.

Queries accept at most 128 Unicode characters, including surrounding whitespace in the original input. Control characters and bidirectional formatting controls are rejected. The TUI and direct commands share the same search scope and limits.

`list --limit` limits displayed groups after searching and sorting; omitting it shows all matches, and `--limit 0` shows none. JSON `groups` contains the displayed result, while `processes` and `coverage` retain the full sample. Search does not recalculate classifications.

`inspect` accepts a PID, group short ID, or full ID and always takes a fresh sample:

- **PID:** inspects the process currently using that PID, without detecting reuse between commands. Instance consistency checks during collection still apply.
- **Group short ID:** text `list` and the TUI show an 8-character lowercase hexadecimal ID derived from the full group ID. Colliding IDs within a sample are lengthened until distinct. Short IDs are generated over the full sample, so searching, sorting, and truncation do not change them. Their length may change with the collision set; use the current `list` result. Only complete current short IDs are accepted, not arbitrary prefixes.
- **Full ID:** used in JSON `id`, group `process_ids`, and exact TUI selection. Full process IDs bind the boot session, PID, and start time; expired IDs never bind to replacement processes. An unattributed group's full ID stays stable while its grouping key is unchanged, even when members change. Inspection shows current members, not a fixed set of process instances.

A target matching both a PID and short ID, matching multiple groups, or matching nothing fails with exit code 1. With `--json`, the command emits a schema 2 error with `error.code: runtime_error` and a message, with diagnostics still on stderr. For ambiguous targets, use the full ID from `list --json`. All IDs are for inspection, not permission to stop anything.

Text identifies processes by PID. Group `inspect` starts with the name, instance count, and total RSS, then lists members. `status`, `list`, `inspect`, and text `watch` use local `HH:MM:SS` sample times; JSON `sampled_at_unix_ms` remains Unix milliseconds. The system memory formula is explained only in doctor text and its JSON notes; JSON `system.used_definition` is retained.

`doctor` explains sampling coverage and capabilities without requesting permissions or creating or checking persistent state. Ordinary inspection needs no extra permissions; unreadable process metrics carry an explicit explanation.

## Memory and attribution

The system overview includes physical memory, used memory, compression, swap, and memory pressure. Process memory consistently uses RSS, the physical memory currently resident for a process. RSS and system used memory measure different things; summing process RSS does not replace the system figure. The kernel pressure level is not guaranteed to match Activity Monitor's graph exactly.

The first CPU sample establishes a baseline. Later samples use actual elapsed time to calculate a percentage of one core. Coverage counts and each metric's validity fields describe what could be read.

Application grouping uses evidence such as the main application and installation location inside a bundle. Only ordinary applications (AppKit activation policy Regular, usually shown in the Dock) can lead a group. Helper applications nested inside a main application's bundle join that application. Standalone menu-bar, background, and ownership-conflict processes remain unattributed. They are grouped by executable path and UID, or by process name and UID when the path is unreadable. Different UIDs never merge; an unreadable UID leaves a separate instance. Each process belongs to exactly one group; aggregation does not establish application ownership.

<a id="development-labels"></a>

### AI and developer-tool labels

Labels require a process owned by the current user, a valid instance identity, and an executable path matching one of these layouts:

| Label | Matching executable path |
|---|---|
| Codex CLI | `/Applications/ChatGPT.app/Contents/Resources/codex-cli/CodexCLI.app/Contents/MacOS/codex` |
| Claude Code | `/Users/<user>/.local/share/claude/versions/<major.minor.patch>`, with 1–9 ASCII digits in each component |

Unattributed groups prefer reliable developer-tool labels. Native Claude Code names include the version from the matched installation path, such as `Claude Code 2.1.294`, without executing the program to query its version. Codex CLI keeps the label alone. Matching uses the installation layout, not a runtime version or pinned observed version. Claude Code version suffixes, extra path levels, and other installation locations do not match. Labels explain installation source; they do not verify signatures, project ownership, task completion, sharing, or whether memory can safely be reclaimed. Unknown node or python processes are not guessed to belong to an AI task.

### Metric sources

| Metric | macOS source and calculation |
|---|---|
| `system.total_bytes` | `sysctl hw.memsize`, physical memory in bytes |
| `system.used_bytes` | `host_statistics64(HOST_VM_INFO64)`: `(internal_page_count.saturating_sub(purgeable_count) + wire_count + compressor_page_count) × sysconf(_SC_PAGESIZE)` |
| `system.compressed_bytes` | `compressor_page_count × page_size` from the same VM counters, actual compressed storage |
| `system.cached_bytes` | `(external_page_count + purgeable_count) × page_size` from the same VM counters, estimated cache |
| `system.swap_used_bytes` | `xsu_used` bytes from `sysctl vm.swapusage` |
| `system.pressure` | `sysctl kern.memorystatus_vm_pressure_level`: 1/2/4 map to Normal/Elevated/High |
| `processes[].memory_bytes` | `libproc PROC_PIDTASKINFO.pti_resident_size`, RSS bytes |
| `processes[].cpu_one_core_percent` | User + system Mach ticks from `PROC_PIDTASKINFO`, converted with `mach_timebase_info`; the CPU-time delta for the same instance divided by actual monotonic elapsed time, multiplied by 100. The first sample only establishes a baseline |
| `groups[].memory_bytes` | Sum of RSS for distinct member instances. A single member retains its validity and reason; missing values or overflow in a multi-member sum make the total unknown |

## Output contract

JSON uses `schema_version: 2`. The following table lists the **complete top-level key set** for each successful output and for a runtime error. Key order is not a contract. Memory quantities use bytes; text displays MiB/GiB. `sampled_at_unix_ms` is the sample time in Unix milliseconds, and `collected_in_ms` is collection duration in milliseconds.

| Output | Exact top-level keys |
|---|---|
| `status --json` | `schema_version`, `sampled_at_unix_ms`, `collected_in_ms`, `system`, `coverage`, `diagnostics` |
| `list --json` | `schema_version`, `sampled_at_unix_ms`, `collected_in_ms`, `system`, `processes`, `groups`, `coverage`, `diagnostics`, `view` |
| `inspect --json` | `schema_version`, `sampled_at_unix_ms`, `group`, `processes` |
| `watch --json` | `schema_version`, `sampled_at_unix_ms`, `collected_in_ms`, `system`, `processes`, `groups`, `coverage`, `diagnostics` |
| `doctor --json` | `schema_version`, `version`, `platform`, `architecture`, `sampled_at_unix_ms`, `coverage`, `diagnostics`, `capabilities`, `notes` |
| `license --json` | `schema_version`, `license`, `third_party`, `text` |
| Error JSON | `schema_version`, `error` |

`status` contains system metrics, coverage counts/notes, and a diagnostics array. `list` adds process and group arrays and `view` metadata: `search`, `sort`, `total_groups`, `matched_groups`, and `shown_groups`. `groups` reflects searching, sorting, and `--limit`; `processes` and `coverage` retain the full sample.

`inspect` has a `group` object when inspecting a group, or `null` when inspecting a process. `processes` contains the current group members or the single selected process. Group objects include `short_id`; full IDs remain in `id` and `process_ids`. Process attribution's `attribution.application.activation_policy` is currently always `regular` when an application is present.

`watch --json` is JSONL: each line is a complete snapshot with the exact key set above, no surrounding array and no `view`. It includes full process and group arrays. A pipe interrupted during output may end with an incomplete line; consumers should discard that line.

`doctor` reports the package `version`, Rust platform/architecture names (`macos` and `aarch64` on the supported platform), coverage, diagnostics, and explanatory `notes`. Its `capabilities` object contains exactly `read_only: true`, `ai_attribution_enabled: true`, and `background_service: false`. It has no `system` object.

`license` reports `license: "GPL-3.0-only"`, a boolean `third_party`, and the requested license/notice `text`. `license --third-party --json` uses the same keys with `third_party: true`; the `license` field still identifies Bree's project license. Neither variant samples processes.

### Metric values and validity

Every `Metric` has exactly three keys: `value`, `status`, and `reason`. This applies to the six system metrics (`total_bytes`, `used_bytes`, `compressed_bytes`, `swap_used_bytes`, `cached_bytes`, `pressure`), process `memory_bytes` and `cpu_one_core_percent`, and group `memory_bytes`, including `inspect` results.

`value` contains the measured value when `status` is `ok`, including a genuine zero; otherwise it is `null`. Memory values are integer bytes, CPU is a percentage of one core, and pressure is `normal`, `elevated`, or `high`. `reason` is `null` for an available value, or an explanation string for an unavailable value. Reason text is explanatory, not a stable machine-readable error code.

| `status` | Meaning |
|---|---|
| `ok` | A measured value is available |
| `denied` | Access to the metric was denied |
| `unsupported` | The metric or source is unavailable on this platform or interface |
| `exited` | The process exited before the metric could be read |
| `stale` | The sample or baseline no longer describes a consistent instance/value |
| `unknown` | No reliable value, including an initial CPU baseline, unreadable source, incomplete group sum, or overflow |

### Errors and exit codes

Error JSON contains exactly `schema_version` and `error`; the nested `error` contains exactly `code` and `message`. Runtime failures such as a missing or ambiguous inspection target use `code: "runtime_error"` with an explanatory string. The cancellation error branch defines `code: "cancelled"` if a cancelled operation reaches error reporting. A JSON error object is not guaranteed for every nonzero exit: argument parsing errors write text to stderr without JSON, cancellation can exit immediately, and output failures may prevent a complete error from being written.

stdout contains only results; diagnostics go to stderr. Default `list`/`watch` JSON omits executable and application paths. `inspect` JSON replaces the current `HOME` prefix with `~`; paths outside that prefix remain visible, and project names may remain. Review it before sharing.

| Exit code | Meaning |
|---|---|
| 0 | Successful completion, even if some metrics are unavailable |
| 1 | Runtime failure, including collection, missing/ambiguous inspection targets, output, or cancellation-handler setup |
| 2 | Invalid arguments |
| 130 | Command cancelled, including Ctrl+C |

### Compatibility and schema history

Within the 1.x series, compatible schema 2 changes only add fields; existing fields are not deleted or given different meanings. Deleting a field or changing its meaning requires a higher `schema_version` and an explicit list of changes in the release notes. Consumers should accept additional fields. Exact-key contract tests deliberately also fail on additions, requiring maintainers to update the tests and both guides together.

Schema 2 removed `policy` from `list`/`watch`/`inspect`, `policy_error` and `policy_state_valid` from related outputs, `policy_summary` from `status`, and `protection_reasons` and `quit_supported` from process objects. `doctor` no longer exports `rule_storage` or `capability_gate_reason`. Its `capabilities` removed `rules_enabled`, `dry_run_enabled`, `cleanup_enabled`, `a1_enabled`, `a2_enabled`, `cleanup_session_enabled`, and `history_enabled`; it now contains only `read_only`, `ai_attribution_enabled`, and `background_service`.

Schema 2 also removed every `Metric.source`, retaining `value`/`status`/`reason`; sources are listed above. Groups gained `short_id`; full IDs in `id` and `process_ids` retain their meanings. `sampled_at_unix_ms` remains Unix milliseconds.

## License and source

Bree is free and its source is released under [GPL-3.0](../LICENSE). `bree license` displays the project license offline; `bree license --third-party` displays bundled third-party notices. Both support `--json`, sample no processes, and save no data. Matching source is available in the source assets and tags on [Releases](https://github.com/yuyongyan29-dev/Bree/releases).
