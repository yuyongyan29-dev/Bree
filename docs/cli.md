# 使用 Bree

Bree 在终端查看本机系统内存与应用／进程占用，提供规则、预演和处理记录。当前版本为 `0.3.0-alpha.7`，已验证环境是 Apple Silicon、macOS 27.0.1。

**应用正常退出功能尚未启用，当前程序不会向应用发送退出请求。** 允许规则也不会产生可执行的自动候选，`clean` 只会生成零目标或跳过结果。当前版本不提供强制结束、后台自动清理或开机启动。

安装方法见 [安装说明](installation.md)，源码构建与贡献见 [中文贡献指南](../CONTRIBUTING.zh-CN.md)。

## 终端界面

运行 `bree` 打开英文界面。首页 Home 初始选中 Memory（内存占用）入口；初次分析后静置，资源页和 watch 默认每 2 秒刷新。

| 按键 | 操作 |
|---|---|
| ↑／↓、Enter | 选择与进入 |
| R | 刷新当前页面 |
| Esc | 取消搜索编辑；已有查询时先清空，再返回；处理会话中取消当次 |
| S | 设置 |
| Q／Ctrl+C | 非搜索编辑状态下 Q 退出，编辑时 Q 输入文本；Memory 页 Ctrl+C 始终退出，处理会话中先取消 |

最小交互尺寸为 48 列 × 16 行。Home 首页将紧凑的静态像素吉祥物放在 Bree 字标前，内存概览与菜单在下方左对齐，操作提示紧随菜单。

| 显示条件 | 首页标识 |
|---|---|
| 至少 60 列 × 26 行，且支持 RGB 或 256 色 | 20 列 × 10 行的静态像素吉祥物，紧接其后的三行 Bree 字标 |
| 至少 60 列 × 22 行，但不满足上面的条件 | 三行 Bree 字标 |
| 更小的可交互窗口 | 紧凑的单行 `bree` 字标 |

终端声明 truecolor／24bit／direct 等 RGB 能力时使用原像素配色；仅声明 `TERM` 为 `*256color` 时使用六级近似色。存在 `NO_COLOR`（包括空值）、`TERM` 为空／`dumb` 或没有支持的色彩声明时，隐藏吉祥物并使用对应尺寸的字标。这里的 `NO_COLOR` 只控制吉祥物，界面强调色仍保留。

图案内部的不透明像素采用自身颜色，图案之外的普通背景与文字继承终端默认色，适配浅色和深色终端。图案不播放动画，不启动后台工作线程，不需要额外字体或新的运行依赖；直接命令和 JSON 输出不包含图案。

首页选择 `1. Preview` 后按 Enter，或按 P 键，都打开只读预演，显示规则分类与逐项理由；Needs review 查看待确认项目；History 查看会话记录。

Memory 页按 `/` 进入搜索编辑，Enter 提交，Esc 取消编辑并保留原查询；Ctrl+U 清空编辑框，Backspace 逐字符删除。编辑时字母 Q 输入文本，Ctrl+C 始终退出。退出编辑后，有已应用查询时 Esc 先清空查询，再按 Esc 返回首页。应用查询后，Tab 仍切换分类筛选，O 切换内存／名称排序，R 刷新。搜索只影响显示，不修改分类或规则。

编辑框清空或修改后仍需 Enter 才应用。界面的 `N/M matching in filter` 中，N 为当前查询匹配数，M 为当前分类在查询前的分组数；无匹配时显示 `No matches in this filter`。

## 直接命令

```sh
bree status                  # 单次系统内存概览
bree status --json
bree list --limit 20          # 按应用归属分组查看占用
bree list --search Safari --sort name  # 搜索匹配分组并按名称排序
bree list --json
bree inspect '<对象 ID>'     # 使用 list 返回的 ID 查看当前实例
bree inspect '<对象 ID>' --json
bree watch                   # 前台持续观察
bree watch --json --count 3
bree doctor                  # 数据覆盖与能力说明
bree clean --dry-run          # 只读预演
bree clean --dry-run --json
bree history                 # 读取处理记录
bree history --json --limit 20
```

无交互终端时，裸 `bree` 输出帮助。`watch` 在交互终端显示界面，在管道中输出文本，`--json` 则逐行输出 JSON 快照。`--count` 限制采样次数，`--interval` 可指定 1–60 秒间隔。

`list` 默认使用 `--sort memory` 按内存占用排序，也可使用 `--sort name` 按分组名排序。`--search` 忽略查询首尾空白与大小写；非纯数字查询按应用分组名、成员进程名或成员 bundle ID 做子串匹配。纯数字查询只按完整 PID 精确匹配，不匹配名称子串或 PID 前缀。空查询不限制结果，不扫描路径、完整命令行或环境变量。

查询最多 128 个 Unicode 字符，原输入的首尾空白也计入长度；控制字符和双向格式控制符会被拒绝。TUI 和直接命令使用相同的查询范围与限制。

`list --limit` 限制查询和排序后显示的组数。JSON 的 `groups` 为显示结果，`processes`、`coverage` 和 `policy` 仍保留完整样本；搜索不重算分类，不改变规则。`inspect` 会重新采样，只查看仍匹配的当前实例；旧 ID 不会被重新绑定到替代进程，也不可当作停止凭据。

`doctor` 不申请系统权限、不发送退出请求，也不通过写文件探测数据目录；`write_status: not_probed` 不保证未来写入一定成功。普通查看无需额外权限，不可读取的进程指标会明确说明。

## 内存与归属

系统概览包括物理内存、已用量、压缩、swap 和内存压力。进程内存统一使用 RSS，表示当前驻留物理内存；它与系统“已用内存”口径不同，不能简单相加互相替代。内核压力等级也不保证与活动监视器的曲线完全相同。

CPU 首次采样只建立基线，后续采样使用实际时间差计算单核百分比。采样覆盖数和每个指标的有效性字段说明哪些数据可读。

应用分组基于主应用和 bundle 内安装位置等证据。只有普通应用（AppKit 激活策略为 Regular，通常显示在程序坞中）作为主应用；嵌套在主应用 bundle 内的 helper 应用并入该主应用，独立的菜单栏或后台应用不单独成组，显示为未归属进程，也不能设置规则。归属冲突与未知进程独立保留，每个进程只加入一组。

<a id="development-labels"></a>

### AI／开发工具标签

标签要求进程属于当前用户且实例身份有效，并且可执行文件路径符合以下布局：

| 标签 | 匹配的可执行文件路径 |
|---|---|
| Codex CLI | `/Applications/ChatGPT.app/Contents/Resources/codex-cli/CodexCLI.app/Contents/MacOS/codex` |
| Claude Code | `/Users/<user>/.local/share/claude/versions/<major.minor.patch>`，版本的三段均为 1–9 位 ASCII 数字 |

匹配安装布局，不读取运行时版本或锁定已观察过的版本号；Claude Code 的版本后缀、额外路径层级及其他安装位置不会匹配。标签只解释安装来源，不验证签名、任务所属项目、完成状态、共享关系或可安全回收性；未知的 node、python 等进程不会被猜测为某个 AI 任务。

## 允许与保护规则

在 Details（详情）页按 A 设置 Allow（允许）规则，按 P 设置 Protect（保护）规则。核对 bundle ID、应用安装路径与主可执行路径后 Enter 保存，Esc 取消。

按 S 打开 Settings（设置），Enter 查看规则范围，D 确认移除该条规则。移除只影响选中的规则，不回滚其他窗口的新修改。

规则针对确切应用安装，不使用应用名称、通配符、PID 或整棵子进程树。保护优先于允许；系统、前台、身份缺失、归属冲突、未知 helper 和 AI 开发工具保持保护／只读。

没有规则的可靠普通应用可能被归入 Needs review（待确认）分类，表示可以主动设置范围。**允许只表示你的规则选择；当前正常退出能力关闭，因此 Automatic（自动候选）仍为零。** 详情页单次退出也保持禁用。

## 预演、会话与历史

`clean --dry-run` 冻结当次采样与规则，显示分类数量和逐项理由，不执行退出请求。预演页保持当次结果，按 R 才重新采样预演。

交互终端可以运行 `bree clean` 查看处理界面。非交互批次需要显式提供 `--yes`：

```sh
bree clean --yes --json
```

`--yes` 不绕过保护、身份或能力检查，不能与 `--dry-run` 同用。当前能力关闭，无允许对象时记录零目标；命中允许规则的对象仍可能记录为跳过，不发送请求。零请求批次没有操作后资源数据，不声称释放内存。

会话期间只允许一个执行者。另一窗口的预演或会话占用时会明确失败，不会重复执行。Ctrl+C／Esc 取消当次，Q 先取消再退出；结果分别记录取消、跳过、错误和资源观察，不把提交请求当作退出成功。Bree 不会在拒绝或超时后自动强退。

`history` 只读取记录，不启动会话或重放请求。历史页 R 重新读取，Enter 查看单次结果；缺少结束记录时显示 `Unfinished`（JSON 状态为 `unfinished`），结果保持未知，损坏记录明确报错。

## JSON 与退出码

JSON 的 `schema_version` 为 1。数值内部使用字节，文本显示 MiB／GiB。每个指标包含 `value`、`status`、`source` 和 `reason`；未知或无权限时使用 `null`，不填零。

`list` 新增 `view` 元数据：`search` 为查询，`sort` 为排序方式，`total_groups` 为完整样本的分组总数，`matched_groups` 为查询匹配数，`shown_groups` 为限制数量后显示的分组数。`groups` 受查询、排序与 `--limit` 影响，`processes`、`coverage`、`policy` 保留完整样本，`schema_version` 仍为 1。

进程归属中的 `attribution.application.activation_policy` 记录主应用的激活策略，当前输出中只会出现 `regular`。

`list`／`watch` 输出含策略分类及有效性；`clean --dry-run` 含采样时间、规则修订、分类数量、逐项理由与 `read_only: true`。会话结果另含运行 ID、逐项事实、资源变化、取消标记和错误；缺失的操作后资源为 `null`。

stdout 只输出命令结果，诊断写 stderr。默认 `list`／`watch` JSON 省略路径；`inspect` JSON 将用户主目录替换为 `~`，仍可能包含项目名，分享前请检查。

| 退出码 | 含义 |
|---|---|
| 0 | 正常完成，包括零目标批次 |
| 1 | 核心读取、预演、存储、记录或资源复查错误 |
| 2 | 参数错误或非交互 clean 未提供 --yes |
| 3 | 存在拒绝、仍运行、未核验或其他未全部退出的结果 |
| 4 | 全部处理目标均在请求发送前跳过 |
| 130 | 命令被取消 |

普通查看成功时仍可有部分指标缺失。TUI 当次取消后留在结果页，界面退出码不能替代逐项结果。管道输出中途取消时，最后一行可能不完整，消费者应丢弃该行。

## 本地数据

默认数据目录是 `~/Library/Application Support/Bree`，可用绝对路径 `BREE_DATA_DIR` 指定其他目录。规则保存为 `state.json`，摘要与处理记录保存为 `journal.jsonl`；目录权限 0700，文件权限 0600。

普通查看、history 和 doctor 不创建文件，首次保存规则／预演／会话才初始化存储。规则文件上限 1 MiB，记录默认保留 7 天，总量上限 10 MiB，按完整记录轮转。记录不保存进程路径、完整命令行、环境变量、提示词或聊天内容。

配置损坏、版本未知或权限不安全时，占用查看继续，规则、预演与处理关闭；Bree 不会重置原文件。日志无法写入时不完成预演或提交规则。修复原文件后可在设置页按 R 重读；若错误指出文件已替换，请重新读取确认当前状态。

卸载程序会保留数据目录，详见 [安装说明](installation.md)。反馈问题可前往 [Issues](https://github.com/yuyongyan29-dev/Bree/issues)，分享诊断前检查本地信息。

## 许可与源码

Bree 免费提供，源码按 [GPL-3.0](../LICENSE) 发布。`bree license` 离线显示项目许可，`bree license --third-party` 显示程序附带的第三方声明；两者也支持 `--json`，不采样进程、不读取或创建用户规则。对应版本源码在 [Releases](https://github.com/yuyongyan29-dev/Bree/releases) 的源码附件与同名标签中提供。
