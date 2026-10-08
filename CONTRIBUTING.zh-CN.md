# 参与 Bree

[English](CONTRIBUTING.md) · [中文](CONTRIBUTING.zh-CN.md)

欢迎报告问题、改善文档、提交修复或讨论功能。Bree 是用于本机内存查看与解释的免费 macOS CLI，交互界面与直接命令共用采集、规则和预演逻辑。

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
| `src/` | CLI、TUI、采集、策略、存储和预演 |
| `src/platform/` | macOS 能力封装及其他平台的能力说明 |
| `tests/` | 命令行为、真实采集与终端测试 |
| `scripts/` | 性能与终端验证 |
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
sh -n distribution/install.sh distribution/package.sh distribution/notarize.sh
python3 distribution/tests/test_install.py
python3 distribution/tests/test_release.py
```

提交时请记录实际运行的检查与结果。安装器和发行工具测试使用模拟下载、构建、签名工具与公证响应，不访问远端、真实钥匙串或用户配置。模拟的 Accepted 响应不构成公证证据。

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

入口重新构建 release，运行现有 benchmark、终端、主题、信号检查，以及 Home／资源页长测；它在实测期间持有排他锁，直到本次子进程回收。Home 静置 5 分钟，显式进入 TUI 资源页与独立 watch 各观察 10 分钟。结果汇总保留源码清单、二进制 SHA、命令、退出码和原始日志，分别标记 failed、not-run 与 unknown；原始日志可能包含本机应用名称，不能提交。不要与构建、其他性能采样或原生 UI 实验同时运行测量。

脚本的独立回归检查为 `python3 -m unittest discover -s scripts/tests`，CI 也运行此命令；测试使用模拟子进程和临时目录，不依赖预构建的 `target/release/bree`、开发者终端配置或真实用户数据。PTY 检查不能替代真实终端的明暗主题、字体、小窗口与缩放视觉检查；统一入口将这项标为 not-run，另行记录实际检查。单机数据只证明当前构建在该系统上的观察结果，不能扩大发行支持范围。

构建与本地实验产物保存在已忽略的 `target/` 或 `.artifacts/` 中，请勿提交个人路径或原始进程快照。

## 签名发行准备

已发布的 Alpha 仍为 ad-hoc 签名、未公证。以下维护者工具用于准备后续发行；加入工具不会改变已发布资产或安装承诺。

存在并行工作区时，在构建、打包命令或实测**开始前**，使用 `fcntl.flock` 取得与稳定性入口相同绝对路径的排他锁，并持有至子进程退出。打包脚本不会自动取得这把共享锁；它的输出目录锁只防止两个写入者同时向同一目录打包。

`distribution/package.sh` 构建锁定依赖的 release，检查 arm64 可执行文件、版本与系统库依赖，然后准备本地资产。未指定签名选项时，保留 linker 的 ad-hoc 签名和原有输出文件集合。使用证书 SHA-1 或钥匙串中的完整身份名称签名暂存副本：

```sh
sh distribution/package.sh \
  --sign-identity 'CERTIFICATE_SHA1_OR_FULL_NAME' \
  --output "$PWD/.artifacts/distribution/signed-candidate"
```

脚本要求 hardened runtime 与在线安全时间戳，严格验证签名，输出 Authority 链、TeamIdentifier 与 runtime 标志，并在签名后重新检查可执行文件。签名或时间戳失败即停止，不回退到未签名资产。二进制 SHA-256 与生成的 Formula 对应最终签名后的字节，`bree-version.txt` 对应已核对的版本；已有输出文件不会被替换。

公开发行需要 **Developer ID Application** 证书及本机钥匙串中的私钥。Apple Development 证书可用于验证本机签名流程，但产物不得发布。脚本根据实际签名的 Authority 分类，不根据传入的身份字符串猜测。若 macOS 询问是否允许 `codesign` 访问私钥，需要用户响应；不要修改钥匙串访问设置或导出私钥来绕过弹窗。

公证前，使用 `xcrun notarytool store-credentials 'bree-notary'` 交互式建立 profile，只在安全提示中输入凭据。密码与 API 私钥内容不得放在命令参数、环境变量、仓库文件或日志中。Developer ID 候选产物与 profile 就绪后，可单独执行以下**上传至 Apple** 的操作：

```sh
sh distribution/notarize.sh \
  --binary "$PWD/.artifacts/distribution/signed-candidate/bree-aarch64-apple-darwin" \
  --keychain-profile 'bree-notary' \
  --output "$PWD/.artifacts/distribution/notary-candidate"
```

输出目录必须是新目录。脚本冻结副本，拒绝非 Developer ID 签名、缺少 hardened runtime 或安全时间戳的文件，使用 `ditto -c -k --keepParent` 创建 zip，再运行 `notarytool submit --wait`。私有证据目录保留二进制摘要、签名、submission ID／状态、命令退出码和公证日志。任何错误或非 Accepted 状态都算失败；Accepted 也必须取得日志并检查警告。30 分钟等待超时不会取消 Apple 的后台处理，重试前先检查已保存的提交。不要提交这些本地产物。

分别核验**签名、公证提交、Accepted 状态与 Gatekeeper 放行**。独立二进制和 zip 不能 staple；Accepted 不证明干净离线环境可以启动。针对实际 curl 下载与 Homebrew bottle，检查 `codesign -dvvv`、`codesign --verify --strict`、`spctl -a -t exec -vv` 和 `xattr -l`，再在干净标准账号测试全新安装、升级、失败保留旧程序和卸载。记录在线／离线行为，不移除 quarantine 或绕过系统校验。参考 Apple 的[公证要求](https://developer.apple.com/documentation/security/notarizing-macos-software-before-distribution)与[自定义流程](https://developer.apple.com/documentation/security/customizing-the-notarization-workflow)。

在可丢弃的 Homebrew 环境执行 `brew install --build-bottle <tap>/bree` 与 `brew bottle <tap>/bree`。比较签名候选、安装后的二进制与 bottle 解包后二进制的 SHA-256 和签名信息；测试安装 bottle 后再次比较。Homebrew 可能修改需要重定位的二进制并重新签名，因此打包成功不等于保留原签名。字节或 Authority 变化时停止。不要用用户已有的全局安装做这个实验。参考 [Homebrew bottle 文档](https://docs.brew.sh/Bottles)。

本地打包、公证提交、发布 GitHub Release、更新 tap 和修改 `distribution/latest-version.txt` 是独立动作。包版本来自 `Cargo.toml`，默认 curl 版本由 `distribution/latest-version.txt` 单独选择。准备匹配的源码与许可资产，核验最终分发形式后，再按授权范围发布。

## 行为约定

- 缺失指标保持 `null` 并说明原因，不用零值代替。进程内存使用统一口径，不能重复统计分组。
- 对象 ID 用于查看当前实例，不可跨启动当作停止凭据。保护规则优先于允许规则。
- Bree 不会结束应用。规则和预演不能启用正常退出、强制结束或后台清理。
- JSON stdout 只输出结果，诊断写 stderr；保持已声明的 schema 与有效性字段。

指标与能力范围见 [使用说明](docs/cli.md)。

## 提交 Pull Request

使用独立分支，保持改动范围清楚。描述具体问题、修改后的行为、复现或验证步骤，以及尚未验证的范围。修复问题时引用对应 Issue；功能改动同步更新有关文档。

请不要把与 CLI 无关的产品设计、研究素材或未使用的构建产物加入仓库。

## 代码许可

提交的贡献按 Bree 的 GPL-3.0 许可发布。不要提交密钥、个人配置、内部开发文档、实验输出或设计素材。
