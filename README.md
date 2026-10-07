# Bree

[![CI](https://github.com/yuyongyan29-dev/Bree/actions/workflows/ci.yml/badge.svg)](https://github.com/yuyongyan29-dev/Bree/actions/workflows/ci.yml)
[![GPL-3.0](https://img.shields.io/badge/license-GPL--3.0-blue)](LICENSE)
[![Release](https://img.shields.io/github/v/release/yuyongyan29-dev/Bree?include_prereleases)](https://github.com/yuyongyan29-dev/Bree/releases)

**看清内存占用，让每个选择都有依据。**

```text
      .-.._  z
     ( -.- )__       bree
   .-'     `-.
  (   .----.  )      在终端了解你的 Mac
   `-(_  __)-'
      `----'
```

Bree 是免费的 macOS 终端工具。打开一个界面，查看系统内存压力、应用与进程占用，再追到每个对象的归属和数据依据。无需账号或订阅；日常查看在本机完成。

A free terminal tool for understanding memory usage on your Mac.

[安装](#安装) · [使用说明](docs/cli.md) · [参与贡献](CONTRIBUTING.md) · [反馈问题](https://github.com/yuyongyan29-dev/Bree/issues)

当前版本 **`0.3.0-alpha.1`**。已验证环境为 **Apple Silicon · macOS 27.0.1**，其他 macOS 版本尚待验证。**本版本不会结束应用：正常退出功能尚未启用，清理入口保留预演与结果说明。**

## 安装

两种方式均下载预编译程序，无需安装 Rust、Cargo、Python 或 Node.js。

### Homebrew

已有 Homebrew，在终端执行：

```sh
brew install --cask yuyongyan29-dev/tap/bree
bree
```

### curl

不需要 Homebrew，使用 macOS 自带工具即可安装：

```sh
curl -fsSL https://raw.githubusercontent.com/yuyongyan29-dev/Bree/main/distribution/install.sh | sh
```

默认安装到 `~/.local/bin/bree`。如果终端提示找不到 `bree`，先在当前终端执行：

```sh
export PATH="$HOME/.local/bin:$PATH"
bree
```

将同一行 `export` 加入 `~/.zshrc`，以后新终端也能直接运行。安装器校验 SHA-256 和版本，失败时保留旧程序，不使用 sudo，也不改写 shell 配置。

升级、卸载、自定义目录与兼容范围见 [安装说明](docs/installation.md)。

## 能做什么

- **查看整体状态**：内存总量、已用量、压缩、交换空间与内存压力。
- **找到占用来源**：按有证据的应用归属分组，查看进程、安装位置和指标来源；未知或无权限的数据明确标记。
- **持续观察**：在前台查看变化，或使用文本、JSON／JSONL 接入自己的脚本。
- **管理规则与预演**：设置确切应用安装的允许／保护规则，查看分类理由和处理记录。实际退出能力仍关闭。
- **适配你的终端**：键盘操作，继承浅色／深色背景，窄窗口使用紧凑布局，无需额外字体。

没有后台常驻或开机启动。普通查看无需额外系统权限；读不到的数据保留缺失状态。

## 使用

```sh
bree                         # 打开终端界面
bree status                  # 系统内存概览
bree list --limit 20          # 应用与进程占用
bree inspect '<对象 ID>'     # 使用 list 返回的 ID 查看详情
bree watch                   # 前台持续观察
bree doctor                  # 检查能力与数据可用性
bree clean --dry-run --json   # 只读预演
bree history --json           # 处理记录
bree license                  # 查看 GPL-3.0 许可
```

界面快捷键：↑↓ 选择、Enter 进入、R 刷新、Esc 返回、S 设置、Q／Ctrl+C 退出。最小交互尺寸为 48 列 × 16 行。

直接命令支持 `--json`，例如：

```sh
bree status --json
bree watch --json --count 3
```

JSON 含 `schema_version: 1` 和数据有效性；未知值为 `null`，stdout 只输出结果，诊断写入 stderr。完整命令、规则范围和记录行为见 [CLI 说明](docs/cli.md)。

## 当前进展

内存采集、交互界面、规则与预演、处理会话及历史框架已经实现。正常退出应用需要进一步验证精确实例控制与文档保存行为，通过前保持关闭；不提供强制结束或后台自动清理。

详细行为与当前能力范围见 [使用说明](docs/cli.md)。

## 从源码构建

开发环境需要 Rust 1.96.0 与 Xcode 命令行工具，版本由 `rust-toolchain.toml` 和 `Cargo.lock` 固定。安装预编译程序无需这些开发依赖。

```sh
git clone https://github.com/yuyongyan29-dev/Bree.git
cd Bree
cargo build --locked --release
./target/release/bree
```

本仓库专用于 CLI 的开发与发行。`src/` 保存 Rust 实现，`tests/` 保存行为测试，`docs/` 保存用户使用与安装说明，`distribution/` 保存安装和发行工具。贡献流程与检查命令见 [CONTRIBUTING.md](CONTRIBUTING.md)。

## 反馈

欢迎通过 [Issues](https://github.com/yuyongyan29-dev/Bree/issues) 反馈问题或提交 Pull Request。

## 许可

Bree 源码采用 [GPL-3.0](LICENSE)，免费提供。分发修改版时，须遵守 GPL 并提供对应源码；商业使用仍然允许。第三方组件保留各自许可，见 [THIRD-PARTY-NOTICES.txt](THIRD-PARTY-NOTICES.txt)。
