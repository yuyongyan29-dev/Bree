//! Narrow, sample-verified development labels. Labels never authorize stopping.
use crate::model::ProcessInfo;
use crate::policy::reliable_identity;
use serde::{Deserialize, Serialize};
use std::path::{Component, Path};

const CODEX_APP_CLI: &str =
    "/Applications/ChatGPT.app/Contents/Resources/codex-cli/CodexCLI.app/Contents/MacOS/codex";
const VERIFIED_CLAUDE_VERSION: &str = "2.1.292";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DevelopmentLabel {
    pub label: String,
    pub evidence: String,
    pub confidence: String,
}

/// Matching is confined to native executable layouts observed on this machine.
/// It does not inspect argv, environment variables, parents, ports, or CPU usage.
/// Confidence describes installation path evidence, not signature or binary integrity.
pub fn label_process(process: &ProcessInfo) -> Option<DevelopmentLabel> {
    if !reliable_identity(process) || process.uid != Some(unsafe { libc::geteuid() }) {
        return None;
    }
    let executable = process.executable_path.as_deref()?;
    let (label, observed) = if executable == CODEX_APP_CLI {
        (
            "Codex CLI",
            "主应用内确切 Codex CLI 可执行路径；安装布局以本机 0.160.1 样本验证，运行时未读取版本",
        )
    } else if verified_claude_native_installation(executable) {
        (
            "Claude Code",
            "Claude 原生版本安装的确切可执行路径；仅纳入本机已验证的 2.1.292 布局",
        )
    } else {
        return None;
    };
    Some(DevelopmentLabel {
        label: label.into(),
        evidence: format!(
            "{observed}，当前微秒级实例身份有效。此标签仅解释开发工具安装来源；未验证签名、任务项目、完成或共享关系，不构成停止依据。"
        ),
        confidence: "installation_path".into(),
    })
}

fn verified_claude_native_installation(path: &str) -> bool {
    if path.chars().any(char::is_control)
        || path
            .split('/')
            .skip(1)
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return false;
    }
    let Ok(relative) = Path::new(path).strip_prefix("/Users") else {
        return false;
    };
    let components: Vec<_> = relative.components().collect();
    matches!(components.as_slice(), [Component::Normal(_), local, share, claude, versions, version]
        if *local == Component::Normal(".local".as_ref())
        && *share == Component::Normal("share".as_ref())
        && *claude == Component::Normal("claude".as_ref())
        && *versions == Component::Normal("versions".as_ref())
        && *version == Component::Normal(VERIFIED_CLAUDE_VERSION.as_ref()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Attribution, Category, Metric, ProcessIdentity, Validity};

    fn process(path: Option<&str>) -> ProcessInfo {
        let identity = ProcessIdentity {
            boot_session: "boot".into(),
            pid: 42,
            start_seconds: Some(10),
            start_microseconds: Some(20),
            status: Validity::Ok,
        };
        ProcessInfo {
            id: identity.object_id(),
            identity,
            parent_pid: None,
            uid: Some(unsafe { libc::geteuid() }),
            name: "name deliberately not used".into(),
            executable_path: path.map(str::to_owned),
            memory_bytes: Metric::ok(1, "test"),
            metric_kind: "rss".into(),
            cpu_one_core_percent: Metric::ok(0.0, "test"),
            category: Category::Unknown,
            attribution: Attribution {
                application: None,
                method: "unattributed".into(),
                confidence: "unknown".into(),
                explanation: "test".into(),
            },
            protection_reasons: vec![],
            quit_supported: false,
        }
    }

    #[test]
    fn verified_native_layouts_label_without_exposing_user_paths() {
        let codex = label_process(&process(Some(CODEX_APP_CLI))).unwrap();
        assert_eq!(codex.label, "Codex CLI");
        assert_eq!(codex.confidence, "installation_path");
        let claude = label_process(&process(Some(
            "/Users/示例/.local/share/claude/versions/2.1.292",
        )))
        .unwrap();
        assert_eq!(claude.label, "Claude Code");
        assert!(!claude.evidence.contains("/Users"));
        assert!(claude.evidence.contains("不构成停止依据"));
    }

    #[test]
    fn generic_runtimes_names_versions_and_similar_paths_do_not_label() {
        for path in [
            "/opt/homebrew/bin/node",
            "/usr/bin/python3",
            "/usr/local/bin/claude",
            "/Users/test/.local/share/claude/versions/2.1.293",
            "/Users/test/.local/share/claude/versions/2.1.292/helper",
            "/Users/test/other/.local/share/claude/versions/2.1.292",
            "/Users/test/.local/share/claude/versions/../2.1.292",
            "/Applications/Fake.app/Contents/MacOS/codex",
        ] {
            let mut candidate = process(Some(path));
            candidate.name = "claude".into();
            assert!(label_process(&candidate).is_none(), "{path}");
        }
        assert!(label_process(&process(None)).is_none());
    }

    #[test]
    fn stale_identity_and_other_user_do_not_label() {
        let mut candidate = process(Some(CODEX_APP_CLI));
        candidate.identity.start_microseconds = Some(21);
        assert!(label_process(&candidate).is_none());
        candidate.id = candidate.identity.object_id();
        candidate.identity.status = Validity::Stale;
        assert!(label_process(&candidate).is_none());
        candidate.identity.status = Validity::Ok;
        candidate.uid = candidate.uid.map(|uid| uid + 1);
        assert!(label_process(&candidate).is_none());
    }
}
