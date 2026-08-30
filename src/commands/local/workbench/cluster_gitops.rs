//! Reconcile a cluster gitops tree onto the **shared** local control plane.
//!
//! One local CP (dory/colima/kind) serves many worktrees. Every checkout has
//! the same committed definitions, but only one watcher reconciles the
//! Cluster-owned tree selected for that control plane:
//!
//! ```text
//! <project>/                        # checkout root
//!   .gitops/local/cluster/          # CP: PSQLStack, AuthStack, packages…
//!   .gitops/local/environment.yaml  # reusable checkout Environment
//!   clients/foo/.gitops/local/      # editable local application charts
//!   platform/api/.gitops/local/
//! ```
//!
//! Env Applications only isolate **app** namespaces. Cluster YAML is applied
//! once to the CP and reconciled by Crossplane.
//!
//! Watched like env gitops: change a file → `kubectl apply` → CP.

use crate::commands::local::kubectl_command;
use serde::{Deserialize, Serialize};
use serde_yaml::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};

const INVENTORY_SCHEMA_VERSION: u32 = 1;

/// Ownership category for a Cluster tree entry. Categories describe ownership,
/// not a user-authored apply wave; readiness and object identity determine the
/// eventual reconcile order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ClusterCategory {
    Registry,
    Providers,
    Configurations,
    Functions,
    Platform,
    Shared,
    Rbac,
    /// Compatibility bucket for a documented tree extension. It sorts after
    /// the canonical categories and is still inventory-owned.
    Other,
}

impl ClusterCategory {
    fn from_relative_path(path: &Path) -> Self {
        match path
            .components()
            .next()
            .and_then(|c| c.as_os_str().to_str())
        {
            Some("registry") => Self::Registry,
            Some("providers") => Self::Providers,
            Some("configurations") | Some("packages") => Self::Configurations,
            Some("functions") => Self::Functions,
            Some("platform") => Self::Platform,
            Some("shared") => Self::Shared,
            Some("rbac") => Self::Rbac,
            _ => Self::Other,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ClusterObjectIdentity {
    pub api_version: String,
    pub kind: String,
    pub namespace: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClusterManifest {
    pub identity: ClusterObjectIdentity,
    pub category: ClusterCategory,
    pub source: String,
    pub revision: String,
    pub document: usize,
    #[serde(skip)]
    pub path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClusterInventoryEntry {
    pub identity: ClusterObjectIdentity,
    pub category: ClusterCategory,
    pub source: String,
    pub revision: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClusterInventory {
    pub schema_version: u32,
    pub cluster_path: String,
    pub entries: Vec<ClusterInventoryEntry>,
}

impl ClusterInventory {
    pub fn new(cluster_path: &Path, manifests: &[ClusterManifest]) -> Self {
        let mut entries = manifests
            .iter()
            .map(|manifest| ClusterInventoryEntry {
                identity: manifest.identity.clone(),
                category: manifest.category,
                source: manifest.source.clone(),
                revision: manifest.revision.clone(),
            })
            .collect::<Vec<_>>();
        entries.sort_by(|a, b| a.identity.cmp(&b.identity));
        Self {
            schema_version: INVENTORY_SCHEMA_VERSION,
            cluster_path: cluster_path.to_string_lossy().into_owned(),
            entries,
        }
    }
}

/// Load an exact Cluster inventory. Malformed, old, or mismatched inventories
/// fail closed; callers must not infer ownership from a namespace or label.
pub fn load_cluster_inventory(path: &Path) -> Result<Option<ClusterInventory>, Box<dyn Error>> {
    if !path.exists() {
        return Ok(None);
    }
    let text = fs::read_to_string(path)?;
    let inventory: ClusterInventory = serde_json::from_str(&text)
        .map_err(|error| format!("invalid Cluster inventory {}: {error}", path.display()))?;
    if inventory.schema_version != INVENTORY_SCHEMA_VERSION {
        return Err(format!(
            "unsupported Cluster inventory schema {} at {}",
            inventory.schema_version,
            path.display()
        )
        .into());
    }
    if inventory
        .entries
        .windows(2)
        .any(|pair| pair[0].identity >= pair[1].identity)
    {
        return Err(format!(
            "Cluster inventory {} is not canonically ordered",
            path.display()
        )
        .into());
    }
    Ok(Some(inventory))
}

/// Atomically persist a last-known-good inventory. The file contains only
/// object identities and content revisions, never rendered values.
pub fn save_cluster_inventory(
    path: &Path,
    inventory: &ClusterInventory,
) -> Result<(), Box<dyn Error>> {
    if inventory.schema_version != INVENTORY_SCHEMA_VERSION {
        return Err("cannot persist an unsupported Cluster inventory schema".into());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temp = path.with_extension("json.tmp");
    fs::write(&temp, serde_json::to_vec_pretty(inventory)?)?;
    fs::rename(&temp, path)?;
    Ok(())
}

/// Compute exact identities present in the old inventory but absent from the
/// accepted new snapshot. This is intentionally pure; deletion is a separate
/// exact-identity operation so a failed apply never advances the inventory.
pub fn stale_cluster_inventory(
    previous: &ClusterInventory,
    current: &ClusterInventory,
) -> Vec<ClusterInventoryEntry> {
    let current_ids = current
        .entries
        .iter()
        .map(|entry| &entry.identity)
        .collect::<std::collections::BTreeSet<_>>();
    previous
        .entries
        .iter()
        .filter(|entry| !current_ids.contains(&entry.identity))
        .cloned()
        .collect()
}

#[derive(Debug, Clone, Default)]
pub struct ClusterReconcileResult {
    pub applied: Vec<PathBuf>,
    pub skipped: Vec<PathBuf>,
    pub errors: Vec<String>,
    pub pruned: Vec<ClusterObjectIdentity>,
}

/// Resolve and validate every Kubernetes document in the Cluster tree before
/// any apply. This rejects malformed/duplicate identities and symlink escapes
/// up front, and returns a deterministic source/revision inventory.
pub fn resolve_cluster_tree(cluster_path: &Path) -> Result<Vec<ClusterManifest>, Box<dyn Error>> {
    let root = cluster_path
        .canonicalize()
        .map_err(|error| format!("cluster path {}: {error}", cluster_path.display()))?;
    if !root.is_dir() {
        return Err(format!("cluster path is not a directory: {}", root.display()).into());
    }

    let paths = collect_cluster_manifests(&root)?;
    let mut manifests = Vec::new();
    let mut identities = BTreeMap::<ClusterObjectIdentity, String>::new();
    for path in paths {
        let relative = path
            .strip_prefix(&root)
            .map_err(|_| format!("Cluster source escaped root: {}", path.display()))?;
        let source = relative
            .to_string_lossy()
            .replace(std::path::MAIN_SEPARATOR, "/");
        let text = fs::read_to_string(&path)?;
        let revision = content_revision(text.as_bytes());
        let documents = serde_yaml::Deserializer::from_str(&text);
        let mut document_count = 0;
        for (index, document) in documents.enumerate() {
            let value = Value::deserialize(document)
                .map_err(|error| format!("invalid YAML document {source}#{index}: {error}"))?;
            if value.is_null() {
                continue;
            }
            let mapping = value.as_mapping().ok_or_else(|| {
                format!("Cluster document {source}#{index} must be a YAML mapping")
            })?;
            let api_version = string_field(mapping, "apiVersion", source.as_str(), index)?;
            let kind = string_field(mapping, "kind", source.as_str(), index)?;
            let metadata = mapping
                .get(Value::String("metadata".into()))
                .and_then(Value::as_mapping)
                .ok_or_else(|| format!("Cluster document {source}#{index} is missing metadata"))?;
            let name = metadata
                .get(Value::String("name".into()))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .ok_or_else(|| {
                    format!("Cluster document {source}#{index} is missing metadata.name")
                })?
                .to_string();
            let namespace = metadata
                .get(Value::String("namespace".into()))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .trim()
                .to_string();
            let identity = ClusterObjectIdentity {
                api_version,
                kind,
                namespace,
                name,
            };
            if let Some(previous) = identities.insert(identity.clone(), source.clone()) {
                return Err(format!(
                    "duplicate Cluster object {}/{} from {} and {}",
                    identity.kind, identity.name, previous, source
                )
                .into());
            }
            manifests.push(ClusterManifest {
                identity,
                category: ClusterCategory::from_relative_path(relative),
                source: source.clone(),
                revision: revision.clone(),
                document: index,
                path: path.clone(),
            });
            document_count += 1;
        }
        if document_count == 0 {
            return Err(format!("Cluster source {source} contains no Kubernetes documents").into());
        }
    }
    manifests.sort_by(|a, b| {
        a.category
            .cmp(&b.category)
            .then_with(|| a.identity.cmp(&b.identity))
            .then_with(|| a.source.cmp(&b.source))
            .then_with(|| a.document.cmp(&b.document))
    });
    Ok(manifests)
}

fn string_field(
    mapping: &serde_yaml::Mapping,
    field: &str,
    source: &str,
    document: usize,
) -> Result<String, Box<dyn Error>> {
    mapping
        .get(Value::String(field.to_string()))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .ok_or_else(|| format!("Cluster document {source}#{document} is missing {field}").into())
}

fn content_revision(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("sha256:{:x}", hasher.finalize())
}

/// Collect YAML manifests under cluster_path (recursive).
/// Skips examples, docs, and non-manifest files. Symlinks are rejected rather
/// than followed so a committed tree cannot escape the declared mount root.
pub fn collect_cluster_manifests(cluster_path: &Path) -> Result<Vec<PathBuf>, Box<dyn Error>> {
    let root = cluster_path
        .canonicalize()
        .map_err(|error| format!("cluster path {}: {error}", cluster_path.display()))?;
    if !root.is_dir() {
        return Err(format!("cluster path is not a directory: {}", root.display()).into());
    }
    let mut out = Vec::new();
    collect_manifests_rec(&root, &root, &mut out)?;
    out.sort_by(|a, b| {
        let a_category = a
            .strip_prefix(&root)
            .map(ClusterCategory::from_relative_path)
            .unwrap_or(ClusterCategory::Other);
        let b_category = b
            .strip_prefix(&root)
            .map(ClusterCategory::from_relative_path)
            .unwrap_or(ClusterCategory::Other);
        a_category.cmp(&b_category).then_with(|| a.cmp(b))
    });
    Ok(out)
}

fn collect_manifests_rec(
    dir: &Path,
    root: &Path,
    out: &mut Vec<PathBuf>,
) -> Result<(), Box<dyn Error>> {
    if !dir.is_dir() {
        return Ok(());
    }
    for ent in fs::read_dir(dir)? {
        let ent = ent?;
        let path = ent.path();
        // Skip common non-manifest trees before inspecting symlinks or
        // canonicalizing their contents. A skipped directory may itself be a
        // symlink (for example, a checked-out node_modules tree) and must not
        // make an otherwise valid Cluster tree fail closed.
        let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
        if matches!(name, ".git" | "node_modules" | "target" | "_output") {
            continue;
        }
        let file_type = ent.file_type()?;
        if file_type.is_symlink() {
            return Err(format!("Cluster tree must not contain symlink {}", path.display()).into());
        }
        let canonical = path.canonicalize().map_err(|error| {
            format!(
                "unable to resolve Cluster source {}: {error}",
                path.display()
            )
        })?;
        if !canonical.starts_with(root) {
            return Err(format!("Cluster source escapes root: {}", path.display()).into());
        }
        if path.is_dir() {
            collect_manifests_rec(&path, root, out)?;
            continue;
        }
        // The controller intentionally collects every candidate YAML before
        // parsing it. `resolve_cluster_tree` must reject malformed/unknown
        // documents rather than silently treating them as out-of-scope.
        if is_candidate_yaml(&path) {
            out.push(path);
        }
    }
    Ok(())
}

fn is_candidate_yaml(path: &Path) -> bool {
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    (name.ends_with(".yaml") || name.ends_with(".yml"))
        && !name.contains(".example.")
        && !name.ends_with(".yaml.example")
        && !name.ends_with(".yml.example")
        && name != "readme.yaml"
        && name != "secrets.yaml"
}

#[cfg(test)]
fn path_under_packages(cluster_path: &Path, path: &Path) -> bool {
    let root = cluster_path
        .canonicalize()
        .unwrap_or_else(|_| cluster_path.to_path_buf());
    path.strip_prefix(root)
        .ok()
        .and_then(|relative| relative.components().next())
        .map(|component| component.as_os_str() == "packages")
        .unwrap_or(false)
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

/// Reconcile a validated Cluster tree and maintain an exact last-known-good
/// inventory. This is the controller-facing API; the legacy wrapper above
/// remains a one-shot compatibility adapter without persistence.
pub fn reconcile_cluster_dir_with_inventory(
    cluster_path: &Path,
    inventory_path: &Path,
    dry_run: bool,
) -> Result<ClusterReconcileResult, Box<dyn Error>> {
    let root = cluster_path
        .canonicalize()
        .map_err(|error| format!("cluster path {}: {error}", cluster_path.display()))?;
    let manifests = resolve_cluster_tree(&root)?;
    let current = ClusterInventory::new(&root, &manifests);
    let previous = load_cluster_inventory(inventory_path)?;
    if let Some(previous) = previous.as_ref() {
        if previous.cluster_path != current.cluster_path {
            return Err(format!(
                "Cluster inventory {} belongs to {}, not {}",
                inventory_path.display(),
                previous.cluster_path,
                current.cluster_path
            )
            .into());
        }
    }

    let mut result = ClusterReconcileResult::default();
    let mut applied_files = std::collections::BTreeSet::new();
    for manifest in &manifests {
        if !applied_files.insert(manifest.path.clone()) {
            continue;
        }
        match apply_one(&manifest.path, dry_run) {
            Ok(()) => result.applied.push(manifest.path.clone()),
            Err(error) => result
                .errors
                .push(format!("{}: {error}", manifest.path.display())),
        }
    }
    if !result.errors.is_empty() {
        return Ok(result);
    }

    if let Some(previous) = previous.as_ref() {
        let mut stale = stale_cluster_inventory(previous, &current);
        // Remove dependents before their package/provider substrate. This is
        // still exact-inventory deletion; category only supplies the safe
        // reverse ordering after identity has been proven stale.
        stale.sort_by(|a, b| {
            b.category
                .cmp(&a.category)
                .then_with(|| b.identity.cmp(&a.identity))
        });
        for stale in stale {
            if !dry_run {
                delete_exact(&stale.identity)?;
            }
            result.pruned.push(stale.identity);
        }
    }
    if !dry_run {
        save_cluster_inventory(inventory_path, &current)?;
    }
    Ok(result)
}

fn delete_exact(identity: &ClusterObjectIdentity) -> Result<(), Box<dyn Error>> {
    let resource = qualified_resource_name(identity);
    let mut args = vec!["delete", resource.as_str(), identity.name.as_str()];
    if !identity.namespace.is_empty() {
        args.extend(["-n", identity.namespace.as_str()]);
    }
    args.push("--ignore-not-found=true");
    run_kubectl(&args)
}

fn qualified_resource_name(identity: &ClusterObjectIdentity) -> String {
    let kind = identity.kind.to_ascii_lowercase();
    if let Some((group, _version)) = identity.api_version.split_once('/') {
        let group = group.trim();
        if !group.is_empty() {
            return format!("{kind}.{group}");
        }
    }
    kind
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

    #[test]
    fn resolves_category_owned_documents_before_any_apply() {
        let dir = std::env::temp_dir().join(format!(
            "hops-cg-resolve-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        for category in ["shared", "providers", "registry"] {
            fs::create_dir_all(dir.join(category)).unwrap();
        }
        fs::write(
            dir.join("shared/base.yaml"),
            "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: shared\n",
        )
        .unwrap();
        fs::write(
            dir.join("providers/aws.yaml"),
            "apiVersion: pkg.crossplane.io/v1\nkind: Provider\nmetadata:\n  name: aws\n",
        )
        .unwrap();
        fs::write(
            dir.join("registry/service.yaml"),
            "apiVersion: v1\nkind: Service\nmetadata:\n  name: registry\n  namespace: crossplane-system\n",
        )
        .unwrap();

        let manifests = resolve_cluster_tree(&dir).unwrap();
        assert_eq!(manifests.len(), 3);
        assert_eq!(manifests[0].category, ClusterCategory::Registry);
        assert_eq!(manifests[1].category, ClusterCategory::Providers);
        assert_eq!(manifests[2].category, ClusterCategory::Shared);
        assert!(manifests
            .iter()
            .all(|manifest| manifest.revision.starts_with("sha256:")));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn duplicate_identity_is_rejected_before_mutation() {
        let dir = std::env::temp_dir().join(format!(
            "hops-cg-duplicate-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(dir.join("providers")).unwrap();
        fs::create_dir_all(dir.join("shared")).unwrap();
        let document = "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: duplicate\n";
        fs::write(dir.join("providers/one.yaml"), document).unwrap();
        fs::write(dir.join("shared/two.yaml"), document).unwrap();
        let error = resolve_cluster_tree(&dir).unwrap_err().to_string();
        assert!(error.contains("duplicate Cluster object"), "{error}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn inventory_round_trip_and_stale_diff_are_exact() {
        let dir = std::env::temp_dir().join(format!(
            "hops-cg-inventory-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(dir.join("providers")).unwrap();
        fs::write(
            dir.join("providers/aws.yaml"),
            "apiVersion: pkg.crossplane.io/v1\nkind: Provider\nmetadata:\n  name: aws\n",
        )
        .unwrap();
        let manifests = resolve_cluster_tree(&dir).unwrap();
        let old = ClusterInventory::new(&dir, &manifests);
        let inventory_path = dir.join("state/cluster-inventory.json");
        save_cluster_inventory(&inventory_path, &old).unwrap();
        let loaded = load_cluster_inventory(&inventory_path).unwrap().unwrap();
        assert_eq!(loaded, old);

        let empty = ClusterInventory {
            schema_version: INVENTORY_SCHEMA_VERSION,
            cluster_path: dir.to_string_lossy().into_owned(),
            entries: Vec::new(),
        };
        let stale = stale_cluster_inventory(&loaded, &empty);
        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0].identity.name, "aws");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn qualified_resource_name_includes_api_group() {
        let grouped = ClusterObjectIdentity {
            api_version: "pkg.crossplane.io/v1".into(),
            kind: "Provider".into(),
            namespace: String::new(),
            name: "aws".into(),
        };
        assert_eq!(
            qualified_resource_name(&grouped),
            "provider.pkg.crossplane.io"
        );

        let core = ClusterObjectIdentity {
            api_version: "v1".into(),
            kind: "Namespace".into(),
            namespace: String::new(),
            name: "default".into(),
        };
        assert_eq!(qualified_resource_name(&core), "namespace");
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_cluster_sources_fail_closed() {
        use std::os::unix::fs::symlink;
        let dir = std::env::temp_dir().join(format!(
            "hops-cg-symlink-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let outside =
            std::env::temp_dir().join(format!("hops-cg-outside-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(
            outside.join("owned.yaml"),
            "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: outside\n",
        )
        .unwrap();
        symlink(&outside, dir.join("shared")).unwrap();
        let error = resolve_cluster_tree(&dir).unwrap_err().to_string();
        assert!(error.contains("symlink"), "{error}");
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&outside);
    }

    #[cfg(unix)]
    #[test]
    fn skipped_symlinked_directories_are_ignored() {
        use std::os::unix::fs::symlink;
        let dir = std::env::temp_dir().join(format!(
            "hops-cg-skipped-symlink-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let outside =
            std::env::temp_dir().join(format!("hops-cg-skipped-outside-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(
            outside.join("ignored.yaml"),
            "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: ignored\n",
        )
        .unwrap();
        symlink(&outside, dir.join("node_modules")).unwrap();

        let manifests = collect_cluster_manifests(&dir).unwrap();
        assert!(manifests.is_empty());

        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&outside);
    }
}
