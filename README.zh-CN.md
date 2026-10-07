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
  <a href="docs/cli.md">完整说明</a> ·
  <a href="https://github.com/yuyongyan29-dev/Bree/issues">反馈问题</a>
</p>

Bree 把系统内存压力、应用与进程占用放进一个键盘操作的终端界面。查看谁占用了内存，追到每个对象的归属和指标依据，也能用直接命令与 JSON 持续观察。

**当前为 `0.3.0-alpha.2`：已验证原生 Apple Silicon、macOS 27.0.1，其他系统组合尚待验证。应用正常退出功能未启用，本版本不会结束应用；规则和清理入口提供预演与会话摘要。**

## 安装

两种方式均使用预编译程序，**无需安装 Rust、Cargo、Python 或 Node.js**。安装与升级需联网，日常查看在本机完成。

### Homebrew

已有 Homebrew，安装后直接启动：

```sh
brew install yuyongyan29-dev/tap/bree
bree
```

通过 Bree 的独立 tap 安装预编译 bottle，目前面向原生 Apple Silicon、macOS 27；其他版本尚未验证。

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

升级、卸载、自定义目录与兼容范围见 [安装说明](docs/installation.md)。当前程序未完成 Developer ID 签名与公证，干净账号安装仍待验证。

## 能做什么

| 你想了解 | Bree 提供 |
|---|---|
| Mac 的内存状况如何？ | 总量、已用量、压缩、交换空间与内存压力 |
| 哪个应用或进程占用了内存？ | 按有证据的应用归属分组，查看实例、安装位置与指标来源 |
| 占用是否还在变化？ | 前台持续观察，以及文本、JSON／JSONL 输出 |
| 某个对象为什么被保护或跳过？ | 允许／保护规则、分类理由、只读预演与处理历史 |

未知或无权限的数据明确标记，不填成零。背景与普通文字继承终端设置，适配浅色、深色和窄窗口，无需额外字体。没有后台常驻或开机启动，也不提供强制结束或后台自动清理。

## 使用

```sh
bree                         # 打开终端界面
bree status                  # 系统内存概览
bree list --limit 20          # 应用与进程占用
bree inspect '<对象 ID>'     # 使用 list 返回的 ID 查看详情
bree watch                   # 前台持续观察
bree doctor                  # 检查能力与数据可用性
bree clean --dry-run --json   # 只读预演
bree history --json           # 读取处理历史
bree license                  # 查看 GPL-3.0 许可
```

**↑↓** 选择 · **Enter** 进入 · **R** 刷新 · **Esc** 返回 · **S** 设置 · **Q／Ctrl+C** 退出。最小交互尺寸为 48 列 × 16 行。

需要脚本输出时：

```sh
bree status --json
bree watch --json --count 3
```

JSON 含 `schema_version: 1` 与数据有效性，未知值为 `null`；stdout 只输出结果，诊断写入 stderr。命令选项、规则范围和本地数据说明见 [完整使用说明](docs/cli.md)。

## 反馈与贡献

遇到问题或有建议，欢迎提交 [Issue](https://github.com/yuyongyan29-dev/Bree/issues)。请附 Bree 版本、macOS 版本、架构和复现步骤，分享诊断前检查应用名称与本地路径。

构建与检查指引见 [CONTRIBUTING.md](CONTRIBUTING.md)。

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
