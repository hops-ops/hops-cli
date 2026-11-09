//! Speed-first source delivery: hostPath when probe passes, mutagen-class fallback.
//!
//! Probe checks whether the host path is visible **on the Kubernetes node**
//! (not merely whether it exists on the laptop). Sync mode actually transfers
//! files into cluster-dev pods using mutagen when available, else tar|kubectl
//! exec with the same ignore list.

use std::error::Error;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::commands::local::kubectl_command;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryStrategy {
    /// Worktree path is visible on the node — mount hostPath.
    HostPath,
    /// Probe failed — mutagen-class (or equivalent) host→pod sync.
    Sync,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryProbe {
    /// Absolute host worktree / project root path.
    pub host_path: PathBuf,
    /// Whether the path is visible on the Kubernetes node.
    pub host_path_visible: bool,
    /// Optional detail for status/verbose (probe command result).
    pub detail: String,
}

/// Default paths excluded from mutagen-class sync (LWB-REQ-150).
pub fn default_sync_ignores() -> Vec<&'static str> {
    vec![
        "node_modules",
        "target",
        ".git",
        "dist",
        "build",
        ".svelte-kit",
        "playwright-report",
        "test-results",
        "_output",
        ".cache",
        ".turbo",
        "coverage",
        // macOS noise that bloats tar and confuses extract
        ".DS_Store",
    ]
}

/// Auto-select delivery strategy from probe result (LWB-REQ-140, LWB-REQ-240).
/// Prefers hostPath when capable; never requires user choice.
pub fn select_delivery_strategy(probe: &DeliveryProbe) -> DeliveryStrategy {
    if probe.host_path_visible {
        DeliveryStrategy::HostPath
    } else {
        DeliveryStrategy::Sync
    }
}

impl DeliveryStrategy {
    pub fn as_str(self) -> &'static str {
        match self {
            DeliveryStrategy::HostPath => "hostPath",
            DeliveryStrategy::Sync => "sync",
        }
    }

    /// Runtime values fragment for helm inject.
    pub fn helm_mode_value(self) -> &'static str {
        self.as_str()
    }
}

/// Build a probe result from a pure boolean (unit-test / fake backend).
pub fn probe_from_visibility(
    host_path: &Path,
    visible: bool,
    detail: impl Into<String>,
) -> DeliveryProbe {
    DeliveryProbe {
        host_path: host_path.to_path_buf(),
        host_path_visible: visible,
        detail: detail.into(),
    }
}

/// Whether a relative path component should be excluded from sync sessions.
pub fn path_is_sync_excluded(path: &Path) -> bool {
    let ignores = default_sync_ignores();
    path.components().any(|c| {
        let s = c.as_os_str().to_string_lossy();
        ignores.iter().any(|ig| *ig == s.as_ref())
    })
}

/// Mutagen CLI `--ignore` flags derived from the shared ignore list (shipped path).
pub fn mutagen_ignore_args() -> Vec<String> {
    default_sync_ignores()
        .into_iter()
        .flat_map(|ig| vec!["--ignore".to_string(), ig.to_string()])
        .collect()
}

/// GNU/BSD tar `--exclude=` flags for the shared ignore list (shipped path).
pub fn tar_exclude_args() -> Vec<String> {
    let mut args: Vec<String> = default_sync_ignores()
        .into_iter()
        .map(|ig| format!("--exclude={ig}"))
        .collect();
    // AppleDouble resource forks (`._file`) — pattern exclude (bsdtar/gnutar).
    args.push("--exclude=._*".into());
    args
}

/// Build mutagen sync create argv (without program name) for a host→pod session.
///
/// Destination uses the kubectl-exec transport form consumed by
/// [`start_mutagen_session`]; pure so unit tests can assert ignore wiring.
pub fn build_mutagen_create_args(
    session_name: &str,
    host_path: &Path,
    dest_url: &str,
) -> Vec<String> {
    let mut args = vec![
        "sync".into(),
        "create".into(),
        "--name".into(),
        session_name.into(),
        "--sync-mode".into(),
        "one-way-replica".into(),
    ];
    args.extend(mutagen_ignore_args());
    args.push(host_path.display().to_string());
    args.push(dest_url.into());
    args
}

/// Session name for a workspace app pair (DNS-safe-ish).
pub fn sync_session_name(workspace: &str, app: &str) -> String {
    format!("hops-lwb-{workspace}-{app}")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// Target pod for source delivery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncPodTarget {
    pub namespace: String,
    pub pod: String,
    pub container: Option<String>,
    pub mount_path: String,
    pub app_name: String,
    /// EmptyDir volume checked again before each background copy.
    pub source_volume: String,
    /// Per-app host directory to tar/sync (NOT a shared monorepo root for all apps).
    pub host_source_path: PathBuf,
}

/// Result of attaching delivery after reconcile.
#[derive(Debug, Clone)]
pub struct DeliveryAttachResult {
    pub strategy: DeliveryStrategy,
    pub probe: DeliveryProbe,
    /// Mutagen session names started (if any).
    pub mutagen_sessions: Vec<String>,
    /// Background sync watcher PIDs (tar-based continuous sync).
    pub sync_pids: Vec<u32>,
    pub messages: Vec<String>,
}

/// Abstraction over node path visibility for tests.
pub trait NodePathProber {
    fn probe(&self, host_path: &Path) -> Result<DeliveryProbe, Box<dyn Error>>;
}

/// System prober: host dir must exist, then verify the **node** can see it.
///
/// Strategies (first success wins):
/// 1. Docker exec into containers matching Ready node names (kind/dory-style).
/// 2. Short-lived probe Pod with hostPath mount (portable; detects FailedMount).
pub struct SystemNodeProber;

impl NodePathProber for SystemNodeProber {
    fn probe(&self, host_path: &Path) -> Result<DeliveryProbe, Box<dyn Error>> {
        probe_node_path_visibility(host_path)
    }
}

/// Production probe entrypoint.
pub fn probe_node_path_visibility(host_path: &Path) -> Result<DeliveryProbe, Box<dyn Error>> {
    if !host_path.is_dir() {
        return Err(format!(
            "source is not a directory: {}; cannot mount or sync it",
            host_path.display()
        )
        .into());
    }
    let abs = host_path
        .canonicalize()
        .unwrap_or_else(|_| host_path.to_path_buf());

    // 1) Docker node containers (kind / dory / similar)
    match try_docker_node_probe(&abs) {
        Ok(Some(probe)) => return Ok(probe),
        Ok(None) => {}
        Err(e) => log::debug!("docker node probe skipped: {e}"),
    }

    // 2) kubectl hostPath probe pod
    // An inconclusive observation must not change an existing workload's
    // volume type. Only a confirmed missing node path can select sync.
    try_kubectl_hostpath_probe(&abs)
}

struct PathProbePod(String);

impl Drop for PathProbePod {
    fn drop(&mut self) {
        let _ = kubectl_command(&[
            "--request-timeout=2s",
            "delete",
            "pod",
            &self.0,
            "-n",
            "default",
            "--wait=false",
            "--ignore-not-found=true",
        ])
        .output();
    }
}

fn try_docker_node_probe(host_path: &Path) -> Result<Option<DeliveryProbe>, Box<dyn Error>> {
    if !command_exists("docker") || !command_exists("kubectl") {
        return Ok(None);
    }
    let nodes_json =
        kubectl_command(&["--request-timeout=2s", "get", "nodes", "-o", "json"]).output()?;
    if !nodes_json.status.success() {
        return Ok(None);
    }
    let value: serde_json::Value = serde_json::from_slice(&nodes_json.stdout)?;
    let Some(items) = value.get("items").and_then(|i| i.as_array()) else {
        return Ok(None);
    };
    let mut node_names = Vec::new();
    for item in items {
        if let Some(name) = item.pointer("/metadata/name").and_then(|v| v.as_str()) {
            node_names.push(name.to_string());
        }
    }
    if node_names.is_empty() {
        return Ok(None);
    }

    // Candidate container names: exact node name, kind-style, dory-k8s, hops-control-plane
    let mut candidates: Vec<String> = node_names.clone();
    candidates.push("dory-k8s".into());
    candidates.push("hops-control-plane".into());
    for n in &node_names {
        // kind often uses <cluster>-control-plane matching node name already
        if let Some(stripped) = n.strip_suffix("-control-plane") {
            candidates.push(format!("{stripped}-control-plane"));
        }
    }
    candidates.sort();
    candidates.dedup();

    let path_str = host_path.display().to_string();
    for container in candidates {
        let status = bounded_probe_command(
            Command::new("docker").args(["exec", &container, "test", "-d", &path_str]),
            Duration::from_secs(3),
        )
        .map(|output| output.status);
        match status {
            Ok(s) if s.success() => {
                return Ok(Some(probe_from_visibility(
                    host_path,
                    true,
                    format!("docker exec {container}: path visible on node"),
                )));
            }
            Ok(status) => {
                // Path missing: only treat as definitive for containers that match an
                // actual node name. Hardcoded fallbacks (`dory-k8s`, `hops-control-plane`)
                // may be a *different* cluster (e.g. product dory k3s vs kind-hops) and
                // must not short-circuit before trying the real node container.
                if status.code() == Some(1)
                    && docker_container_running(&container)
                    && node_names.iter().any(|n| n == &container)
                {
                    return Ok(Some(probe_from_visibility(
                        host_path,
                        false,
                        format!("docker exec {container}: path not present on node"),
                    )));
                }
            }
            Err(_) => continue,
        }
    }
    Ok(None)
}

fn bounded_probe_command(
    command: &mut Command,
    timeout: Duration,
) -> Result<std::process::Output, Box<dyn Error>> {
    // Docker's client can stall on a stale engine socket. These probe commands
    // emit only a bool (or nothing), so output cannot fill a pipe while waiting.
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let deadline = Instant::now() + timeout;
    loop {
        if child.try_wait()?.is_some() {
            return Ok(child.wait_with_output()?);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err("Docker node visibility probe timed out".into());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn docker_container_running(name: &str) -> bool {
    bounded_probe_command(
        Command::new("docker").args(["inspect", "-f", "{{.State.Running}}", name]),
        Duration::from_secs(3),
    )
    .ok()
    .is_some_and(|out| {
        out.status.success() && String::from_utf8_lossy(&out.stdout).trim() == "true"
    })
}

fn try_kubectl_hostpath_probe(host_path: &Path) -> Result<DeliveryProbe, Box<dyn Error>> {
    // Deletion is asynchronous. Reusing a PID-based name races the preceding
    // probe's deletion (and can observe its old phase or logs).
    let name = format!("hops-path-probe-{}", uuid::Uuid::new_v4().simple());
    let _cleanup = PathProbePod(name.clone());
    let path_str = host_path.display().to_string();
    // Escape for YAML double quotes
    let path_yaml = path_str.replace('\\', "\\\\").replace('"', "\\\"");
    let yaml = format!(
        r#"apiVersion: v1
kind: Pod
metadata:
  name: {name}
  namespace: default
  labels:
    app.kubernetes.io/managed-by: hops-local-gitops
    hops.ops.com.ai/probe: path-visibility
spec:
  restartPolicy: Never
  terminationGracePeriodSeconds: 1
  containers:
    - name: probe
      image: busybox:1.36
      command:
        - sh
        - -c
        - "test -d /probe && echo HOPS_PATH_VISIBLE || echo HOPS_PATH_MISSING"
      volumeMounts:
        - name: host
          mountPath: /probe
          readOnly: true
  volumes:
    - name: host
      hostPath:
        path: "{path_yaml}"
        type: Directory
"#
    );

    // Apply
    let mut apply = kubectl_command(&["--request-timeout=5s", "create", "-f", "-"]);
    let mut child = apply
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(yaml.as_bytes())?;
    }
    let apply_out = child.wait_with_output()?;
    if !apply_out.status.success() {
        return Err(format!(
            "probe pod apply failed: {}",
            String::from_utf8_lossy(&apply_out.stderr)
        )
        .into());
    }

    // Wait up to ~15s for Succeeded/Failed
    let mut observation = None;
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(400));
        let phase_out = kubectl_command(&[
            "--request-timeout=2s",
            "get",
            "pod",
            &name,
            "-n",
            "default",
            "-o",
            "jsonpath={.status.phase}",
        ])
        .output()?;
        let phase = String::from_utf8_lossy(&phase_out.stdout)
            .trim()
            .to_string();

        // FailedMount appears in events / container statuses
        let desc = kubectl_command(&[
            "--request-timeout=2s",
            "describe",
            "pod",
            &name,
            "-n",
            "default",
        ])
        .output()?;
        let desc_s = String::from_utf8_lossy(&desc.stdout);
        if source_path_is_missing(&desc_s, &path_str) {
            observation = Some((
                false,
                "kubectl hostPath probe: FailedMount (path not on node)".into(),
            ));
            break;
        }

        if phase == "Succeeded" || phase == "Failed" || phase == "Running" {
            let logs = kubectl_command(&[
                "--request-timeout=2s",
                "logs",
                &name,
                "-n",
                "default",
                "--tail=20",
                "--pod-running-timeout=1s",
            ])
            .output()?;
            let log_s = String::from_utf8_lossy(&logs.stdout);
            if let Some(visible) = probe_visibility(&phase, &log_s) {
                observation = Some((
                    visible,
                    format!("kubectl hostPath probe: phase={phase}, visible={visible}"),
                ));
                break;
            }
            if phase == "Failed" {
                break;
            }
            // Running without output is still pending, not proof of a missing
            // path. Give the probe time to emit its marker.
        }
    }

    finish_path_probe(host_path, observation)
}

fn source_path_is_missing(description: &str, source: &str) -> bool {
    // Token/config projection failures are not evidence that source is absent.
    description.contains("hostPath type check failed") && description.contains(source)
}

fn probe_visibility(phase: &str, logs: &str) -> Option<bool> {
    if logs.contains("HOPS_PATH_VISIBLE") || phase == "Succeeded" {
        // The owned probe's Directory hostPath had to mount for it to succeed.
        if logs.contains("HOPS_PATH_MISSING") {
            Some(false)
        } else {
            Some(true)
        }
    } else if logs.contains("HOPS_PATH_MISSING") {
        Some(false)
    } else {
        None
    }
}

fn finish_path_probe(
    host_path: &Path,
    observation: Option<(bool, String)>,
) -> Result<DeliveryProbe, Box<dyn Error>> {
    match observation {
        Some((visible, detail)) => Ok(probe_from_visibility(host_path, visible, detail)),
        None => Err(format!(
            "hostPath probe for {} timed out or failed without a visibility result; leaving source delivery unchanged",
            host_path.display()
        ).into()),
    }
}

fn command_exists(program: &str) -> bool {
    Command::new("sh")
        .args(["-c", &format!("command -v {program} >/dev/null 2>&1")])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

// ── Sync attach (real delivery) ─────────────────────────────────────────────

/// Pure argv for the host-side `tar cf` half of sync (includes ignore excludes).
pub fn build_tar_cf_args(host_path: &Path) -> Vec<String> {
    let mut tar_args: Vec<String> = vec!["cf".into(), "-".into()];
    tar_args.extend(tar_exclude_args());
    tar_args.push("-C".into());
    tar_args.push(host_path.display().to_string());
    tar_args.push(".".into());
    tar_args
}

/// Pure argv for the kubectl exec half of sync (extract into mount_path).
pub fn build_kubectl_tar_extract_args(target: &SyncPodTarget) -> Vec<String> {
    let mut kubectl_args = vec![
        "exec".into(),
        "-i".into(),
        "-n".into(),
        target.namespace.clone(),
        target.pod.clone(),
    ];
    if let Some(c) = &target.container {
        kubectl_args.push("-c".into());
        kubectl_args.push(c.clone());
    }
    kubectl_args.push("--".into());
    kubectl_args.push("tar".into());
    kubectl_args.push("xf".into());
    kubectl_args.push("-".into());
    kubectl_args.push("-C".into());
    kubectl_args.push(target.mount_path.clone());
    kubectl_args
}

/// One-shot tar|kubectl exec sync of host_path → pod mount, applying ignore list.
/// Retries briefly while the container becomes exec-ready.
pub fn sync_directory_to_pod(
    host_path: &Path,
    target: &SyncPodTarget,
) -> Result<(), Box<dyn Error>> {
    if !host_path.is_dir() {
        return Err(format!("sync source not a directory: {}", host_path.display()).into());
    }
    let mut last_err = String::new();
    // Pods often become exec-ready a few seconds after Running; keep trying.
    for attempt in 1..=12 {
        match sync_directory_to_pod_once(host_path, target) {
            Ok(()) => return Ok(()),
            Err(e) => {
                last_err = e.to_string();
                log::warn!(
                    "sync attempt {attempt}/12 for {} failed: {last_err}",
                    target.pod
                );
                std::thread::sleep(Duration::from_secs(2));
            }
        }
    }
    Err(format!("sync failed after retries: {last_err}").into())
}

fn sync_directory_to_pod_once(
    host_path: &Path,
    target: &SyncPodTarget,
) -> Result<(), Box<dyn Error>> {
    let tar_args = build_tar_cf_args(host_path);

    let mut tar = Command::new("tar")
        .args(&tar_args)
        .env("COPYFILE_DISABLE", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed to spawn tar: {e}"))?;

    // Strip the leading "exec" so kubectl_command can inject --context.
    let kubectl_args = build_kubectl_tar_extract_args(target);
    let kubectl_refs: Vec<&str> = kubectl_args.iter().map(String::as_str).collect();
    let mut kubectl = kubectl_command(&kubectl_refs)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed to spawn kubectl exec: {e}"))?;

    // Pipe tar stdout → kubectl stdin
    if let (Some(mut tar_out), Some(mut k_in)) = (tar.stdout.take(), kubectl.stdin.take()) {
        std::io::copy(&mut tar_out, &mut k_in).map_err(|e| format!("pipe tar→kubectl: {e}"))?;
    }
    let tar_status = tar.wait()?;
    let k_out = kubectl.wait_with_output()?;
    if !tar_status.success() {
        return Err(format!("tar failed with {tar_status}").into());
    }
    if !k_out.status.success() {
        return Err(format!(
            "kubectl exec tar extract failed: {}",
            String::from_utf8_lossy(&k_out.stderr)
        )
        .into());
    }
    write_sync_marker(target)?;
    Ok(())
}

fn write_sync_marker(target: &SyncPodTarget) -> Result<(), Box<dyn Error>> {
    let marker = format!("{}/.hops-synced", target.mount_path.trim_end_matches('/'));
    let mut args = vec![
        "exec".to_string(),
        "-n".to_string(),
        target.namespace.clone(),
        target.pod.clone(),
    ];
    if let Some(c) = &target.container {
        args.push("-c".to_string());
        args.push(c.clone());
    }
    args.push("--".to_string());
    args.push("sh".to_string());
    args.push("-c".to_string());
    args.push(format!("touch {marker}"));
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = kubectl_command(&refs).output()?;
    if !out.status.success() {
        return Err(format!(
            "failed to write sync marker: {}",
            String::from_utf8_lossy(&out.stderr)
        )
        .into());
    }
    Ok(())
}

/// Discover Running pods labeled for this workspace and attach per-app host paths.
///
/// `app_host_paths`: app name → absolute host directory to sync into that pod.
pub fn discover_sync_targets(
    namespace: &str,
    workspace: &str,
    mount_path: &str,
    app_host_paths: &std::collections::BTreeMap<String, PathBuf>,
) -> Result<Vec<SyncPodTarget>, Box<dyn Error>> {
    let label = format!("hops.ops.com.ai/local-env={workspace}");
    let json =
        kubectl_command(&["get", "pods", "-n", namespace, "-l", &label, "-o", "json"]).output()?;
    if !json.status.success() {
        return Err(format!(
            "kubectl get pods failed: {}",
            String::from_utf8_lossy(&json.stderr)
        )
        .into());
    }
    let value: serde_json::Value = serde_json::from_slice(&json.stdout)?;
    sync_targets_from_pods(&value, namespace, mount_path, app_host_paths)
}

fn sync_targets_from_pods(
    value: &serde_json::Value,
    namespace: &str,
    mount_path: &str,
    app_host_paths: &std::collections::BTreeMap<String, PathBuf>,
) -> Result<Vec<SyncPodTarget>, Box<dyn Error>> {
    let mut out = Vec::new();
    for item in value
        .get("items")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
    {
        if item.pointer("/status/phase").and_then(|v| v.as_str()) != Some("Running") {
            continue;
        }
        let Some(pod) = item.pointer("/metadata/name").and_then(|v| v.as_str()) else {
            continue;
        };
        let Some(app) = item
            .pointer("/metadata/labels/hops.ops.com.ai~1local-app")
            .and_then(|v| v.as_str())
        else {
            continue;
        };
        // Never fall back to the caller's current directory or a fuzzy app
        // match: only explicitly declared roots may be copied into a pod.
        let Some(host_source_path) = app_host_paths.get(app) else {
            continue;
        };
        for container in item
            .pointer("/spec/containers")
            .and_then(|v| v.as_array())
            .into_iter()
            .flatten()
        {
            let Some(source_volume) = sync_source_volume(
                item.get("spec").unwrap_or(&serde_json::Value::Null),
                container,
                mount_path,
            ) else {
                continue;
            };
            let Some(name) = container.get("name").and_then(|v| v.as_str()) else {
                continue;
            };
            out.push(SyncPodTarget {
                namespace: namespace.to_string(),
                pod: pod.to_string(),
                container: Some(name.to_string()),
                mount_path: mount_path.to_string(),
                app_name: app.to_string(),
                source_volume,
                host_source_path: host_source_path.clone(),
            });
            // Multiple containers share the same source volume; copy once.
            break;
        }
    }
    Ok(out)
}

pub(crate) fn has_sync_source_mount(
    spec: &serde_json::Value,
    container: &serde_json::Value,
    mount_path: &str,
) -> bool {
    sync_source_volume(spec, container, mount_path).is_some()
}

fn sync_source_volume(
    spec: &serde_json::Value,
    container: &serde_json::Value,
    mount_path: &str,
) -> Option<String> {
    for mount in container
        .get("volumeMounts")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
    {
        if mount.get("mountPath").and_then(|v| v.as_str()) != Some(mount_path) {
            continue;
        }
        let name = mount.get("name")?.as_str()?;
        if spec
            .get("volumes")
            .and_then(|v| v.as_array())
            .into_iter()
            .flatten()
            .any(|volume| {
                volume.get("name").and_then(|v| v.as_str()) == Some(name)
                    && volume.get("emptyDir").is_some()
            })
        {
            return Some(name.to_string());
        }
    }
    None
}

/// Start mutagen session if mutagen is on PATH; returns session name.
pub fn start_mutagen_session(
    session_name: &str,
    host_path: &Path,
    dest_url: &str,
) -> Result<(), Box<dyn Error>> {
    if !command_exists("mutagen") {
        return Err("mutagen not on PATH".into());
    }
    // Terminate any prior session with same name
    let _ = Command::new("mutagen")
        .args(["sync", "terminate", session_name])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let args = build_mutagen_create_args(session_name, host_path, dest_url);
    let output = Command::new("mutagen").args(&args).output()?;
    if !output.status.success() {
        return Err(format!(
            "mutagen sync create failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    Ok(())
}

/// Terminate mutagen sessions by name (best-effort).
pub fn stop_mutagen_sessions(sessions: &[String]) {
    for s in sessions {
        let _ = Command::new("mutagen")
            .args(["sync", "terminate", s])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

#[derive(Debug, serde::Serialize, serde::Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct DeliveryRuntime {
    mutagen_sessions: Vec<String>,
    sync_pids: Vec<u32>,
}

/// Record delivery processes so the next reconcile or `local down` can stop them.
pub(crate) fn save_delivery_runtime(
    state_dir: &Path,
    workspace: &str,
    sessions: &[String],
    pids: &[u32],
) -> Result<(), Box<dyn Error>> {
    let dir = state_dir.join("runtime");
    std::fs::create_dir_all(&dir)?;
    let runtime = DeliveryRuntime {
        mutagen_sessions: sessions.to_vec(),
        sync_pids: pids.to_vec(),
    };
    let temporary = dir.join(format!(
        ".{workspace}-{}.delivery.tmp",
        uuid::Uuid::new_v4()
    ));
    std::fs::write(&temporary, serde_json::to_string_pretty(&runtime)?)?;
    if let Err(error) = std::fs::rename(&temporary, dir.join(format!("{workspace}.delivery.json")))
    {
        let _ = std::fs::remove_file(temporary);
        return Err(error.into());
    }
    Ok(())
}

/// Stop delivery processes recorded for a workspace (best-effort).
pub(crate) fn stop_delivery_runtime(state_dir: &Path, workspace: &str) {
    let path = state_dir
        .join("runtime")
        .join(format!("{workspace}.delivery.json"));
    if let Ok(text) = std::fs::read_to_string(&path) {
        if let Ok(rt) = serde_json::from_str::<DeliveryRuntime>(&text) {
            stop_mutagen_sessions(&rt.mutagen_sessions);
            stop_tar_watchers(&rt.sync_pids, workspace);
        }
    }
    let _ = std::fs::remove_file(path);
}

/// Attach sync delivery for all targets: each target uses its own `host_source_path`.
///
/// `watch`: when true and mutagen unavailable, spawn a multi-target tar re-sync loop.
pub fn attach_sync_delivery(
    targets: &[SyncPodTarget],
    workspace: &str,
    watch: bool,
) -> Result<DeliveryAttachResult, Box<dyn Error>> {
    let probe = probe_from_visibility(Path::new("."), false, "sync attach (per-app hosts)");
    let mut result = DeliveryAttachResult {
        strategy: DeliveryStrategy::Sync,
        probe,
        mutagen_sessions: Vec::new(),
        sync_pids: Vec::new(),
        messages: Vec::new(),
    };

    initialize_sync(targets, |target| {
        sync_directory_to_pod(&target.host_source_path, target)
    })?;
    let mutagen = command_exists("mutagen");
    for target in targets {
        let host_path = &target.host_source_path;
        let session = sync_session_name(workspace, &target.app_name);
        if mutagen {
            let dest = format!(
                "kubectl://{}/{}{}",
                target.namespace, target.pod, target.mount_path
            );
            match start_mutagen_session(&session, host_path, &dest) {
                Ok(()) => {
                    result.mutagen_sessions.push(session);
                    result.messages.push(format!(
                        "mutagen session for {} ← {}",
                        target.pod,
                        host_path.display()
                    ));
                    continue;
                }
                Err(e) => {
                    result.messages.push(format!(
                        "mutagen unavailable for {}: {e}; using tar sync",
                        target.pod
                    ));
                }
            }
        }
    }

    let _ = watch;
    // Mutagen sessions target a particular pod. Keep the label-resolving
    // watcher as well so a rollout's replacement pods receive their source.
    if !targets.is_empty() {
        match spawn_tar_sync_watcher(targets.to_vec(), workspace) {
            Ok(pid) => {
                result.sync_pids.push(pid);
                result.messages.push(format!("tar sync watcher pid={pid}"));
            }
            Err(error) => {
                stop_mutagen_sessions(&result.mutagen_sessions);
                return Err(error);
            }
        }
    }

    Ok(result)
}

fn initialize_sync(
    targets: &[SyncPodTarget],
    mut sync: impl FnMut(&SyncPodTarget) -> Result<(), Box<dyn Error>>,
) -> Result<(), Box<dyn Error>> {
    if targets.is_empty() {
        return Err("no source pods available for sync; retry when containers are running".into());
    }
    let mut failures = Vec::new();
    for target in targets {
        if let Err(error) = sync(target) {
            failures.push(format!("{}: {error}", target.pod));
        }
    }
    if !failures.is_empty() {
        return Err(format!("initial source delivery failed: {}", failures.join("; ")).into());
    }
    Ok(())
}

/// Commit new delivery state before retiring the old processes. In particular,
/// do not terminate a newly replaced mutagen session with the same name.
pub(crate) fn replace_delivery_runtime(
    state_dir: &Path,
    workspace: &str,
    result: &DeliveryAttachResult,
) -> Result<(), Box<dyn Error>> {
    let path = state_dir
        .join("runtime")
        .join(format!("{workspace}.delivery.json"));
    let old = std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<DeliveryRuntime>(&text).ok())
        .unwrap_or_default();
    if let Err(error) = save_delivery_runtime(
        state_dir,
        workspace,
        &result.mutagen_sessions,
        &result.sync_pids,
    ) {
        stop_mutagen_sessions(&result.mutagen_sessions);
        stop_tar_watchers(&result.sync_pids, workspace);
        return Err(error);
    }
    stop_mutagen_sessions(
        &old.mutagen_sessions
            .into_iter()
            .filter(|s| !result.mutagen_sessions.contains(s))
            .collect::<Vec<_>>(),
    );
    let stale = old
        .sync_pids
        .into_iter()
        .filter(|pid| !result.sync_pids.contains(pid))
        .collect::<Vec<_>>();
    stop_tar_watchers(&stale, workspace);
    Ok(())
}

/// Stop only watcher process groups whose full command contains our workspace marker.
fn stop_tar_watchers(pids: &[u32], workspace: &str) {
    let marker = format!("hops-source-sync:{workspace}");
    for pid in pids {
        let owned = Command::new("ps")
            // The marker follows a long shell script. Request unlimited width
            // on both BSD and procps, even when COLUMNS limits piped output.
            .args(["-ww", "-p", &pid.to_string(), "-o", "command="])
            .output()
            .ok()
            .is_some_and(|out| {
                out.status.success()
                    && String::from_utf8_lossy(&out.stdout)
                        .split_whitespace()
                        .any(|part| part == marker)
            });
        if owned {
            #[cfg(unix)]
            // SAFETY: only a verified watcher leader created by this module
            // names the private process group; stop its transfer children too.
            unsafe {
                libc::kill(-(*pid as i32), libc::SIGTERM);
            }
        }
    }
}

/// Build the continuous multi-app tar re-sync shell script (testable pure builder).
///
/// Pod names are **not** frozen at attach time. Each loop resolves the current
/// Running pod for `hops.ops.com.ai/local-app=<app>` so rollouts (new emptyDir)
/// still receive source. Syncs when:
/// - host tree fingerprint changes, or
/// - the live pod is missing `/.hops-synced` (fresh pod / failed prior sync).
pub fn build_multi_app_tar_watch_script(targets: &[SyncPodTarget]) -> String {
    let mut script = r#"set +e
set -o pipefail
export COPYFILE_DISABLE=1
KCTX="${HOPS_KUBE_CONTEXT:-}"
k() {
  if [ -n "$KCTX" ]; then command kubectl --context "$KCTX" "$@"; else command kubectl "$@"; fi
}
pods_for() {
  k get pod -n "$1" -l "hops.ops.com.ai/local-app=$2" \
    --field-selector=status.phase=Running \
    -o jsonpath='{range .items[*]}{.metadata.name}{" "}{end}' 2>/dev/null
}
is_sync_pod() {
  # A rollout may still have old hostPath pods. Never extract an archive into
  # those: that would write back into the developer's source directory.
  value=$(k get pod -n "$1" "$2" -o "jsonpath={.spec.volumes[?(@.name==\"$3\")].emptyDir}" 2>/dev/null)
  [ -n "$value" ]
}
sync_one() {
  app="$1"; host="$2"; ns="$3"; mount="$4"; cont="$5"; volume="$6"
  pods=$(pods_for "$ns" "$app")
  [ -n "$pods" ] || return 1
  failed=0
  for pod in $pods; do
    is_sync_pod "$ns" "$pod" "$volume" || continue
    echo "sync $app -> $ns/$pod" >&2
    if tar cf - __EXCLUDES__ -C "$host" . 2>/dev/null \
      | k exec -i -n "$ns" "$pod" -c "$cont" -- tar xf - -C "$mount" 2>/dev/null
    then
      k exec -n "$ns" "$pod" -c "$cont" -- touch "$mount/.hops-synced" 2>/dev/null || failed=1
    else
      echo "sync FAILED $app -> $ns/$pod" >&2
      failed=1
    fi
  done
  return "$failed"
}
needs_one() {
  ns="$1"; app="$2"; mount="$3"; cont="$4"; volume="$5"
  pods=$(pods_for "$ns" "$app")
  [ -n "$pods" ] || return 0
  for pod in $pods; do
    is_sync_pod "$ns" "$pod" "$volume" || continue
    k exec -n "$ns" "$pod" -c "$cont" -- test -f "$mount/.hops-synced" 2>/dev/null || return 0
  done
  return 1
}
"#.replace("__EXCLUDES__", &tar_exclude_args().join(" "));
    let mut unique = std::collections::BTreeSet::new();
    let mut selected = Vec::new();
    for target in targets {
        let key = (
            &target.app_name,
            &target.namespace,
            &target.mount_path,
            &target.container,
            &target.source_volume,
        );
        if unique.insert(key) {
            selected.push(target);
        }
    }
    script.push_str("sync_all() {\n  all_failed=0\n");
    for target in &selected {
        let args = [
            &target.app_name,
            &target.host_source_path.display().to_string(),
            &target.namespace,
            &target.mount_path,
            target.container.as_deref().unwrap_or(""),
            &target.source_volume,
        ]
        .into_iter()
        .map(shell_quote)
        .collect::<Vec<_>>()
        .join(" ");
        script.push_str(&format!("  sync_one {args} || all_failed=1\n"));
    }
    script.push_str("  return \"$all_failed\"\n}\nneeds_marker() {\n");
    for target in &selected {
        let args = [
            &*target.namespace,
            &*target.app_name,
            &*target.mount_path,
            target.container.as_deref().unwrap_or(""),
            &*target.source_volume,
        ]
        .into_iter()
        .map(shell_quote)
        .collect::<Vec<_>>()
        .join(" ");
        script.push_str(&format!("  needs_one {args} && return 0\n"));
    }
    script.push_str("  return 1\n}\n");
    let roots = targets
        .iter()
        .map(|target| &target.host_source_path)
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .map(|root| shell_quote(&root.display().to_string()))
        .collect::<Vec<_>>()
        .join(" ");
    script.push_str(&format!(
        r#"
prev=""
while true; do
  cur=$(find {roots} -type f \
    ! -path '*/node_modules/*' ! -path '*/target/*' ! -path '*/.git/*' \
    ! -path '*/dist/*' ! -path '*/.svelte-kit/*' \
    -print0 2>/dev/null | xargs -0 sh -c '
      for file do
        stat -c "%Y" "$file" 2>/dev/null || stat -f "%m" "$file" 2>/dev/null
      done
    ' sh | cksum | awk '{{print $1}}')
  if [ "$cur" != "$prev" ] || needs_marker; then
    if sync_all; then prev="$cur"; fi
  fi
  sleep 2
done
"#
    ));
    script
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn spawn_tar_sync_watcher(
    targets: Vec<SyncPodTarget>,
    workspace: &str,
) -> Result<u32, Box<dyn Error>> {
    let script = build_multi_app_tar_watch_script(&targets);
    // Log path: ~/.hops/local/runtime/delivery-watch.log (shared; PIDs are per-workspace).
    let log_path = crate::commands::local::local_state_dir().ok().map(|d| {
        let p = d.join("runtime").join("delivery-watch.log");
        let _ = std::fs::create_dir_all(p.parent().unwrap_or(d.as_path()));
        p
    });
    let (stdout, stderr) = if let Some(ref path) = log_path {
        let f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        let f2 = f.try_clone()?;
        (Stdio::from(f), Stdio::from(f2))
    } else {
        (Stdio::null(), Stdio::null())
    };
    let mut child = Command::new("bash");
    child
        .arg("-c")
        .arg(script)
        .arg(format!("hops-source-sync:{workspace}"))
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(stderr);
    // Pass through hops' selected context so the watcher targets the same cluster
    // as `hops local gitops environment` (see HOPS_KUBE_CONTEXT_ENV).
    if let Ok(ctx) = std::env::var(crate::commands::local::HOPS_KUBE_CONTEXT_ENV) {
        if !ctx.is_empty() {
            child.env(crate::commands::local::HOPS_KUBE_CONTEXT_ENV, ctx);
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        child.process_group(0);
    }
    let child = child.spawn()?;
    if let Some(path) = log_path {
        log::info!("tar sync watcher log: {}", path.display());
    }
    Ok(child.id())
}

/// Whether a path change under host_path should trigger re-sync (not chart re-apply).
/// Uses the same ignore rules as tar/mutagen delivery.
pub fn should_resync_on_path_change(changed: &Path) -> bool {
    !path_is_sync_excluded(changed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn engine_probe_deadline_terminates_a_stalled_client() {
        let started = Instant::now();
        assert!(bounded_probe_command(Command::new("sleep").arg("30"), Duration::ZERO).is_err());
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn unrelated_failed_mount_is_not_a_missing_source_directory() {
        assert!(!source_path_is_missing(
            "FailedMount: service account token unavailable",
            "/source"
        ));
        assert!(!source_path_is_missing(
            "hostPath type check failed: /other is not a directory",
            "/source"
        ));
        assert!(source_path_is_missing(
            "hostPath type check failed: /source is not a directory",
            "/source"
        ));
    }

    #[test]
    fn generated_watcher_is_valid_bash_with_quoted_source_paths() {
        let mut target = target("api");
        target.host_source_path = PathBuf::from("/source/quote' and $(no-command)");
        let script = build_multi_app_tar_watch_script(&[target]);
        let mut bash = Command::new("bash")
            .arg("-n")
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        bash.stdin
            .take()
            .unwrap()
            .write_all(script.as_bytes())
            .unwrap();
        assert!(bash.wait().unwrap().success());
    }

    #[cfg(unix)]
    #[test]
    fn stale_delivery_pid_cannot_terminate_an_unrelated_process() {
        let state = std::env::temp_dir().join(format!("hops-pid-owner-{}", uuid::Uuid::new_v4()));
        let mut unrelated = Command::new("sleep").arg("30").spawn().unwrap();
        save_delivery_runtime(&state, "preview", &[], &[unrelated.id()]).unwrap();
        stop_delivery_runtime(&state, "preview");
        let still_running = unrelated.try_wait().unwrap().is_none();
        let _ = unrelated.kill();
        let _ = unrelated.wait();
        let _ = std::fs::remove_dir_all(state);
        assert!(
            still_running,
            "delivery cleanup must verify process ownership before signaling a PID"
        );
    }

    #[cfg(unix)]
    #[test]
    fn delivery_cleanup_stops_a_real_watcher_with_narrow_columns() {
        use std::os::unix::fs::PermissionsExt;

        const CHILD_ENV: &str = "HOPS_TEST_NARROW_WATCHER_CHILD";
        let Ok(root) = std::env::var(CHILD_ENV) else {
            // Isolate COLUMNS, HOME and PATH from parallel tests. The child runs
            // only this test and its kubectl cannot contact a real cluster.
            let root = std::env::temp_dir().join(format!("hops-narrow-{}", uuid::Uuid::new_v4()));
            let bin = root.join("bin");
            std::fs::create_dir_all(&bin).unwrap();
            std::fs::write(bin.join("kubectl"), "#!/bin/sh\nexit 0\n").unwrap();
            std::fs::set_permissions(bin.join("kubectl"), std::fs::Permissions::from_mode(0o755))
                .unwrap();
            let path = std::env::join_paths(
                std::iter::once(bin)
                    .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
            )
            .unwrap();
            let output = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    std::thread::current().name().unwrap(),
                    "--nocapture",
                ])
                .env(CHILD_ENV, &root)
                .env("HOME", &root)
                .env("PATH", path)
                .env("COLUMNS", "40")
                .output()
                .unwrap();
            let _ = std::fs::remove_dir_all(root);
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        };

        struct Watcher(u32, bool);
        impl Drop for Watcher {
            fn drop(&mut self) {
                // Always reap our child, including if an assertion fails.
                if !self.1 {
                    unsafe {
                        libc::kill(-(self.0 as i32), libc::SIGTERM);
                        libc::waitpid(self.0 as i32, std::ptr::null_mut(), 0);
                    }
                }
            }
        }

        let root = PathBuf::from(root);
        let mut target = target("api");
        target.host_source_path = root.clone();
        let workspace = format!("narrow-{}", uuid::Uuid::new_v4());
        let marker = format!("hops-source-sync:{workspace}");
        let mut watcher = Watcher(
            spawn_tar_sync_watcher(vec![target], &workspace).unwrap(),
            false,
        );
        let command_line = |wide| {
            let mut ps = Command::new("ps");
            if wide {
                ps.arg("-ww");
            }
            let output = ps
                .args(["-p", &watcher.0.to_string(), "-o", "command="])
                .output()
                .unwrap();
            String::from_utf8_lossy(&output.stdout).into_owned()
        };
        // Wait for spawn's fork/exec transition before inspecting ownership.
        let deadline = Instant::now() + Duration::from_secs(2);
        while !command_line(true)
            .split_whitespace()
            .any(|part| part == marker)
        {
            assert!(Instant::now() < deadline, "watcher did not start");
            std::thread::sleep(Duration::from_millis(10));
        }
        #[cfg(target_os = "linux")]
        assert!(
            !command_line(false).contains(&marker),
            "fixture must reproduce Linux ps truncation"
        );

        save_delivery_runtime(&root, &workspace, &[], &[watcher.0]).unwrap();
        stop_delivery_runtime(&root, &workspace);
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let mut status = 0;
            let waited = unsafe { libc::waitpid(watcher.0 as i32, &mut status, libc::WNOHANG) };
            if waited == watcher.0 as i32 {
                watcher.1 = true;
                assert!(libc::WIFSIGNALED(status));
                assert_eq!(libc::WTERMSIG(status), libc::SIGTERM);
                break;
            }
            assert!(
                Instant::now() < deadline,
                "delivery cleanup left its watcher alive"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!root
            .join("runtime")
            .join(format!("{workspace}.delivery.json"))
            .exists());
    }

    #[test]
    fn pending_or_failed_probe_is_not_a_missing_path() {
        assert_eq!(probe_visibility("Running", ""), None);
        assert_eq!(probe_visibility("Failed", ""), None);
        assert_eq!(probe_visibility("Running", "HOPS_PATH_VISIBLE"), Some(true));
        assert_eq!(probe_visibility("Succeeded", ""), Some(true));
        assert_eq!(
            probe_visibility("Succeeded", "HOPS_PATH_MISSING"),
            Some(false)
        );
        assert!(finish_path_probe(Path::new("/source"), None).is_err());
        assert!(
            !finish_path_probe(
                Path::new("/source"),
                Some((false, "confirmed missing".into()))
            )
            .unwrap()
            .host_path_visible
        );
    }

    fn target(app: &str) -> SyncPodTarget {
        SyncPodTarget {
            namespace: "preview".into(),
            pod: format!("{app}-pod"),
            container: Some(app.into()),
            mount_path: "/workspace".into(),
            source_volume: "source".into(),
            app_name: app.into(),
            host_source_path: PathBuf::from(format!("/source/{app}")),
        }
    }

    #[test]
    fn initial_sync_must_complete_for_every_target() {
        assert!(initialize_sync(&[], |_| Ok(())).is_err());
        let mut copied = Vec::new();
        let result = initialize_sync(&[target("api"), target("ui")], |target| {
            copied.push(target.app_name.clone());
            if target.app_name == "api" {
                Err("copy failed".into())
            } else {
                Ok(())
            }
        });
        assert!(result.unwrap_err().to_string().contains("api-pod"));
        assert_eq!(copied, ["api", "ui"]);
    }

    #[test]
    fn discovers_only_declared_emptydir_source_pods() {
        let pod = |name: &str, app: &str, source: serde_json::Value| {
            serde_json::json!({
                "metadata": {"name": name, "labels": {"hops.ops.com.ai/local-app": app}},
                "status": {"phase": "Running"},
                "spec": {
                    "containers": [{"name": "app", "volumeMounts": [{"name":"source", "mountPath":"/workspace"}]}],
                    "volumes": [source]
                }
            })
        };
        let value = serde_json::json!({"items": [
            pod("new", "api", serde_json::json!({"name":"source", "emptyDir":{}})),
            pod("old", "api", serde_json::json!({"name":"source", "hostPath":{"path":"/source"}})),
            pod("unrelated", "other", serde_json::json!({"name":"source", "emptyDir":{}}))
        ]});
        let paths = std::collections::BTreeMap::from([("api".into(), PathBuf::from("/declared"))]);
        let targets = sync_targets_from_pods(&value, "preview", "/workspace", &paths).unwrap();
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].pod, "new");
        assert_eq!(targets[0].host_source_path, PathBuf::from("/declared"));
    }

    #[test]
    fn prefers_host_path_when_probe_passes() {
        let probe =
            probe_from_visibility(Path::new("/Users/dev/proj"), true, "path exists on node");
        assert_eq!(select_delivery_strategy(&probe), DeliveryStrategy::HostPath);
    }

    #[test]
    fn falls_back_to_sync_when_probe_fails() {
        let probe = probe_from_visibility(Path::new("/Users/dev/proj"), false, "not on node");
        assert_eq!(select_delivery_strategy(&probe), DeliveryStrategy::Sync);
    }

    #[test]
    fn sync_ignores_build_artifacts() {
        assert!(path_is_sync_excluded(Path::new("ui/node_modules/x")));
        assert!(path_is_sync_excluded(Path::new("api/target/debug")));
        assert!(path_is_sync_excluded(Path::new(".git/config")));
        assert!(!path_is_sync_excluded(Path::new(
            "ui/src/routes/+page.svelte"
        )));
    }

    #[test]
    fn mutagen_and_tar_args_include_default_ignores() {
        let m = mutagen_ignore_args();
        assert!(m
            .windows(2)
            .any(|w| w[0] == "--ignore" && w[1] == "node_modules"));
        assert!(m.windows(2).any(|w| w[0] == "--ignore" && w[1] == "target"));
        let t = tar_exclude_args();
        assert!(t.iter().any(|a| a == "--exclude=node_modules"));
        assert!(t.iter().any(|a| a == "--exclude=target"));
        assert!(t.iter().any(|a| a == "--exclude=.git"));
    }

    #[test]
    fn build_mutagen_create_args_wires_ignores_and_paths() {
        let args = build_mutagen_create_args(
            "hops-lwb-alice-ui",
            Path::new("/proj"),
            "kubectl://ns/pod/workspace",
        );
        assert_eq!(args[0], "sync");
        assert_eq!(args[1], "create");
        assert!(args.contains(&"--name".into()));
        assert!(args.contains(&"hops-lwb-alice-ui".into()));
        assert!(args.contains(&"/proj".into()));
        assert!(args.contains(&"kubectl://ns/pod/workspace".into()));
        assert!(args
            .windows(2)
            .any(|w| w[0] == "--ignore" && w[1] == "node_modules"));
    }

    #[test]
    fn build_tar_cf_args_is_what_sync_directory_to_pod_runs() {
        // Drives the real helper used by sync_directory_to_pod — not a parallel reimplementation.
        let args = build_tar_cf_args(Path::new("/Users/me/proj"));
        assert_eq!(args[0], "cf");
        assert_eq!(args[1], "-");
        assert!(args.iter().any(|a| a == "--exclude=node_modules"));
        assert!(args.iter().any(|a| a == "--exclude=target"));
        assert!(args.iter().any(|a| a == "--exclude=.git"));
        let c_idx = args.iter().position(|a| a == "-C").unwrap();
        assert_eq!(args[c_idx + 1], "/Users/me/proj");
        assert_eq!(args.last().map(String::as_str), Some("."));
    }

    #[test]
    fn build_kubectl_tar_extract_args_targets_pod_mount() {
        let t = SyncPodTarget {
            namespace: "alice".into(),
            pod: "e2e-ui-ui-xyz".into(),
            container: Some("ui".into()),
            mount_path: "/workspace".into(),
            source_volume: "source".into(),
            app_name: "e2e-ui-ui".into(),
            host_source_path: PathBuf::from("/proj/ui"),
        };
        let args = build_kubectl_tar_extract_args(&t);
        assert!(args.contains(&"exec".into()));
        assert!(args.contains(&"alice".into()));
        assert!(args.contains(&"e2e-ui-ui-xyz".into()));
        assert!(args.contains(&"-c".into()));
        assert!(args.contains(&"ui".into()));
        assert!(args.contains(&"/workspace".into()));
    }

    #[test]
    fn multi_app_watch_script_resyncs_every_target_with_own_host() {
        let targets = vec![
            SyncPodTarget {
                namespace: "ns".into(),
                pod: "api-pod".into(), // frozen name must NOT be used by watcher
                container: Some("api".into()),
                mount_path: "/workspace".into(),
                source_volume: "source".into(),
                app_name: "e2e-ui-api".into(),
                host_source_path: PathBuf::from("/proj"),
            },
            SyncPodTarget {
                namespace: "ns".into(),
                pod: "ui-pod".into(),
                container: Some("ui".into()),
                mount_path: "/workspace".into(),
                source_volume: "source".into(),
                app_name: "e2e-ui-ui".into(),
                host_source_path: PathBuf::from("/proj/ui"),
            },
        ];
        let script = build_multi_app_tar_watch_script(&targets);
        assert!(script.contains("sync_all()"));
        assert!(script.contains("sync_one()"));
        assert!(script.contains("needs_marker()"));
        assert!(script.contains("HOPS_KUBE_CONTEXT") || script.contains("KCTX"));
        // Resolve by label — not frozen pod names from attach time
        assert!(
            script.contains("hops.ops.com.ai/local-app=$app")
                || script.contains("hops.ops.com.ai/local-app=")
        );
        assert!(script.contains("e2e-ui-api"));
        assert!(script.contains("e2e-ui-ui"));
        assert!(script.contains("/proj"));
        assert!(script.contains("/proj/ui"));
        // Frozen pod names from the attach snapshot must not be baked into the loop
        assert!(
            !script.contains("\"api-pod\"") && !script.contains("\"ui-pod\""),
            "watcher must re-resolve pods by label, not freeze attach-time names"
        );
        assert!(
            script.contains("sync_all\n")
                || script.contains("sync_all\r")
                || script.contains("sync_all")
        );
        assert!(script.contains(".hops-synced"));
    }

    #[test]
    fn should_resync_source_but_not_ignored_build_dirs() {
        assert!(should_resync_on_path_change(Path::new(
            "/proj/ui/src/App.svelte"
        )));
        assert!(!should_resync_on_path_change(Path::new(
            "/proj/ui/node_modules/x"
        )));
        assert!(!should_resync_on_path_change(Path::new(
            "/proj/api/target/debug/foo"
        )));
    }

    #[test]
    fn strategy_string_stable_for_registry() {
        assert_eq!(DeliveryStrategy::HostPath.as_str(), "hostPath");
        assert_eq!(DeliveryStrategy::Sync.as_str(), "sync");
    }

    #[test]
    fn fake_prober_selects_sync_when_node_cannot_see_mac_path() {
        struct AlwaysHidden;
        impl NodePathProber for AlwaysHidden {
            fn probe(&self, host_path: &Path) -> Result<DeliveryProbe, Box<dyn Error>> {
                Ok(probe_from_visibility(
                    host_path,
                    false,
                    "simulated: Mac path not on Linux node",
                ))
            }
        }
        let probe = AlwaysHidden.probe(Path::new("/Users/me/proj")).unwrap();
        assert!(!probe.host_path_visible);
        assert_eq!(select_delivery_strategy(&probe), DeliveryStrategy::Sync);
    }

    #[test]
    fn select_delivery_strategy_never_assumes_hostpath_for_is_dir_only() {
        // Document the contract: mere host is_dir is NOT enough — probe must set visible.
        let host_exists_but_node_blind = probe_from_visibility(
            Path::new("/Users/me/proj"),
            false,
            "is_dir alone is not enough",
        );
        assert_eq!(
            select_delivery_strategy(&host_exists_but_node_blind),
            DeliveryStrategy::Sync
        );
    }

    #[test]
    fn real_tar_invocation_excludes_node_modules_using_shipped_args() {
        // Drive the same argv builder sync_directory_to_pod uses, then run real tar.
        let dir = std::env::temp_dir().join(format!(
            "lwb-tar-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join("node_modules/pkg")).unwrap();
        std::fs::write(dir.join("src/app.js"), "console.log(1)\n").unwrap();
        std::fs::write(dir.join("node_modules/pkg/index.js"), "secret\n").unwrap();

        let args = build_tar_cf_args(&dir);
        let archive = dir.join("out.tar");
        // Replace stdout "-" with a file for inspection
        let mut file_args: Vec<String> = args
            .into_iter()
            .map(|a| {
                if a == "-" {
                    archive.display().to_string()
                } else {
                    a
                }
            })
            .collect();
        // tar cf <file> ...
        assert_eq!(file_args[0], "cf");
        let status = Command::new("tar").args(&file_args).status().unwrap();
        assert!(status.success(), "tar failed with shipped exclude args");

        let list = Command::new("tar")
            .args(["tf", &archive.display().to_string()])
            .output()
            .unwrap();
        assert!(list.status.success());
        let listing = String::from_utf8_lossy(&list.stdout);
        assert!(
            listing.contains("src/app.js") || listing.contains("./src/app.js"),
            "expected source file in archive, got:\n{listing}"
        );
        assert!(
            !listing.contains("node_modules"),
            "node_modules must be excluded by shipped tar args, got:\n{listing}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
