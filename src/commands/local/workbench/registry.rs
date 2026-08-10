//! Workspace registry: name → namespace (+ path metadata) under ~/.hops/local/envs/.

use serde::{Deserialize, Serialize};
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};

pub const ENVS_SUBDIR: &str = "envs";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceRecord {
    /// Workspace name (user-facing).
    pub name: String,
    /// Kubernetes namespace — same as the slugified workspace name (`--name`).
    pub namespace: String,
    /// Absolute path to the env Application directory.
    pub env_path: String,
    /// Absolute project root (parent of env path when known).
    #[serde(default)]
    pub project_root: Option<String>,
    /// Source delivery strategy selected.
    #[serde(default)]
    pub delivery_mode: Option<String>,
    /// ISO-ish timestamp of last up.
    #[serde(default)]
    pub updated_at: Option<String>,
    /// Bound local cluster name (kind `--name` / logical CP id). LWB-REQ-256.
    #[serde(default)]
    pub cluster_name: Option<String>,
    /// Resolved kube context for that cluster (e.g. `kind-hops`).
    #[serde(default)]
    pub kube_context: Option<String>,
}

/// Resolve cluster binding for a workspace up/down/status operation.
///
/// - **Sticky:** when the workspace is already bound and `requested_cluster` is
///   omitted, return the **bound** cluster (do not fall back to process default).
/// - No prior record: accept `requested` (or default) and bind.
/// - Explicit request matching bound: keep bound context.
/// - Explicit request differing without `rebind`: error.
/// - Explicit request differing with `rebind`: accept new.
pub fn resolve_cluster_binding(
    existing: Option<&WorkspaceRecord>,
    requested_cluster: Option<&str>,
    default_cluster: &str,
    default_kube_context: &str,
    rebind: bool,
) -> Result<(String, String), String> {
    let explicit = requested_cluster
        .map(str::trim)
        .filter(|s| !s.is_empty());

    // Sticky core: no explicit --cluster-name → keep bound cluster if any.
    if explicit.is_none() {
        if let Some(rec) = existing {
            if let Some(bound) = rec.cluster_name.as_deref().filter(|s| !s.is_empty()) {
                let ctx = rec
                    .kube_context
                    .clone()
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| kube_context_for_cluster(bound, default_cluster, default_kube_context));
                return Ok((bound.to_string(), ctx));
            }
        }
    }

    let requested = explicit.unwrap_or(default_cluster);
    let kube = kube_context_for_cluster(requested, default_cluster, default_kube_context);

    match existing.and_then(|e| e.cluster_name.as_deref().filter(|s| !s.is_empty())) {
        None => Ok((requested.to_string(), kube)),
        // rebind always refreshes name + kube context (even if name is unchanged —
        // e.g. same logical "hops" but context dory → kind-hops).
        Some(_bound) if rebind => Ok((requested.to_string(), kube)),
        Some(bound) if bound == requested => {
            let ctx = existing
                .and_then(|e| e.kube_context.clone())
                .filter(|s| !s.is_empty())
                .unwrap_or(kube);
            Ok((bound.to_string(), ctx))
        }
        Some(bound) => Err(format!(
            "workspace is bound to cluster `{bound}`; pass `--rebind-cluster` to move to `{requested}`"
        )),
    }
}

/// Derive kube context for a logical cluster name.
pub fn kube_context_for_cluster(
    cluster_name: &str,
    default_cluster: &str,
    default_kube_context: &str,
) -> String {
    if cluster_name == default_cluster {
        return default_kube_context.to_string();
    }
    // kind contexts are kind-<name>; product dory/colima keep their default context
    // only when the name matches the default cluster identity.
    if default_kube_context.starts_with("kind-") || default_kube_context.is_empty() {
        format!("kind-{cluster_name}")
    } else {
        default_kube_context.to_string()
    }
}

/// Activate process kube context (+ kind cluster name) from a workspace record.
/// Returns the bound cluster name and context when present.
pub fn activate_workspace_cluster(
    record: &WorkspaceRecord,
) -> Option<(String, String)> {
    let cluster = record.cluster_name.as_deref()?.trim();
    if cluster.is_empty() {
        return None;
    }
    let ctx = record
        .kube_context
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("kind-{cluster}"));

    std::env::set_var(crate::commands::local::HOPS_KUBE_CONTEXT_ENV, &ctx);
    crate::commands::local::backend::kind::set_active_cluster_name(cluster);
    Some((cluster.to_string(), ctx))
}

/// DNS-1123-ish slug for a workspace name.
pub fn slugify_name(name: &str) -> String {
    let lower = name.trim().to_ascii_lowercase();
    let mut out = String::with_capacity(lower.len());
    let mut last_dash = false;
    for ch in lower.chars() {
        let ok = ch.is_ascii_alphanumeric() || ch == '-' || ch == '_';
        if ok {
            let c = if ch == '_' { '-' } else { ch };
            if c == '-' {
                if !last_dash && !out.is_empty() {
                    out.push('-');
                    last_dash = true;
                }
            } else {
                out.push(c);
                last_dash = false;
            }
        } else if !last_dash && !out.is_empty() {
            out.push('-');
            last_dash = true;
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "workspace".to_string()
    } else {
        // DNS-1123 label max 63.
        trimmed.chars().take(63).collect()
    }
}

/// Namespace = DNS-1123 slug of `--name` (e.g. `dogfood` →
/// `e2e-ui-ui.dogfood.svc.cluster.local`).
pub fn namespace_for_name(name: &str) -> String {
    slugify_name(name)
}

/// Default workspace name from cwd basename.
///
/// For a git worktree at `…/worktrees/my-feature`, that becomes `my-feature`
/// → namespace `my-feature`.
pub fn default_name_from_cwd(cwd: &Path) -> String {
    cwd.file_name()
        .and_then(|s| s.to_str())
        .map(slugify_name)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "workspace".to_string())
}

pub fn ensure_envs_dir(state_dir: &Path) -> Result<PathBuf, Box<dyn Error>> {
    let dir = state_dir.join(ENVS_SUBDIR);
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn record_path(state_dir: &Path, name: &str) -> PathBuf {
    state_dir
        .join(ENVS_SUBDIR)
        .join(format!("{}.json", slugify_name(name)))
}

pub fn save_workspace(state_dir: &Path, record: &WorkspaceRecord) -> Result<PathBuf, Box<dyn Error>> {
    ensure_envs_dir(state_dir)?;
    let path = record_path(state_dir, &record.name);
    let json = serde_json::to_string_pretty(record)?;
    fs::write(&path, json)?;
    Ok(path)
}

pub fn load_workspace(state_dir: &Path, name: &str) -> Result<Option<WorkspaceRecord>, Box<dyn Error>> {
    let path = record_path(state_dir, name);
    if !path.exists() {
        return Ok(None);
    }
    let text = fs::read_to_string(&path)?;
    let record: WorkspaceRecord = serde_json::from_str(&text)?;
    Ok(Some(record))
}

pub fn remove_workspace(state_dir: &Path, name: &str) -> Result<bool, Box<dyn Error>> {
    let path = record_path(state_dir, name);
    if path.exists() {
        fs::remove_file(&path)?;
        Ok(true)
    } else {
        Ok(false)
    }
}

pub fn list_workspaces(state_dir: &Path) -> Result<Vec<WorkspaceRecord>, Box<dyn Error>> {
    let dir = state_dir.join(ENVS_SUBDIR);
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut records = Vec::new();
    for entry in fs::read_dir(&dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let text = fs::read_to_string(&path)?;
        match serde_json::from_str::<WorkspaceRecord>(&text) {
            Ok(r) => records.push(r),
            Err(e) => log::warn!("skip corrupt workspace record {}: {e}", path.display()),
        }
    }
    records.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(records)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify_and_namespace_are_dns_safe() {
        assert_eq!(slugify_name("My Worktree"), "my-worktree");
        assert_eq!(namespace_for_name("My Worktree"), "my-worktree");
        assert_eq!(slugify_name("___"), "workspace");
        assert_eq!(namespace_for_name("alice"), "alice");
        assert_eq!(namespace_for_name("bob"), "bob");
        assert_ne!(namespace_for_name("alice"), namespace_for_name("bob"));
    }

    #[test]
    fn registry_round_trip_and_concurrent_names() {
        let dir = std::env::temp_dir().join(format!(
            "lwb-reg-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&dir).unwrap();

        let a = WorkspaceRecord {
            name: "alice".into(),
            namespace: namespace_for_name("alice"),
            env_path: "/proj/gitops/env/local".into(),
            project_root: Some("/proj".into()),
            delivery_mode: Some("hostPath".into()),
            updated_at: None,
            cluster_name: Some("hops".into()),
            kube_context: Some("kind-hops".into()),
        };
        let b = WorkspaceRecord {
            name: "bob".into(),
            namespace: namespace_for_name("bob"),
            env_path: "/proj/gitops/env/local".into(),
            project_root: Some("/proj".into()),
            delivery_mode: Some("sync".into()),
            updated_at: None,
            cluster_name: Some("dogfood".into()),
            kube_context: Some("kind-dogfood".into()),
        };
        save_workspace(&dir, &a).unwrap();
        save_workspace(&dir, &b).unwrap();

        let loaded_a = load_workspace(&dir, "alice").unwrap().unwrap();
        let loaded_b = load_workspace(&dir, "bob").unwrap().unwrap();
        assert_eq!(loaded_a.namespace, "alice");
        assert_eq!(loaded_b.namespace, "bob");
        assert_ne!(loaded_a.namespace, loaded_b.namespace);

        let all = list_workspaces(&dir).unwrap();
        assert_eq!(all.len(), 2);
        assert!(remove_workspace(&dir, "alice").unwrap());
        assert!(load_workspace(&dir, "alice").unwrap().is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn default_name_from_cwd_uses_basename() {
        assert_eq!(
            default_name_from_cwd(Path::new("/Users/x/dev/my-feature")),
            "my-feature"
        );
    }

    #[test]
    fn resolve_cluster_binding_sticky_and_rebind() {
        let existing = WorkspaceRecord {
            name: "alice".into(),
            namespace: "alice".into(),
            env_path: "/p".into(),
            project_root: None,
            delivery_mode: None,
            updated_at: None,
            cluster_name: Some("hops".into()),
            kube_context: Some("kind-hops".into()),
        };
        // Same cluster: ok
        let (c, k) =
            resolve_cluster_binding(Some(&existing), Some("hops"), "hops", "kind-hops", false)
                .unwrap();
        assert_eq!(c, "hops");
        assert_eq!(k, "kind-hops");
        // Different without rebind: err
        assert!(
            resolve_cluster_binding(Some(&existing), Some("dogfood"), "hops", "kind-hops", false)
                .is_err()
        );
        // Rebind: ok
        let (c2, k2) =
            resolve_cluster_binding(Some(&existing), Some("dogfood"), "hops", "kind-hops", true)
                .unwrap();
        assert_eq!(c2, "dogfood");
        assert_eq!(k2, "kind-dogfood");
        // Rebind same name refreshes stale kube context (dory → kind-hops)
        let stale = WorkspaceRecord {
            cluster_name: Some("hops".into()),
            kube_context: Some("dory".into()),
            ..existing.clone()
        };
        let (c4, k4) =
            resolve_cluster_binding(Some(&stale), Some("hops"), "hops", "kind-hops", true)
                .unwrap();
        assert_eq!(c4, "hops");
        assert_eq!(k4, "kind-hops", "rebind must refresh kube context");
        // First bind persists default
        let (c3, k3) =
            resolve_cluster_binding(None, None, "hops", "kind-hops", false).unwrap();
        assert_eq!(c3, "hops");
        assert_eq!(k3, "kind-hops");
    }

    #[test]
    fn sticky_omitted_request_keeps_bound_cluster_not_process_default() {
        // Core sticky case: bound to dogfood; process default is hops; no --cluster-name.
        let existing = WorkspaceRecord {
            name: "alice".into(),
            namespace: "alice".into(),
            env_path: "/p".into(),
            project_root: None,
            delivery_mode: None,
            updated_at: None,
            cluster_name: Some("dogfood".into()),
            kube_context: Some("kind-dogfood".into()),
        };
        let (c, k) =
            resolve_cluster_binding(Some(&existing), None, "hops", "kind-hops", false).unwrap();
        assert_eq!(c, "dogfood", "must keep sticky bind when request omitted");
        assert_eq!(k, "kind-dogfood");
        // Without stored kube_context, still derive kind-<bound>
        let no_ctx = WorkspaceRecord {
            kube_context: None,
            ..existing.clone()
        };
        let (c2, k2) =
            resolve_cluster_binding(Some(&no_ctx), None, "hops", "kind-hops", false).unwrap();
        assert_eq!(c2, "dogfood");
        assert_eq!(k2, "kind-dogfood");
    }

    #[test]
    fn cluster_fields_round_trip() {
        let dir = std::env::temp_dir().join(format!(
            "lwb-reg-cluster-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&dir).unwrap();
        let r = WorkspaceRecord {
            name: "alice".into(),
            namespace: "alice".into(),
            env_path: "/p".into(),
            project_root: None,
            delivery_mode: Some("hostPath".into()),
            updated_at: None,
            cluster_name: Some("dogfood".into()),
            kube_context: Some("kind-dogfood".into()),
        };
        save_workspace(&dir, &r).unwrap();
        let loaded = load_workspace(&dir, "alice").unwrap().unwrap();
        assert_eq!(loaded.cluster_name.as_deref(), Some("dogfood"));
        assert_eq!(loaded.kube_context.as_deref(), Some("kind-dogfood"));
        let _ = fs::remove_dir_all(&dir);
    }
}
