//! GitOps watch path filtering.

use std::path::Path;

/// Paths that should never trigger a GitOps reconcile.
pub fn should_ignore_watch_path(path: &Path) -> bool {
    path.components().any(|component| {
        let value = component.as_os_str().to_string_lossy();
        matches!(
            value.as_ref(),
            "node_modules"
                | "target"
                | ".git"
                | ".svelte-kit"
                | "dist"
                | "build"
                | "_output"
                | ".cache"
                | "playwright-report"
                | "test-results"
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ignores_build_artifact_paths() {
        assert!(should_ignore_watch_path(Path::new(
            "/proj/ui/node_modules/foo/index.js"
        )));
        assert!(should_ignore_watch_path(Path::new(
            "/proj/api/target/debug/x"
        )));
        assert!(should_ignore_watch_path(Path::new("/proj/.git/objects/aa")));
        assert!(!should_ignore_watch_path(Path::new(
            "/proj/ui/src/routes/+page.svelte"
        )));
    }
}
