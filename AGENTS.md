# AGENTS.md

本文件适用于整个 Bree 仓库，补充全局代理规则。项目级代理说明维护在这里；如需 `CLAUDE.md` 兼容入口，只引用本文件，不复制规则。

## 项目定位与事实来源

- Bree 是轻量的 macOS 本机内存只读查看器，无持久状态，使用 Rust 实现 CLI 与键盘 TUI，只采集、展示和导出内存信息。AI／开发工具标签用于解释归属，不代表任务已完成或可以安全回收。
- 当前仓库是独立 CLI。不要因其他产品形态引入 Web 前端、本地 HTTP 服务、默认常驻进程或运行时语言环境。普通命令完成后退出，持续观察由资源页与显式 `watch` 承担。
- 实际行为先查看实现和测试，再核对 [使用说明](docs/cli.zh-CN.md)。安装与兼容范围查看 [安装说明](docs/installation.zh-CN.md)，开发流程查看 [贡献指南](CONTRIBUTING.zh-CN.md)。历史设计不能证明功能已经实现或平台已经验证。
- 工具链以 [rust-toolchain.toml](rust-toolchain.toml) 为准；包版本、依赖与锁定结果分别以 [Cargo.toml](Cargo.toml)、[Cargo.lock](Cargo.lock) 为准。不要把本机或 CI 测试通过扩大为所有 macOS 版本受支持。
- CLI 帮助、错误和 TUI 当前使用英文。用户行为或安装方式变化时同步更新 [英文 README](README.md)、[中文 README](README.zh-CN.md) 与相关 `docs/`，避免两种语言承诺不同能力。

## 源码入口

| 改动内容 | 主要入口 |
|---|---|
| 参数、命令分发、退出码 | `src/main.rs` |
| TUI 页面、输入、刷新与终端恢复 | `src/tui.rs` |
| 首页标识与静态像素素材 | `src/brand.rs`、`assets/bree-mascot.txt` |
| 数据模型、指标有效性、实例身份 | `src/model.rs` |
| 采集、分组与开发工具归属 | `src/collect.rs`、`src/attribution.rs`、`src/platform/macos.rs` |
| 搜索与排序、文本和 JSON 导出 | `src/query.rs`、`src/output.rs` |
| 集成验证 | `tests/`、`scripts/` |
| 安装、打包与依赖许可声明 | `distribution/`、`.github/workflows/ci.yml` |

CLI 与 TUI 共用采集、查询和输出逻辑；修复共同问题时改共享模块，避免各自维护一套行为。macOS 专属能力留在平台适配层，AppKit 控制对象保留在主线程，不传入采样工作线程。

## 不可绕过的行为边界

### 指标、查询与输出

- 未知、无权限、不支持、失效与真实零值分开处理，保留 `Metric` 的 `value/status/reason`。内部使用字节，文本使用 MiB／GiB；进程统一使用 RSS，不把分组总量当作系统已用或可回收内存。
- 每个进程只属于一个分组；归属冲突与未知对象明确保留。不依据名称或高内存占用推断某个 AI 任务已完成。CPU 首次采样只建基线，后续使用实际时间差。
- 只有 AppKit 激活策略为 Regular 的普通应用能作为分组主应用；嵌套在其 bundle 内的 helper 应用并入该应用，菜单栏、后台应用及归属冲突进程保持未归属，但按同一可执行路径与同一 UID 聚合展示；路径不可读时按同名与同一 UID 聚合，UID 不可读时保留独立实例。无应用归属的分组优先使用可靠开发工具标签，原生 Claude Code 名称附带安装路径中的版本。开发工具标签按安装布局匹配，不锁定单个版本号；具体规则见 [使用说明](docs/cli.zh-CN.md)。
- 搜索和排序使用共享 `src/query.rs`。搜索仅改变显示结果，不改变分类；`list` 的 `groups` 可筛选和截断，`processes`、`coverage` 保留完整样本。
- 保持 JSON schema 2、有效性字段和退出码契约；不静默改变现有字段含义。stdout 只输出结果，诊断写 stderr，`watch --json` 保持 JSONL。变更对应检查在 `tests/cli_contract.rs`。
- 文本采样时间使用 `libc::localtime_r` 转换为本地 `HH:MM:SS`，JSON 保留 `sampled_at_unix_ms`。指标来源集中记录在 `docs/cli.zh-CN.md`；内存公式仅在 doctor 文本和 JSON notes 中解释，JSON `system.used_definition` 保留。
- 文本进程使用 PID，分组使用稳定派生的短 ID，完整样本内碰撞时加长；显示筛选不重算短 ID。JSON 分组含 `short_id`，完整 `id` 继续用于 JSON 和 TUI 精确选中。`inspect` 接受 PID、短 ID 与完整 ID；PID 输入不检测跨命令的复用，完整进程 ID 失效不得重绑定，歧义或缺失返回结构化错误。未归属分组 ID 在分组键不变时保持稳定，inspect 展示当前成员。
- 复用 `safe_text` 和路径脱敏逻辑处理外部名称与输出；默认导出不暴露路径，不采集或记录完整命令行、环境变量、提示词与聊天内容。

### 终端

- Home 首次采样后静置；不要为了动画或装饰添加持续扫描。资源页与 watch 的刷新行为保持可控。
- 退出、取消、信号、终端断开和错误路径都要恢复终端模式与文件标志，并回收采样线程。改输入时保留搜索编辑与普通导航的按键区别；改布局时覆盖最小 48×16、窗口缩放及浅／深色终端。

## 验证方式

代码改动按 [CI](.github/workflows/ci.yml) 和贡献指南运行：

```sh
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
```

需要运行 release 二进制或验证打包时先执行 `cargo build --locked --release`，不要把旧构建的运行结果当作本次改动的验证。

定位失败时改用 `cargo test --locked --no-fail-fast`：默认命令在第一个失败的测试二进制后停止，后续集成测试不会运行，结果会被隐藏。

- 新增或修改的测试不依赖开发者终端（`TERM`、`COLORTERM`、`NO_COLOR`）或真实用户目录：启动子进程时显式设置这些变量与独立的临时 `HOME`，单元测试在代码中固定相关配置。否则可能出现本机失败而 CI 通过（或相反）的结果。
- TUI 输入、主题或终端生命周期改动：按影响选用 `scripts/terminal-check.py`、`scripts/theme-check.py`、`scripts/signal-check.py`，结合 `tests/tui_signals.rs`；PTY 测试不能代替真实终端的视觉检查。
- 采集或刷新性能改动：使用 `tests/collection_live.rs`、`tests/cpu_load.rs` 及相应的 benchmark／soak 脚本，记录平台与实际测量结果。先阅读脚本参数与副作用，再运行。
- 安装器或发行工具改动增加以下检查，安装器测试使用模拟下载：

  ```sh
  sh -n distribution/install.sh distribution/package.sh
  python3 distribution/tests/test_install.py
  ```

- 纯文档改动核对事实、路径与 Markdown，并运行 `git diff --check`，无需无关的构建或原生实验。交付说明只报告实际执行的验证，注明未验证的平台或交互。

## 并行工作区与分支

- 默认一次只做一件事：在主仓库检出上从最新 `main` 开任务分支（以 `codex/` 等前缀命名）直接修改，不新建工作区。
- 只有确实需要同时开发时，才在 `../Bree-worktrees/` 下使用独立工作区，同一时间最多两条并行线。每个工作区与分支同一时间只有一个写入者。
- 交给代理（如 Codex）的任务说明写明：可以使用只读子 Agent（阅读代码、检索资料、审查改动）；构建、测试和修改文件只由主代理执行，不交给子 Agent；构建与测试限制并发，例如 `CARGO_BUILD_JOBS=4`。每个工作区有独立的 `target/`，依赖要全部重新编译，多个会话同时构建和测试会明显加重本机负载。
- 未经该工作区负责人同意，不提交、rebase、暂存或丢弃其他工作区的未提交改动。需要临时移动改动时先备份，完成后逐项核对。
- 提交、推送、rebase 或改写分支前，重新查看 `git status` 与最近提交；出现不是自己造成的文件、提交或分支变化时，停止受影响步骤并报告。
- 不开 PR：验收通过后直接提交并推送到 `main`。推送前把改动 rebase 到最新的 `origin/main`，按上文“验证方式”在 rebase 后的代码上运行对应检查（代码改动至少 fmt、严格 Clippy 与 `cargo test --locked --no-fail-fast`），全部通过后用普通 `git push`。
- 禁止对 `main` 强推（包括 `--force-with-lease`），不改写已推送的历史。推送被拒绝说明远端已有新提交：重新 rebase、复验后再推。
- `main` 的 CI 在推送后运行。失败时停止后续推送，用新的修复或回退提交处理。任务分支与工作区用完后另行清理本地分支。

## 仓库与发行

- 本地构建与实验输出放在已忽略的 `target/` 或 `.artifacts/`；不提交原始进程快照、个人路径、密钥、内部设计资料或未使用素材。不要因目录被忽略就删除用户已有内容。
- `distribution/package.sh` 仅准备本地发行资产。包版本取自 `Cargo.toml`，默认 curl 安装版本由 `distribution/latest-version.txt` 单独选择；不能仅因改了包版本就声称安装入口已经更新。
- 依赖改动核对 `Cargo.lock` 与 `THIRD-PARTY-NOTICES.txt`，声明生成入口是 `distribution/notices.py`。保留现有项目许可与随程序分发的声明。
- 安装器保持校验后原子替换、失败保留旧程序，不自动使用 sudo、改写 shell 配置、移除 quarantine 或绕过系统校验。
- 推送代码、本地打包、发布 GitHub Release 与更新 Homebrew tap 是不同动作，按用户授权范围执行并分别报告；本地成功不代表发行资产、默认安装入口或干净账号安装已验证。
