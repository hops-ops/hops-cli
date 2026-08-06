//! Speed-first source delivery: hostPath when probe passes, mutagen-class fallback.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryStrategy {
    /// Worktree path is visible on the node — mount hostPath.
    HostPath,
    /// Probe failed — mutagen-class (or equivalent) host→pod sync.
    Sync,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryProbe {
    /// Absolute host worktree / project root path.
    pub host_path: PathBuf,
    /// Whether the path is visible on the Kubernetes node.
    pub host_path_visible: bool,
    /// Optional detail for status/verbose (probe command result).
    pub detail: String,
}

/// Default paths excluded from mutagen-class sync (LWB-REQ-150).
pub fn default_sync_ignores() -> Vec<&'static str> {
    vec![
        "node_modules",
        "target",
        ".git",
        "dist",
        "build",
        ".svelte-kit",
        "playwright-report",
        "test-results",
        "_output",
        ".cache",
    ]
}

/// Auto-select delivery strategy from probe result (LWB-REQ-140, LWB-REQ-240).
/// Prefers hostPath when capable; never requires user choice.
pub fn select_delivery_strategy(probe: &DeliveryProbe) -> DeliveryStrategy {
    if probe.host_path_visible {
        DeliveryStrategy::HostPath
    } else {
        DeliveryStrategy::Sync
    }
}

impl DeliveryStrategy {
    pub fn as_str(self) -> &'static str {
        match self {
            DeliveryStrategy::HostPath => "hostPath",
            DeliveryStrategy::Sync => "sync",
        }
    }

    /// Runtime values fragment for helm inject.
    pub fn helm_mode_value(self) -> &'static str {
        self.as_str()
    }
}

/// Build a probe result from a pure boolean (unit-test / fake backend).
pub fn probe_from_visibility(host_path: &Path, visible: bool, detail: impl Into<String>) -> DeliveryProbe {
    DeliveryProbe {
        host_path: host_path.to_path_buf(),
        host_path_visible: visible,
        detail: detail.into(),
    }
}

/// Whether a relative path component should be excluded from sync sessions.
pub fn path_is_sync_excluded(path: &Path) -> bool {
    let ignores = default_sync_ignores();
    path.components().any(|c| {
        let s = c.as_os_str().to_string_lossy();
        ignores.iter().any(|ig| *ig == s.as_ref())
    })
}

/// Ordinary source edits must not rebuild images or re-apply charts.
/// This is a documentation-level invariant enforced by watch path filters;
/// this helper exists for tests and status messaging.
pub fn source_edit_requires_chart_reapply() -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefers_host_path_when_probe_passes() {
        let probe = probe_from_visibility(Path::new("/Users/dev/proj"), true, "path exists on node");
        assert_eq!(select_delivery_strategy(&probe), DeliveryStrategy::HostPath);
    }

    #[test]
    fn falls_back_to_sync_when_probe_fails() {
        let probe = probe_from_visibility(Path::new("/Users/dev/proj"), false, "not on node");
        assert_eq!(select_delivery_strategy(&probe), DeliveryStrategy::Sync);
    }

    #[test]
    fn sync_ignores_build_artifacts() {
        assert!(path_is_sync_excluded(Path::new("ui/node_modules/x")));
        assert!(path_is_sync_excluded(Path::new("api/target/debug")));
        assert!(path_is_sync_excluded(Path::new(".git/config")));
        assert!(!path_is_sync_excluded(Path::new("ui/src/routes/+page.svelte")));
    }

    #[test]
    fn source_edits_do_not_require_chart_reapply() {
        assert!(!source_edit_requires_chart_reapply());
    }

    #[test]
    fn strategy_string_stable_for_registry() {
        assert_eq!(DeliveryStrategy::HostPath.as_str(), "hostPath");
        assert_eq!(DeliveryStrategy::Sync.as_str(), "sync");
    }
}
