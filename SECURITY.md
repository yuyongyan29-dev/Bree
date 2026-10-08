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
- Application quitting is disabled in the current alpha. Allow rules, `--yes`, and environment variables cannot enable it. Bree does not force-quit applications or perform automatic background cleanup.
- Rules, previews, and session summaries can write local state; `--dry-run` means no quit requests, not no disk writes. Ordinary viewing, `history`, and `doctor` do not create data files. See the [user guide](docs/cli.md) for data locations, permissions, and retention.

## 中文说明

当前只对 `0.3.0-alpha` 线的最新发行版尽力提供安全修复，旧版不再维护。请通过仓库 Security → Advisories → **Report a vulnerability** [私下报告](https://github.com/yuyongyan29-dev/Bree/security/advisories/new)，不要公开开 Issue 或发布漏洞细节。报告前移除密钥、环境变量、个人路径等隐私信息；不承诺具体响应或修复时限。

Bree 在本机采集内存信息，不联网采集；安装和升级需联网。默认 `list`／`watch` 导出省略路径，`inspect` 仍可能显示脱敏路径与项目名。应用退出能力保持关闭；预演不发送退出请求，但会写入本地摘要。
