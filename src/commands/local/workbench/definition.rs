//! Kubernetes-shaped Cluster and independently reusable Environment loading.
//!
//! This module intentionally stops at a validated, immutable handoff. The
//! long-running controller consumes [`LoadedDefinition`] in the next rollout
//! task; `hops local up` currently owns definition validation and named local
//! cluster create/reuse only.

use crate::commands::local::backend::{self, Backend, ClusterProvider, DockerProvider, SizeArgs};
use clap::Args;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_yaml::{Mapping, Value};
use std::collections::BTreeSet;
use std::error::Error;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

pub const API_VERSION: &str = "hops.local/v1alpha1";
pub const DEFAULT_DEFINITION_FILE: &str = "cluster.yaml";
pub const CLUSTER_MANIFESTS_PATH: &str = ".gitops/cluster";
pub const PROMOTE_CHART_PATH: &str = ".gitops/promote";

#[derive(Args, Debug, Clone)]
pub struct UpArgs {
    /// Cluster definition. Defaults to ./cluster.yaml.
    #[arg(short = 'f', long = "file", value_name = "PATH")]
    pub file: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct UpOverrides<'a> {
    pub cluster_provider: Option<ClusterProvider>,
    pub docker_provider: Option<DockerProvider>,
    pub legacy_backend: Option<Backend>,
    pub cluster_name: Option<&'a str>,
    pub context: Option<&'a str>,
    pub dory_name: Option<&'a str>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LoadedDefinition {
    pub source: PathBuf,
    pub cluster: ClusterDefinition,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LoadedEnvironment {
    pub source: PathBuf,
    pub environment: EnvironmentDefinition,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ClusterDefinition {
    pub name: String,
    pub cluster_provider: ClusterProvider,
    pub docker_provider: DockerProvider,
    pub mount_root: PathBuf,
    pub manifests_path: PathBuf,
    pub secret_sync: Option<SecretSyncDefinition>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SecretSyncDefinition {
    /// Resolved input path only. Value representation and Vault ownership are
    /// intentionally left to the separately approved secret-sync contract.
    pub path: PathBuf,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EnvironmentDefinition {
    pub name: String,
    pub namespace: String,
    pub cluster_ref: String,
    pub root: PathBuf,
    pub values: Mapping,
    pub deploys: Vec<DeployDefinition>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DeployDefinition {
    pub application_root: PathBuf,
    pub promote_chart: PathBuf,
    pub values: Mapping,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DocumentProbe {
    api_version: String,
    kind: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ClusterDocument {
    api_version: String,
    kind: String,
    metadata: ObjectMetadata,
    spec: ClusterSpec,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct EnvironmentDocument {
    api_version: String,
    kind: String,
    metadata: ObjectMetadata,
    spec: EnvironmentSpec,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ObjectMetadata {
    name: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ClusterSpec {
    cluster_provider: ClusterProvider,
    docker_provider: DockerProvider,
    mount_root: PathBuf,
    manifests: ManifestsSpec,
    #[serde(default)]
    secret_sync: Option<SecretSyncSpec>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ManifestsSpec {
    path: PathBuf,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SecretSyncSpec {
    path: PathBuf,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct EnvironmentSpec {
    cluster_ref: ClusterReference,
    root: PathBuf,
    #[serde(default)]
    namespace: Option<String>,
    #[serde(default)]
    values: Mapping,
    deploys: Vec<DeploySpec>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ClusterReference {
    name: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DeploySpec {
    path: PathBuf,
    #[serde(default)]
    values: Mapping,
}

pub fn run_up(args: &UpArgs, overrides: UpOverrides<'_>) -> Result<(), Box<dyn Error>> {
    let cwd = std::env::current_dir()?;
    let source = definition_path(args.file.as_deref(), &cwd);

    // All parsing, identity, provider, and filesystem validation happens
    // before process state, local state, or the cluster can be mutated.
    let definition = load_definition(&source)?;
    validate_overrides(&definition, overrides)?;

    if let Some(name) = overrides
        .dory_name
        .map(str::trim)
        .filter(|name| !name.is_empty())
    {
        backend::persist_dory_context_name(name)?;
    }

    backend::kind::set_active_cluster_name(&definition.cluster.name);
    backend::apply_docker_provider_env(definition.cluster.docker_provider)?;

    let existing_kind = definition.cluster.cluster_provider == ClusterProvider::Kind
        && backend::kind::cluster_exists();
    if definition.cluster.cluster_provider == ClusterProvider::Kind {
        backend::kind::set_extra_mount_root(&definition.cluster.mount_root);
        if existing_kind {
            backend::kind::ensure_configured_mount_root(&definition.cluster.mount_root)?;
        }
    }

    let explicit_context = overrides
        .context
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let active_backend = backend::activate_with_providers(
        Some(definition.cluster.cluster_provider),
        Some(definition.cluster.docker_provider),
        Some(&definition.cluster.name),
        explicit_context,
    )?;
    active_backend.start(&SizeArgs::default(), false)?;
    backend::persist_providers(
        backend::providers::ProviderPair {
            cluster: definition.cluster.cluster_provider,
            docker: definition.cluster.docker_provider,
        },
        Some(&definition.cluster.name),
    )?;

    log::info!(
        "Cluster '{}' ready: context={} clusterProvider={} dockerProvider={} mountRoot={} definition={}",
        definition.cluster.name,
        expected_context(&definition, overrides),
        definition.cluster.cluster_provider,
        definition.cluster.docker_provider,
        definition.cluster.mount_root.display(),
        definition.source.display()
    );
    log::info!(
        "Validated shared manifests: {}",
        definition.cluster.manifests_path.display()
    );
    log::info!(
        "Cluster definition contains no worktree inventory; register each checkout with `hops local gitops worktree <environment.yaml> --name <environment>`"
    );

    Ok(())
}

pub fn definition_path(file: Option<&Path>, cwd: &Path) -> PathBuf {
    match file {
        Some(path) if path.is_absolute() => path.to_path_buf(),
        Some(path) => cwd.join(path),
        None => cwd.join(DEFAULT_DEFINITION_FILE),
    }
}

pub fn load_definition(path: &Path) -> Result<LoadedDefinition, Box<dyn Error>> {
    let source = path.canonicalize().map_err(|error| {
        format!(
            "unable to resolve Cluster definition {}: {error}",
            path.display()
        )
    })?;
    if !source.is_file() {
        return Err(format!("Cluster definition is not a file: {}", source.display()).into());
    }
    let definition_root = source
        .parent()
        .ok_or_else(|| format!("Cluster definition has no parent: {}", source.display()))?
        .canonicalize()?;
    let yaml = fs::read_to_string(&source).map_err(|error| {
        format!(
            "unable to read Cluster definition {}: {error}",
            source.display()
        )
    })?;

    let mut clusters = Vec::<ClusterDocument>::new();
    for (index, document) in serde_yaml::Deserializer::from_str(&yaml).enumerate() {
        let number = index + 1;
        let value = Value::deserialize(document).map_err(|error| {
            format!(
                "{} document {number}: invalid YAML: {error}",
                source.display()
            )
        })?;
        if value.is_null() {
            continue;
        }
        let probe: DocumentProbe = parse_document(value.clone(), &source, number)?;
        if probe.api_version != API_VERSION {
            return Err(format!(
                "{} document {number}: unsupported apiVersion {:?}; expected {API_VERSION}",
                source.display(),
                probe.api_version
            )
            .into());
        }
        match probe.kind.as_str() {
            "Cluster" => clusters.push(parse_document(value, &source, number)?),
            "Environment" => {
                return Err(format!(
                    "{} document {number}: Environment instances must not be committed in cluster.yaml; pass a separate Environment file to `hops local gitops worktree`",
                    source.display()
                )
                .into())
            }
            other => {
                return Err(format!(
                    "{} document {number}: unsupported kind {other:?}; expected Cluster",
                    source.display()
                )
                .into())
            }
        }
    }

    if clusters.len() != 1 {
        return Err(format!(
            "{}: expected exactly one {API_VERSION} Cluster document, found {}",
            source.display(),
            clusters.len()
        )
        .into());
    }
    let raw_cluster: ClusterDocument = clusters.remove(0);
    debug_assert_eq!(raw_cluster.api_version, API_VERSION);
    debug_assert_eq!(raw_cluster.kind, "Cluster");
    validate_dns_label("Cluster.metadata.name", &raw_cluster.metadata.name)?;
    let provider_pair = backend::providers::ProviderPair {
        cluster: raw_cluster.spec.cluster_provider,
        docker: raw_cluster.spec.docker_provider,
    };
    provider_pair
        .validate()
        .map_err(|error| format!("Cluster.spec provider pair is invalid: {error}"))?;

    let mount_root = resolve_mount_root(
        &definition_root,
        &raw_cluster.spec.mount_root,
        "Cluster.spec.mountRoot",
    )?;
    ensure_within(&mount_root, &definition_root, "Cluster definition")?;
    if raw_cluster.spec.manifests.path != Path::new(CLUSTER_MANIFESTS_PATH) {
        return Err(format!(
            "Cluster.spec.manifests.path must be exactly {CLUSTER_MANIFESTS_PATH:?}; got {:?}",
            raw_cluster.spec.manifests.path.display().to_string()
        )
        .into());
    }
    let manifests_path = resolve_bounded_path(
        &mount_root,
        &definition_root,
        &raw_cluster.spec.manifests.path,
        "Cluster.spec.manifests.path",
        true,
    )?;
    let secret_sync = raw_cluster
        .spec
        .secret_sync
        .map(|secret| {
            resolve_bounded_path(
                &mount_root,
                &mount_root,
                &secret.path,
                "Cluster.spec.secretSync.path",
                false,
            )
            .map(|path| SecretSyncDefinition { path })
        })
        .transpose()?;

    Ok(LoadedDefinition {
        source,
        cluster: ClusterDefinition {
            name: raw_cluster.metadata.name,
            cluster_provider: raw_cluster.spec.cluster_provider,
            docker_provider: raw_cluster.spec.docker_provider,
            mount_root,
            manifests_path,
            secret_sync,
        },
    })
}

pub fn load_environment_definition(
    path: &Path,
    cluster: &LoadedDefinition,
    name_override: Option<&str>,
    namespace_override: Option<&str>,
) -> Result<LoadedEnvironment, Box<dyn Error>> {
    let source = path.canonicalize().map_err(|error| {
        format!(
            "unable to resolve Environment definition {}: {error}",
            path.display()
        )
    })?;
    if !source.is_file() {
        return Err(format!("Environment definition is not a file: {}", source.display()).into());
    }
    ensure_within(
        &cluster.cluster.mount_root,
        &source,
        "Environment definition",
    )?;
    let definition_root = source
        .parent()
        .ok_or_else(|| format!("Environment definition has no parent: {}", source.display()))?
        .canonicalize()?;
    let yaml = fs::read_to_string(&source).map_err(|error| {
        format!(
            "unable to read Environment definition {}: {error}",
            source.display()
        )
    })?;

    let mut environments = Vec::<EnvironmentDocument>::new();
    for (index, document) in serde_yaml::Deserializer::from_str(&yaml).enumerate() {
        let number = index + 1;
        let value = Value::deserialize(document).map_err(|error| {
            format!(
                "{} document {number}: invalid YAML: {error}",
                source.display()
            )
        })?;
        if value.is_null() {
            continue;
        }
        let probe: DocumentProbe = parse_document(value.clone(), &source, number)?;
        if probe.api_version != API_VERSION {
            return Err(format!(
                "{} document {number}: unsupported apiVersion {:?}; expected {API_VERSION}",
                source.display(),
                probe.api_version
            )
            .into());
        }
        if probe.kind != "Environment" {
            return Err(format!(
                "{} document {number}: unsupported kind {:?}; expected Environment",
                source.display(),
                probe.kind
            )
            .into());
        }
        environments.push(parse_document(value, &source, number)?);
    }
    if environments.len() != 1 {
        return Err(format!(
            "{}: expected exactly one {API_VERSION} Environment document, found {}",
            source.display(),
            environments.len()
        )
        .into());
    }

    let raw = environments.remove(0);
    debug_assert_eq!(raw.api_version, API_VERSION);
    debug_assert_eq!(raw.kind, "Environment");
    let name = name_override
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(&raw.metadata.name)
        .to_string();
    validate_dns_label("Environment runtime name", &name)?;
    if raw.spec.cluster_ref.name != cluster.cluster.name {
        return Err(format!(
            "Environment {name:?} references Cluster {:?}, but the selected definition contains {:?}",
            raw.spec.cluster_ref.name, cluster.cluster.name
        )
        .into());
    }
    let namespace = namespace_override
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .or(raw.spec.namespace)
        .unwrap_or_else(|| name.clone());
    validate_dns_label("Environment namespace", &namespace)?;
    let root = resolve_bounded_path(
        &cluster.cluster.mount_root,
        &definition_root,
        &raw.spec.root,
        &format!("Environment {name:?} spec.root"),
        true,
    )?;

    let mut seen_deploys = BTreeSet::new();
    let mut deploys = Vec::with_capacity(raw.spec.deploys.len());
    for deploy in raw.spec.deploys {
        let application_root = resolve_bounded_path(
            &cluster.cluster.mount_root,
            &root,
            &deploy.path,
            &format!("Environment {name:?} deploys[].path"),
            true,
        )?;
        if !seen_deploys.insert(application_root.clone()) {
            return Err(format!(
                "Environment {name:?} contains duplicate deploy application root {}",
                application_root.display()
            )
            .into());
        }
        let promote_chart = resolve_bounded_path(
            &cluster.cluster.mount_root,
            &application_root,
            Path::new(PROMOTE_CHART_PATH),
            &format!("Environment {name:?} deploy promote chart"),
            false,
        )?;
        deploys.push(DeployDefinition {
            application_root,
            promote_chart,
            values: deploy.values,
        });
    }

    Ok(LoadedEnvironment {
        source,
        environment: EnvironmentDefinition {
            name,
            namespace,
            cluster_ref: raw.spec.cluster_ref.name,
            root,
            values: raw.spec.values,
            deploys,
        },
    })
}

fn parse_document<T: DeserializeOwned>(
    value: Value,
    source: &Path,
    number: usize,
) -> Result<T, Box<dyn Error>> {
    serde_yaml::from_value(value).map_err(|error| {
        format!(
            "{} document {number}: definition schema error: {error}",
            source.display()
        )
        .into()
    })
}

fn validate_overrides(
    definition: &LoadedDefinition,
    overrides: UpOverrides<'_>,
) -> Result<(), Box<dyn Error>> {
    if overrides.legacy_backend.is_some()
        && (overrides.cluster_provider.is_some() || overrides.docker_provider.is_some())
    {
        return Err(
            "deprecated --backend cannot be combined with --cluster-provider or --docker-provider"
                .into(),
        );
    }

    if let Some(legacy) = overrides.legacy_backend {
        let mapped = backend::providers::provider_pair_for_legacy_backend(legacy);
        log::warn!(
            "--backend is deprecated; use --cluster-provider {} --docker-provider {}",
            mapped.cluster,
            mapped.docker
        );
        if mapped.cluster != definition.cluster.cluster_provider
            || mapped.docker != definition.cluster.docker_provider
        {
            return Err(format!(
                "deprecated --backend {legacy} maps to {}/{} but Cluster {:?} declares {}/{}; update the definition or remove the conflicting flag",
                mapped.cluster,
                mapped.docker,
                definition.cluster.name,
                definition.cluster.cluster_provider,
                definition.cluster.docker_provider
            )
            .into());
        }
    }
    if let Some(cluster_provider) = overrides.cluster_provider {
        if cluster_provider != definition.cluster.cluster_provider {
            return Err(format!(
                "--cluster-provider {cluster_provider} conflicts with Cluster.spec.clusterProvider {}",
                definition.cluster.cluster_provider
            )
            .into());
        }
    }
    if let Some(docker_provider) = overrides.docker_provider {
        if docker_provider != definition.cluster.docker_provider {
            return Err(format!(
                "--docker-provider {docker_provider} conflicts with Cluster.spec.dockerProvider {}",
                definition.cluster.docker_provider
            )
            .into());
        }
    }
    if let Some(cluster_name) = overrides
        .cluster_name
        .map(str::trim)
        .filter(|name| !name.is_empty())
    {
        if cluster_name != definition.cluster.name {
            return Err(format!(
                "--cluster-name {cluster_name:?} conflicts with Cluster.metadata.name {:?}",
                definition.cluster.name
            )
            .into());
        }
    }
    if let Some(context) = overrides
        .context
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        let expected = expected_context(definition, overrides);
        if context != expected {
            return Err(format!(
                "--context {context:?} conflicts with Cluster {:?}; expected {expected:?}",
                definition.cluster.name
            )
            .into());
        }
    }
    if overrides
        .dory_name
        .is_some_and(|name| name.trim().is_empty())
    {
        return Err("--dory-name must not be empty".into());
    }
    Ok(())
}

fn expected_context(definition: &LoadedDefinition, overrides: UpOverrides<'_>) -> String {
    match definition.cluster.cluster_provider {
        ClusterProvider::Kind => format!("kind-{}", definition.cluster.name),
        ClusterProvider::Colima => "colima".to_string(),
        ClusterProvider::Dory => overrides
            .dory_name
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| Backend::Dory.kube_context()),
    }
}

fn validate_dns_label(field: &str, value: &str) -> Result<(), Box<dyn Error>> {
    let valid = !value.is_empty()
        && value.len() <= 63
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        && value
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
        && value
            .as_bytes()
            .last()
            .is_some_and(u8::is_ascii_alphanumeric);
    if valid {
        Ok(())
    } else {
        Err(
            format!("{field} must be a lowercase DNS-1123 label (1-63 characters), got {value:?}")
                .into(),
        )
    }
}

fn resolve_bounded_path(
    boundary: &Path,
    base: &Path,
    relative: &Path,
    field: &str,
    require_directory: bool,
) -> Result<PathBuf, Box<dyn Error>> {
    if relative.is_absolute() {
        return Err(format!("{field} must be relative, got {}", relative.display()).into());
    }

    let mut components = Vec::<OsString>::new();
    for component in relative.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(value) => components.push(value.to_os_string()),
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(format!(
                    "{field} contains forbidden traversal or root component: {}",
                    relative.display()
                )
                .into())
            }
        }
    }

    let boundary = boundary.canonicalize().map_err(|error| {
        format!(
            "unable to canonicalize {field} boundary {}: {error}",
            boundary.display()
        )
    })?;
    let mut current = base.canonicalize().map_err(|error| {
        format!(
            "unable to canonicalize {field} base {}: {error}",
            base.display()
        )
    })?;
    ensure_within(&boundary, &current, field)?;

    let mut index = 0;
    while index < components.len() {
        let next = current.join(&components[index]);
        match fs::symlink_metadata(&next) {
            Ok(_) => {
                current = next.canonicalize().map_err(|error| {
                    format!("unable to canonicalize {field} {}: {error}", next.display())
                })?;
                ensure_within(&boundary, &current, field)?;
                index += 1;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                current = next;
                index += 1;
                while index < components.len() {
                    current.push(&components[index]);
                    index += 1;
                }
            }
            Err(error) => {
                return Err(format!("unable to inspect {field} {}: {error}", next.display()).into())
            }
        }
    }
    ensure_within(&boundary, &current, field)?;

    if require_directory && !current.is_dir() {
        return Err(format!("{field} directory does not exist: {}", current.display()).into());
    }
    Ok(current)
}

fn resolve_mount_root(
    definition_root: &Path,
    relative: &Path,
    field: &str,
) -> Result<PathBuf, Box<dyn Error>> {
    if relative.is_absolute() {
        return Err(format!("{field} must be relative, got {}", relative.display()).into());
    }

    let candidate = definition_root.join(relative);
    let resolved = candidate.canonicalize().map_err(|error| {
        format!(
            "unable to canonicalize {field} {}: {error}",
            candidate.display()
        )
    })?;
    if !resolved.is_dir() {
        return Err(format!("{field} directory does not exist: {}", resolved.display()).into());
    }
    Ok(resolved)
}

fn ensure_within(boundary: &Path, candidate: &Path, field: &str) -> Result<(), Box<dyn Error>> {
    if candidate == boundary || candidate.starts_with(boundary) {
        Ok(())
    } else {
        Err(format!(
            "{field} escapes Cluster.spec.mountRoot: {} is outside {}",
            candidate.display(),
            boundary.display()
        )
        .into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        root: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "hops-cluster-definition-{}-{}",
                std::process::id(),
                uuid::Uuid::new_v4()
            ));
            fs::create_dir_all(root.join(".gitops/cluster")).unwrap();
            fs::create_dir_all(root.join("apps/gateway")).unwrap();
            fs::create_dir_all(root.join("services/api")).unwrap();
            let root = root.canonicalize().unwrap();
            Self { root }
        }

        fn write(&self, yaml: &str) -> PathBuf {
            let path = self.root.join(DEFAULT_DEFINITION_FILE);
            fs::write(&path, yaml).unwrap();
            path
        }

        fn write_environment(&self, yaml: &str) -> PathBuf {
            let path = self.root.join("environment.yaml");
            fs::write(&path, yaml).unwrap();
            path
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn valid_yaml() -> &'static str {
        r#"apiVersion: hops.local/v1alpha1
kind: Cluster
metadata:
  name: project-dev
spec:
  clusterProvider: kind
  dockerProvider: dory
  mountRoot: .
  manifests:
    path: .gitops/cluster
"#
    }

    fn valid_environment_yaml() -> &'static str {
        r#"apiVersion: hops.local/v1alpha1
kind: Environment
metadata:
  name: local
spec:
  clusterRef:
    name: project-dev
  root: .
  values:
    local: true
  deploys:
    - path: apps/gateway
      values:
        preview: false
    - path: services/api
"#
    }

    #[test]
    fn parses_cluster_only_and_reusable_environment() {
        let fixture = Fixture::new();
        let loaded = load_definition(&fixture.write(valid_yaml())).unwrap();
        assert_eq!(loaded.cluster.name, "project-dev");
        assert_eq!(loaded.cluster.cluster_provider, ClusterProvider::Kind);
        assert_eq!(loaded.cluster.docker_provider, DockerProvider::Dory);

        let environment = load_environment_definition(
            &fixture.write_environment(valid_environment_yaml()),
            &loaded,
            Some("feature-auth"),
            None,
        )
        .unwrap();
        assert_eq!(environment.environment.name, "feature-auth");
        assert_eq!(environment.environment.namespace, "feature-auth");
        assert_eq!(environment.environment.root, fixture.root);
        assert_eq!(
            environment.environment.deploys[0].promote_chart,
            fixture.root.join("apps/gateway/.gitops/promote")
        );
    }

    #[test]
    fn rejects_embedded_environment() {
        let fixture = Fixture::new();
        let yaml = format!("{}\n---\n{}", valid_yaml(), valid_environment_yaml());
        let error = load_definition(&fixture.write(&yaml)).unwrap_err();
        assert!(error.to_string().contains("must not be committed"));
    }

    #[test]
    fn nested_worktree_definition_can_mount_its_meta_root() {
        let fixture = Fixture::new();
        let worktree = fixture.root.join(".worktrees/feature-auth");
        fs::create_dir_all(worktree.join(".gitops/cluster")).unwrap();
        let source = worktree.join(DEFAULT_DEFINITION_FILE);
        let yaml = valid_yaml().replacen("mountRoot: .", "mountRoot: ../..", 1);
        fs::write(&source, yaml).unwrap();

        let loaded = load_definition(&source).unwrap();

        assert_eq!(loaded.cluster.mount_root, fixture.root);
        assert_eq!(
            loaded.cluster.manifests_path,
            worktree.join(".gitops/cluster")
        );
    }

    #[test]
    fn rejects_zero_or_multiple_cluster_documents() {
        let fixture = Fixture::new();
        let error = load_definition(&fixture.write("\n")).unwrap_err();
        assert!(error.to_string().contains("exactly one"));

        let duplicate = format!("{}\n---\n{}\n", valid_yaml(), valid_yaml());
        let error = load_definition(&fixture.write(&duplicate)).unwrap_err();
        assert!(error.to_string().contains("found 2"));
    }

    #[test]
    fn rejects_unknown_fields_versions_kinds_and_cluster_refs() {
        let fixture = Fixture::new();
        let unknown = valid_yaml().replacen(
            "  mountRoot: .",
            "  mountRoot: .\n  unexpectedField: true",
            1,
        );
        assert!(load_definition(&fixture.write(&unknown))
            .unwrap_err()
            .to_string()
            .contains("unknown field"));

        let version = valid_yaml().replacen(API_VERSION, "hops.local/v9", 1);
        assert!(load_definition(&fixture.write(&version))
            .unwrap_err()
            .to_string()
            .contains("unsupported apiVersion"));

        let kind = valid_yaml().replacen("kind: Cluster", "kind: Namespace", 1);
        assert!(load_definition(&fixture.write(&kind))
            .unwrap_err()
            .to_string()
            .contains("unsupported kind"));

        let loaded = load_definition(&fixture.write(valid_yaml())).unwrap();
        let reference = valid_environment_yaml().replacen(
            "name: project-dev\n  root",
            "name: other\n  root",
            1,
        );
        assert!(load_environment_definition(
            &fixture.write_environment(&reference),
            &loaded,
            None,
            None,
        )
        .unwrap_err()
        .to_string()
        .contains("references Cluster"));
    }

    #[test]
    fn rejects_duplicate_deploy_identity() {
        let fixture = Fixture::new();
        let loaded = load_definition(&fixture.write(valid_yaml())).unwrap();
        let duplicate = valid_environment_yaml()
            .replace("    - path: services/api", "    - path: apps/gateway");
        assert!(load_environment_definition(
            &fixture.write_environment(&duplicate),
            &loaded,
            None,
            None,
        )
        .unwrap_err()
        .to_string()
        .contains("duplicate deploy"));
    }

    #[test]
    fn rejects_non_mapping_values_and_invalid_names() {
        let fixture = Fixture::new();
        let loaded = load_definition(&fixture.write(valid_yaml())).unwrap();
        let scalar_values =
            valid_environment_yaml().replacen("  values:\n    local: true", "  values: true", 1);
        assert!(load_environment_definition(
            &fixture.write_environment(&scalar_values),
            &loaded,
            None,
            None,
        )
        .unwrap_err()
        .to_string()
        .contains("schema error"));

        let invalid_name = valid_yaml().replacen("name: project-dev", "name: Project_Dev", 1);
        assert!(load_definition(&fixture.write(&invalid_name))
            .unwrap_err()
            .to_string()
            .contains("DNS-1123"));
    }

    #[test]
    fn requires_explicit_hidden_cluster_manifest_path() {
        let fixture = Fixture::new();
        for path in ["gitops/cluster", ".gitops/deploy", "./.gitops/cluster"] {
            let yaml = valid_yaml().replacen(".gitops/cluster", path, 1);
            let error = load_definition(&fixture.write(&yaml)).unwrap_err();
            assert!(error.to_string().contains("must be exactly"), "{error}");
        }
    }

    #[test]
    fn rejects_absolute_traversal_and_symlink_escape() {
        let fixture = Fixture::new();
        let absolute = valid_yaml().replacen("mountRoot: .", "mountRoot: /tmp", 1);
        assert!(load_definition(&fixture.write(&absolute))
            .unwrap_err()
            .to_string()
            .contains("must be relative"));

        let loaded = load_definition(&fixture.write(valid_yaml())).unwrap();
        let traversal = valid_environment_yaml().replacen("root: .", "root: ../outside", 1);
        assert!(load_environment_definition(
            &fixture.write_environment(&traversal),
            &loaded,
            None,
            None,
        )
        .unwrap_err()
        .to_string()
        .contains("forbidden traversal"));

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let outside = fixture
                .root
                .parent()
                .unwrap()
                .join(format!("outside-definition-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(&outside).unwrap();
            symlink(&outside, fixture.root.join("escape")).unwrap();
            let escaped = valid_environment_yaml().replacen("root: .", "root: escape", 1);
            let error = load_environment_definition(
                &fixture.write_environment(&escaped),
                &loaded,
                None,
                None,
            )
            .unwrap_err();
            assert!(error.to_string().contains("escapes Cluster.spec.mountRoot"));

            symlink(&outside, fixture.root.join("secret-link")).unwrap();
            let escaped_secret = valid_yaml().replacen(
                "  manifests:\n    path: .gitops/cluster",
                "  manifests:\n    path: .gitops/cluster\n  secretSync:\n    path: secret-link",
                1,
            );
            let error = load_definition(&fixture.write(&escaped_secret)).unwrap_err();
            assert!(error.to_string().contains("escapes Cluster.spec.mountRoot"));
            fs::remove_dir_all(outside).unwrap();
        }
    }

    #[test]
    fn validates_cli_identity_without_mutation() {
        let fixture = Fixture::new();
        let loaded = load_definition(&fixture.write(valid_yaml())).unwrap();
        validate_overrides(
            &loaded,
            UpOverrides {
                cluster_provider: Some(ClusterProvider::Kind),
                docker_provider: Some(DockerProvider::Dory),
                cluster_name: Some("project-dev"),
                context: Some("kind-project-dev"),
                ..UpOverrides::default()
            },
        )
        .unwrap();

        let error = validate_overrides(
            &loaded,
            UpOverrides {
                docker_provider: Some(DockerProvider::Colima),
                ..UpOverrides::default()
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("conflicts"));
    }
}
