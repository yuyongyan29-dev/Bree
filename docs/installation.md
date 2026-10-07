# 安装 Bree

Bree 免费提供，提供 Homebrew 和 curl 两个安装入口。两者使用同一份预编译程序；无需账号，也无需安装 Rust、Cargo、Python 或 Node.js。

## 兼容范围

| 项目 | 当前范围 |
|---|---|
| 程序版本 | `0.3.0-alpha.2` |
| 架构 | 原生 Apple Silicon（arm64） |
| 已验证系统 | macOS 27.0.1 |
| Homebrew bottle | Apple Silicon、macOS 27 |
| 其他 macOS 版本 | 尚待验证，不列为已支持组合 |
| Intel、Linux、Windows | 暂不提供安装包 |

在 Apple Silicon Mac 上请使用原生终端。curl 安装器会拒绝 Rosetta 的 x86_64 环境。安装与升级需要访问 GitHub，Bree 日常查看本机资源无需联网。

当前 Alpha 的应用正常退出能力保持关闭，实际程序不会结束应用。发行程序仅有 ad-hoc 签名，尚未完成 Developer ID 签名、公证或干净账号安装验证；安装器不会自动移除 quarantine 或绕过系统校验。

## Homebrew 安装

先安装并按提示配置 [Homebrew](https://brew.sh/)，然后执行：

```sh
brew install yuyongyan29-dev/tap/bree
bree --version
bree
```

完整的 tap 名称会自动选择 [Bree 的安装定义](https://github.com/yuyongyan29-dev/homebrew-tap)，无需另外手动添加 tap。Formula 固定版本 URL 和 SHA-256，默认下载匹配系统的预编译 bottle 并安装 `bree` 命令。

匹配 bottle 的标准安装无需 Xcode 命令行工具，也无需 Rust。当前 tap 仅提供 macOS 27 arm64 bottle，并限制最低系统为 macOS 27；其他版本尚未完成发行验证。Homebrew 自身的系统要求见 [官方安装说明](https://docs.brew.sh/Installation)。

此前短暂提供的 cask 已撤下：未公证程序带 quarantine 安装后会被 Gatekeeper 阻挡。若你装过旧 cask，先执行 `brew uninstall --cask bree`，再使用上面的 Formula 安装命令。安装脚本不修改系统安全设置。

升级：

```sh
brew update
brew upgrade yuyongyan29-dev/tap/bree
```

卸载：

```sh
brew uninstall bree
```

## curl 安装

使用 macOS 自带的 shell、curl 与 SHA-256 工具：

```sh
curl -fsSL https://raw.githubusercontent.com/yuyongyan29-dev/Bree/main/distribution/install.sh | sh
```

默认安装到 `~/.local/bin/bree`，不需要 sudo。默认从仓库的 `distribution/latest-version.txt` 读取发行版本，再从固定的 `vVERSION` 标签下载程序与校验文件，因此 Alpha 版本也可通过同一条命令安装。SHA-256 和 `bree --version` 检查通过后，才原子替换旧程序；下载、校验或版本检查失败会保留原文件。

安装器不修改 shell 配置。如果 `~/.local/bin` 尚未加入 PATH，在当前终端执行：

```sh
export PATH="$HOME/.local/bin:$PATH"
bree --version
bree
```

把同一行 `export` 保存到 `~/.zshrc`，以后新终端也能使用。其他 shell 请使用自己的配置文件。

### 安装到自定义目录

选择由自己管理的可写目录：

```sh
curl -fsSL https://raw.githubusercontent.com/yuyongyan29-dev/Bree/main/distribution/install.sh \
  | sh -s -- --bin-dir "$HOME/bin"
```

如果该目录不在 PATH 中，安装器会打印应添加的配置行。它不会覆盖符号链接；已有 Homebrew 管理的安装请用 Homebrew 升级，或选择另一个目录。

### 固定版本与升级

固定到当前版本：

```sh
curl -fsSL https://raw.githubusercontent.com/yuyongyan29-dev/Bree/main/distribution/install.sh \
  | sh -s -- --version 0.3.0-alpha.2
```

再次执行默认安装命令即可升级到安装器选择的版本，也可以显式使用 `--version latest`。下载地址可通过 `BREE_RELEASE_BASE_URL` 指向自己的 HTTPS Release 镜像，路径应遵循本仓库的发行资产布局；可用 `BREE_VERSION_URL` 指定 HTTPS 版本元数据地址。

### 卸载

删除 curl 安装的文件即可；自定义目录请替换路径：

```sh
rm "$HOME/.local/bin/bree"
```

Homebrew 与 curl 都保留本地规则和记录，默认位于 `~/Library/Application Support/Bree`。普通查看不会创建这些文件，首次保存规则／预演／处理会话才初始化数据目录。

## 检查安装

```sh
command -v bree
bree --version
bree doctor
bree status --json
```

`command -v` 可确认当前使用的程序路径。若曾同时使用两种渠道，PATH 中靠前的目录决定运行哪个版本；后续升级请使用该安装渠道。

缺少进程指标不等于安装失败，`doctor` 和结果中的有效性字段会说明可用范围。其他问题请在 [Issues](https://github.com/yuyongyan29-dev/Bree/issues) 提供系统版本、架构、安装方式和错误输出。分享数据前检查其中的应用名与本地路径。
