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
    default_sync_ignores()
        .into_iter()
        .map(|ig| format!("--exclude={ig}"))
        .collect()
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
        return Ok(probe_from_visibility(
            host_path,
            false,
            "host path is not a directory",
        ));
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
    match try_kubectl_hostpath_probe(&abs) {
        Ok(probe) => return Ok(probe),
        Err(e) => log::debug!("kubectl hostPath probe failed: {e}"),
    }

    // Fail closed → Sync (do not assume hostPath for Mac paths on remote nodes)
    Ok(probe_from_visibility(
        &abs,
        false,
        "node path not visible (docker+kubectl probes failed or inconclusive); selecting sync",
    ))
}

fn try_docker_node_probe(host_path: &Path) -> Result<Option<DeliveryProbe>, Box<dyn Error>> {
    if !command_exists("docker") || !command_exists("kubectl") {
        return Ok(None);
    }
    let nodes_json = Command::new("kubectl")
        .args(["get", "nodes", "-o", "json"])
        .output()?;
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
        let status = Command::new("docker")
            .args(["exec", &container, "test", "-d", &path_str])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        match status {
            Ok(s) if s.success() => {
                return Ok(Some(probe_from_visibility(
                    host_path,
                    true,
                    format!("docker exec {container}: path visible on node"),
                )));
            }
            Ok(_) => {
                // Container exists but path missing → definitive not visible if we hit a real node
                if docker_container_running(&container) {
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

fn docker_container_running(name: &str) -> bool {
    Command::new("docker")
        .args([
            "inspect",
            "-f",
            "{{.State.Running}}",
            name,
        ])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim() == "true")
        .unwrap_or(false)
}

fn try_kubectl_hostpath_probe(host_path: &Path) -> Result<DeliveryProbe, Box<dyn Error>> {
    let name = format!(
        "hops-path-probe-{}",
        std::process::id() % 100_000
    );
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
    let mut child = Command::new("kubectl")
        .args(["apply", "-f", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(yaml.as_bytes())?;
    }
    let apply_out = child.wait_with_output()?;
    if !apply_out.status.success() {
        let _ = Command::new("kubectl")
            .args(["delete", "pod", &name, "-n", "default", "--wait=false"])
            .output();
        return Err(format!(
            "probe pod apply failed: {}",
            String::from_utf8_lossy(&apply_out.stderr)
        )
        .into());
    }

    // Wait up to ~15s for Succeeded/Failed
    let mut visible = false;
    let mut detail = String::from("kubectl hostPath probe: timeout");
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(400));
        let phase_out = Command::new("kubectl")
            .args([
                "get",
                "pod",
                &name,
                "-n",
                "default",
                "-o",
                "jsonpath={.status.phase}",
            ])
            .output()?;
        let phase = String::from_utf8_lossy(&phase_out.stdout).trim().to_string();

        // FailedMount appears in events / container statuses
        let desc = Command::new("kubectl")
            .args(["describe", "pod", &name, "-n", "default"])
            .output()?;
        let desc_s = String::from_utf8_lossy(&desc.stdout);
        if desc_s.contains("FailedMount")
            || desc_s.contains("failed to mount")
            || desc_s.contains("hostPath type check failed")
            || desc_s.contains("not a directory")
        {
            visible = false;
            detail = "kubectl hostPath probe: FailedMount (path not on node)".into();
            break;
        }

        if phase == "Succeeded" || phase == "Failed" || phase == "Running" {
            let logs = Command::new("kubectl")
                .args(["logs", &name, "-n", "default", "--tail=20"])
                .output()?;
            let log_s = String::from_utf8_lossy(&logs.stdout);
            if log_s.contains("HOPS_PATH_VISIBLE") {
                visible = true;
                detail = "kubectl hostPath probe: path visible on node".into();
            } else if log_s.contains("HOPS_PATH_MISSING") {
                visible = false;
                detail = "kubectl hostPath probe: mount empty/missing".into();
            } else if phase == "Succeeded" {
                // Treat success without marker as visible (container started = mount ok)
                visible = true;
                detail = format!("kubectl hostPath probe: phase={phase}");
            } else {
                visible = false;
                detail = format!("kubectl hostPath probe: phase={phase} logs={log_s}");
            }
            break;
        }
    }

    let _ = Command::new("kubectl")
        .args([
            "delete",
            "pod",
            &name,
            "-n",
            "default",
            "--wait=false",
            "--ignore-not-found=true",
        ])
        .output();

    Ok(probe_from_visibility(host_path, visible, detail))
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
    for attempt in 1..=8 {
        match sync_directory_to_pod_once(host_path, target) {
            Ok(()) => return Ok(()),
            Err(e) => {
                last_err = e.to_string();
                log::debug!("sync attempt {attempt} failed: {last_err}");
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
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed to spawn tar: {e}"))?;

    let kubectl_args = build_kubectl_tar_extract_args(target);

    let mut kubectl = Command::new("kubectl")
        .args(&kubectl_args)
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
        "exec".into(),
        "-n".into(),
        target.namespace.clone(),
        target.pod.clone(),
    ];
    if let Some(c) = &target.container {
        args.push("-c".into());
        args.push(c.clone());
    }
    args.push("--".into());
    args.push("sh".into());
    args.push("-c".into());
    args.push(format!("touch {marker}"));
    let out = Command::new("kubectl").args(&args).output()?;
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
    let json = Command::new("kubectl")
        .args([
            "get",
            "pods",
            "-n",
            namespace,
            "-l",
            &format!("hops.ops.com.ai/local-env={workspace}"),
            "-o",
            "json",
        ])
        .output()?;
    if !json.status.success() {
        return Err(format!(
            "kubectl get pods failed: {}",
            String::from_utf8_lossy(&json.stderr)
        )
        .into());
    }
    let value: serde_json::Value = serde_json::from_slice(&json.stdout)?;
    let mut out = Vec::new();
    if let Some(items) = value.get("items").and_then(|i| i.as_array()) {
        for item in items {
            let phase = item
                .pointer("/status/phase")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            // Only Running pods accept kubectl exec for tar extract.
            if phase != "Running" {
                continue;
            }
            let pod = item
                .pointer("/metadata/name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if pod.is_empty() {
                continue;
            }
            let app = item
                .pointer("/metadata/labels/hops.ops.com.ai~1local-app")
                .and_then(|v| v.as_str())
                .or_else(|| {
                    item.pointer("/metadata/labels/app.kubernetes.io~1name")
                        .and_then(|v| v.as_str())
                })
                .unwrap_or("app")
                .to_string();
            let container = item
                .pointer("/spec/containers/0/name")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let host_source_path = app_host_paths
                .get(&app)
                .cloned()
                .or_else(|| {
                    // Fuzzy: app name contains key
                    app_host_paths
                        .iter()
                        .find(|(k, _)| app.contains(k.as_str()) || k.contains(&app))
                        .map(|(_, p)| p.clone())
                })
                .unwrap_or_else(|| PathBuf::from("."));
            out.push(SyncPodTarget {
                namespace: namespace.to_string(),
                pod,
                container,
                mount_path: mount_path.to_string(),
                app_name: app,
                host_source_path,
            });
        }
    }
    Ok(out)
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

    if targets.is_empty() {
        result
            .messages
            .push("no Running pods to sync into yet; re-run up after pods are Ready".into());
        return Ok(result);
    }

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

        // Tar-based real sync (per-app host path + default_sync_ignores).
        // Per-target errors must not abort the other apps.
        match sync_directory_to_pod(host_path, target) {
            Ok(()) => result.messages.push(format!(
                "tar sync {} → {}/{}:{}",
                host_path.display(),
                target.namespace,
                target.pod,
                target.mount_path
            )),
            Err(e) => result.messages.push(format!(
                "tar sync FAILED for {} ← {}: {e}",
                target.pod,
                host_path.display()
            )),
        }
    }

    // Always keep a multi-app tar re-sync loop for Sync mode: emptyDir is wiped
    // on container restart, so a one-shot tar is not durable without continuous
    // delivery. `--watch` only controls gitops chart re-apply (caller).
    let _ = watch;
    if result.mutagen_sessions.is_empty() && !targets.is_empty() {
        // Immediate second pass after a short delay (covers race with first start).
        for target in targets {
            let _ = sync_directory_to_pod(&target.host_source_path, target);
        }
        match spawn_tar_sync_watcher(targets.to_vec()) {
            Ok(pid) => {
                result.sync_pids.push(pid);
                result
                    .messages
                    .push(format!("tar sync watcher pid={pid} (all apps, continuous)"));
            }
            Err(e) => result
                .messages
                .push(format!("tar sync watcher not started: {e}")),
        }
    }

    Ok(result)
}

/// Build the continuous multi-app tar re-sync shell script (testable pure builder).
///
/// Each target gets its **own** host path; every target is resynced on change
/// (not a single `sync_one` redefined in a loop).
pub fn build_multi_app_tar_watch_script(targets: &[SyncPodTarget]) -> String {
    let excludes = tar_exclude_args().join(" ");
    let mut script = String::from("set -e\n");
    script.push_str("sync_all() {\n");
    for t in targets {
        let cont = t
            .container
            .as_ref()
            .map(|c| format!("-c {c} "))
            .unwrap_or_default();
        let host = t.host_source_path.display();
        script.push_str(&format!(
            "  tar cf - {excl} -C \"{host}\" . | kubectl exec -i -n \"{ns}\" \"{pod}\" {cont}-- tar xf - -C \"{mount}\" 2>/dev/null || true\n",
            excl = excludes,
            host = host,
            ns = t.namespace,
            pod = t.pod,
            cont = cont,
            mount = t.mount_path,
        ));
    }
    script.push_str("}\n");

    // Fingerprint union of all host roots so any app source change triggers full resync.
    let mut find_parts = Vec::new();
    for t in targets {
        find_parts.push(format!("\"{}\"", t.host_source_path.display()));
    }
    let roots = find_parts.join(" ");
    script.push_str(&format!(
        r#"
prev=""
while true; do
  cur=$(find {roots} -type f \
    ! -path '*/node_modules/*' ! -path '*/target/*' ! -path '*/.git/*' \
    ! -path '*/dist/*' ! -path '*/.svelte-kit/*' \
    -print0 2>/dev/null | xargs -0 stat -f '%m' 2>/dev/null | cksum | awk '{{print $1}}')
  if [ "$cur" != "$prev" ]; then
    prev="$cur"
    sync_all
  fi
  sleep 2
done
"#,
        roots = roots
    ));
    script
}

fn spawn_tar_sync_watcher(targets: Vec<SyncPodTarget>) -> Result<u32, Box<dyn Error>> {
    let script = build_multi_app_tar_watch_script(&targets);
    let child = Command::new("sh")
        .arg("-c")
        .arg(script)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
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

    #[test]
    fn prefers_host_path_when_probe_passes() {
        let probe =
            probe_from_visibility(Path::new("/Users/dev/proj"), true, "path exists on node");
        assert_eq!(
            select_delivery_strategy(&probe),
            DeliveryStrategy::HostPath
        );
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
        assert!(m.windows(2).any(|w| w[0] == "--ignore" && w[1] == "node_modules"));
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
        assert!(args.windows(2).any(|w| w[0] == "--ignore" && w[1] == "node_modules"));
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
            namespace: "hops-wt-alice".into(),
            pod: "e2e-ui-ui-xyz".into(),
            container: Some("ui".into()),
            mount_path: "/workspace".into(),
            app_name: "e2e-ui-ui".into(),
            host_source_path: PathBuf::from("/proj/ui"),
        };
        let args = build_kubectl_tar_extract_args(&t);
        assert!(args.contains(&"exec".into()));
        assert!(args.contains(&"hops-wt-alice".into()));
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
                pod: "api-pod".into(),
                container: Some("api".into()),
                mount_path: "/workspace".into(),
                app_name: "e2e-ui-api".into(),
                host_source_path: PathBuf::from("/proj"),
            },
            SyncPodTarget {
                namespace: "ns".into(),
                pod: "ui-pod".into(),
                container: Some("ui".into()),
                mount_path: "/workspace".into(),
                app_name: "e2e-ui-ui".into(),
                host_source_path: PathBuf::from("/proj/ui"),
            },
        ];
        let script = build_multi_app_tar_watch_script(&targets);
        // One function that syncs ALL apps — not a redefined single-target sync_one
        assert!(script.contains("sync_all()"));
        assert!(!script.contains("sync_one()"));
        // Both host roots and both pods present
        assert!(script.contains("-C \"/proj\""));
        assert!(script.contains("-C \"/proj/ui\""));
        assert!(script.contains("api-pod"));
        assert!(script.contains("ui-pod"));
        // Call site invokes sync_all (not last-only)
        assert!(script.contains("sync_all\n") || script.contains("sync_all\r"));
        let api_lines = script.matches("api-pod").count();
        let ui_lines = script.matches("ui-pod").count();
        assert!(api_lines >= 1 && ui_lines >= 1);
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
        let host_exists_but_node_blind =
            probe_from_visibility(Path::new("/Users/me/proj"), false, "is_dir alone is not enough");
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
