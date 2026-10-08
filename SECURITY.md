# Security policy

## Supported versions

Bree is currently in the `0.3.0-alpha` release line. Security fixes are provided on a best-effort basis only for the latest release in that line. Older alpha releases are not maintained; update to the latest alpha before checking whether a problem still occurs. See [Releases](https://github.com/yuyongyan29-dev/Bree/releases) and the [installation guide](docs/installation.md).

## Reporting a vulnerability

Please report suspected vulnerabilities privately through [GitHub private vulnerability reporting](https://github.com/yuyongyan29-dev/Bree/security/advisories/new). On this repository's **Security** page, open **Advisories** and select **Report a vulnerability**.

Do not open a public issue or post exploit details in a public pull request or discussion. Include:

- Bree version, macOS version, architecture, and installation method.
- A description of the affected behavior and its security impact.
- Minimal reproduction steps or a proof of concept using synthetic data.
- Relevant logs with secrets, environment variables, personal paths, and other private data removed.

Maintainers review reports and coordinate fixes and disclosure on a best-effort basis. There is no guaranteed response or resolution deadline. Keep vulnerability details private while coordinating disclosure with the maintainers.

## Security boundaries

- Bree inspects memory locally on the Mac. Resource collection does not contact network services or send process data elsewhere. Downloading installation files and updates requires internet access.
- Default `list` and `watch` exports omit executable and application paths. Explicit `inspect` output can contain paths with the home directory replaced by `~`, and can still reveal project names. Review output before sharing it.
- Bree does not collect or record full process command lines, environment variables, prompts, or chat content. Application and process names can still appear in output.
- Bree is a read-only viewer. It does not stop applications or perform automatic background cleanup.
- Bree has no persistent state. It does not read, create, migrate, or delete data in the former Bree data directory; users may remove old data manually.

## 中文说明

当前只对 `0.3.0-alpha` 线的最新发行版尽力提供安全修复，旧版不再维护。请通过仓库 Security → Advisories → **Report a vulnerability** [私下报告](https://github.com/yuyongyan29-dev/Bree/security/advisories/new)，不要公开开 Issue 或发布漏洞细节。报告前移除密钥、环境变量、个人路径等隐私信息；不承诺具体响应或修复时限。

Bree 在本机采集内存信息，不联网采集；安装和升级需联网。默认 `list`／`watch` 导出省略路径，`inspect` 仍可能显示脱敏路径与项目名。Bree 是无持久状态的只读查看器，不结束应用，不读取、创建、迁移或删除旧数据目录；旧数据可由用户手动删除。
