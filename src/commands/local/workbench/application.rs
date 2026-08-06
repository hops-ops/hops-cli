//! Parse hops.local Application documents and resolve chart source paths.

use serde::{Deserialize, Serialize};
use serde_yaml::Value;
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};

pub const APPLICATION_API_VERSION: &str = "hops.local/v1alpha1";
pub const APPLICATION_KIND: &str = "Application";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Application {
    pub api_version: String,
    pub kind: String,
    pub metadata: ApplicationMetadata,
    pub spec: ApplicationSpec,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApplicationMetadata {
    pub name: String,
    #[serde(default)]
    pub labels: Option<std::collections::BTreeMap<String, String>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApplicationSpec {
    pub source: Source,
    #[serde(default)]
    pub destination: Destination,
    #[serde(default)]
    pub sync_policy: SyncPolicy,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Source {
    /// Path to chart directory, relative to the Application YAML file.
    pub path: String,
    /// Host path to sync/mount as the app worktree (relative to this YAML file).
    ///
    /// **Per-app** — not the monorepo root for every pod. Examples:
    /// - UI: `../../../ui` (directory with package.json / vite)
    /// - API monorepo: `../../..` (directory with Cargo.toml / workspace)
    ///
    /// When omitted, defaults to the service directory that owns `.gitops`
    /// (parent of the `.gitops` component on the chart path).
    #[serde(default)]
    pub delivery_path: Option<String>,
    #[serde(default)]
    pub helm: HelmSource,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct HelmSource {
    #[serde(default)]
    pub values: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Destination {
    #[serde(default)]
    pub namespace: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncPolicy {
    #[serde(default)]
    pub prune: bool,
}

impl Default for SyncPolicy {
    fn default() -> Self {
        Self { prune: false }
    }
}

/// Parse a single Application document from YAML text.
pub fn parse_application_yaml(yaml: &str) -> Result<Application, Box<dyn Error>> {
    let app: Application = serde_yaml::from_str(yaml)
        .map_err(|e| format!("failed to parse Application YAML: {e}"))?;
    if app.api_version != APPLICATION_API_VERSION {
        return Err(format!(
            "unsupported apiVersion {:?} (expected {})",
            app.api_version, APPLICATION_API_VERSION
        )
        .into());
    }
    if app.kind != APPLICATION_KIND {
        return Err(format!(
            "unsupported kind {:?} (expected {})",
            app.kind, APPLICATION_KIND
        )
        .into());
    }
    if app.metadata.name.trim().is_empty() {
        return Err("Application metadata.name is required".into());
    }
    if app.spec.source.path.trim().is_empty() {
        return Err("Application spec.source.path is required".into());
    }
    Ok(app)
}

/// Resolve `spec.source.path` relative to the Application file's directory.
pub fn resolve_source_path(app_file: &Path, source_path: &str) -> Result<PathBuf, Box<dyn Error>> {
    let base = app_file
        .parent()
        .ok_or_else(|| format!("Application path has no parent: {}", app_file.display()))?;
    let joined = base.join(source_path);
    // Do not require canonicalize (charts may be created in tests before write completes).
    Ok(normalize_path(&joined))
}

/// Resolve the **per-app** host directory to mount/sync into the container.
///
/// Precedence:
/// 1. `spec.source.deliveryPath` (relative to Application file)
/// 2. Default: service root that owns the chart (directory containing `.gitops`)
pub fn resolve_delivery_host_path(
    app_file: &Path,
    app: &Application,
) -> Result<PathBuf, Box<dyn Error>> {
    if let Some(rel) = app
        .spec
        .source
        .delivery_path
        .as_ref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        let p = resolve_source_path(app_file, rel)?;
        return Ok(p
            .canonicalize()
            .unwrap_or(p));
    }
    let chart = resolve_source_path(app_file, &app.spec.source.path)?;
    Ok(service_root_from_chart_path(&chart)
        .canonicalize()
        .unwrap_or_else(|_| service_root_from_chart_path(&chart)))
}

/// Given `…/<service>/.gitops/deploy`, return `…/<service>`.
/// If `.gitops` is not in the path, return the chart path itself.
pub fn service_root_from_chart_path(chart_path: &Path) -> PathBuf {
    let mut comps: Vec<_> = chart_path.components().collect();
    // Find `.gitops` and drop it and everything after.
    if let Some(idx) = comps
        .iter()
        .position(|c| c.as_os_str() == std::ffi::OsStr::new(".gitops"))
    {
        comps.truncate(idx);
        let mut out = PathBuf::new();
        for c in comps {
            out.push(c.as_os_str());
        }
        if out.as_os_str().is_empty() {
            chart_path.to_path_buf()
        } else {
            out
        }
    } else {
        chart_path.to_path_buf()
    }
}

/// Collapse `.` and `..` without requiring the path to exist.
fn normalize_path(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
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

/// Load all Application YAMLs from a directory (or a single file).
pub fn load_applications(env_path: &Path) -> Result<Vec<(PathBuf, Application)>, Box<dyn Error>> {
    if !env_path.exists() {
        return Err(format!("env path does not exist: {}", env_path.display()).into());
    }
    if env_path.is_file() {
        let text = fs::read_to_string(env_path)?;
        let app = parse_application_yaml(&text)?;
        return Ok(vec![(env_path.to_path_buf(), app)]);
    }

    let mut entries: Vec<PathBuf> = fs::read_dir(env_path)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.is_file()
                && p.extension()
                    .and_then(|e| e.to_str())
                    .map(|ext| ext == "yaml" || ext == "yml")
                    .unwrap_or(false)
        })
        .collect();
    entries.sort();

    let mut apps = Vec::new();
    for path in entries {
        let text = fs::read_to_string(&path)
            .map_err(|e| format!("read {}: {e}", path.display()))?;
        // Skip non-Application docs quietly if kind mismatches after parse attempt.
        match parse_application_yaml(&text) {
            Ok(app) => apps.push((path, app)),
            Err(e) => {
                // Multi-doc or unrelated YAML: try first document only already failed.
                return Err(format!("{}: {e}", path.display()).into());
            }
        }
    }
    Ok(apps)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const SAMPLE: &str = r#"
apiVersion: hops.local/v1alpha1
kind: Application
metadata:
  name: e2e-ui-api
spec:
  source:
    path: ../../../api/.gitops/deploy
    helm:
      values:
        local: true
        appRuntime: cluster-dev
  destination:
    namespace: hops-wt-default
  syncPolicy:
    prune: false
"#;

    #[test]
    fn parse_application_yaml_accepts_local_subset() {
        let app = parse_application_yaml(SAMPLE).unwrap();
        assert_eq!(app.metadata.name, "e2e-ui-api");
        assert_eq!(app.spec.source.path, "../../../api/.gitops/deploy");
        assert_eq!(
            app.spec.destination.namespace.as_deref(),
            Some("hops-wt-default")
        );
        assert!(!app.spec.sync_policy.prune);
        let values = app.spec.source.helm.values.unwrap();
        assert_eq!(values["local"], Value::Bool(true));
        assert_eq!(values["appRuntime"], Value::String("cluster-dev".into()));
    }

    #[test]
    fn parse_rejects_wrong_api_version() {
        let bad = SAMPLE.replace(APPLICATION_API_VERSION, "v1");
        assert!(parse_application_yaml(&bad).is_err());
    }

    #[test]
    fn resolve_source_path_relative_to_application_file() {
        let app_file = Path::new("/proj/gitops/env/local/api.yaml");
        let resolved = resolve_source_path(app_file, "../../../api/.gitops/deploy").unwrap();
        assert_eq!(resolved, PathBuf::from("/proj/api/.gitops/deploy"));
    }

    #[test]
    fn service_root_from_chart_is_parent_of_gitops() {
        assert_eq!(
            service_root_from_chart_path(Path::new("/proj/ui/.gitops/deploy")),
            PathBuf::from("/proj/ui")
        );
        assert_eq!(
            service_root_from_chart_path(Path::new("/proj/api/.gitops/deploy")),
            PathBuf::from("/proj/api")
        );
    }

    #[test]
    fn resolve_delivery_host_path_uses_delivery_path_not_shared_monorepo() {
        let app_file = Path::new("/proj/gitops/env/local/ui.yaml");
        let ui = parse_application_yaml(
            r#"
apiVersion: hops.local/v1alpha1
kind: Application
metadata:
  name: e2e-ui-ui
spec:
  source:
    path: ../../../ui/.gitops/deploy
    deliveryPath: ../../../ui
"#,
        )
        .unwrap();
        let host = resolve_delivery_host_path(app_file, &ui).unwrap();
        assert_eq!(host, PathBuf::from("/proj/ui"));

        let api = parse_application_yaml(
            r#"
apiVersion: hops.local/v1alpha1
kind: Application
metadata:
  name: e2e-ui-api
spec:
  source:
    path: ../../../api/.gitops/deploy
    deliveryPath: ../../..
"#,
        )
        .unwrap();
        let host_api = resolve_delivery_host_path(app_file, &api).unwrap();
        assert_eq!(host_api, PathBuf::from("/proj"));
        // UI and API host paths must differ in the dogfood layout
        assert_ne!(host, host_api);
    }

    #[test]
    fn resolve_delivery_host_path_defaults_to_service_root() {
        let app_file = Path::new("/proj/gitops/env/local/ui.yaml");
        let ui = parse_application_yaml(
            r#"
apiVersion: hops.local/v1alpha1
kind: Application
metadata:
  name: e2e-ui-ui
spec:
  source:
    path: ../../../ui/.gitops/deploy
"#,
        )
        .unwrap();
        let host = resolve_delivery_host_path(app_file, &ui).unwrap();
        assert_eq!(host, PathBuf::from("/proj/ui"));
    }

    #[test]
    fn load_applications_from_directory() {
        let dir = tempfile_dir("lwb-apps");
        write_file(
            &dir.join("api.yaml"),
            SAMPLE,
        );
        write_file(
            &dir.join("ui.yaml"),
            &SAMPLE.replace("e2e-ui-api", "e2e-ui-ui"),
        );
        let apps = load_applications(&dir).unwrap();
        assert_eq!(apps.len(), 2);
        assert_eq!(apps[0].1.metadata.name, "e2e-ui-api");
        assert_eq!(apps[1].1.metadata.name, "e2e-ui-ui");
    }

    fn tempfile_dir(prefix: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "{}-{}-{}",
            prefix,
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_file(path: &Path, body: &str) {
        let mut f = fs::File::create(path).unwrap();
        f.write_all(body.as_bytes()).unwrap();
    }
}
