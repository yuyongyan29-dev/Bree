<p align="center">
  <img src="assets/bree-brand.png" alt="Bree：橙色睡眠小动物图标与 bree 字标" width="600">
</p>

<p align="center">
  <strong>看清内存占用，让每个选择都有依据。</strong><br>
  免费的 macOS 终端工具 · 本机查看 · 无需账号或订阅
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
  <a href="#安装">安装</a> ·
  <a href="#使用">使用</a> ·
  <a href="docs/cli.zh-CN.md">完整说明</a> ·
  <a href="https://github.com/yuyongyan29-dev/Bree/issues">反馈问题</a>
</p>

Bree 把系统内存压力、应用与进程占用放进一个键盘操作的终端界面。查看谁占用了内存，追到每个对象的归属和指标依据，也能用直接命令与 JSON 持续观察。

**Bree 是无持久状态的只读查看器。正式支持 macOS 27、Apple Silicon，已在维护者的 Mac（Mac17,3、Apple M5、macOS 27.0.1）上实测。包版本为 `0.4.0-alpha.1`。**

0.4 包含不兼容变更：移除 `clean`、`history`，JSON 使用 schema 2。Bree 不读取、迁移或删除此前 0.3 及更早版本留下的数据；不再需要时，可手动删除 `~/Library/Application Support/Bree`。

<!-- screenshot: TUI Home and Resources -->

## 安装

其他 macOS 版本未在维护者机器上实测，不在支持范围内。CI 在 macOS 15 上能编译并通过自动测试，仅作参考。Intel 不支持，curl 安装器会拒绝。

两种方式均使用预编译程序，**无需安装 Rust、Cargo、Python 或 Node.js**。安装与升级需联网，日常查看在本机完成。

### Homebrew

已有 Homebrew，安装后直接启动：

```sh
brew install yuyongyan29-dev/tap/bree
bree
```

Bree 的独立 tap 仅提供原生 Apple Silicon、macOS 27 的预编译 bottle。

### curl

不需要 Homebrew，使用 macOS 自带工具即可：

```sh
curl -fsSL https://raw.githubusercontent.com/yuyongyan29-dev/Bree/main/distribution/install.sh | sh
```

默认安装到 `~/.local/bin/bree`。如果这个目录已在 PATH 中，直接运行 `bree`；否则先执行：

```sh
export PATH="$HOME/.local/bin:$PATH"
bree
```

将同一行 `export` 加入 `~/.zshrc`，以后新终端也能直接运行。安装器校验 SHA-256 和版本后才替换旧程序，检查失败时保留原文件，不使用 sudo，也不改写 shell 配置。

升级、卸载、自定义目录与兼容范围见 [安装说明](docs/installation.zh-CN.md)。发行程序没有 Developer ID 签名或公证，干净账号安装仍待验证。

## 能做什么

| 你想了解 | Bree 提供 |
|---|---|
| Mac 的内存状况如何？ | 总量、已用量、压缩、交换空间与内存压力 |
| 哪个应用或进程占用了内存？ | 按有证据的应用归属分组，查看进程详情，按名称、bundle ID 或 PID 搜索 |
| 进程属于哪个 AI／开发工具？ | 按[已说明的可执行文件安装布局](docs/cli.zh-CN.md#development-labels)标注 ChatGPT.app 内置的 Codex CLI 与原生安装的 Claude Code，不锁定版本号；标签只解释安装来源，不代表任务已完成或内存可以安全回收 |
| 占用是否还在变化？ | 前台持续观察，以及文本、JSON／JSONL 输出 |

未知或无权限的数据明确标记，不填成零。

未归属进程按同一可执行路径与同一 UID 合并；路径不可读时按同名与同一 UID 合并，UID 不可读时保留独立实例。分组优先使用开发工具标签，原生 Claude Code 附带安装路径中的版本，例如 `Claude Code 2.1.294`，不执行程序查询版本。应用分组方式不变，聚合不代表已确认应用归属。

Home 首页的静态像素吉祥物不增加动画、后台进程或额外依赖，直接命令与 JSON 输出不含图案。

没有后台常驻或开机启动，也不提供强制结束或后台自动清理。

## 使用

```sh
bree                         # 打开终端界面
bree status                  # 系统内存概览
bree list --limit 20          # 应用与进程占用
bree list --search Safari --sort name  # 搜索匹配分组并按名称排序
bree inspect 12345            # 查看当前使用该 PID 的进程
bree inspect '<分组短 ID>'    # 查看 list 返回分组的当前成员
bree watch                   # 前台持续观察
bree doctor                  # 检查能力与数据可用性
bree license                  # 查看 GPL-3.0 许可
```

文本以 PID 标识进程，以稳定的短 ID 标识分组，TUI 也显示分组短 ID。`inspect` 接受 PID、分组短 ID 和完整 ID：PID 输入不检测跨命令的 PID 复用，完整进程 ID 仍绑定采样实例。短 ID 默认 8 位十六进制，同一完整样本内碰撞时自动加长，显示筛选不重新生成。对象缺失或有歧义时退出码为 1，`--json` 返回结构化错误。未归属分组的键不变时 ID 保持稳定，查看的是当前成员。

文本采样时间显示本地 `HH:MM:SS`，内存公式由 `doctor` 解释。分组详情先列名称、实例数和总 RSS。输出节选（数值仅作示例）：

```text
Sampled at: 14:32:08 · Collection 20 ms
Memory       Instances  Category      Name                 ID
128.0 MiB    2          Unattributed  Claude Code 2.1.294   8c73a1de
```

列表默认按内存占用排序，`--sort name` 可改为按分组名排序。查询忽略首尾空白与大小写，非纯数字查询按分组名、成员进程名或成员 bundle ID 做子串匹配；纯数字查询只按完整 PID 精确匹配，不匹配名称子串或 PID 前缀，不搜索路径、完整命令行或环境变量。

在 **Memory** 页按 **/** 编辑搜索，**Enter** 提交，**Esc** 取消编辑并保留原查询。非编辑状态下，**Esc** 先清空已应用的查询，之后再按返回首页。**Ctrl+U** 清空输入，**Backspace** 逐字符删除；编辑时 **Q** 作为文本输入，**Ctrl+C** 始终退出。应用查询后仍可分类筛选、排序与刷新，搜索只改变显示列表，不改变原有分类。

首页只有 **1. Memory** 入口，按 **Enter** 打开。**↑↓** 选择 · **Enter** 查看详情 · **R** 刷新 · 非搜索编辑状态下 **Q** 退出。最小交互尺寸为 48 列 × 16 行。

需要脚本输出时：

```sh
bree status --json
bree list --search Safari --sort name --json
bree watch --json --count 3
```

JSON 保持 `schema_version: 2`。指标包含 `value`、`status`、`reason`，来源在使用说明中集中记录；分组新增 `short_id`，`id` 与 `process_ids` 中的完整 ID 以及 Unix 毫秒字段 `sampled_at_unix_ms` 含义不变。`list` 的 `groups` 为查询、排序与 `--limit` 后的显示结果，`processes`、`coverage` 保留完整样本；`view` 元数据含 `search`、`sort`、`total_groups`、`matched_groups` 和 `shown_groups`。未知值为 `null`；stdout 只输出结果，诊断写入 stderr。命令选项、指标定义与 schema 变更见 [完整使用说明](docs/cli.zh-CN.md#输出契约)。

## 已知限制

- Bree 只读：不结束应用，不保存数据，也没有后台服务。
- 发行程序未做 Developer ID 签名、未公证，仅有 ad-hoc 签名；干净账号安装仍未验证。
- 正式支持仅限维护者 Mac 上实测的 macOS 27、Apple Silicon。其他 macOS 版本未在维护者机器上实测、不支持；macOS 15 CI 结果只作参考。Intel 不支持，curl 安装器会拒绝；Homebrew bottle 只提供 macOS 27。
- `inspect <pid>` 不检测两次命令之间的 PID 复用。需要绑定采样实例时使用完整进程 ID。
- `inspect --json` 可能包含项目名和当前 `HOME` 前缀之外的路径，分享前请检查。

## 反馈与贡献

遇到问题或有建议，欢迎提交 [Issue](https://github.com/yuyongyan29-dev/Bree/issues)。请附 Bree 版本、macOS 版本、架构和复现步骤，分享诊断前检查应用名称与本地路径。

构建与检查指引见 [中文贡献指南](CONTRIBUTING.zh-CN.md)。

安全漏洞请按 [SECURITY.md](SECURITY.md) 私下报告，不要公开开 Issue。

<details>
<summary>从源码构建</summary>

开发环境需要 Rust 1.96.0 与 Xcode 命令行工具，版本由 `rust-toolchain.toml` 和 `Cargo.lock` 固定。

```sh
git clone https://github.com/yuyongyan29-dev/Bree.git
cd Bree
cargo build --locked --release
./target/release/bree
```

</details>

## 许可

Bree 源码采用 [GPL-3.0](LICENSE)，免费提供，允许商业使用。分发修改版时，须遵守 GPL 并提供对应源码。第三方组件保留各自许可，见 [THIRD-PARTY-NOTICES.txt](THIRD-PARTY-NOTICES.txt)。
