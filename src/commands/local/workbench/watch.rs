//! GitOps watch path filtering: re-reconcile only on env YAML / chart changes.

use super::application::{load_applications, resolve_source_path, Application};
use std::error::Error;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchPathClass {
    /// Env Application YAML or chart template/values — should re-reconcile.
    ChartOrEnv,
    /// Ordinary app source under the project — must NOT re-helm.
    AppSource,
    /// Build artifacts / VCS — ignore entirely.
    Ignored,
}

/// Paths that should never trigger gitops re-reconcile.
pub fn should_ignore_watch_path(path: &Path) -> bool {
    path.components().any(|c| {
        let s = c.as_os_str().to_string_lossy();
        matches!(
            s.as_ref(),
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

/// Build the set of roots to watch: env dir + each Application chart path.
pub fn watch_roots_for_applications(
    env_path: &Path,
) -> Result<Vec<PathBuf>, Box<dyn Error>> {
    let mut roots = Vec::new();
    let env_canon = env_path
        .canonicalize()
        .unwrap_or_else(|_| env_path.to_path_buf());
    roots.push(env_canon);

    let apps = load_applications(env_path)?;
    for (app_file, app) in apps {
        let chart = resolve_source_path(&app_file, &app.spec.source.path)?;
        if chart.exists() {
            roots.push(chart.canonicalize().unwrap_or(chart));
        } else {
            roots.push(chart);
        }
    }
    roots.sort();
    roots.dedup();
    Ok(roots)
}

/// Classify a changed path relative to known env/chart roots and project root.
///
/// - Under env dir or any chart root → ChartOrEnv
/// - Under ignored dirs → Ignored
/// - Otherwise (app source) → AppSource
pub fn is_chart_or_env_path(
    changed: &Path,
    env_path: &Path,
    chart_paths: &[PathBuf],
) -> WatchPathClass {
    if should_ignore_watch_path(changed) {
        return WatchPathClass::Ignored;
    }

    let changed_norm = normalize(changed);
    let env_norm = normalize(env_path);
    if path_is_under(&changed_norm, &env_norm) {
        // Only YAML under env counts as env change; still ChartOrEnv for any env path.
        return WatchPathClass::ChartOrEnv;
    }
    for chart in chart_paths {
        let chart_norm = normalize(chart);
        if path_is_under(&changed_norm, &chart_norm) {
            return WatchPathClass::ChartOrEnv;
        }
    }
    WatchPathClass::AppSource
}

fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in p.components() {
        match comp {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn path_is_under(path: &Path, root: &Path) -> bool {
    path == root || path.starts_with(root)
}

/// Helper for tests/docs: whether a change should trigger re-reconcile.
pub fn should_reconcile_on_change(
    changed: &Path,
    env_path: &Path,
    chart_paths: &[PathBuf],
) -> bool {
    is_chart_or_env_path(changed, env_path, chart_paths) == WatchPathClass::ChartOrEnv
}

#[allow(dead_code)]
fn _use_app(_: &Application) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ignores_build_artifact_paths() {
        assert!(should_ignore_watch_path(Path::new(
            "/proj/ui/node_modules/foo/index.js"
        )));
        assert!(should_ignore_watch_path(Path::new("/proj/api/target/debug/x")));
        assert!(should_ignore_watch_path(Path::new("/proj/.git/objects/aa")));
        assert!(!should_ignore_watch_path(Path::new(
            "/proj/ui/src/routes/+page.svelte"
        )));
    }

    #[test]
    fn chart_and_env_trigger_reconcile_source_does_not() {
        let env = PathBuf::from("/proj/gitops/env/local");
        let charts = vec![
            PathBuf::from("/proj/api/.gitops/deploy"),
            PathBuf::from("/proj/ui/.gitops/deploy"),
        ];

        assert!(should_reconcile_on_change(
            Path::new("/proj/gitops/env/local/api.yaml"),
            &env,
            &charts
        ));
        assert!(should_reconcile_on_change(
            Path::new("/proj/api/.gitops/deploy/templates/service.yaml"),
            &env,
            &charts
        ));
        assert!(should_reconcile_on_change(
            Path::new("/proj/ui/.gitops/deploy/values.yaml"),
            &env,
            &charts
        ));
        // Ordinary app source — must NOT re-apply charts
        assert!(!should_reconcile_on_change(
            Path::new("/proj/ui/src/routes/+page.svelte"),
            &env,
            &charts
        ));
        assert!(!should_reconcile_on_change(
            Path::new("/proj/crates/service/src/lib.rs"),
            &env,
            &charts
        ));
        // Ignored even if under chart-ish names
        assert_eq!(
            is_chart_or_env_path(
                Path::new("/proj/ui/node_modules/x"),
                &env,
                &charts
            ),
            WatchPathClass::Ignored
        );
    }
}
