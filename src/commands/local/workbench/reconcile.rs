//! Application reconcile: helm template + label inject + apply.

use super::application::{load_applications, resolve_source_path, Application};
use serde_yaml::Value;
use std::collections::BTreeMap;
use std::error::Error;
use std::path::{Path, PathBuf};
use std::process::Command;

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
    /// Extra values merged last (runtime inject).
    pub runtime_values: BTreeMap<String, Value>,
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
        let values_path = std::env::temp_dir().join(format!(
            "hops-lwb-values-{}-{}.yaml",
            std::process::id(),
            release
        ));
        std::fs::write(&values_path, values_yaml)?;
        let output = Command::new("helm")
            .args([
                "template",
                release,
                &chart_path.to_string_lossy(),
                "--namespace",
                namespace,
                "--values",
                &values_path.to_string_lossy(),
            ])
            .output()?;
        let _ = std::fs::remove_file(&values_path);
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
        for doc in split_yaml_docs_owned(yaml) {
            if doc.trim().is_empty() {
                continue;
            }
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
pub fn merge_helm_values(
    app_values: Option<&Value>,
    runtime: &BTreeMap<String, Value>,
) -> Value {
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

fn split_yaml_docs_owned(s: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut buf = String::new();
    for line in s.lines() {
        if line.trim() == "---" {
            if !buf.trim().is_empty() {
                parts.push(std::mem::take(&mut buf));
            }
        } else {
            buf.push_str(line);
            buf.push('\n');
        }
    }
    if !buf.trim().is_empty() {
        parts.push(buf);
    }
    parts
}

fn inject_labels_into_value(value: &mut Value, labels: &BTreeMap<String, String>) {
    let Some(root) = value.as_mapping_mut() else {
        return;
    };
    let meta_key = Value::String("metadata".into());
    if !root.contains_key(&meta_key) {
        root.insert(meta_key.clone(), Value::Mapping(serde_yaml::Mapping::new()));
    }
    let Some(meta) = root.get_mut(&meta_key).and_then(|v| v.as_mapping_mut()) else {
        return;
    };
    let labels_key = Value::String("labels".into());
    if !meta.contains_key(&labels_key) {
        meta.insert(labels_key.clone(), Value::Mapping(serde_yaml::Mapping::new()));
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
    for doc in split_yaml_docs_owned(rendered) {
        if doc.trim().is_empty() {
            continue;
        }
        let mut value: Value = serde_yaml::from_str(&doc)
            .map_err(|e| format!("parse rendered manifest: {e}\n---\n{doc}"))?;
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

fn build_runtime_values(opts: &ReconcileOptions) -> BTreeMap<String, Value> {
    let mut runtime = opts.runtime_values.clone();
    runtime
        .entry("local".into())
        .or_insert(Value::Bool(true));
    runtime.insert("namespace".into(), Value::String(opts.namespace.clone()));
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
        m.insert(
            WORKSPACE_ENV_LABEL.to_string(),
            opts.workspace_name.clone(),
        );
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

    let runtime = build_runtime_values(opts);
    let merged = merge_helm_values(app.spec.source.helm.values.as_ref(), &runtime);
    let values_yaml = values_to_yaml(&merged)?;
    let release = sanitize_release_name(&app.metadata.name);
    let rendered = helm.template(&release, &chart_path, &opts.namespace, &values_yaml)?;
    let labels = inject_labels(&opts.workspace_name, &app.metadata.name);
    let labeled = render_labels_into_manifests(&rendered, &labels)?;
    let labeled = ensure_namespace_on_docs(&labeled, &opts.namespace)?;

    let applied = if opts.dry_run {
        false
    } else {
        kubectl.apply(&labeled)?;
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

fn ensure_namespace_on_docs(yaml: &str, namespace: &str) -> Result<String, Box<dyn Error>> {
    let mut out = Vec::new();
    for doc in split_yaml_docs_owned(yaml) {
        if doc.trim().is_empty() {
            continue;
        }
        let mut value: Value = serde_yaml::from_str(&doc)?;
        if let Some(root) = value.as_mapping_mut() {
            let kind = root
                .get(Value::String("kind".into()))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let cluster_scoped = matches!(
                kind,
                "Namespace" | "ClusterRole" | "ClusterRoleBinding" | "CustomResourceDefinition"
            );
            if !cluster_scoped {
                let meta_key = Value::String("metadata".into());
                if let Some(meta) = root.get_mut(&meta_key).and_then(|v| v.as_mapping_mut()) {
                    meta.insert(
                        Value::String("namespace".into()),
                        Value::String(namespace.to_string()),
                    );
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
appRuntime: cluster-dev
image:
  tag: app
"#,
        )
        .unwrap();
        let mut runtime = BTreeMap::new();
        runtime.insert("namespace".into(), Value::String("hops-wt-alice".into()));
        runtime.insert("appRuntime".into(), Value::String("host".into()));
        let merged = merge_helm_values(Some(&app), &runtime);
        assert_eq!(merged["local"], Value::Bool(true));
        assert_eq!(merged["appRuntime"], Value::String("host".into()));
        assert_eq!(merged["namespace"], Value::String("hops-wt-alice".into()));
        assert_eq!(merged["image"]["tag"], Value::String("app".into()));
    }

    #[test]
    fn inject_labels_contains_required_keys() {
        let labels = inject_labels("alice", "e2e-ui-api");
        assert_eq!(
            labels.get("app.kubernetes.io/managed-by").map(String::as_str),
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
"#;
        let labels = inject_labels("ws", "app");
        let out = render_labels_into_manifests(rendered, &labels).unwrap();
        assert!(out.contains("app.kubernetes.io/managed-by: hops-local-gitops"));
        assert!(out.contains("hops.ops.com.ai/local-env: ws"));
        assert!(out.contains("hops.ops.com.ai/local-app: app"));
        assert!(out.contains("existing: keep"));
        assert_eq!(out.matches("hops-local-gitops").count(), 2);
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
"#;
        std::fs::write(env.join("app.yaml"), app_yaml).unwrap();

        let helm = MockHelm {
            body: "apiVersion: v1\nkind: Service\nmetadata:\n  name: t\n".into(),
        };
        let kubectl = MockKubectl {
            applied: Mutex::new(Vec::new()),
            namespaces: Mutex::new(Vec::new()),
        };
        let opts = ReconcileOptions {
            namespace: "hops-wt-alice".into(),
            workspace_name: "alice".into(),
            runtime_values: BTreeMap::new(),
            dry_run: false,
        };
        let results = reconcile_applications(&env, &opts, &helm, &kubectl).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].namespace, "hops-wt-alice");
        assert!(results[0].applied);
        let applied = kubectl.applied.lock().unwrap();
        assert_eq!(applied.len(), 1);
        assert!(applied[0].contains("hops-local-gitops"));
        assert!(applied[0].contains("hops.ops.com.ai/local-env: alice"));
        assert!(applied[0].contains("namespace: hops-wt-alice"));
        assert_eq!(
            kubectl.namespaces.lock().unwrap().as_slice(),
            &["hops-wt-alice".to_string()]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
