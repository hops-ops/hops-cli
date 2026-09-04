//! Durable ownership and cleanup primitives for the Local Workbench controller.
//!
//! The controller owns a Cluster only after `gitops cluster` acquires its
//! lease.  The lease is deliberately a small, file-backed record: it is
//! diagnostic state and conflict protection, not a second desired-state
//! document.  Secrets and rendered Helm values never enter this state.

use super::definition::{DeployType, LoadedEnvironment};
use super::reconcile::{
    ensure_environment_namespace, HelmRunner, KubectlApplier, KustomizeRunner, ReconcileOptions,
    ReconcileResult,
};
use crate::commands::local::workbench::registry::{
    list_workspaces, remove_workspace, WorkspaceRecord,
};
use crate::commands::local::{kubectl_command, local_state_dir};
use serde::{Deserialize, Serialize};
use serde_yaml::Value;
use std::error::Error;
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Write};
#[cfg(unix)]
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const CONTROLLER_SCHEMA_VERSION: u32 = 1;
const CONTROLLER_MODE: &str = "gitops";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ControllerLease {
    pub schema_version: u32,
    pub mode: String,
    pub cluster_name: String,
    pub definition_path: String,
    pub kube_context: String,
    pub pid: u32,
    pub started_at: u64,
}

#[derive(Debug)]
pub struct ControllerHandle {
    lock_path: PathBuf,
    pub lease: ControllerLease,
    pub reused: bool,
}

struct ControllerRecoveryLock {
    #[cfg(unix)]
    file: File,
}

impl Drop for ControllerRecoveryLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            // SAFETY: `file` remains open for the lifetime of this guard and
            // `LOCK_UN` only releases this process's advisory lock.
            let _ = unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
        }
    }
}

impl ControllerHandle {
    pub fn is_owner(&self) -> bool {
        !self.reused
    }
}

impl Drop for ControllerHandle {
    fn drop(&mut self) {
        if self.reused {
            return;
        }
        // Do not remove a lock that another process replaced after this
        // controller stopped.  Read-and-compare is intentionally fail-closed.
        let Ok(text) = fs::read_to_string(&self.lock_path) else {
            return;
        };
        let Ok(current) = serde_json::from_str::<ControllerLease>(&text) else {
            return;
        };
        if current == self.lease {
            let _ = fs::remove_file(&self.lock_path);
        }
    }
}

pub fn controller_state_dir(cluster_name: &str) -> Result<PathBuf, Box<dyn Error>> {
    Ok(local_state_dir()?
        .join("clusters")
        .join(super::slugify_name(cluster_name)))
}

pub fn controller_lock_path(cluster_name: &str) -> Result<PathBuf, Box<dyn Error>> {
    Ok(controller_state_dir(cluster_name)?.join("controller.lock"))
}

/// Forget machine-local ownership only when the selected backend no longer
/// exists. A live controller remains a real conflict even when its backend was
/// removed out-of-band; the user must stop that process before a new owner can
/// safely start.
pub fn reset_absent_cluster_state(cluster_name: &str) -> Result<bool, Box<dyn Error>> {
    let state_dir = local_state_dir()?;
    let cluster_dir = state_dir
        .join("clusters")
        .join(super::slugify_name(cluster_name));
    reset_absent_cluster_state_at(&state_dir, &cluster_dir, cluster_name)
}

fn reset_absent_cluster_state_at(
    state_dir: &Path,
    cluster_dir: &Path,
    cluster_name: &str,
) -> Result<bool, Box<dyn Error>> {
    if !cluster_dir.exists() {
        return Ok(false);
    }
    let lock_path = cluster_dir.join("controller.lock");
    if lock_path.is_file() {
        if let Ok(lease) = serde_json::from_slice::<ControllerLease>(&fs::read(&lock_path)?) {
            if controller_pid_is_live(lease.pid) {
                return Err(format!(
                    "Cluster {:?} backend is absent, but GitOps controller pid {} is still live; stop that process before restarting from clean state",
                    cluster_name, lease.pid
                )
                .into());
            }
        }
    }

    fs::remove_dir_all(cluster_dir)?;
    for record in list_workspaces(state_dir)? {
        if record.cluster_name.as_deref() == Some(cluster_name) {
            remove_workspace(state_dir, &record.name)?;
        }
    }
    Ok(true)
}

/// Guard imperative package/provider installers from becoming a second owner
/// after a Cluster controller has claimed the backend. This deliberately
/// reports the owner without attempting adoption or stale-lock cleanup.
pub fn reject_imperative_owner(cluster_name: &str) -> Result<(), Box<dyn Error>> {
    let path = controller_lock_path(cluster_name)?;
    if !path.is_file() {
        return Ok(());
    }
    let lease: ControllerLease = serde_json::from_slice(&fs::read(&path)?).map_err(|error| {
        format!(
            "GitOps controller lock {} is malformed; refusing imperative apply: {error}",
            path.display()
        )
    })?;
    Err(format!(
        "Cluster {:?} is owned by GitOps controller pid {} ({}); use the Cluster tree/controller, or explicitly hand off the backend before running an imperative installer",
        cluster_name, lease.pid, lease.definition_path
    )
    .into())
}

/// Acquire the single GitOps owner for a Cluster. Existing matching ownership
/// is reported as `reused`; a different live/unknown owner is rejected rather
/// than guessed or adopted.
pub fn acquire_controller(
    cluster_name: &str,
    definition_path: &Path,
    kube_context: &str,
    dry_run: bool,
) -> Result<ControllerHandle, Box<dyn Error>> {
    let lease = ControllerLease {
        schema_version: CONTROLLER_SCHEMA_VERSION,
        mode: CONTROLLER_MODE.to_string(),
        cluster_name: cluster_name.to_string(),
        definition_path: definition_path.to_string_lossy().into_owned(),
        kube_context: kube_context.to_string(),
        pid: std::process::id(),
        started_at: unix_timestamp(),
    };
    let path = controller_lock_path(cluster_name)?;
    if dry_run {
        return Ok(ControllerHandle {
            lock_path: path,
            lease,
            reused: true,
        });
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    loop {
        match create_controller_lock(&path, &lease) {
            Ok(true) => {
                return Ok(ControllerHandle {
                    lock_path: path,
                    lease,
                    reused: false,
                });
            }
            Ok(false) => {
                let Some(current) = read_controller_lease_if_present(&path)? else {
                    continue;
                };
                if controller_lease_matches(&current, &lease) {
                    if current.pid == std::process::id() || controller_pid_is_live(current.pid) {
                        return Ok(ControllerHandle {
                            lock_path: path,
                            lease: current,
                            reused: true,
                        });
                    }

                    let _recovery = acquire_controller_recovery_lock(&path)?;
                    let Some(recovered) = read_controller_lease_if_present(&path)? else {
                        continue;
                    };
                    if !controller_lease_matches(&recovered, &lease) {
                        continue;
                    }
                    if recovered.pid == std::process::id() || controller_pid_is_live(recovered.pid)
                    {
                        return Ok(ControllerHandle {
                            lock_path: path,
                            lease: recovered,
                            reused: true,
                        });
                    }

                    fs::remove_file(&path)?;
                    log::info!(
                        "Recovered GitOps controller lock {} from dead pid {}",
                        path.display(),
                        recovered.pid
                    );
                    // Keep the recovery lock held while establishing the new
                    // owner. An older Hops process may not honor the advisory
                    // lock, so create_new remains the final ownership arbiter.
                    match create_controller_lock(&path, &lease) {
                        Ok(true) => {
                            return Ok(ControllerHandle {
                                lock_path: path,
                                lease,
                                reused: false,
                            });
                        }
                        Ok(false) => continue,
                        Err(error) => return Err(error),
                    }
                }
                return Err(format!(
                "Cluster {:?} is already owned by a different GitOps controller (pid {}, definition {}); stop or explicitly hand off that controller before retrying",
                cluster_name, current.pid, current.definition_path
            )
            .into());
            }
            Err(error) => return Err(error),
        }
    }
}

fn create_controller_lock(path: &Path, lease: &ControllerLease) -> Result<bool, Box<dyn Error>> {
    let encoded = serde_json::to_vec_pretty(lease)?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or("controller lock path has no UTF-8 filename")?;
    let pending_path = path.with_file_name(format!(
        ".{file_name}.pending-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&pending_path)?;
    if let Err(error) = file.write_all(&encoded).and_then(|_| file.sync_all()) {
        let _ = fs::remove_file(&pending_path);
        return Err(error.into());
    }
    let published = match fs::hard_link(&pending_path, path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == ErrorKind::AlreadyExists => Ok(false),
        Err(error) => Err(error.into()),
    };
    if let Err(error) = fs::remove_file(&pending_path) {
        log::warn!(
            "Could not remove pending controller lock {}: {}",
            pending_path.display(),
            error
        );
    }
    published
}

fn controller_lease_matches(current: &ControllerLease, expected: &ControllerLease) -> bool {
    current.schema_version == expected.schema_version
        && current.cluster_name == expected.cluster_name
        && current.definition_path == expected.definition_path
        && current.kube_context == expected.kube_context
        && current.mode == expected.mode
}

fn read_controller_lease_if_present(
    path: &Path,
) -> Result<Option<ControllerLease>, Box<dyn Error>> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    serde_json::from_slice(&bytes).map(Some).map_err(|error| {
        format!(
            "GitOps controller lock {} is malformed; refusing adoption: {error}",
            path.display()
        )
        .into()
    })
}

#[cfg(unix)]
fn acquire_controller_recovery_lock(
    controller_path: &Path,
) -> Result<ControllerRecoveryLock, Box<dyn Error>> {
    let recovery_path = controller_path.with_extension("lock.recovery");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&recovery_path)?;
    // SAFETY: `file` is a valid open descriptor and stays owned by the guard.
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
    if result != 0 {
        return Err(format!(
            "failed to serialize recovery through {}: {}",
            recovery_path.display(),
            std::io::Error::last_os_error()
        )
        .into());
    }
    Ok(ControllerRecoveryLock { file })
}

#[cfg(not(unix))]
fn acquire_controller_recovery_lock(
    controller_path: &Path,
) -> Result<ControllerRecoveryLock, Box<dyn Error>> {
    Err(format!(
        "automatic dead-controller recovery is not supported on this platform for {}",
        controller_path.display()
    )
    .into())
}

/// Return whether a controller process is still live. A failure to inspect a
/// process is treated as live so a permission/tooling problem cannot turn into
/// implicit adoption of another owner's lock.
fn controller_pid_is_live(pid: u32) -> bool {
    if pid == std::process::id() {
        return true;
    }
    #[cfg(unix)]
    {
        let Ok(pid) = libc::pid_t::try_from(pid) else {
            return true;
        };
        // SAFETY: signal 0 does not deliver a signal; it only checks whether
        // the process exists and whether this user may inspect it.
        if unsafe { libc::kill(pid, 0) } == 0 {
            return true;
        }
        std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        true
    }
}

/// Release a Cluster controller as part of an explicit `gitops cluster
/// --down`. A live owner must be stopped by its owning process first; a stale
/// lock is removable only when the requested definition matches it.
pub fn release_controller_for_down(
    cluster_name: &str,
    definition_path: &Path,
) -> Result<(), Box<dyn Error>> {
    let path = controller_lock_path(cluster_name)?;
    if !path.is_file() {
        return Ok(());
    }
    let lease: ControllerLease = serde_json::from_slice(&fs::read(&path)?).map_err(|error| {
        format!(
            "GitOps controller lock {} is malformed; refusing Cluster down: {error}",
            path.display()
        )
    })?;
    let expected = definition_path
        .canonicalize()
        .unwrap_or_else(|_| definition_path.to_path_buf())
        .to_string_lossy()
        .into_owned();
    if lease.cluster_name != cluster_name || lease.definition_path != expected {
        return Err(format!(
            "GitOps controller lock {} does not match Cluster {:?}; refusing implicit handoff",
            path.display(),
            cluster_name
        )
        .into());
    }
    if lease.pid != std::process::id() && controller_pid_is_live(lease.pid) {
        return Err(format!(
            "Cluster {:?} is still owned by live GitOps controller pid {}; stop that controller before `gitops cluster --down`",
            cluster_name, lease.pid
        )
        .into());
    }
    fs::remove_file(path)?;
    Ok(())
}

fn unix_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OwnedObject {
    pub api_version: String,
    pub kind: String,
    pub namespace: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnvironmentDeploySnapshot {
    /// Source root containing the deploy directory.
    pub source_root: String,
    pub source_path: String,
    #[serde(default)]
    pub deploy_type: DeployType,
    #[serde(default)]
    pub recursive: bool,
    pub app_name: String,
    pub objects: Vec<OwnedObject>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnvironmentSnapshot {
    pub schema_version: u32,
    pub cluster_name: String,
    pub kube_context: String,
    pub name: String,
    pub namespace: String,
    pub source_path: String,
    pub root: String,
    pub deploys: Vec<EnvironmentDeploySnapshot>,
    /// Namespace deletion is never inferred. This remains false until a
    /// future explicit exclusivity contract records otherwise.
    pub namespace_exclusive: bool,
}

pub fn environment_state_path(
    cluster_name: &str,
    environment_name: &str,
) -> Result<PathBuf, Box<dyn Error>> {
    Ok(controller_state_dir(cluster_name)?
        .join("environments")
        .join(format!("{}.json", super::slugify_name(environment_name))))
}

pub fn save_environment_snapshot(
    cluster_name: &str,
    kube_context: &str,
    environment: &LoadedEnvironment,
    results: &[ReconcileResult],
) -> Result<PathBuf, Box<dyn Error>> {
    let deploys = environment
        .environment
        .deploys
        .iter()
        .zip(results)
        .map(|(deploy, result)| {
            Ok(EnvironmentDeploySnapshot {
                source_root: deploy.source_root.to_string_lossy().into_owned(),
                source_path: deploy.source_path.to_string_lossy().into_owned(),
                deploy_type: deploy.deploy_type,
                recursive: deploy.recursive,
                app_name: result.app_name.clone(),
                objects: owned_objects_from_yaml(&result.rendered_yaml)?,
            })
        })
        .collect::<Result<Vec<_>, Box<dyn Error>>>()?;
    if deploys.len() != environment.environment.deploys.len() {
        return Err("cannot persist Environment ownership: reconcile result count does not match deploy count".into());
    }
    let snapshot = EnvironmentSnapshot {
        schema_version: CONTROLLER_SCHEMA_VERSION,
        cluster_name: cluster_name.to_string(),
        kube_context: kube_context.to_string(),
        name: environment.environment.name.clone(),
        namespace: environment.environment.namespace.clone(),
        source_path: environment.source.to_string_lossy().into_owned(),
        root: environment.environment.root.to_string_lossy().into_owned(),
        deploys,
        namespace_exclusive: false,
    };
    let path = environment_state_path(cluster_name, &snapshot.name)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temp = path.with_extension("json.tmp");
    fs::write(&temp, serde_json::to_vec_pretty(&snapshot)?)?;
    fs::rename(&temp, &path)?;
    Ok(path)
}

/// Render and reconcile every explicit deploy directory in a validated
/// Environment. Protected identity values are injected after user values are
/// merged.
fn environment_helm_values(
    loaded: &LoadedEnvironment,
    deploy: &super::definition::DeployDefinition,
) -> serde_yaml::Mapping {
    let mut values = loaded.environment.values.clone();
    merge_mapping(&mut values, &deploy.values);
    values.insert(Value::String("local".into()), Value::Bool(true));
    values.insert(
        Value::String("localDomain".into()),
        Value::String(loaded.environment.local_domain.clone()),
    );
    values.insert(
        Value::String("environment".into()),
        string_mapping(&[
            ("name", &loaded.environment.name),
            ("namespace", &loaded.environment.namespace),
        ]),
    );
    values.insert(
        Value::String("source".into()),
        string_mapping(&[
            ("localPath", &deploy.source_root.to_string_lossy()),
            ("path", &deploy.source_path.to_string_lossy()),
            ("type", deploy.deploy_type.as_str()),
        ]),
    );
    values
}

pub fn reconcile_environment<H: HelmRunner, K: KubectlApplier, R: KustomizeRunner>(
    loaded: &LoadedEnvironment,
    opts: &ReconcileOptions,
    helm: &H,
    kustomize: &R,
    kubectl: &K,
) -> Result<Vec<ReconcileResult>, Box<dyn Error>> {
    ensure_environment_namespace(opts, kubectl)?;
    let mut results = Vec::new();
    let mut errors = Vec::new();
    for deploy in &loaded.environment.deploys {
        let values = environment_helm_values(loaded, deploy);
        let app_name = local_deploy_name(deploy);
        match super::reconcile::reconcile_deploy(
            &deploy.source_path,
            deploy.deploy_type,
            deploy.recursive,
            &app_name,
            Value::Mapping(values),
            opts,
            helm,
            kustomize,
            kubectl,
        ) {
            Ok(result) => results.push(result),
            Err(error) => errors.push(format!("{app_name}: {error}")),
        }
    }
    if errors.is_empty() {
        Ok(results)
    } else {
        Err(format!(
            "reconcile failed for {} deploy(s):\n  - {}",
            errors.len(),
            errors.join("\n  - ")
        )
        .into())
    }
}

fn merge_mapping(base: &mut serde_yaml::Mapping, overlay: &serde_yaml::Mapping) {
    for (key, value) in overlay {
        match (base.get_mut(key), value) {
            (Some(Value::Mapping(base_map)), Value::Mapping(overlay_map)) => {
                merge_mapping(base_map, overlay_map)
            }
            _ => {
                base.insert(key.clone(), value.clone());
            }
        }
    }
}

fn string_mapping(values: &[(&str, &str)]) -> Value {
    let mut mapping = serde_yaml::Mapping::new();
    for (key, value) in values {
        mapping.insert(
            Value::String((*key).to_string()),
            Value::String((*value).to_string()),
        );
    }
    Value::Mapping(mapping)
}

fn local_deploy_name(deploy: &super::definition::DeployDefinition) -> String {
    super::definition::local_deploy_name(deploy)
}

pub fn load_environment_snapshot(
    cluster_name: &str,
    environment_name: &str,
) -> Result<Option<EnvironmentSnapshot>, Box<dyn Error>> {
    let path = environment_state_path(cluster_name, environment_name)?;
    if !path.exists() {
        return Ok(None);
    }
    let snapshot: EnvironmentSnapshot = serde_json::from_slice(&fs::read(&path)?)?;
    if snapshot.schema_version != CONTROLLER_SCHEMA_VERSION
        || snapshot.cluster_name != cluster_name
        || snapshot.name != environment_name
    {
        return Err(format!(
            "Environment ownership snapshot {} is invalid or mismatched",
            path.display()
        )
        .into());
    }
    Ok(Some(snapshot))
}

/// Load every durable Environment ownership snapshot for a Cluster. Invalid
/// snapshots fail closed instead of being guessed from filenames or labels.
pub fn list_environment_snapshots(
    cluster_name: &str,
) -> Result<Vec<EnvironmentSnapshot>, Box<dyn Error>> {
    let directory = controller_state_dir(cluster_name)?.join("environments");
    if !directory.is_dir() {
        return Ok(Vec::new());
    }
    let mut paths = fs::read_dir(&directory)?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|value| value.to_str()) == Some("json"))
        .collect::<Vec<_>>();
    paths.sort();
    let mut snapshots = Vec::new();
    for path in paths {
        let snapshot: EnvironmentSnapshot =
            serde_json::from_slice(&fs::read(&path)?).map_err(|error| {
                format!(
                    "Environment ownership snapshot {} is malformed: {error}",
                    path.display()
                )
            })?;
        if snapshot.schema_version != CONTROLLER_SCHEMA_VERSION
            || snapshot.cluster_name != cluster_name
            || snapshot.name.trim().is_empty()
        {
            return Err(format!(
                "Environment ownership snapshot {} is invalid or mismatched",
                path.display()
            )
            .into());
        }
        snapshots.push(snapshot);
    }
    Ok(snapshots)
}

/// Delete only objects proven by the last accepted Environment snapshot. A
/// missing snapshot is an already-down/no-proven-ownership result and performs
/// no inferred namespace or label deletion.
pub fn down_environment(
    cluster_name: &str,
    environment_name: &str,
) -> Result<bool, Box<dyn Error>> {
    let Some(snapshot) = load_environment_snapshot(cluster_name, environment_name)? else {
        return Ok(false);
    };
    if snapshot.namespace.is_empty() {
        return Err("Environment ownership snapshot has no namespace; refusing cleanup".into());
    }
    if let Ok(state_dir) = local_state_dir() {
        if let Err(error) = super::ingress::stop_ingress_access(&state_dir, environment_name) {
            log::warn!("Environment ingress-access cleanup: {error}");
        }
        if let Err(error) = super::net::stop_host_access(&state_dir, environment_name) {
            log::warn!("Environment host-access cleanup: {error}");
        }
        super::delivery::stop_delivery_runtime(&state_dir, environment_name);
    }
    let mut objects = snapshot
        .deploys
        .iter()
        .flat_map(|deploy| deploy.objects.iter())
        .cloned()
        .collect::<Vec<_>>();
    objects.sort_by(|a, b| {
        b.kind
            .cmp(&a.kind)
            .then_with(|| b.name.cmp(&a.name))
            .then_with(|| b.namespace.cmp(&a.namespace))
    });
    for object in objects {
        // Cluster-scoped objects must never be owned by an Environment.
        if object.namespace.is_empty() {
            return Err(format!(
                "Environment snapshot contains cluster-scoped {} {}; refusing cleanup",
                object.kind, object.name
            )
            .into());
        }
        let kind = object.kind.to_ascii_lowercase();
        let args = [
            "delete",
            kind.as_str(),
            object.name.as_str(),
            "--namespace",
            object.namespace.as_str(),
            "--ignore-not-found=true",
            "--wait=true",
        ];
        let output = kubectl_command(&args).output()?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!(
                "Environment cleanup failed for {} {}: {}",
                object.kind,
                object.name,
                stderr.trim()
            )
            .into());
        }
    }
    let path = environment_state_path(cluster_name, environment_name)?;
    fs::remove_file(path)?;
    if let Ok(state_dir) = local_state_dir() {
        let _ = remove_workspace(&state_dir, environment_name)?;
    }
    Ok(true)
}

fn owned_objects_from_yaml(yaml: &str) -> Result<Vec<OwnedObject>, Box<dyn Error>> {
    let mut objects = Vec::new();
    for document in serde_yaml::Deserializer::from_str(yaml) {
        let value = Value::deserialize(document)?;
        if value.is_null() {
            continue;
        }
        let mapping = value
            .as_mapping()
            .ok_or("rendered Environment document must be a mapping")?;
        let api_version = mapping
            .get(Value::String("apiVersion".into()))
            .and_then(Value::as_str)
            .ok_or("rendered Environment object is missing apiVersion")?;
        let kind = mapping
            .get(Value::String("kind".into()))
            .and_then(Value::as_str)
            .ok_or("rendered Environment object is missing kind")?;
        let metadata = mapping
            .get(Value::String("metadata".into()))
            .and_then(Value::as_mapping)
            .ok_or("rendered Environment object is missing metadata")?;
        let name = metadata
            .get(Value::String("name".into()))
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
            .ok_or("rendered Environment object is missing metadata.name")?;
        let namespace = metadata
            .get(Value::String("namespace".into()))
            .and_then(Value::as_str)
            .unwrap_or_default();
        objects.push(OwnedObject {
            api_version: api_version.to_string(),
            kind: kind.to_string(),
            namespace: namespace.to_string(),
            name: name.to_string(),
        });
    }
    Ok(objects)
}

/// Validate the durable identity before an Environment operation may mutate
/// a registered workspace.
pub fn validate_environment_identity(
    snapshot: &EnvironmentSnapshot,
    record: &WorkspaceRecord,
    cluster_name: &str,
) -> Result<(), Box<dyn Error>> {
    if snapshot.cluster_name != cluster_name
        || record.cluster_name.as_deref() != Some(cluster_name)
        || record.name != snapshot.name
        || record.namespace != snapshot.namespace
    {
        return Err(format!(
            "Environment {:?} registration does not match Cluster {:?}; refusing inferred ownership",
            snapshot.name, cluster_name
        )
        .into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protected_helm_values_include_local_domain_and_override_user_values() {
        let environment_values =
            serde_yaml::from_str("local: false\nlocalDomain: public.example.com\npreview: false\n")
                .unwrap();
        let deploy_values =
            serde_yaml::from_str("localDomain: other.localhost\npreview: true\n").unwrap();
        let deploy = super::super::definition::DeployDefinition {
            source_path: PathBuf::from("/project/apps/gateway/.gitops/local"),
            source_root: PathBuf::from("/project/apps/gateway"),
            deploy_type: DeployType::Helm,
            recursive: false,
            values: deploy_values,
        };
        let loaded = LoadedEnvironment {
            source: PathBuf::from("/project/.gitops/local/environment.yaml"),
            environment: super::super::definition::EnvironmentDefinition {
                name: "feature-auth".into(),
                namespace: "feature-auth-ns".into(),
                cluster_ref: "project-dev".into(),
                local_domain: "gitkb.localhost".into(),
                root: PathBuf::from("/project"),
                values: environment_values,
                deploys: vec![deploy.clone()],
            },
        };

        let values = environment_helm_values(&loaded, &deploy);

        assert_eq!(values["local"], Value::Bool(true));
        assert_eq!(
            values["localDomain"],
            Value::String("gitkb.localhost".into())
        );
        assert_eq!(values["preview"], Value::Bool(true));
        assert_eq!(
            values["environment"]["name"],
            Value::String("feature-auth".into())
        );
        assert_eq!(
            values["environment"]["namespace"],
            Value::String("feature-auth-ns".into())
        );
    }

    #[test]
    fn lease_round_trip_and_reuse_metadata() {
        let root = std::env::temp_dir().join(format!(
            "hops-controller-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let definition = root.join("project/.gitops/local/cluster.yaml");
        fs::create_dir_all(definition.parent().unwrap()).unwrap();
        let cluster_name = format!("demo-{}", uuid::Uuid::new_v4().simple());
        let first = acquire_controller(&cluster_name, &definition, "kind-demo", false).unwrap();
        assert!(first.is_owner());
        let second = acquire_controller(&cluster_name, &definition, "kind-demo", false).unwrap();
        assert!(!second.is_owner());
        assert_eq!(second.lease.pid, std::process::id());
        drop(second);
        drop(first);
        let lock = controller_lock_path(&cluster_name).unwrap();
        assert!(!lock.exists());
        let _ = fs::remove_dir_all(lock.parent().unwrap());
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn unrepresentable_controller_pid_fails_closed() {
        assert!(controller_pid_is_live(u32::MAX));
    }

    #[cfg(unix)]
    #[test]
    fn dead_matching_controller_lock_is_recovered() {
        let root = std::env::temp_dir().join(format!(
            "hops-controller-recovery-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let definition = root.join("project/.gitops/local/cluster.yaml");
        fs::create_dir_all(definition.parent().unwrap()).unwrap();
        let cluster_name = format!("recovery-{}", uuid::Uuid::new_v4().simple());
        let lock = controller_lock_path(&cluster_name).unwrap();
        fs::create_dir_all(lock.parent().unwrap()).unwrap();
        let stale = ControllerLease {
            schema_version: CONTROLLER_SCHEMA_VERSION,
            mode: CONTROLLER_MODE.into(),
            cluster_name: cluster_name.clone(),
            definition_path: definition.to_string_lossy().into_owned(),
            kube_context: "kind-recovery".into(),
            pid: i32::MAX as u32,
            started_at: 0,
        };
        fs::write(&lock, serde_json::to_vec_pretty(&stale).unwrap()).unwrap();

        let handle =
            acquire_controller(&cluster_name, &definition, "kind-recovery", false).unwrap();
        assert!(handle.is_owner());
        assert_eq!(handle.lease.pid, std::process::id());
        let stored: ControllerLease = serde_json::from_slice(&fs::read(&lock).unwrap()).unwrap();
        assert_eq!(stored, handle.lease);

        drop(handle);
        assert!(!lock.exists());
        let _ = fs::remove_dir_all(lock.parent().unwrap());
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn concurrent_dead_lock_recovery_establishes_one_owner() {
        use std::sync::{Arc, Barrier};

        for _ in 0..16 {
            let root = std::env::temp_dir().join(format!(
                "hops-controller-race-{}-{}",
                std::process::id(),
                uuid::Uuid::new_v4()
            ));
            let definition = root.join("project/.gitops/local/cluster.yaml");
            fs::create_dir_all(definition.parent().unwrap()).unwrap();
            let cluster_name = format!("race-{}", uuid::Uuid::new_v4().simple());
            let lock = controller_lock_path(&cluster_name).unwrap();
            fs::create_dir_all(lock.parent().unwrap()).unwrap();
            let stale = ControllerLease {
                schema_version: CONTROLLER_SCHEMA_VERSION,
                mode: CONTROLLER_MODE.into(),
                cluster_name: cluster_name.clone(),
                definition_path: definition.to_string_lossy().into_owned(),
                kube_context: "kind-race".into(),
                pid: i32::MAX as u32,
                started_at: 0,
            };
            fs::write(&lock, serde_json::to_vec_pretty(&stale).unwrap()).unwrap();

            let contenders = 8;
            let barrier = Arc::new(Barrier::new(contenders));
            let mut threads = Vec::new();
            for _ in 0..contenders {
                let barrier = Arc::clone(&barrier);
                let cluster_name = cluster_name.clone();
                let definition = definition.clone();
                threads.push(std::thread::spawn(move || {
                    let result = acquire_controller(&cluster_name, &definition, "kind-race", false)
                        .map_err(|error| error.to_string());
                    let is_owner = result
                        .as_ref()
                        .map(ControllerHandle::is_owner)
                        .unwrap_or(false);
                    barrier.wait();
                    result.map(|_| is_owner)
                }));
            }
            let owner_count = threads
                .into_iter()
                .map(|thread| thread.join().unwrap().unwrap())
                .filter(|is_owner| *is_owner)
                .count();
            assert_eq!(owner_count, 1);
            assert!(!lock.exists());

            let _ = fs::remove_dir_all(lock.parent().unwrap());
            let _ = fs::remove_dir_all(root);
        }
    }

    #[cfg(unix)]
    #[test]
    fn dead_mismatched_controller_lock_still_fails_closed() {
        let root = std::env::temp_dir().join(format!(
            "hops-controller-mismatch-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let definition = root.join("project/.gitops/local/cluster.yaml");
        fs::create_dir_all(definition.parent().unwrap()).unwrap();
        let cluster_name = format!("mismatch-{}", uuid::Uuid::new_v4().simple());
        let lock = controller_lock_path(&cluster_name).unwrap();
        fs::create_dir_all(lock.parent().unwrap()).unwrap();
        let stale = ControllerLease {
            schema_version: CONTROLLER_SCHEMA_VERSION,
            mode: CONTROLLER_MODE.into(),
            cluster_name: cluster_name.clone(),
            definition_path: "/another/worktree/.gitops/local/cluster.yaml".into(),
            kube_context: "kind-mismatch".into(),
            pid: i32::MAX as u32,
            started_at: 0,
        };
        fs::write(&lock, serde_json::to_vec_pretty(&stale).unwrap()).unwrap();

        let error =
            acquire_controller(&cluster_name, &definition, "kind-mismatch", false).unwrap_err();
        assert!(error.to_string().contains("different GitOps controller"));
        assert!(lock.exists());

        let _ = fs::remove_dir_all(lock.parent().unwrap());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn absent_cluster_reset_clears_stale_state_but_rejects_a_live_owner() {
        let state_dir = std::env::temp_dir().join(format!(
            "hops-absent-cluster-reset-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let cluster_name = "project-dev";
        let cluster_dir = state_dir.join("clusters/project-dev");
        fs::create_dir_all(cluster_dir.join("environments")).unwrap();
        fs::write(cluster_dir.join("cluster-inventory.json"), "{}").unwrap();
        let lease = ControllerLease {
            schema_version: CONTROLLER_SCHEMA_VERSION,
            mode: CONTROLLER_MODE.into(),
            cluster_name: cluster_name.into(),
            definition_path: "/old/worktree/.gitops/local/cluster.yaml".into(),
            kube_context: "kind-project-dev".into(),
            pid: std::process::id(),
            started_at: 0,
        };
        fs::write(
            cluster_dir.join("controller.lock"),
            serde_json::to_vec(&lease).unwrap(),
        )
        .unwrap();

        let error =
            reset_absent_cluster_state_at(&state_dir, &cluster_dir, cluster_name).unwrap_err();
        assert!(error.to_string().contains("still live"));
        assert!(cluster_dir.exists());

        fs::remove_file(cluster_dir.join("controller.lock")).unwrap();
        assert!(reset_absent_cluster_state_at(&state_dir, &cluster_dir, cluster_name).unwrap());
        assert!(!cluster_dir.exists());
        assert!(!reset_absent_cluster_state_at(&state_dir, &cluster_dir, cluster_name).unwrap());
        let _ = fs::remove_dir_all(state_dir);
    }

    #[test]
    fn owned_objects_record_empty_namespace_for_cluster_scope() {
        let objects = owned_objects_from_yaml(
            "apiVersion: v1\nkind: Namespace\nmetadata:\n  name: forbidden\n",
        )
        .unwrap();
        assert!(objects[0].namespace.is_empty());
    }
}
