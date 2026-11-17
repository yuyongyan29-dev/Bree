# AGENTS.md

本文件适用于整个 Bree 仓库，补充全局代理规则。项目级代理说明维护在这里；如需 `CLAUDE.md` 兼容入口，只引用本文件，不复制规则。

## 项目定位与事实来源

- Bree 是轻量的 macOS 本机内存只读查看器，无持久状态，使用 Rust 实现 CLI 与键盘 TUI，只采集、展示和导出内存信息。AI／开发工具标签用于解释归属，不代表任务已完成或可以安全回收。
- 当前仓库是独立 CLI。不要因其他产品形态引入 Web 前端、本地 HTTP 服务、默认常驻进程或运行时语言环境。普通命令完成后退出，持续观察由资源页与显式 `watch` 承担。
- 实际行为先查看实现和测试，再核对 [使用说明](docs/cli.zh-CN.md)。安装与兼容范围查看 [安装说明](docs/installation.zh-CN.md)，开发流程查看 [贡献指南](CONTRIBUTING.zh-CN.md)。历史设计不能证明功能已经实现或平台已经验证。
- 工具链以 [rust-toolchain.toml](rust-toolchain.toml) 为准；包版本、依赖与锁定结果分别以 [Cargo.toml](Cargo.toml)、[Cargo.lock](Cargo.lock) 为准。不要把本机或 CI 测试通过扩大为所有 macOS 版本受支持。
- CLI 帮助、错误和 TUI 当前使用英文。用户行为或安装方式变化时同步更新 [英文 README](README.md)、[中文 README](README.zh-CN.md) 与相关 `docs/`，避免两种语言承诺不同能力。

## 源码入口

读取或修改 src/、tests/、scripts/、distribution/、Cargo.toml，或执行开发验证时，读取 [agent-development](docs/agent-development.md)。

### 指标、查询与输出

读取或修改 src/、tests/，或改变采集、查询、输出、终端行为时，读取 [agent-contracts](docs/agent-contracts.md)。

## 并行工作区与分支

- 默认一次只做一件事：在主仓库检出上从最新 `main` 开任务分支（以 `codex/` 等前缀命名）直接修改，不新建工作区。
- 只有确实需要同时开发时，才在 `../Bree-worktrees/` 下使用独立工作区，同一时间最多两条并行线。每个工作区与分支同一时间只有一个写入者。
- 交给代理（如 Codex）的任务说明写明：可以使用只读子 Agent（阅读代码、检索资料、审查改动）；构建、测试和修改文件只由主代理执行，不交给子 Agent；构建与测试限制并发，例如 `CARGO_BUILD_JOBS=4`。每个工作区有独立的 `target/`，依赖要全部重新编译，多个会话同时构建和测试会明显加重本机负载。
- 未经该工作区负责人同意，不提交、rebase、暂存或丢弃其他工作区的未提交改动。需要临时移动改动时先备份，完成后逐项核对。
- 提交、推送、rebase 或改写分支前，重新查看 `git status` 与最近提交；出现不是自己造成的文件、提交或分支变化时，停止受影响步骤并报告。
- 不开 PR：验收通过后直接提交并推送到 `main`。推送前把改动 rebase 到最新的 `origin/main`，按[验证方式](docs/agent-development.md)在 rebase 后的代码上运行对应检查（代码改动至少 fmt、严格 Clippy 与 `cargo test --locked --no-fail-fast`），全部通过后用普通 `git push`。
- 禁止对 `main` 强推（包括 `--force-with-lease`），不改写已推送的历史。推送被拒绝说明远端已有新提交：重新 rebase、复验后再推。
- `main` 的 CI 在推送后运行。失败时停止后续推送，用新的修复或回退提交处理。任务分支与工作区用完后另行清理本地分支。

## 仓库与发行

- 本地构建与实验输出放在已忽略的 `target/` 或 `.artifacts/`；不提交原始进程快照、个人路径、密钥、内部设计资料或未使用素材。不要因目录被忽略就删除用户已有内容。
- `distribution/package.sh` 仅准备本地发行资产。包版本取自 `Cargo.toml`，默认 curl 安装版本由 `distribution/latest-version.txt` 单独选择；不能仅因改了包版本就声称安装入口已经更新。
- 依赖改动核对 `Cargo.lock` 与 `THIRD-PARTY-NOTICES.txt`，声明生成入口是 `distribution/notices.py`。保留现有项目许可与随程序分发的声明。
- 安装器保持校验后原子替换、失败保留旧程序，不自动使用 sudo、改写 shell 配置、移除 quarantine 或绕过系统校验。
- 推送代码、本地打包、发布 GitHub Release 与更新 Homebrew tap 是不同动作，按用户授权范围执行并分别报告；本地成功不代表发行资产、默认安装入口或干净账号安装已验证。
