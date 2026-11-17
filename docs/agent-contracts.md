---
paths:
  - "src/**/*"
  - "tests/**/*"
---

# agent-contracts

项目按需指南。代码路径与命令均以项目根目录为基准；原有边界和例外继续适用。

# AGENTS.md

## 不可绕过的行为边界

### 指标、查询与输出

- 未知、无权限、不支持、失效与真实零值分开处理，保留 `Metric` 的 `value/status/reason`。内部使用字节，文本使用 MiB／GiB；进程统一使用 RSS，不把分组总量当作系统已用或可回收内存。
- 每个进程只属于一个分组；归属冲突与未知对象明确保留。不依据名称或高内存占用推断某个 AI 任务已完成。CPU 首次采样只建基线，后续使用实际时间差。
- 只有 AppKit 激活策略为 Regular 的普通应用能作为分组主应用；菜单栏、后台应用及归属冲突进程保持未归属。开发工具标签按安装布局匹配，不锁定单个版本号。helper 并入、未归属聚合与标签命名的具体规则见 [使用说明](cli.zh-CN.md)。
- 搜索和排序使用共享 `src/query.rs`。搜索仅改变显示结果，不改变分类；`list` 的 `groups` 可筛选和截断，`processes`、`coverage` 保留完整样本。
- 保持 JSON schema 2、有效性字段和退出码契约；不静默改变现有字段含义。stdout 只输出结果，诊断写 stderr，`watch --json` 保持 JSONL。变更对应检查在 `tests/cli_contract.rs`。
- 文本采样时间使用 `libc::localtime_r` 转换为本地 `HH:MM:SS`，JSON 保留 `sampled_at_unix_ms`。指标来源集中记录在 `docs/cli.zh-CN.md`；内存公式仅在 doctor 文本和 JSON notes 中解释，JSON `system.used_definition` 保留。
- 文本进程使用 PID，分组使用稳定派生的短 ID，完整样本内碰撞时加长；显示筛选不重算短 ID。JSON 分组含 `short_id`，完整 `id` 继续用于 JSON 和 TUI 精确选中。`inspect` 接受 PID、短 ID 与完整 ID；PID 输入不检测跨命令的复用，完整进程 ID 失效不得重绑定，歧义或缺失返回结构化错误。未归属分组 ID 在分组键不变时保持稳定，inspect 展示当前成员。
- 复用 `safe_text` 和路径脱敏逻辑处理外部名称与输出；默认导出不暴露路径，不采集或记录完整命令行、环境变量、提示词与聊天内容。

# AGENTS.md

## 不可绕过的行为边界

### 终端

- Home 首次采样后静置；不要为了动画或装饰添加持续扫描。资源页与 watch 的刷新行为保持可控。
- 退出、取消、信号、终端断开和错误路径都要恢复终端模式与文件标志，并回收采样线程。改输入时保留搜索编辑与普通导航的按键区别；改布局时覆盖最小 48×16、窗口缩放及浅／深色终端。
