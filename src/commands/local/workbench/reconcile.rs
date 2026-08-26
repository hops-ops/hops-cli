//! Application reconcile: helm template + label inject + apply.

use super::application::{load_applications, resolve_source_path, Application};
use serde::{Deserialize, Serialize};
use serde_yaml::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Label value for `app.kubernetes.io/managed-by`.
pub const MANAGED_BY_VALUE: &str = "hops-local-gitops";
/// Workspace / env label key.
pub const WORKSPACE_ENV_LABEL: &str = "hops.ops.com.ai/local-env";
/// App name label key.
pub const WORKSPACE_APP_LABEL: &str = "hops.ops.com.ai/local-app";

#[derive(Debug, Clone, Default)]
pub struct ReconcileOptions {
    /// Destination namespace override (from --namespace / workspace name).
    pub namespace: String,
    /// Workspace name for labels.
    pub workspace_name: String,
    /// Extra values merged last (runtime inject) — shared across apps.
    pub runtime_values: BTreeMap<String, Value>,
    /// Per-app hostPath for source delivery (app name → absolute host path).
    /// Injected as `sourceDelivery.hostPath` for that app only.
    pub app_delivery_host_paths: BTreeMap<String, PathBuf>,
    /// Delivery mode string injected when set (`hostPath` | `sync` | `none`).
    pub delivery_mode: Option<String>,
    /// When true, only render (no apply). Used by tests.
    pub dry_run: bool,
}

#[derive(Debug, Clone)]
pub struct ReconcileResult {
    pub app_name: String,
    pub chart_path: PathBuf,
    pub namespace: String,
    pub rendered_yaml: String,
    pub applied: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ManagedObjectRef {
    api_version: String,
    kind: String,
    name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    namespace: Option<String>,
}

/// Abstraction over `helm template` for tests.
pub trait HelmRunner {
    fn template(
        &self,
        release: &str,
        chart_path: &Path,
        namespace: &str,
        values_yaml: &str,
    ) -> Result<String, Box<dyn Error>>;
}

/// Abstraction over kubectl apply for tests.
pub trait KubectlApplier {
    fn ensure_namespace(
        &self,
        namespace: &str,
        labels: &BTreeMap<String, String>,
    ) -> Result<(), Box<dyn Error>>;
    fn apply(&self, yaml: &str) -> Result<(), Box<dyn Error>>;
    fn prune(
        &self,
        app_name: &str,
        inventory_namespace: &str,
        desired_yaml: &str,
    ) -> Result<(), Box<dyn Error>>;
    fn record_inventory(
        &self,
        app_name: &str,
        workspace_name: &str,
        inventory_namespace: &str,
        desired_yaml: &str,
    ) -> Result<(), Box<dyn Error>>;
}

struct TemporaryValuesFile {
    path: PathBuf,
}

impl TemporaryValuesFile {
    fn create(contents: &str) -> Result<Self, Box<dyn Error>> {
        let path =
            std::env::temp_dir().join(format!("hops-lwb-values-{}.yaml", uuid::Uuid::new_v4()));
        let temporary = Self { path };
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary.path)?;
        file.write_all(contents.as_bytes())?;
        Ok(temporary)
    }
}

impl Drop for TemporaryValuesFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Real helm binary runner.
pub struct SystemHelm;

impl HelmRunner for SystemHelm {
    fn template(
        &self,
        release: &str,
        chart_path: &Path,
        namespace: &str,
        values_yaml: &str,
    ) -> Result<String, Box<dyn Error>> {
        let values_file = TemporaryValuesFile::create(values_yaml)?;
        let output = Command::new("helm")
            .args([
                "template",
                release,
                &chart_path.to_string_lossy(),
                "--namespace",
                namespace,
                "--values",
                &values_file.path.to_string_lossy(),
            ])
            .output()?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!(
                "helm template failed for {}: {}",
                chart_path.display(),
                stderr
            )
            .into());
        }
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }
}

/// Real kubectl apply via stdin helpers in parent module.
pub struct SystemKubectl;

impl KubectlApplier for SystemKubectl {
    fn ensure_namespace(
        &self,
        namespace: &str,
        labels: &BTreeMap<String, String>,
    ) -> Result<(), Box<dyn Error>> {
        let mut label_lines = String::new();
        for (k, v) in labels {
            label_lines.push_str(&format!("    {k}: {v}\n"));
        }
        let yaml = format!(
            "apiVersion: v1\nkind: Namespace\nmetadata:\n  name: {namespace}\n  labels:\n{label_lines}"
        );
        crate::commands::local::kubectl_apply_stdin(&yaml)
    }

    fn apply(&self, yaml: &str) -> Result<(), Box<dyn Error>> {
        // Apply one document at a time so a missing platform CRD (e.g. PSQLCluster
        // when the pack is not installed) does not prevent core Deploy/Service apply.
        let mut hard_errors = Vec::new();
        for doc in parse_yaml_docs(yaml)? {
            let doc = serde_yaml::to_string(&doc)?;
            match crate::commands::local::kubectl_apply_stdin(&doc) {
                Ok(()) => {}
                Err(e) => {
                    let msg = e.to_string();
                    if is_soft_apply_error(&msg) {
                        log::warn!("skipping resource (platform CRD/type unavailable): {msg}");
                    } else {
                        hard_errors.push(msg);
                    }
                }
            }
        }
        if hard_errors.is_empty() {
            Ok(())
        } else {
            Err(format!("kubectl apply failed:\n  - {}", hard_errors.join("\n  - ")).into())
        }
    }

    fn prune(
        &self,
        app_name: &str,
        inventory_namespace: &str,
        desired_yaml: &str,
    ) -> Result<(), Box<dyn Error>> {
        let Some(previous) = load_inventory(app_name, inventory_namespace)? else {
            // First prune-enabled reconcile seeds inventory after a successful
            // apply. It must not infer ownership from broad label queries.
            return Ok(());
        };
        // A worktree Application may render shared resources in another
        // namespace, but its prune inventory is owned by this workspace. Never
        // let one workspace delete another namespace's shared identity objects.
        let previous = object_refs_in_namespace(previous, inventory_namespace);
        let desired =
            object_refs_in_namespace(managed_object_refs(desired_yaml)?, inventory_namespace);
        let stale = stale_object_refs(&previous, &desired);
        if stale.is_empty() {
            return Ok(());
        }

        let delete_yaml = object_refs_as_delete_yaml(&stale)?;
        let mut child = crate::commands::local::kubectl_command(&[
            "delete",
            "--ignore-not-found=true",
            "--wait=true",
            "-f",
            "-",
        ])
        .stdin(Stdio::piped())
        .spawn()?;
        child
            .stdin
            .as_mut()
            .ok_or("failed to open kubectl delete stdin")?
            .write_all(delete_yaml.as_bytes())?;
        let status = child.wait()?;
        if !status.success() {
            return Err(format!("kubectl prune exited with {status}").into());
        }
        log::info!(
            "Pruned {} stale object(s) for Application {}",
            stale.len(),
            app_name
        );
        Ok(())
    }

    fn record_inventory(
        &self,
        app_name: &str,
        workspace_name: &str,
        inventory_namespace: &str,
        desired_yaml: &str,
    ) -> Result<(), Box<dyn Error>> {
        let refs =
            object_refs_in_namespace(managed_object_refs(desired_yaml)?, inventory_namespace);
        let resources_json = serde_json::to_string(&refs)?;
        let name = inventory_name(app_name);
        let inventory = serde_json::json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": {
                "name": name,
                "namespace": inventory_namespace,
                "labels": {
                    "app.kubernetes.io/managed-by": MANAGED_BY_VALUE,
                    (WORKSPACE_ENV_LABEL): workspace_name,
                    (WORKSPACE_APP_LABEL): app_name,
                }
            },
            "data": {
                "resources.json": resources_json,
            }
        });
        crate::commands::local::kubectl_apply_stdin(&serde_yaml::to_string(&inventory)?)
    }
}

fn inventory_name(app_name: &str) -> String {
    format!("hops-lgi-{}", sanitize_release_name(app_name))
}

fn managed_object_refs(yaml: &str) -> Result<Vec<ManagedObjectRef>, Box<dyn Error>> {
    let mut refs = BTreeSet::new();
    for value in parse_yaml_docs(yaml)? {
        let Some(root) = value.as_mapping() else {
            continue;
        };
        let api_version = root
            .get(Value::String("apiVersion".into()))
            .and_then(Value::as_str);
        let kind = root
            .get(Value::String("kind".into()))
            .and_then(Value::as_str);
        let metadata = root
            .get(Value::String("metadata".into()))
            .and_then(Value::as_mapping);
        let name = metadata
            .and_then(|m| m.get(Value::String("name".into())))
            .and_then(Value::as_str);
        let (Some(api_version), Some(kind), Some(name)) = (api_version, kind, name) else {
            continue;
        };
        let namespace = metadata
            .and_then(|m| m.get(Value::String("namespace".into())))
            .and_then(Value::as_str)
            .filter(|ns| !ns.is_empty())
            .map(str::to_string);
        refs.insert(ManagedObjectRef {
            api_version: api_version.to_string(),
            kind: kind.to_string(),
            name: name.to_string(),
            namespace,
        });
    }
    Ok(refs.into_iter().collect())
}

fn stale_object_refs(
    previous: &[ManagedObjectRef],
    desired: &[ManagedObjectRef],
) -> Vec<ManagedObjectRef> {
    let desired: BTreeSet<_> = desired.iter().collect();
    previous
        .iter()
        .filter(|item| !desired.contains(item))
        .cloned()
        .collect()
}

fn object_refs_in_namespace(refs: Vec<ManagedObjectRef>, namespace: &str) -> Vec<ManagedObjectRef> {
    refs.into_iter()
        .filter(|item| item.namespace.as_deref() == Some(namespace))
        .collect()
}

fn object_refs_as_delete_yaml(refs: &[ManagedObjectRef]) -> Result<String, Box<dyn Error>> {
    let mut docs = Vec::new();
    for item in refs {
        let mut metadata = serde_json::Map::new();
        metadata.insert("name".into(), serde_json::Value::String(item.name.clone()));
        if let Some(namespace) = &item.namespace {
            metadata.insert(
                "namespace".into(),
                serde_json::Value::String(namespace.clone()),
            );
        }
        let object = serde_json::json!({
            "apiVersion": item.api_version,
            "kind": item.kind,
            "metadata": metadata,
        });
        docs.push(serde_yaml::to_string(&object)?);
    }
    Ok(docs.join("---\n"))
}

fn load_inventory(
    app_name: &str,
    inventory_namespace: &str,
) -> Result<Option<Vec<ManagedObjectRef>>, Box<dyn Error>> {
    let name = inventory_name(app_name);
    let output = crate::commands::local::kubectl_command(&[
        "-n",
        inventory_namespace,
        "get",
        "configmap",
        &name,
        "-o",
        "json",
    ])
    .output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("NotFound") || stderr.contains("not found") {
            return Ok(None);
        }
        return Err(format!("failed to read GitOps inventory {name}: {stderr}").into());
    }
    let config_map: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    let raw = config_map
        .pointer("/data/resources.json")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| format!("GitOps inventory {name} has no data.resources.json"))?;
    Ok(Some(serde_json::from_str(raw)?))
}

/// Missing CRDs / unknown types are expected until platform packs are installed.
fn is_soft_apply_error(msg: &str) -> bool {
    let lower = msg.to_ascii_lowercase();
    lower.contains("no matches for kind")
        || lower.contains("no matches for")
        || lower.contains("ensure crds are installed")
        || lower.contains("the server doesn't have a resource type")
}

/// Merge chart-level application values with runtime inject.
/// Precedence: base (app helm values) ← runtime_values (runtime wins on key clash).
pub fn merge_helm_values(app_values: Option<&Value>, runtime: &BTreeMap<String, Value>) -> Value {
    let mut out = serde_yaml::Mapping::new();
    if let Some(Value::Mapping(m)) = app_values {
        for (k, v) in m {
            out.insert(k.clone(), deep_clone_value(v));
        }
    }
    for (k, v) in runtime {
        out.insert(Value::String(k.clone()), v.clone());
    }
    Value::Mapping(out)
}

fn deep_clone_value(v: &Value) -> Value {
    // serde_yaml::Value is already cloneable
    v.clone()
}

/// Labels stamped on every managed object.
pub fn inject_labels(workspace_name: &str, app_name: &str) -> BTreeMap<String, String> {
    let mut labels = BTreeMap::new();
    labels.insert(
        "app.kubernetes.io/managed-by".to_string(),
        MANAGED_BY_VALUE.to_string(),
    );
    labels.insert(WORKSPACE_ENV_LABEL.to_string(), workspace_name.to_string());
    labels.insert(WORKSPACE_APP_LABEL.to_string(), app_name.to_string());
    labels
}

fn parse_yaml_docs(s: &str) -> Result<Vec<Value>, Box<dyn Error>> {
    use serde::Deserialize;

    let mut docs = Vec::new();
    for document in serde_yaml::Deserializer::from_str(s) {
        let value = Value::deserialize(document)?;
        if !value.is_null() {
            docs.push(value);
        }
    }
    Ok(docs)
}

fn inject_labels_into_value(value: &mut Value, labels: &BTreeMap<String, String>) {
    let Some(root) = value.as_mapping_mut() else {
        return;
    };
    inject_labels_into_metadata_map(root, labels);

    // Workload kinds: also stamp pod template labels so selectors/discovery work.
    let kind = root
        .get(Value::String("kind".into()))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if matches!(
        kind,
        "Deployment" | "StatefulSet" | "DaemonSet" | "Job" | "ReplicaSet"
    ) {
        if let Some(spec) = root
            .get_mut(Value::String("spec".into()))
            .and_then(|v| v.as_mapping_mut())
        {
            if let Some(template) = spec
                .get_mut(Value::String("template".into()))
                .and_then(|v| v.as_mapping_mut())
            {
                inject_labels_into_metadata_map(template, labels);
            }
        }
    }
}

fn inject_labels_into_metadata_map(
    obj: &mut serde_yaml::Mapping,
    labels: &BTreeMap<String, String>,
) {
    let meta_key = Value::String("metadata".into());
    if !obj.contains_key(&meta_key) {
        obj.insert(meta_key.clone(), Value::Mapping(serde_yaml::Mapping::new()));
    }
    let Some(meta) = obj.get_mut(&meta_key).and_then(|v| v.as_mapping_mut()) else {
        return;
    };
    let labels_key = Value::String("labels".into());
    if !meta.contains_key(&labels_key) {
        meta.insert(
            labels_key.clone(),
            Value::Mapping(serde_yaml::Mapping::new()),
        );
    }
    let Some(label_map) = meta.get_mut(&labels_key).and_then(|v| v.as_mapping_mut()) else {
        return;
    };
    for (k, v) in labels {
        label_map.insert(Value::String(k.clone()), Value::String(v.clone()));
    }
}

/// Inject labels into every document in a multi-doc YAML stream.
pub fn render_labels_into_manifests(
    rendered: &str,
    labels: &BTreeMap<String, String>,
) -> Result<String, Box<dyn Error>> {
    let mut out_docs = Vec::new();
    for mut value in
        parse_yaml_docs(rendered).map_err(|e| format!("parse rendered manifest: {e}"))?
    {
        inject_labels_into_value(&mut value, labels);
        out_docs.push(serde_yaml::to_string(&value)?);
    }
    Ok(out_docs.join("---\n"))
}

fn chart_has_chart_yaml(chart_path: &Path) -> bool {
    chart_path.join("Chart.yaml").is_file() || chart_path.join("Chart.yml").is_file()
}

fn values_to_yaml(values: &Value) -> Result<String, Box<dyn Error>> {
    Ok(serde_yaml::to_string(values)?)
}

fn build_runtime_values(opts: &ReconcileOptions, app_name: &str) -> BTreeMap<String, Value> {
    let mut runtime = opts.runtime_values.clone();
    runtime.entry("local".into()).or_insert(Value::Bool(true));
    runtime.insert("namespace".into(), Value::String(opts.namespace.clone()));

    // sourceDelivery: mode + hostPath (usually the git worktree root for all apps).
    let mut sd = serde_yaml::Mapping::new();
    if let Some(mode) = &opts.delivery_mode {
        sd.insert(Value::String("mode".into()), Value::String(mode.clone()));
    }
    if let Some(host) = opts.app_delivery_host_paths.get(app_name) {
        sd.insert(
            Value::String("hostPath".into()),
            Value::String(host.display().to_string()),
        );
    }
    if !sd.is_empty() {
        // Merge into existing sourceDelivery mapping if runtime already has one.
        if let Some(Value::Mapping(existing)) = runtime.get("sourceDelivery").cloned() {
            let mut merged = existing;
            for (k, v) in sd {
                merged.insert(k, v);
            }
            runtime.insert("sourceDelivery".into(), Value::Mapping(merged));
        } else {
            runtime.insert("sourceDelivery".into(), Value::Mapping(sd));
        }
    }
    runtime
}

/// Reconcile all Applications under `env_path`.
pub fn reconcile_applications<H: HelmRunner, K: KubectlApplier>(
    env_path: &Path,
    opts: &ReconcileOptions,
    helm: &H,
    kubectl: &K,
) -> Result<Vec<ReconcileResult>, Box<dyn Error>> {
    let apps = load_applications(env_path)?;
    if apps.is_empty() {
        return Err(format!(
            "no Application YAML files found under {}",
            env_path.display()
        )
        .into());
    }

    let ns_labels = {
        let mut m = BTreeMap::new();
        m.insert(
            "app.kubernetes.io/managed-by".to_string(),
            MANAGED_BY_VALUE.to_string(),
        );
        m.insert(WORKSPACE_ENV_LABEL.to_string(), opts.workspace_name.clone());
        m
    };

    if !opts.dry_run {
        kubectl.ensure_namespace(&opts.namespace, &ns_labels)?;
    }

    let mut results = Vec::new();
    let mut errors: Vec<String> = Vec::new();

    for (app_file, app) in apps {
        match reconcile_one(&app_file, &app, opts, helm, kubectl) {
            Ok(r) => results.push(r),
            Err(e) => errors.push(format!("{}: {e}", app.metadata.name)),
        }
    }

    if !errors.is_empty() {
        return Err(format!(
            "reconcile failed for {} app(s):\n  - {}",
            errors.len(),
            errors.join("\n  - ")
        )
        .into());
    }
    Ok(results)
}

/// Reconcile one contained Helm chart directly, without materializing a
/// generated Application YAML file. This is the Local Workbench path: the
/// `(application root, chart path)` pair is the durable deploy identity and
/// the chart's rendered KRM is applied under the Environment ownership labels.
pub fn reconcile_deploy_chart<H: HelmRunner, K: KubectlApplier>(
    chart_path: &Path,
    app_name: &str,
    values: Value,
    opts: &ReconcileOptions,
    helm: &H,
    kubectl: &K,
) -> Result<ReconcileResult, Box<dyn Error>> {
    let chart_path = chart_path
        .canonicalize()
        .map_err(|error| format!("chart path {}: {error}", chart_path.display()))?;
    let parent = chart_path
        .parent()
        .ok_or_else(|| format!("chart path has no parent: {}", chart_path.display()))?;
    let source_name = chart_path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| format!("chart path has no portable name: {}", chart_path.display()))?;
    let app_file = parent.join(".hops-direct-application.yaml");
    let application = Application {
        api_version: super::application::APPLICATION_API_VERSION.to_string(),
        kind: super::application::APPLICATION_KIND.to_string(),
        metadata: super::application::ApplicationMetadata {
            name: app_name.to_string(),
            labels: None,
        },
        spec: super::application::ApplicationSpec {
            source: super::application::Source {
                path: source_name.to_string(),
                delivery_path: None,
                helm: super::application::HelmSource {
                    values: Some(values),
                },
            },
            destination: super::application::Destination {
                namespace: Some(opts.namespace.clone()),
            },
            sync_policy: super::application::SyncPolicy { prune: true },
        },
    };
    reconcile_one(&app_file, &application, opts, helm, kubectl)
}

fn reconcile_one<H: HelmRunner, K: KubectlApplier>(
    app_file: &Path,
    app: &Application,
    opts: &ReconcileOptions,
    helm: &H,
    kubectl: &K,
) -> Result<ReconcileResult, Box<dyn Error>> {
    let chart_path = resolve_source_path(app_file, &app.spec.source.path)?;
    if !chart_path.exists() {
        return Err(format!("chart path does not exist: {}", chart_path.display()).into());
    }
    if !chart_has_chart_yaml(&chart_path) {
        return Err(format!(
            "not a Helm chart (missing Chart.yaml): {}",
            chart_path.display()
        )
        .into());
    }

    let runtime = build_runtime_values(opts, &app.metadata.name);
    let merged = merge_helm_values(app.spec.source.helm.values.as_ref(), &runtime);
    let values_yaml = values_to_yaml(&merged)?;
    let release = sanitize_release_name(&app.metadata.name);
    let rendered = helm.template(&release, &chart_path, &opts.namespace, &values_yaml)?;
    reject_cluster_scoped_environment_objects(&rendered)?;
    let labels = inject_labels(&opts.workspace_name, &app.metadata.name);
    let labeled = render_labels_into_manifests(&rendered, &labels)?;
    let labeled = ensure_namespace_on_docs(&labeled, &opts.namespace)?;

    let applied = if opts.dry_run {
        false
    } else {
        if app.spec.sync_policy.prune {
            kubectl.prune(&app.metadata.name, &opts.namespace, &labeled)?;
        }
        kubectl.apply(&labeled)?;
        if app.spec.sync_policy.prune {
            kubectl.record_inventory(
                &app.metadata.name,
                &opts.workspace_name,
                &opts.namespace,
                &labeled,
            )?;
        }
        true
    };

    Ok(ReconcileResult {
        app_name: app.metadata.name.clone(),
        chart_path,
        namespace: opts.namespace.clone(),
        rendered_yaml: labeled,
        applied,
    })
}

fn reject_cluster_scoped_environment_objects(yaml: &str) -> Result<(), Box<dyn Error>> {
    for value in parse_yaml_docs(yaml)? {
        let Some(root) = value.as_mapping() else {
            return Err("rendered Environment document must be a mapping".into());
        };
        let kind = root
            .get(Value::String("kind".into()))
            .and_then(Value::as_str)
            .unwrap_or("");
        if is_cluster_scoped_kind(kind) {
            let name = root
                .get(Value::String("metadata".into()))
                .and_then(Value::as_mapping)
                .and_then(|metadata| metadata.get(Value::String("name".into())))
                .and_then(Value::as_str)
                .unwrap_or("<unknown>");
            return Err(format!(
                "Environment chart rendered cluster-scoped {kind} {name}; declare it in the Cluster tree"
            )
            .into());
        }
    }
    Ok(())
}

fn sanitize_release_name(name: &str) -> String {
    let mut s: String = name
        .to_ascii_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    while s.contains("--") {
        s = s.replace("--", "-");
    }
    s.trim_matches('-').chars().take(53).collect()
}

/// Kinds that are cluster-scoped (never get a namespace stamp).
fn is_cluster_scoped_kind(kind: &str) -> bool {
    matches!(
        kind,
        "Namespace"
            | "ClusterRole"
            | "ClusterRoleBinding"
            | "CustomResourceDefinition"
            | "ClusterProviderConfig"
            | "ClusterSecretStore"
            | "PriorityClass"
            | "StorageClass"
            | "PersistentVolume"
            | "MutatingWebhookConfiguration"
            | "ValidatingWebhookConfiguration"
    )
}

/// Workload / app kinds that live in the worktree app namespace when the chart
/// did not set `metadata.namespace`. Shared identity MRs (Project, Role,
/// HumanUser, …) set their own namespace in the chart and are **not** rewritten.
fn is_app_workload_kind(api_version: &str, kind: &str) -> bool {
    // Core / apps workloads
    if matches!(
        kind,
        "Deployment"
            | "StatefulSet"
            | "DaemonSet"
            | "ReplicaSet"
            | "Job"
            | "CronJob"
            | "Service"
            | "ServiceAccount"
            | "ConfigMap"
            | "Secret"
            | "PersistentVolumeClaim"
            | "NetworkPolicy"
            | "Ingress"
            | "HorizontalPodAutoscaler"
            | "PodDisruptionBudget"
            | "Role"
            | "RoleBinding"
    ) {
        // k8s rbac Role/RoleBinding use rbac.authorization.k8s.io — not Zitadel Role
        if kind == "Role" || kind == "RoleBinding" {
            return api_version.starts_with("rbac.authorization.k8s.io/");
        }
        return true;
    }
    // External Secrets (worktree OIDC secret materialization)
    if kind == "ExternalSecret" {
        return true;
    }
    // Worktree-scoped Zitadel OIDC app + instance Features (Login V2 for this UI)
    if kind == "Oidc" && api_version.contains("application.zitadel") {
        return true;
    }
    if kind == "Features" && api_version.contains("instance.zitadel") {
        return true;
    }
    false
}

/// Stamp worktree namespace only onto app workloads that lack `metadata.namespace`.
///
/// Shared identity resources (e.g. Project/Role/HumanUser with
/// `projectNamespace: default`) keep the namespace declared by the chart.
/// Never overwrites an already-set namespace.
pub fn ensure_namespace_on_docs(yaml: &str, namespace: &str) -> Result<String, Box<dyn Error>> {
    let mut out = Vec::new();
    for mut value in parse_yaml_docs(yaml)? {
        if let Some(root) = value.as_mapping_mut() {
            let kind = root
                .get(Value::String("kind".into()))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let api_version = root
                .get(Value::String("apiVersion".into()))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if is_cluster_scoped_kind(&kind) {
                out.push(serde_yaml::to_string(&value)?);
                continue;
            }
            let meta_key = Value::String("metadata".into());
            if let Some(meta) = root.get_mut(&meta_key).and_then(|v| v.as_mapping_mut()) {
                let ns_key = Value::String("namespace".into());
                let has_ns = meta
                    .get(&ns_key)
                    .and_then(|v| v.as_str())
                    .map(|s| !s.is_empty())
                    .unwrap_or(false);
                if !has_ns && is_app_workload_kind(&api_version, &kind) {
                    meta.insert(ns_key, Value::String(namespace.to_string()));
                }
            }
        }
        out.push(serde_yaml::to_string(&value)?);
    }
    Ok(out.join("---\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn merge_helm_values_runtime_wins() {
        let app = serde_yaml::from_str::<Value>(
            r#"
local: true
preview: false
image:
  tag: app
"#,
        )
        .unwrap();
        let mut runtime = BTreeMap::new();
        runtime.insert("namespace".into(), Value::String("alice".into()));
        runtime.insert("preview".into(), Value::Bool(true));
        let merged = merge_helm_values(Some(&app), &runtime);
        assert_eq!(merged["local"], Value::Bool(true));
        assert_eq!(merged["preview"], Value::Bool(true));
        assert_eq!(merged["namespace"], Value::String("alice".into()));
        assert_eq!(merged["image"]["tag"], Value::String("app".into()));
    }

    #[test]
    fn inject_labels_contains_required_keys() {
        let labels = inject_labels("alice", "e2e-ui-api");
        assert_eq!(
            labels
                .get("app.kubernetes.io/managed-by")
                .map(String::as_str),
            Some(MANAGED_BY_VALUE)
        );
        assert_eq!(
            labels.get(WORKSPACE_ENV_LABEL).map(String::as_str),
            Some("alice")
        );
        assert_eq!(
            labels.get(WORKSPACE_APP_LABEL).map(String::as_str),
            Some("e2e-ui-api")
        );
    }

    #[test]
    fn render_labels_into_manifests_stamps_all_docs() {
        let rendered = r#"
apiVersion: v1
kind: Service
metadata:
  name: svc
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: dep
  labels:
    existing: keep
spec:
  template:
    metadata:
      labels:
        app: dep
"#;
        let labels = inject_labels("ws", "app");
        let out = render_labels_into_manifests(rendered, &labels).unwrap();
        assert!(out.contains("app.kubernetes.io/managed-by: hops-local-gitops"));
        assert!(out.contains("hops.ops.com.ai/local-env: ws"));
        assert!(out.contains("hops.ops.com.ai/local-app: app"));
        assert!(out.contains("existing: keep"));
        // Deployment top-level + pod template both labeled
        assert!(out.matches("hops-local-gitops").count() >= 2);
        // Pod template must carry workspace labels for kubectl -l discovery
        let docs: Vec<&str> = out.split("---").collect();
        let dep = docs
            .iter()
            .find(|d| d.contains("kind: Deployment"))
            .unwrap();
        assert!(
            dep.contains("local-env: ws"),
            "deployment/pod template missing local-env: {dep}"
        );
    }

    #[test]
    fn parser_preserves_document_markers_inside_block_scalars() {
        let rendered = "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: sample\ndata:\n  script: |\n    before\n    ---\n    after\n---\napiVersion: v1\nkind: Service\nmetadata:\n  name: sample\n";
        let labels = inject_labels("ws", "app");
        let out = render_labels_into_manifests(rendered, &labels).unwrap();
        let docs = parse_yaml_docs(&out).unwrap();
        assert_eq!(docs.len(), 2);
        assert_eq!(
            docs[0]["data"]["script"].as_str(),
            Some("before\n---\nafter\n")
        );
    }

    #[test]
    fn inventory_diff_prunes_only_removed_exact_objects() {
        let previous = managed_object_refs(
            r#"
apiVersion: application.zitadel.m.crossplane.io/v1alpha1
kind: Oidc
metadata:
  name: e2e-ui-alice-web
  namespace: alice
---
apiVersion: project.zitadel.m.crossplane.io/v1alpha1
kind: Project
metadata:
  name: e2e-ui
  namespace: default
"#,
        )
        .unwrap();
        let desired = managed_object_refs(
            r#"
apiVersion: application.zitadel.m.crossplane.io/v1alpha1
kind: Oidc
metadata:
  name: e2e-ui-alice-web-g1
  namespace: alice
---
apiVersion: project.zitadel.m.crossplane.io/v1alpha1
kind: Project
metadata:
  name: e2e-ui
  namespace: default
"#,
        )
        .unwrap();

        let stale = stale_object_refs(&previous, &desired);
        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0].kind, "Oidc");
        assert_eq!(stale[0].name, "e2e-ui-alice-web");
        assert_eq!(stale[0].namespace.as_deref(), Some("alice"));

        let delete_yaml = object_refs_as_delete_yaml(&stale).unwrap();
        assert!(delete_yaml.contains("application.zitadel.m.crossplane.io/v1alpha1"));
        assert!(delete_yaml.contains("name: e2e-ui-alice-web"));
        assert!(delete_yaml.contains("namespace: alice"));
        assert!(!delete_yaml.contains("e2e-ui-alice-web-g1"));
        assert!(!delete_yaml.contains("kind: Project"));
    }

    #[test]
    fn prune_inventory_is_scoped_to_the_workspace_namespace() {
        let refs = managed_object_refs(
            r#"
apiVersion: application.zitadel.m.crossplane.io/v1alpha1
kind: Oidc
metadata:
  name: e2e-ui-alice-web-g1
  namespace: alice
---
apiVersion: project.zitadel.m.crossplane.io/v1alpha1
kind: Project
metadata:
  name: e2e-ui
  namespace: default
---
apiVersion: example.org/v1
kind: ClusterThing
metadata:
  name: shared
"#,
        )
        .unwrap();

        let scoped = object_refs_in_namespace(refs, "alice");
        assert_eq!(scoped.len(), 1);
        assert_eq!(scoped[0].kind, "Oidc");
        assert_eq!(scoped[0].namespace.as_deref(), Some("alice"));
    }

    struct MockHelm {
        body: String,
    }

    impl HelmRunner for MockHelm {
        fn template(
            &self,
            _release: &str,
            _chart_path: &Path,
            _namespace: &str,
            _values_yaml: &str,
        ) -> Result<String, Box<dyn Error>> {
            Ok(self.body.clone())
        }
    }

    struct MockKubectl {
        applied: Mutex<Vec<String>>,
        namespaces: Mutex<Vec<String>>,
        pruned: Mutex<Vec<String>>,
        inventories: Mutex<Vec<String>>,
    }

    impl KubectlApplier for MockKubectl {
        fn ensure_namespace(
            &self,
            namespace: &str,
            _labels: &BTreeMap<String, String>,
        ) -> Result<(), Box<dyn Error>> {
            self.namespaces.lock().unwrap().push(namespace.to_string());
            Ok(())
        }
        fn apply(&self, yaml: &str) -> Result<(), Box<dyn Error>> {
            self.applied.lock().unwrap().push(yaml.to_string());
            Ok(())
        }
        fn prune(
            &self,
            app_name: &str,
            _inventory_namespace: &str,
            _desired_yaml: &str,
        ) -> Result<(), Box<dyn Error>> {
            self.pruned.lock().unwrap().push(app_name.to_string());
            Ok(())
        }
        fn record_inventory(
            &self,
            app_name: &str,
            _workspace_name: &str,
            _inventory_namespace: &str,
            _desired_yaml: &str,
        ) -> Result<(), Box<dyn Error>> {
            self.inventories.lock().unwrap().push(app_name.to_string());
            Ok(())
        }
    }

    #[test]
    fn reconcile_applications_labels_and_namespace_override() {
        let dir = std::env::temp_dir().join(format!(
            "lwb-rec-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let chart = dir.join("chart");
        std::fs::create_dir_all(chart.join("templates")).unwrap();
        std::fs::write(
            chart.join("Chart.yaml"),
            "apiVersion: v2\nname: t\nversion: 0.1.0\n",
        )
        .unwrap();
        std::fs::write(chart.join("values.yaml"), "local: false\n").unwrap();
        std::fs::write(
            chart.join("templates/svc.yaml"),
            "apiVersion: v1\nkind: Service\nmetadata:\n  name: t\n",
        )
        .unwrap();

        let env = dir.join("env");
        std::fs::create_dir_all(&env).unwrap();
        let app_yaml = r#"
apiVersion: hops.local/v1alpha1
kind: Application
metadata:
  name: demo-app
spec:
  source:
    path: ../chart
    helm:
      values:
        local: true
  destination:
    namespace: should-be-overridden
  syncPolicy:
    prune: true
"#;
        std::fs::write(env.join("app.yaml"), app_yaml).unwrap();

        let helm = MockHelm {
            body: "apiVersion: v1\nkind: Service\nmetadata:\n  name: t\n".into(),
        };
        let kubectl = MockKubectl {
            applied: Mutex::new(Vec::new()),
            namespaces: Mutex::new(Vec::new()),
            pruned: Mutex::new(Vec::new()),
            inventories: Mutex::new(Vec::new()),
        };
        let opts = ReconcileOptions {
            namespace: "alice".into(),
            workspace_name: "alice".into(),
            runtime_values: BTreeMap::new(),
            app_delivery_host_paths: BTreeMap::new(),
            delivery_mode: None,
            dry_run: false,
        };
        let results = reconcile_applications(&env, &opts, &helm, &kubectl).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].namespace, "alice");
        assert!(results[0].applied);
        let applied = kubectl.applied.lock().unwrap();
        assert_eq!(applied.len(), 1);
        assert!(applied[0].contains("hops-local-gitops"));
        assert!(applied[0].contains("hops.ops.com.ai/local-env: alice"));
        assert!(applied[0].contains("namespace: alice"));
        assert_eq!(
            kubectl.namespaces.lock().unwrap().as_slice(),
            &["alice".to_string()]
        );
        assert_eq!(
            kubectl.pruned.lock().unwrap().as_slice(),
            &["demo-app".to_string()]
        );
        assert_eq!(
            kubectl.inventories.lock().unwrap().as_slice(),
            &["demo-app".to_string()]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reconcile_deploy_chart_does_not_materialize_application_file() {
        let dir = std::env::temp_dir().join(format!(
            "lwb-direct-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(dir.join("chart/templates")).unwrap();
        let chart = dir.join("chart");
        std::fs::write(
            chart.join("Chart.yaml"),
            "apiVersion: v2\nname: direct\nversion: 0.1.0\n",
        )
        .unwrap();
        std::fs::write(chart.join("templates/svc.yaml"), "kind: Service\n").unwrap();

        let helm = MockHelm {
            body: "apiVersion: v1\nkind: Service\nmetadata:\n  name: direct\n".into(),
        };
        let kubectl = MockKubectl {
            applied: Mutex::new(Vec::new()),
            namespaces: Mutex::new(Vec::new()),
            pruned: Mutex::new(Vec::new()),
            inventories: Mutex::new(Vec::new()),
        };
        let opts = ReconcileOptions {
            namespace: "local".into(),
            workspace_name: "local".into(),
            runtime_values: BTreeMap::new(),
            app_delivery_host_paths: BTreeMap::new(),
            delivery_mode: None,
            dry_run: true,
        };

        let result = reconcile_deploy_chart(
            &chart,
            "direct-app",
            serde_yaml::from_str("local: true\npreview: false\n").unwrap(),
            &opts,
            &helm,
            &kubectl,
        )
        .unwrap();

        assert_eq!(result.app_name, "direct-app");
        assert_eq!(result.chart_path, chart.canonicalize().unwrap());
        assert!(!result.applied);
        assert!(!dir.join(".hops-direct-application.yaml").exists());
        assert!(kubectl.applied.lock().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ensure_namespace_stamps_workload_missing_ns_only() {
        let yaml = r#"
apiVersion: v1
kind: Service
metadata:
  name: e2e-ui-ui
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: e2e-ui-ui
  namespace: already-set
---
apiVersion: project.zitadel.m.crossplane.io/v1alpha1
kind: Project
metadata:
  name: e2e-ui
  namespace: default
---
apiVersion: project.zitadel.m.crossplane.io/v1alpha1
kind: Role
metadata:
  name: e2e-role-user
  namespace: default
---
apiVersion: user.zitadel.m.crossplane.io/v1alpha1
kind: HumanUser
metadata:
  name: e2e-alice
  namespace: default
---
apiVersion: application.zitadel.m.crossplane.io/v1alpha1
kind: Oidc
metadata:
  name: e2e-ui-alice-web
---
apiVersion: instance.zitadel.m.crossplane.io/v1alpha1
kind: Features
metadata:
  name: e2e-ui-login-v2
---
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRole
metadata:
  name: never-namespaced
"#;
        let out = ensure_namespace_on_docs(yaml, "alice").unwrap();
        // Workload without ns → worktree
        assert!(
            out.contains("name: e2e-ui-ui\n  namespace: alice")
                || out.contains("namespace: alice\n  name: e2e-ui-ui")
                || (out.contains("kind: Service") && out.contains("namespace: alice"))
        );
        // Already set preserved
        assert!(out.contains("namespace: already-set"));
        // Shared identity preserved
        let project_idx = out.find("kind: Project").unwrap();
        let project_slice = &out[project_idx..project_idx + 200.min(out.len() - project_idx)];
        assert!(
            project_slice.contains("namespace: default"),
            "Project must keep default ns, got: {project_slice}"
        );
        assert!(out.contains("name: e2e-role-user"));
        assert!(out.contains("name: e2e-alice"));
        // Oidc/Features missing ns → worktree (app-scoped)
        let oidc_idx = out.find("kind: Oidc").unwrap();
        let oidc_slice = &out[oidc_idx..];
        assert!(
            oidc_slice.contains("namespace: alice"),
            "Oidc without ns should get worktree ns"
        );
        let feat_idx = out.find("kind: Features").unwrap();
        assert!(
            out[feat_idx..].contains("namespace: alice"),
            "Features without ns should get worktree ns"
        );
        // Cluster-scoped: no namespace key forced
        let cr_idx = out.find("kind: ClusterRole").unwrap();
        let cr_doc = &out[cr_idx..];
        // only this doc until end
        assert!(
            !cr_doc.lines().any(|l| l.trim() == "namespace: alice"),
            "ClusterRole must not get worktree ns"
        );
    }

    #[test]
    fn environment_rejects_cluster_scoped_objects_before_apply() {
        let error = reject_cluster_scoped_environment_objects(
            "apiVersion: v1\nkind: Namespace\nmetadata:\n  name: forbidden\n",
        )
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("cluster-scoped Namespace forbidden"),
            "{error}"
        );
    }

    #[test]
    fn ensure_namespace_dual_workspaces_preserve_shared_and_isolate_workloads() {
        let shared = r#"
apiVersion: project.zitadel.m.crossplane.io/v1alpha1
kind: Project
metadata:
  name: e2e-ui
  namespace: default
---
apiVersion: v1
kind: Service
metadata:
  name: e2e-ui-ui
"#;
        let alice = ensure_namespace_on_docs(shared, "alice").unwrap();
        let bob = ensure_namespace_on_docs(shared, "bob").unwrap();
        assert!(alice.contains("namespace: default"));
        assert!(bob.contains("namespace: default"));
        assert!(alice.contains("namespace: alice"));
        assert!(bob.contains("namespace: bob"));
        assert!(!alice.contains("namespace: bob"));
        assert!(!bob.contains("namespace: alice"));
    }

    #[test]
    fn build_runtime_values_injects_delivery_host_path() {
        // Usual case: same worktree root for every app in the workspace.
        let root = PathBuf::from("/worktrees/feature-x");
        let mut hosts = BTreeMap::new();
        hosts.insert("e2e-ui-ui".into(), root.clone());
        hosts.insert("e2e-ui-api".into(), root.clone());
        let opts = ReconcileOptions {
            namespace: "feature-x".into(),
            workspace_name: "feature-x".into(),
            runtime_values: BTreeMap::new(),
            app_delivery_host_paths: hosts,
            delivery_mode: Some("hostPath".into()),
            dry_run: true,
        };
        let ui = build_runtime_values(&opts, "e2e-ui-ui");
        let api = build_runtime_values(&opts, "e2e-ui-api");
        assert_eq!(
            ui["sourceDelivery"]["hostPath"],
            Value::String("/worktrees/feature-x".into())
        );
        assert_eq!(
            ui["sourceDelivery"]["hostPath"],
            api["sourceDelivery"]["hostPath"]
        );
        assert_eq!(
            ui["sourceDelivery"]["mode"],
            Value::String("hostPath".into())
        );
    }
}
