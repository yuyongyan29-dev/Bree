//! Narrow, sample-verified development labels. Labels never authorize stopping.
use crate::model::ProcessInfo;
use serde::{Deserialize, Serialize};
use std::path::{Component, Path};

const CODEX_APP_CLI: &str =
    "/Applications/ChatGPT.app/Contents/Resources/codex-cli/CodexCLI.app/Contents/MacOS/codex";

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
    if !process.reliable_identity() || process.uid != Some(unsafe { libc::geteuid() }) {
        return None;
    }
    let executable = process.executable_path.as_deref()?;
    let (label, observed) = if executable == CODEX_APP_CLI {
        (
            "Codex CLI",
            "Exact Codex CLI executable path inside the main app; installation layout verified locally with a 0.160.1 sample, version not read at runtime",
        )
    } else if verified_claude_native_installation(executable) {
        (
            "Claude Code",
            "Exact executable path of a native Claude installation in ~/.local/share/claude/versions/<numeric version>; layout verified locally with 2.1.290–2.1.294 samples, version not checked against a verified list",
        )
    } else {
        return None;
    };
    Some(DevelopmentLabel {
        label: label.into(),
        evidence: format!(
            "{observed}; the current instance identity with microsecond precision is valid. This label only explains the developer tool installation source; signature, task project, completion, and sharing have not been verified. It does not justify stopping the process."
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
        && version.as_os_str().to_str().is_some_and(numeric_version))
}

/// The native updater names each executable after its release, such as `2.1.294`.
/// Only plain MAJOR.MINOR.PATCH digits match; suffixes, signs and extra parts do not.
fn numeric_version(value: &str) -> bool {
    let parts: Vec<_> = value.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|part| (1..=9).contains(&part.len()) && part.bytes().all(|b| b.is_ascii_digit()))
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
        assert!(claude.evidence.contains("does not justify stopping"));
    }

    #[test]
    fn claude_label_follows_native_updates_without_a_pinned_version() {
        for version in ["2.1.290", "2.1.293", "2.1.294", "3.0.0", "10.20.300"] {
            let path = format!("/Users/test/.local/share/claude/versions/{version}");
            let label = label_process(&process(Some(&path)));
            assert_eq!(
                label.map(|l| l.label).as_deref(),
                Some("Claude Code"),
                "{path}"
            );
        }
    }

    #[test]
    fn generic_runtimes_names_versions_and_similar_paths_do_not_label() {
        for path in [
            "/opt/homebrew/bin/node",
            "/usr/bin/python3",
            "/usr/local/bin/claude",
            "/Users/test/.local/share/claude/versions/2.1.292/helper",
            "/Users/test/.local/share/claude/versions",
            "/Users/test/.local/share/claude/2.1.292",
            "/Users/.local/share/claude/versions/2.1.292",
            "/Users/test/.local/share/claude/versions/latest",
            "/Users/test/.local/share/claude/versions/2.1",
            "/Users/test/.local/share/claude/versions/2.1.294.1",
            "/Users/test/.local/share/claude/versions/2.1.294-beta",
            "/Users/test/.local/share/claude/versions/v2.1.294",
            "/Users/test/.local/share/claude/versions/+2.1.294",
            "/Users/test/.local/share/claude/versions/2..294",
            "/Users/test/.local/share/claude/versions/2.1.",
            "/Users/test/.local/share/claude/versions/1234567890.1.1",
            "/Users/test/.local/share/claude/versions/２.1.294",
            "/Users/test/.local/share/claude/versions/2.1.294 ",
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
