# 使用 Bree

Bree 是无持久状态的只读查看器，在终端采集、展示和导出本机系统内存与应用／进程占用。包版本为 `0.4.0-alpha.1`，已验证环境是 Apple Silicon、macOS 27.0.1。

**Bree 不会结束应用或保存数据。** 0.4 起只保留 `status`、`list`、`inspect`、`watch`、`doctor`、`license`，移除 `clean` 与 `history`，JSON 使用 schema 2。Bree 不提供后台服务或开机启动。

安装方法见 [安装说明](installation.md)，源码构建与贡献见 [中文贡献指南](../CONTRIBUTING.zh-CN.md)。

## 终端界面

运行 `bree` 打开英文界面。首页 Home 初始选中 Memory（内存占用）入口；初次分析后静置，资源页和 watch 默认每 2 秒刷新。

| 按键 | 操作 |
|---|---|
| ↑／↓、Enter | 选择与进入 |
| R | 刷新当前页面 |
| Esc | 取消搜索编辑；已有查询时先清空，再返回 |
| Q／Ctrl+C | 非搜索编辑状态下 Q 退出，编辑时 Q 输入文本；Ctrl+C 始终退出 |

最小交互尺寸为 48 列 × 16 行。Home 首页将紧凑的静态像素吉祥物放在 Bree 字标前，内存概览与菜单在下方左对齐，操作提示紧随菜单。

| 显示条件 | 首页标识 |
|---|---|
| 至少 60 列 × 26 行，且支持 RGB 或 256 色 | 20 列 × 10 行的静态像素吉祥物，紧接其后的三行 Bree 字标 |
| 至少 60 列 × 22 行，但不满足上面的条件 | 三行 Bree 字标 |
| 更小的可交互窗口 | 紧凑的单行 `bree` 字标 |

终端声明 truecolor／24bit／direct 等 RGB 能力时使用原像素配色；仅声明 `TERM` 为 `*256color` 时使用六级近似色。存在 `NO_COLOR`（包括空值）、`TERM` 为空／`dumb` 或没有支持的色彩声明时，隐藏吉祥物并使用对应尺寸的字标。这里的 `NO_COLOR` 只控制吉祥物，界面强调色仍保留。

图案内部的不透明像素采用自身颜色，图案之外的普通背景与文字继承终端默认色，适配浅色和深色终端。图案不播放动画，不启动后台工作线程，不需要额外字体或新的运行依赖；直接命令和 JSON 输出不包含图案。

首页只有 `1. Memory` 入口，按 Enter 打开内存占用页，按 Q 退出。Memory 页按 Enter 查看选中对象的详情。

Memory 页按 `/` 进入搜索编辑，Enter 提交，Esc 取消编辑并保留原查询；Ctrl+U 清空编辑框，Backspace 逐字符删除。编辑时字母 Q 输入文本，Ctrl+C 始终退出。退出编辑后，有已应用查询时 Esc 先清空查询，再按 Esc 返回首页。应用查询后，Tab 仍切换分类筛选，O 切换内存／名称排序，R 刷新。搜索只影响显示，不修改分类。

编辑框清空或修改后仍需 Enter 才应用。界面的 `N/M matching in filter` 中，N 为当前查询匹配数，M 为当前分类在查询前的分组数；无匹配时显示 `No matches in this filter`。

## 直接命令

```sh
bree status                  # 单次系统内存概览
bree status --json
bree list --limit 20          # 按应用归属分组查看占用
bree list --search Safari --sort name  # 搜索匹配分组并按名称排序
bree list --json
bree inspect 12345            # 重新采样当前 PID，不检测跨命令的 PID 复用
bree inspect '<分组短 ID>'    # 使用 list 返回的短 ID 查看当前成员
bree inspect '<完整 ID>' --json
bree watch                   # 前台持续观察
bree watch --json --count 3
bree doctor                  # 数据覆盖与能力说明
bree license                 # 离线查看项目许可
```

无交互终端时，裸 `bree` 输出帮助。`watch` 在交互终端显示界面，在管道中输出文本，`--json` 则逐行输出 JSON 快照。`--count` 限制采样次数，`--interval` 可指定 1–60 秒间隔。

`list` 默认使用 `--sort memory` 按内存占用排序，也可使用 `--sort name` 按分组名排序。`--search` 忽略查询首尾空白与大小写；非纯数字查询按应用分组名、成员进程名或成员 bundle ID 做子串匹配。纯数字查询只按完整 PID 精确匹配，不匹配名称子串或 PID 前缀。空查询不限制结果，不扫描路径、完整命令行或环境变量。

查询最多 128 个 Unicode 字符，原输入的首尾空白也计入长度；控制字符和双向格式控制符会被拒绝。TUI 和直接命令使用相同的查询范围与限制。

`list --limit` 限制查询和排序后显示的组数。JSON 的 `groups` 为显示结果，`processes`、`coverage` 仍保留完整样本；搜索不重算分类。

`inspect` 接受 PID、分组短 ID 和完整 ID，每次都会重新采样：

- **PID**：查看当前使用该 PID 的进程，不检测两次命令之间的 PID 复用。采集期间原有的实例一致性检查不变。
- **分组短 ID**：文本 `list` 与 TUI 显示由完整分组 ID 稳定派生的 8 位小写十六进制 ID；同一样本内发生碰撞时，相关 ID 自动加长到可区分。短 ID 在完整样本上生成，搜索、排序和截断不会改变它；碰撞集合变化时长度可能改变，以当前 `list` 为准。只接受完整的当前短 ID，不支持任意前缀。
- **完整 ID**：继续用于 JSON 的 `id`、组内 `process_ids` 和 TUI 的精确选中。完整进程 ID 绑定启动会话、PID 与启动时间；失效后不会映射到替代进程。未归属分组的键不变时，完整分组 ID 不随成员增减改变，查看的是当前成员，不代表固定的一批进程实例。

PID 与短 ID 同时匹配、短 ID 匹配多个分组或找不到对象时，命令失败，退出码为 1；`--json` 输出 schema 2 的 `error.code: runtime_error` 和说明文字，诊断仍写 stderr。歧义时可使用 `list --json` 中的完整 ID。所有 ID 只用于查看，不是停止凭据。

进程文本以 PID 标识；分组 `inspect` 的首行先列名称、实例数和总 RSS，再列成员。`status`、`list`、`inspect` 和文本 `watch` 的采样时间显示系统本地时区的 `HH:MM:SS`，JSON 的 `sampled_at_unix_ms` 保持 Unix 毫秒。系统内存公式只在 `doctor` 文本和 JSON notes 中解释；JSON `system.used_definition` 仍保留。

`doctor` 说明数据覆盖与采集能力，不申请系统权限，也不创建或检查持久状态。普通查看无需额外权限，不可读取的进程指标会明确说明。

## 内存与归属

系统概览包括物理内存、已用量、压缩、swap 和内存压力。进程内存统一使用 RSS，表示当前驻留物理内存；它与系统“已用内存”口径不同，不能简单相加互相替代。内核压力等级也不保证与活动监视器的曲线完全相同。

CPU 首次采样只建立基线，后续采样使用实际时间差计算单核百分比。采样覆盖数和每个指标的有效性字段说明哪些数据可读。

应用分组基于主应用和 bundle 内安装位置等证据。只有普通应用（AppKit 激活策略为 Regular，通常显示在程序坞中）作为主应用；嵌套在主应用 bundle 内的 helper 应用并入该主应用，独立菜单栏、后台及归属冲突的进程仍保持未归属，按同一可执行路径与同一 UID 聚合展示；路径不可读时按同名与同一 UID 合并。不同 UID 不合并，UID 不可读时保留独立实例。每个进程只属于一个分组，合并不代表已确认应用归属。

<a id="development-labels"></a>

### AI／开发工具标签

标签要求进程属于当前用户且实例身份有效，并且可执行文件路径符合以下布局：

| 标签 | 匹配的可执行文件路径 |
|---|---|
| Codex CLI | `/Applications/ChatGPT.app/Contents/Resources/codex-cli/CodexCLI.app/Contents/MacOS/codex` |
| Claude Code | `/Users/<user>/.local/share/claude/versions/<major.minor.patch>`，版本的三段均为 1–9 位 ASCII 数字 |

无应用归属的分组优先使用可靠的开发工具标签。原生 Claude Code 的名称附带已匹配安装路径中的版本，例如 `Claude Code 2.1.294`；不执行程序查询版本，Codex CLI 保持标签名。匹配安装布局，不读取运行时版本或锁定已观察过的版本号；Claude Code 的版本后缀、额外路径层级及其他安装位置不会匹配。标签只解释安装来源，不验证签名、任务所属项目、完成状态、共享关系或可安全回收性；未知的 node、python 等进程不会被猜测为某个 AI 任务。

### 指标来源

| 指标 | macOS 数据来源与计算方式 |
|---|---|
| `system.total_bytes` | `sysctl hw.memsize`，物理内存字节数 |
| `system.used_bytes` | `host_statistics64(HOST_VM_INFO64)`：`(internal_page_count.saturating_sub(purgeable_count) + wire_count + compressor_page_count) × sysconf(_SC_PAGESIZE)` |
| `system.compressed_bytes` | 同一 VM 计数器中的 `compressor_page_count × page_size`，实际压缩占用 |
| `system.cached_bytes` | 同一 VM 计数器中的 `(external_page_count + purgeable_count) × page_size`，估计缓存 |
| `system.swap_used_bytes` | `sysctl vm.swapusage` 的 `xsu_used` 字节数 |
| `system.pressure` | `sysctl kern.memorystatus_vm_pressure_level`：1／2／4 对应 Normal／Elevated／High |
| `processes[].memory_bytes` | `libproc PROC_PIDTASKINFO.pti_resident_size`，RSS 字节数 |
| `processes[].cpu_one_core_percent` | `PROC_PIDTASKINFO` 的 user + system Mach ticks，经 `mach_timebase_info` 转换后，以同一实例的 CPU 时间增量除以单调时钟的实际间隔，再乘 100；首次采样只建立基线 |
| `groups[].memory_bytes` | 组内不同实例的 RSS 总和；单成员保留该成员的有效性与原因，多成员有缺失或求和溢出时总量为 unknown |

## JSON 与退出码

JSON 的 `schema_version` 为 2。数值内部使用字节，文本显示 MiB／GiB。每个指标包含 `value`、`status` 和 `reason`；未知或无权限时使用 `null`，不填零。

`list --json` 的顶层键为 `schema_version`、`sampled_at_unix_ms`、`collected_in_ms`、`system`、`processes`、`groups`、`coverage`、`diagnostics` 和 `view`。

`view` 元数据中：`search` 为查询，`sort` 为排序方式，`total_groups` 为完整样本的分组总数，`matched_groups` 为查询匹配数，`shown_groups` 为限制数量后显示的分组数。`groups` 受查询、排序与 `--limit` 影响，`processes`、`coverage` 保留完整样本。`watch --json` 按行输出完整快照，不含 `view`。

进程归属中的 `attribution.application.activation_policy` 记录主应用的激活策略，当前输出中只会出现 `regular`。

schema 2 移除了 `list`／`watch`／`inspect` 的 `policy`、所有相关输出的 `policy_error`、`policy_state_valid`，以及 `status` 的 `policy_summary` 和进程对象的 `protection_reasons`、`quit_supported`。`doctor` 不再输出 `rule_storage` 或 `capability_gate_reason`；其 `capabilities` 移除了 `rules_enabled`、`dry_run_enabled`、`cleanup_enabled`、`a1_enabled`、`a2_enabled`、`cleanup_session_enabled`、`history_enabled`，现在只含 `read_only`、`ai_attribution_enabled` 和 `background_service`。

schema 2 还移除了所有 `Metric.source`，保留 `value/status/reason`，来源统一见上表；分组新增 `short_id`，`id` 与 `process_ids` 的完整 ID 含义不变。`sampled_at_unix_ms` 仍为 Unix 毫秒时间戳。

stdout 只输出命令结果，诊断写 stderr。默认 `list`／`watch` JSON 省略路径；`inspect` JSON 将用户主目录替换为 `~`，仍可能包含项目名，分享前请检查。

| 退出码 | 含义 |
|---|---|
| 0 | 正常完成 |
| 1 | 核心读取或输出错误 |
| 2 | 参数错误 |
| 130 | 命令被取消 |

普通查看成功时仍可有部分指标缺失。管道输出中途取消时，最后一行可能不完整，消费者应丢弃该行。

## 许可与源码

Bree 免费提供，源码按 [GPL-3.0](../LICENSE) 发布。`bree license` 离线显示项目许可，`bree license --third-party` 显示程序附带的第三方声明；两者也支持 `--json`，不采样进程、不保存数据。对应版本源码在 [Releases](https://github.com/yuyongyan29-dev/Bree/releases) 的源码附件与同名标签中提供。
