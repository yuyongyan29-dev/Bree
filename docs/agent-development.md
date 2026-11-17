---
paths:
  - "src/**/*"
  - "tests/**/*"
  - "scripts/**/*"
  - "distribution/**/*"
  - "Cargo.toml"
---

# agent-development

项目按需指南。代码路径与命令均以项目根目录为基准；原有边界和例外继续适用。

# AGENTS.md

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

# AGENTS.md

## 源码入口

CLI 与 TUI 共用采集、查询和输出逻辑；修复共同问题时改共享模块，避免各自维护一套行为。macOS 专属能力留在平台适配层，AppKit 控制对象保留在主线程，不传入采样工作线程。

# AGENTS.md

## 验证方式

代码改动按 [CI](../.github/workflows/ci.yml) 和贡献指南运行：

# AGENTS.md

## 验证方式

```sh
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
```

# AGENTS.md

## 验证方式

需要运行 release 二进制或验证打包时先执行 `cargo build --locked --release`，不要把旧构建的运行结果当作本次改动的验证。

# AGENTS.md

## 验证方式

定位失败时改用 `cargo test --locked --no-fail-fast`：默认命令在第一个失败的测试二进制后停止，后续集成测试不会运行，结果会被隐藏。

# AGENTS.md

## 验证方式

- 新增或修改的测试不依赖开发者终端（`TERM`、`COLORTERM`、`NO_COLOR`）或真实用户目录：启动子进程时显式设置这些变量与独立的临时 `HOME`，单元测试在代码中固定相关配置。否则可能出现本机失败而 CI 通过（或相反）的结果。
- TUI 输入、主题或终端生命周期改动：按影响选用 `scripts/terminal-check.py`、`scripts/theme-check.py`、`scripts/signal-check.py`，结合 `tests/tui_signals.rs`；PTY 测试不能代替真实终端的视觉检查。
- 采集或刷新性能改动：使用 `tests/collection_live.rs`、`tests/cpu_load.rs` 及相应的 benchmark／soak 脚本，记录平台与实际测量结果。先阅读脚本参数与副作用，再运行。
- 安装器或发行工具改动增加以下检查，安装器测试使用模拟下载：

# AGENTS.md

## 验证方式

  ```sh
  sh -n distribution/install.sh distribution/package.sh
  python3 distribution/tests/test_install.py
  ```

# AGENTS.md

## 验证方式

- 纯文档改动核对事实、路径与 Markdown，并运行 `git diff --check`，无需无关的构建或原生实验。交付说明只报告实际执行的验证，注明未验证的平台或交互。
