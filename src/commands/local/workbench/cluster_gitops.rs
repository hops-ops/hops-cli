//! Reconcile a cluster gitops tree onto the **shared** local control plane.
//!
//! One local CP (dory/colima/kind) serves many worktrees/projects. Cluster
//! config is **not** per-worktree — it lives at the meta repo root (or any
//! path passed via `--cluster`):
//!
//! ```text
//! <meta>/                           # meta root
//!   gitops/cluster/                 # CP: PSQLStack, AuthStack, packages…
//!   clients/foo/.gitops/deploy/     # per-project charts
//!   platform/api/.gitops/deploy/
//!   gitops/envs/local/              # Application YAMLs → namespace = --name
//! ```
//!
//! Env Applications only isolate **app** namespaces. Cluster YAML is applied
//! once to the CP and reconciled by Crossplane.
//!
//! Watched like env gitops: change a file → `kubectl apply` → CP.

use crate::commands::local::kubectl_command;
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default)]
pub struct ClusterReconcileResult {
    pub applied: Vec<PathBuf>,
    pub skipped: Vec<PathBuf>,
    pub errors: Vec<String>,
}

/// Resolve cluster gitops directory.
///
/// Order:
/// 1. Explicit `override_path` (`--cluster`)
/// 2. Env var `HOPS_LOCAL_CLUSTER`
/// 3. Walk up from `env_path` looking for `gitops/cluster` or `cluster`
/// 4. Walk up from cwd looking for `gitops/cluster`
///
/// Returns the first existing directory. Explicit override that does not exist
/// is left to the caller to error on canonicalize.
pub fn resolve_cluster_path(
    env_path: Option<&Path>,
    override_path: Option<&Path>,
) -> Option<PathBuf> {
    if let Some(p) = override_path {
        return Some(p.to_path_buf());
    }
    if let Ok(p) = std::env::var("HOPS_LOCAL_CLUSTER") {
        let pb = PathBuf::from(p.trim());
        if !p.trim().is_empty() && pb.is_dir() {
            return Some(pb);
        }
    }
    if let Some(env) = env_path {
        if let Some(found) = discover_cluster_path(env) {
            return Some(found);
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        return walk_up_for_cluster(&cwd);
    }
    None
}

/// Discover a cluster tree near an env path (or walk to meta root).
///
/// ```text
/// gitops/envs/local     → sibling gitops/cluster
/// some/deep/project     → walk up → <meta>/gitops/cluster
/// <meta>/gitops         → <meta>/gitops/cluster
/// ```
pub fn discover_cluster_path(env_path: &Path) -> Option<PathBuf> {
    let env = env_path
        .canonicalize()
        .unwrap_or_else(|_| env_path.to_path_buf());

    // Tight layouts first (same gitops/ as envs)
    if let Some(parent) = env.parent() {
        let name = parent.file_name().and_then(|s| s.to_str()).unwrap_or("");
        if name == "envs" || name == "env" {
            if let Some(gitops) = parent.parent() {
                let cluster = gitops.join("cluster");
                if cluster.is_dir() {
                    return Some(cluster);
                }
            }
        }
    }
    if env.file_name().and_then(|s| s.to_str()) == Some("gitops") {
        let cluster = env.join("cluster");
        if cluster.is_dir() {
            return Some(cluster);
        }
    }
    if let Some(parent) = env.parent() {
        let cluster = parent.join("cluster");
        if cluster.is_dir() {
            return Some(cluster);
        }
    }

    // Meta-root walk: any ancestor with gitops/cluster or cluster/
    walk_up_for_cluster(&env)
}

/// Walk from `start` toward filesystem root for `gitops/cluster` or `cluster`.
fn walk_up_for_cluster(start: &Path) -> Option<PathBuf> {
    let mut cur = start.canonicalize().unwrap_or_else(|_| start.to_path_buf());
    loop {
        for candidate in [cur.join("gitops").join("cluster"), cur.join("cluster")] {
            if candidate.is_dir() {
                return Some(candidate);
            }
        }
        if !cur.pop() {
            break;
        }
    }
    None
}

/// Collect YAML manifests under cluster_path (recursive).
/// Skips examples, docs, and non-manifest files.
///
/// Order: `packages/` first (Configuration installs that establish CRDs), then
/// everything else. Alphabetical within each group.
pub fn collect_cluster_manifests(cluster_path: &Path) -> Result<Vec<PathBuf>, Box<dyn Error>> {
    let mut out = Vec::new();
    collect_manifests_rec(cluster_path, &mut out)?;
    out.sort_by(|a, b| {
        let a_pkg = path_under_packages(cluster_path, a);
        let b_pkg = path_under_packages(cluster_path, b);
        match (a_pkg, b_pkg) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => a.cmp(b),
        }
    });
    Ok(out)
}

fn path_under_packages(cluster_path: &Path, path: &Path) -> bool {
    path.strip_prefix(cluster_path)
        .ok()
        .and_then(|rel| rel.components().next())
        .map(|c| c.as_os_str() == "packages")
        .unwrap_or(false)
}

fn collect_manifests_rec(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), Box<dyn Error>> {
    if !dir.is_dir() {
        return Ok(());
    }
    for ent in fs::read_dir(dir)? {
        let ent = ent?;
        let path = ent.path();
        if path.is_dir() {
            // Skip common non-manifest trees
            let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
            if matches!(name, ".git" | "node_modules" | "target" | "_output") {
                continue;
            }
            collect_manifests_rec(&path, out)?;
            continue;
        }
        if should_apply_manifest(&path) {
            out.push(path);
        }
    }
    Ok(())
}

/// Whether a file should be `kubectl apply`'d.
pub fn should_apply_manifest(path: &Path) -> bool {
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if !(name.ends_with(".yaml") || name.ends_with(".yml")) {
        return false;
    }
    if name.ends_with(".example")
        || name.ends_with(".example.yaml")
        || name.ends_with(".example.yml")
    {
        return false;
    }
    if name.contains(".example.") {
        return false;
    }
    // *.yaml.example pattern: file name ends with .example already handled;
    // also skip foo.yaml.example via ends_with .example above when full name is x.yaml.example
    if name.ends_with(".yaml.example") || name.ends_with(".yml.example") {
        return false;
    }
    // Skip docs-named files that sometimes are yaml
    if name == "readme.yaml" || name == "secrets.yaml" {
        return false;
    }
    // Must look like a k8s document
    let Ok(text) = fs::read_to_string(path) else {
        return false;
    };
    let head = text.lines().take(30).collect::<Vec<_>>().join("\n");
    head.contains("apiVersion:") && head.contains("kind:")
}

/// Apply all cluster manifests to the current kube context.
pub fn reconcile_cluster_dir(
    cluster_path: &Path,
    dry_run: bool,
) -> Result<ClusterReconcileResult, Box<dyn Error>> {
    let cluster_path = cluster_path
        .canonicalize()
        .map_err(|e| format!("cluster path {}: {e}", cluster_path.display()))?;
    if !cluster_path.is_dir() {
        return Err(format!(
            "cluster path is not a directory: {}",
            cluster_path.display()
        )
        .into());
    }

    let manifests = collect_cluster_manifests(&cluster_path)?;
    let mut result = ClusterReconcileResult::default();

    if manifests.is_empty() {
        log::info!(
            "cluster gitops: no applyable YAML under {}",
            cluster_path.display()
        );
        return Ok(result);
    }

    log::info!(
        "cluster gitops: reconciling {} manifest(s) from {}",
        manifests.len(),
        cluster_path.display()
    );

    for path in manifests {
        match apply_one(&path, dry_run) {
            Ok(()) => {
                log::info!(
                    "  {} {}",
                    path.strip_prefix(&cluster_path).unwrap_or(&path).display(),
                    if dry_run { "dry-run" } else { "applied" }
                );
                result.applied.push(path);
            }
            Err(e) => {
                let msg = format!("{}: {e}", path.display());
                log::error!("  {msg}");
                result.errors.push(msg);
            }
        }
    }

    if !result.errors.is_empty() && result.applied.is_empty() {
        return Err(format!(
            "cluster gitops: all applies failed ({} error(s))",
            result.errors.len()
        )
        .into());
    }
    if !result.errors.is_empty() {
        log::warn!(
            "cluster gitops: {} applied, {} failed (partial — Crossplane may still reconcile successes)",
            result.applied.len(),
            result.errors.len()
        );
    }
    Ok(result)
}

fn apply_one(path: &Path, dry_run: bool) -> Result<(), Box<dyn Error>> {
    let path_s = path.to_string_lossy();
    let mut args = vec!["apply", "-f", path_s.as_ref()];
    if dry_run {
        args.push("--dry-run=server");
    }
    // Prefer server dry-run; fall back to client if CRDs missing on dry-run only
    match run_kubectl(&args) {
        Ok(()) => Ok(()),
        Err(e) if dry_run => {
            let client_args = vec!["apply", "-f", path_s.as_ref(), "--dry-run=client"];
            run_kubectl(&client_args).map_err(|_| e)
        }
        Err(e) => Err(e),
    }
}

fn run_kubectl(args: &[&str]) -> Result<(), Box<dyn Error>> {
    let output = kubectl_command(args)
        .output()
        .map_err(|e| format!("kubectl failed to start: {e}"))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    Err(format!("{}{}", stdout.trim(), stderr.trim()).into())
}

/// Whether a path under cluster_path should trigger re-reconcile.
pub fn should_reconcile_cluster_change(changed: &Path, cluster_path: &Path) -> bool {
    if crate::commands::local::workbench::watch::should_ignore_watch_path(changed) {
        return false;
    }
    let cluster = cluster_path
        .canonicalize()
        .unwrap_or_else(|_| cluster_path.to_path_buf());
    let changed_norm = changed
        .canonicalize()
        .unwrap_or_else(|_| changed.to_path_buf());
    if !(changed_norm == cluster || changed_norm.starts_with(&cluster)) {
        return false;
    }
    // Any yaml change under cluster, or delete events (path may not exist)
    let name = changed
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    name.ends_with(".yaml") || name.ends_with(".yml") || !changed.exists() // deletion of a prior manifest
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn packages_sort_before_other_manifests() {
        let dir = std::env::temp_dir().join(format!(
            "hops-cg-sort-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let packages = dir.join("packages");
        let auth = dir.join("auth");
        fs::create_dir_all(&packages).unwrap();
        fs::create_dir_all(&auth).unwrap();
        fs::write(
            packages.join("psql-stack.yaml"),
            "apiVersion: pkg.crossplane.io/v1\nkind: Configuration\n",
        )
        .unwrap();
        fs::write(
            auth.join("stack.yaml"),
            "apiVersion: hops.ops.com.ai/v1alpha1\nkind: AuthStack\n",
        )
        .unwrap();
        let manifests = collect_cluster_manifests(&dir).unwrap();
        assert_eq!(manifests.len(), 2);
        assert!(
            manifests[0].ends_with("packages/psql-stack.yaml")
                || manifests[0].file_name().and_then(|s| s.to_str()) == Some("psql-stack.yaml")
        );
        assert!(path_under_packages(&dir, &manifests[0]));
        assert!(!path_under_packages(&dir, &manifests[1]));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn discover_from_envs_local() {
        let dir = std::env::temp_dir().join(format!(
            "hops-cg-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let envs = dir.join("gitops/envs/local");
        let cluster = dir.join("gitops/cluster");
        fs::create_dir_all(&envs).unwrap();
        fs::create_dir_all(&cluster).unwrap();
        let found = discover_cluster_path(&envs).unwrap();
        assert_eq!(
            found.canonicalize().unwrap(),
            cluster.canonicalize().unwrap()
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn discover_walks_up_to_meta_root() {
        let dir = std::env::temp_dir().join(format!(
            "hops-cg-meta-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        // Meta-root layout: cluster at meta root, env deep under a project
        let cluster = dir.join("gitops/cluster");
        let deep_env = dir.join("clients/foo/gitops/envs/local");
        fs::create_dir_all(&cluster).unwrap();
        fs::create_dir_all(&deep_env).unwrap();
        let found = discover_cluster_path(&deep_env).unwrap();
        assert_eq!(
            found.canonicalize().unwrap(),
            cluster.canonicalize().unwrap()
        );
        // explicit override wins
        let other = dir.join("other-cluster");
        fs::create_dir_all(&other).unwrap();
        let resolved = resolve_cluster_path(Some(&deep_env), Some(&other)).unwrap();
        assert_eq!(resolved, other);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn skips_examples_and_docs() {
        let dir = std::env::temp_dir().join(format!("hops-cg-skip-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("stack.yaml"),
            "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: x\n",
        )
        .unwrap();
        fs::write(
            dir.join("aws.yaml.example"),
            "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: ex\n",
        )
        .unwrap();
        fs::write(dir.join("README.md"), "# hi\n").unwrap();
        let m = collect_cluster_manifests(&dir).unwrap();
        assert_eq!(m.len(), 1);
        assert!(m[0].ends_with("stack.yaml"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn should_apply_requires_apiversion_kind() {
        let dir = std::env::temp_dir().join(format!("hops-cg-kind-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let good = dir.join("good.yaml");
        let bad = dir.join("bad.yaml");
        fs::write(
            &good,
            "apiVersion: v1\nkind: Namespace\nmetadata:\n  name: n\n",
        )
        .unwrap();
        fs::write(&bad, "just: a map\n").unwrap();
        assert!(should_apply_manifest(&good));
        assert!(!should_apply_manifest(&bad));
        let _ = fs::remove_dir_all(&dir);
    }
}
