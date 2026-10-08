# 参与 Bree

[English](CONTRIBUTING.md) · [中文](CONTRIBUTING.zh-CN.md)

欢迎报告问题、改善文档、提交修复或讨论功能。Bree 是用于本机内存查看与解释的免费 macOS CLI，交互界面与直接命令共用采集、规则和处理逻辑。

## 报告问题

在 [Issues](https://github.com/yuyongyan29-dev/Bree/issues) 中提供：

- Bree 版本、macOS 版本与 CPU 架构。
- 安装方式、触发命令或界面操作、预期与实际结果。
- 可复现的最小步骤，以及必要的错误输出。

可以运行 `bree --version` 和 `bree doctor` 辅助定位。分享快照前检查应用名称、项目名和本地路径；请勿提交密钥、完整环境变量、聊天内容或个人数据。

安全漏洞请按 [安全报告说明](SECURITY.md) 私下报告，不要公开开 Issue。

## 开发环境

当前实测环境为 Apple Silicon、macOS 27.0.1。源码构建需要 Rust 1.96.0 和 Xcode 命令行工具；Python 3 仅用于安装器测试、脚本回归与性能／终端验证脚本，不是 Bree 的运行依赖。

```sh
git clone https://github.com/yuyongyan29-dev/Bree.git
cd Bree
git switch -c feat/your-change
cargo build --locked --release
./target/release/bree
```

工具链由 `rust-toolchain.toml` 固定，依赖由 `Cargo.lock` 固定。依赖更新需核对 `Cargo.lock` 与 `THIRD-PARTY-NOTICES.txt`，声明生成入口为 `distribution/notices.py`。

## 目录与检查

| 目录 | 内容 |
|---|---|
| `src/` | CLI、TUI、采集、策略、存储和处理会话 |
| `src/platform/` | macOS 能力封装及其他平台的能力说明 |
| `tests/` | 命令行为、真实采集与终端测试 |
| `scripts/` | 性能、终端和隔离退出实验 |
| `distribution/` | curl 安装器、发行准备与安装器测试 |
| `docs/` | 用户使用与安装说明 |

提交代码前运行相关检查：

```sh
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
```

改动安装器或发行工具时再运行：

```sh
sh -n distribution/install.sh distribution/package.sh
python3 distribution/tests/test_install.py
```

提交时请记录实际运行的检查与结果。安装器测试使用模拟下载，不访问远端，也不改写用户配置。

运行需要写入状态的手动验证时，可以指定隔离数据目录：

```sh
BREE_DATA_DIR="$PWD/.artifacts/dev-data" ./target/release/bree
```

只读稳定性与性能验收可运行统一入口。`--lock` 必须是所有并行工作区共用的绝对路径，且父目录已存在；不要为每个工作区创建不同锁或删除锁文件。`--output` 必须是本工作区 `target/` 或 `.artifacts/` 下的新目录，避免覆盖已有证据。

```sh
python3 scripts/stability-check.py \
  --lock /absolute/shared/native-experiment.lock \
  --output "$PWD/.artifacts/stability/run-001"
```

入口重新构建 release，运行现有 benchmark、终端、主题、信号检查，以及历史读取与 Home／资源页长测；它在实测期间持有排他锁，直到本次子进程回收。Home 静置 5 分钟，显式进入 TUI 资源页与独立 watch 各观察 10 分钟。约 10 MiB 的历史 fixture 位于本次输出目录，只调用历史读取。结果汇总保留源码清单、二进制 SHA、命令、退出码和原始日志，分别标记 failed、not-run 与 unknown；原始日志可能包含本机应用名称，不能提交。不要与构建、其他性能采样或原生 UI 实验同时运行测量。

脚本的独立回归检查为 `python3 -m unittest discover -s scripts/tests`，CI 也运行此命令；测试使用模拟子进程和临时目录，不依赖预构建的 `target/release/bree`、开发者终端配置或真实用户数据。PTY 检查不能替代真实终端的明暗主题、字体、小窗口与缩放视觉检查；统一入口将这项标为 not-run，另行记录实际检查。单机数据只证明当前构建在该系统上的观察结果，不能扩大发行支持范围或用于证明 A1 退出能力。

构建与本地实验产物保存在已忽略的 `target/` 或 `.artifacts/` 中，请勿提交个人路径或原始进程快照。

## 行为约定

- 缺失指标保持 `null` 并说明原因，不用零值代替。进程内存使用统一口径，不能重复统计分组。
- 对象 ID 用于查看当前实例，不可跨启动当作停止凭据。保护规则优先于允许规则。
- 实际 A1 正常退出能力当前硬关闭。不得把假 backend 测试通过视为原生能力验证，也不得引入强制结束兜底。
- 涉及退出能力的改动必须提供精确实例控制、前台保护、文档保存、取消、记录失败与重启路径的证据。能力不可靠的对象继续保持只读。
- JSON stdout 只输出结果，诊断写 stderr；保持已声明的 schema 与有效性字段。

指标与能力范围见 [使用说明](docs/cli.md)。独立退出实验只操作自建 fixture，不用现有用户应用作为隐式测试目标。

## 提交 Pull Request

使用独立分支，保持改动范围清楚。描述具体问题、修改后的行为、复现或验证步骤，以及尚未验证的范围。修复问题时引用对应 Issue；功能改动同步更新有关文档。

请不要把与 CLI 无关的产品设计、研究素材或未使用的构建产物加入仓库。

## 代码许可

提交的贡献按 Bree 的 GPL-3.0 许可发布。不要提交密钥、个人配置、内部开发文档、实验输出或设计素材。
