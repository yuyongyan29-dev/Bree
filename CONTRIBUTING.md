# 参与 Bree

欢迎报告问题、改善文档、提交修复或讨论功能。Bree 是免费的 macOS CLI，交互界面与直接命令共用采集、规则和处理逻辑。

## 报告问题

在 [Issues](https://github.com/yuyongyan29-dev/Bree/issues) 中提供：

- Bree 版本、macOS 版本与 CPU 架构。
- 安装方式、触发命令或界面操作、预期与实际结果。
- 可复现的最小步骤，以及必要的错误输出。

可以运行 `bree --version` 和 `bree doctor` 辅助定位。分享快照前检查应用名称、项目名和本地路径；请勿提交完整环境变量、聊天内容或个人数据。

## 开发环境

当前实测环境为 Apple Silicon、macOS 27.0.1。源码构建需要 Rust 1.96.0 和 Xcode 命令行工具；Python 3 仅用于安装器测试与性能／终端验证脚本，不是 Bree 的运行依赖。

```sh
git clone https://github.com/yuyongyan29-dev/Bree.git
cd Bree
git switch -c feat/your-change
cargo build --locked --release
./target/release/bree
```

工具链由 `rust-toolchain.toml` 固定，依赖由 `Cargo.lock` 固定。

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
