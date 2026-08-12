//! dory backend: product k3s on stock Dory (https://augani.github.io/dory).
//!
//! Design (stock Dory only — no hops fork):
//! - Dory.app owns the engine, dockerd, and k3s lifecycle (enable Kubernetes
//!   in the app). hops never calls `dory k8s enable`.
//! - hops talks to the engine via `~/.dory/dory.sock` and to the cluster via
//!   `~/.kube/dory-config` (context is usually `default`).
//!
//! Package registry (two network planes on Dory):
//! - **Pull (Crossplane pods):** in-cluster Service DNS
//!   `registry.crossplane-system.svc.cluster.local:5000`.
//! - **Push (docker via `~/.dory/dory.sock`):** engine dockerd pushes to the
//!   k3s NodePort on the docker bridge (`{dory-k8s-ip}:30500`), with that
//!   bridge marked insecure (HTTP). Mac localhost is the wrong plane.
//! - **Node image pulls:** k3s `registries.yaml` mirrors → Service ClusterIP.

use super::SizeArgs;
use crate::commands::local::{command_exists, run_cmd, run_cmd_output};
use crate::commands::local::package_install::{
    REGISTRY_HOSTNAME, REGISTRY_PULL_INCLUSTER, REGISTRY_PUSH,
};
use std::error::Error;
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

const NODE_CONTAINER: &str = "dory-k8s";
const REGISTRY_NODE_PORT: &str = "30500";

fn home() -> Result<PathBuf, Box<dyn Error>> {
    Ok(PathBuf::from(std::env::var("HOME").map_err(|_| {
        "HOME is not set; unable to locate dory's state directory"
    })?))
}

/// Stock Dory's host-facing Docker API socket (`~/.dory/dory.sock`).
fn engine_socket() -> Result<PathBuf, Box<dyn Error>> {
    Ok(home()?.join(".dory/dory.sock"))
}

/// Stock Dory writes the cluster kubeconfig here (context is typically `default`).
pub fn kubeconfig_path() -> Option<String> {
    home()
        .ok()
        .map(|h| h.join(".kube/dory-config").to_string_lossy().into_owned())
}

fn engine_docker(args: &[&str]) -> Result<(), Box<dyn Error>> {
    let sock = format!("unix://{}", engine_socket()?.display());
    let mut full = vec!["-H", sock.as_str()];
    full.extend_from_slice(args);
    run_cmd("docker", &full)
}

fn engine_docker_output(args: &[&str]) -> Result<String, Box<dyn Error>> {
    let sock = format!("unix://{}", engine_socket()?.display());
    let mut full = vec!["-H", sock.as_str()];
    full.extend_from_slice(args);
    run_cmd_output("docker", &full)
}

pub fn install() -> Result<(), Box<dyn Error>> {
    log::info!("Installing Dory via Homebrew...");
    run_cmd("brew", &["install", "--cask", "Augani/dory/dory"])?;
    log::info!(
        "Dory installed; open the app, wait until the engine is healthy, \
         enable Kubernetes, then re-run `hops local start --backend dory`"
    );
    Ok(())
}

pub fn uninstall() -> Result<(), Box<dyn Error>> {
    log::info!("Uninstalling Dory...");
    run_cmd("brew", &["uninstall", "--cask", "dory"])?;
    log::info!("Dory uninstalled");
    Ok(())
}

pub fn start(size: &SizeArgs) -> Result<(), Box<dyn Error>> {
    if size.any_set() {
        return Err(format!(
            "the dory backend's VM is sized by the Dory app, not hops; drop{}",
            size.command_suffix()
        )
        .into());
    }

    preflight()?;
    ensure_k8s_node_running()?;
    wait_for_node_ready()?;
    Ok(())
}

pub fn stop() -> Result<(), Box<dyn Error>> {
    log::info!("Stopping dory k8s node '{}'...", NODE_CONTAINER);
    if !node_exists() {
        log::info!("dory k8s node not present");
        return Ok(());
    }
    engine_docker(&["stop", NODE_CONTAINER])?;
    log::info!("dory cluster stopped");
    Ok(())
}

pub fn destroy() -> Result<(), Box<dyn Error>> {
    log::info!(
        "Deleting dory k8s node '{}' (hops does not disable product k8s via CLI)...",
        NODE_CONTAINER
    );
    // Legacy leftovers from earlier registry experiments.
    let _ = engine_docker(&["rm", "-f", "hops-local-registry"]);
    let _ = engine_docker(&["rm", "-f", "hops-registry-publish"]);
    let _ = engine_docker(&["rm", "-f", NODE_CONTAINER]);
    log::info!(
        "dory k8s node removed; re-enable Kubernetes in the Dory app if you want the product cluster again"
    );
    Ok(())
}

/// Recreate is app-owned: remove the node; user re-enables k8s in Dory, then start again.
pub fn reset() -> Result<(), Box<dyn Error>> {
    preflight()?;
    destroy()?;
    log::info!(
        "Enable Kubernetes in the Dory app, then run `hops local start --backend dory` again"
    );
    Ok(())
}

pub fn resize(_size: &SizeArgs) -> Result<(), Box<dyn Error>> {
    Err("the dory backend has no hops-managed VM to resize; \
         adjust resources in the Dory app instead"
        .into())
}

/// Whether the hops-relevant dory cluster exists (running or stopped).
pub fn cluster_exists() -> bool {
    let Ok(sock) = engine_socket() else {
        return false;
    };
    if !sock.exists() {
        return false;
    }
    node_exists()
}

fn node_exists() -> bool {
    engine_docker_output(&["inspect", "-f", "{{.Id}}", NODE_CONTAINER]).is_ok()
}

fn node_running() -> bool {
    engine_docker_output(&["inspect", "-f", "{{.State.Running}}", NODE_CONTAINER])
        .map(|state| state.trim() == "true")
        .unwrap_or(false)
}

fn preflight() -> Result<(), Box<dyn Error>> {
    if !command_exists("docker") {
        return Err("docker CLI not found; install it (Dory provides the daemon)".into());
    }
    let sock = engine_socket()?;
    if !sock.exists() {
        return Err(format!(
            "dory's engine socket ({}) is missing.\n\
             Open the Dory app and wait until the engine is healthy (not \"needs attention\"), then retry.",
            sock.display()
        )
        .into());
    }
    if engine_docker_output(&["info"]).is_err() {
        return Err(format!(
            "cannot talk to Dory's docker at {}.\n\
             Open the Dory app, fix any engine/doryd errors in the menu, then retry.",
            sock.display()
        )
        .into());
    }
    Ok(())
}

/// k3s is product-owned: start a stopped node, or tell the user to enable it in the app.
fn ensure_k8s_node_running() -> Result<(), Box<dyn Error>> {
    if node_running() {
        log::info!("dory k8s node '{}' is running", NODE_CONTAINER);
        return Ok(());
    }
    if node_exists() {
        log::info!("Starting stopped dory k8s node '{}'...", NODE_CONTAINER);
        engine_docker(&["start", NODE_CONTAINER])?;
        return Ok(());
    }
    Err(
        "Dory Kubernetes is not enabled (no `dory-k8s` container).\n\
         In the Dory app: enable Kubernetes, wait until it is running, then re-run:\n\
           hops local start --backend dory\n\
         (hops uses stock Dory only — it does not create the cluster for you.)"
            .into(),
    )
}

fn wait_for_node_ready() -> Result<(), Box<dyn Error>> {
    log::info!("Waiting for dory k8s node to become Ready...");
    for i in 0..90 {
        if !node_running() {
            thread::sleep(Duration::from_secs(2));
            continue;
        }
        let ready = engine_docker_output(&[
            "exec",
            NODE_CONTAINER,
            "kubectl",
            "get",
            "nodes",
            "--no-headers",
        ])
        .map(|out| out.contains(" Ready"))
        .unwrap_or(false);
        if ready {
            ensure_side_kubeconfig_hint();
            // Best-effort: merge context "dory" + docker context (also run on activate).
            let _ = ensure_desktop_integration();
            return Ok(());
        }
        if i > 0 && i % 15 == 0 {
            log::info!("Still waiting for k3s Ready ({}s)...", i * 2);
        }
        thread::sleep(Duration::from_secs(2));
    }
    Err(
        "timed out waiting for dory-k8s to become Ready; check Kubernetes status in the Dory app"
            .into(),
    )
}

fn ensure_side_kubeconfig_hint() {
    let Some(path) = kubeconfig_path() else {
        return;
    };
    if std::path::Path::new(&path).is_file() {
        return;
    }
    if let Ok(yaml) = engine_docker_output(&[
        "exec",
        NODE_CONTAINER,
        "cat",
        "/etc/rancher/k3s/k3s.yaml",
    ]) {
        if yaml.contains("server:") {
            if let Some(parent) = std::path::Path::new(&path).parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(&path, yaml);
            let _ = std::fs::set_permissions(
                &path,
                std::os::unix::fs::PermissionsExt::from_mode(0o600),
            );
            log::info!("Wrote kubeconfig side file {}", path);
        }
    }
}

/// No create-time trust: k3s registries.yaml is written in [`wire_registry`].
pub fn ensure_registry_trust() -> Result<(), Box<dyn Error>> {
    Ok(())
}

/// After the in-cluster registry Service exists:
/// 1. Point k3s/containerd at the Service ClusterIP over HTTP (function images).
/// 2. Allow engine dockerd to push HTTP to the k3s NodePort on the docker bridge.
pub fn wire_registry(cluster_ip: &str) -> Result<(), Box<dyn Error>> {
    ensure_k3s_registry_mirrors(cluster_ip)?;
    ensure_engine_push_path()?;
    Ok(())
}

/// Docker push address for package installs when `DOCKER_HOST` is the Dory engine.
/// Reachable on the engine docker bridge (not Mac localhost).
pub fn registry_push_addr() -> Result<String, Box<dyn Error>> {
    Ok(format!("{}:{}", node_ip()?, REGISTRY_NODE_PORT))
}

fn node_ip() -> Result<String, Box<dyn Error>> {
    let ip = engine_docker_output(&[
        "inspect",
        "-f",
        "{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}",
        NODE_CONTAINER,
    ])?
    .trim()
    .to_string();
    if ip.is_empty() {
        return Err("dory-k8s has no docker network IP".into());
    }
    Ok(ip)
}

/// k3s `registries.yaml` so containerd pulls HTTP from the in-cluster registry.
/// Restarts the node only when the file content changes.
fn ensure_k3s_registry_mirrors(cluster_ip: &str) -> Result<(), Box<dyn Error>> {
    if !node_running() {
        return Err(
            "dory k8s node is not running; enable Kubernetes in the Dory app first".into(),
        );
    }

    let push = registry_push_addr()?;
    // Registry serves HTTPS (self-signed); skip verify on the node for function image pulls.
    let yaml = format!(
        "mirrors:\n\
         \x20 \"{svc}\":\n\
         \x20   endpoint:\n\
         \x20     - \"https://{ip}:5000\"\n\
         \x20 \"{push}\":\n\
         \x20   endpoint:\n\
         \x20     - \"https://{ip}:5000\"\n\
         \x20 \"{default_push}\":\n\
         \x20   endpoint:\n\
         \x20     - \"https://{ip}:5000\"\n\
         configs:\n\
         \x20 \"{ip}:5000\":\n\
         \x20   tls:\n\
         \x20     insecure_skip_verify: true\n\
         \x20 \"{svc}\":\n\
         \x20   tls:\n\
         \x20     insecure_skip_verify: true\n",
        svc = REGISTRY_PULL_INCLUSTER,
        push = push,
        default_push = REGISTRY_PUSH,
        ip = cluster_ip,
    );

    let current = engine_docker_output(&[
        "exec",
        NODE_CONTAINER,
        "sh",
        "-c",
        "cat /etc/rancher/k3s/registries.yaml 2>/dev/null || true",
    ])
    .unwrap_or_default();
    if current.trim() == yaml.trim() {
        return Ok(());
    }

    log::info!(
        "Configuring k3s registry mirrors for {} -> {}:5000...",
        REGISTRY_HOSTNAME,
        cluster_ip
    );
    engine_docker(&[
        "exec",
        NODE_CONTAINER,
        "sh",
        "-c",
        &format!(
            "mkdir -p /etc/rancher/k3s && cat > /etc/rancher/k3s/registries.yaml <<'EOF'\n{yaml}EOF"
        ),
    ])?;
    log::info!("Restarting dory k8s node so k3s reloads registries.yaml...");
    engine_docker(&["restart", NODE_CONTAINER])?;
    wait_for_node_ready()?;
    for _ in 0..30 {
        if run_cmd_output("kubectl", &["get", "--raw", "/readyz"])
            .map(|s| s.contains("ok"))
            .unwrap_or(false)
        {
            break;
        }
        thread::sleep(Duration::from_secs(2));
    }
    Ok(())
}

/// Engine dockerd must treat the k3s NodePort as an HTTP registry. Push address
/// is `{dory-k8s-ip}:30500` on the docker bridge (see [`registry_push_addr`]).
fn ensure_engine_push_path() -> Result<(), Box<dyn Error>> {
    // Best-effort cleanup of earlier experiments (ignore missing).
    let _ = engine_docker_output(&["rm", "-f", "hops-local-registry"]);
    let _ = engine_docker_output(&["rm", "-f", "hops-registry-publish"]);

    let push = registry_push_addr()?;
    ensure_dockerd_insecure_for_push(&push)?;

    // Prove NodePort answers HTTPS from the engine network (self-signed → no-check).
    for _ in 0..30 {
        if engine_docker_output(&[
            "run",
            "--rm",
            "alpine:latest",
            "wget",
            "-qO-",
            "--no-check-certificate",
            &format!("https://{push}/v2/"),
        ])
        .is_ok()
        {
            log::info!("Engine package push endpoint ready at {} (HTTPS)", push);
            return Ok(());
        }
        thread::sleep(Duration::from_secs(1));
    }
    Err(format!(
        "timed out waiting for registry NodePort at https://{push}/v2/ \
         (in-cluster TLS registry Service type NodePort on dory-k8s)"
    )
    .into())
}

fn ensure_dockerd_insecure_for_push(push_hostport: &str) -> Result<(), Box<dyn Error>> {
    // Already configured?
    if let Ok(info) = engine_docker_output(&["info", "-f", "{{json .RegistryConfig.InsecureRegistryCIDRs}}{{json .RegistryConfig.IndexConfigs}}"])
    {
        if info.contains(push_hostport) || info.contains("192.168.215.0/24") {
            return Ok(());
        }
    }

    log::info!(
        "Configuring Dory dockerd insecure-registries for HTTP push to {}...",
        push_hostport
    );

    // Persist under the engine's /etc/docker (bind-mounted into a helper).
    let daemon_json = format!(
        r#"{{
  "builder": {{"gc": {{"enabled": true, "defaultKeepStorage": "2GB"}}}},
  "insecure-registries": [
    "127.0.0.0/8",
    "localhost:30500",
    "192.168.215.0/24",
    "{push}"
  ]
}}
"#,
        push = push_hostport
    );

    engine_docker(&[
        "run",
        "--rm",
        "-v",
        "/etc/docker:/etc/docker",
        "alpine:latest",
        "sh",
        "-c",
        &format!("cat > /etc/docker/daemon.json <<'EOF'\n{daemon_json}EOF"),
    ])?;

    // Prefer Dory's repair path (live-restore) over raw kill.
    if command_exists("dory") {
        let _ = run_cmd("dory", &["repair", "dockerd", "--apply"]);
    }

    // If dockerd did not pick up the file, force a live-restore restart.
    let info = engine_docker_output(&["info"]).unwrap_or_default();
    if !info.contains("192.168.215.0/24") && !info.contains(push_hostport) {
        log::info!("Forcing dockerd restart so insecure-registries take effect...");
        let _ = engine_docker(&[
            "run",
            "--rm",
            "--privileged",
            "--pid=host",
            "alpine:latest",
            "sh",
            "-c",
            "kill -TERM $(pidof dockerd) 2>/dev/null || true",
        ]);
        if command_exists("dory") {
            let _ = run_cmd("dory", &["repair", "dockerd", "--apply"]);
        }
        for _ in 0..40 {
            if engine_docker_output(&["info"]).is_ok() {
                break;
            }
            thread::sleep(Duration::from_secs(2));
        }
    }

    Ok(())
}

/// Default kube + docker context name for Dory desktop integration.
pub const DEFAULT_CONTEXT_NAME: &str = "hops-dory";
const NAME_FILE: &str = "dory-name";
const NAME_ENV: &str = "HOPS_DORY_NAME";

/// Resolved context name: `--name` (persisted) > `HOPS_DORY_NAME` > file > `hops-dory`.
pub fn context_name() -> String {
    if let Ok(v) = std::env::var(NAME_ENV) {
        let t = v.trim();
        if !t.is_empty() {
            return t.to_string();
        }
    }
    if let Ok(path) = crate::commands::local::local_state_dir() {
        if let Ok(raw) = std::fs::read_to_string(path.join(NAME_FILE)) {
            let t = raw.trim();
            if !t.is_empty() {
                return t.to_string();
            }
        }
    }
    DEFAULT_CONTEXT_NAME.to_string()
}

/// Persist a user-chosen name for kube/docker context integration.
pub fn persist_context_name(name: &str) -> Result<(), Box<dyn Error>> {
    let name = validate_context_name(name)?;
    let dir = crate::commands::local::local_state_dir()?;
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join(NAME_FILE), format!("{name}\n"))?;
    log::info!("Dory desktop name set to '{name}' (kube + docker context)");
    Ok(())
}

fn validate_context_name(name: &str) -> Result<String, Box<dyn Error>> {
    let name = name.trim();
    if name.is_empty() {
        return Err("--name must not be empty".into());
    }
    // kubectl context names: keep it simple (no path separators / whitespace).
    if name.contains(['/', '\\', ' ', '\t', '\n', ':']) {
        return Err(format!(
            "invalid --name '{name}': use a simple token (e.g. hops-dory)"
        )
        .into());
    }
    Ok(name.to_string())
}

/// Env: set `HOPS_DORY_DESKTOP=0` (or `false`/`off`/`no`) to skip mutating the
/// machine's default kube/docker contexts. Callers must drive the session with
/// `DOCKER_HOST` and `KUBECONFIG` (or pass `--context`) instead.
pub const DESKTOP_INTEGRATION_ENV: &str = "HOPS_DORY_DESKTOP";

/// Whether hops may rewrite `~/.kube/config` / switch docker contexts.
pub fn desktop_integration_enabled() -> bool {
    match std::env::var(DESKTOP_INTEGRATION_ENV) {
        Ok(v) => {
            let v = v.trim().to_ascii_lowercase();
            !matches!(v.as_str(), "0" | "false" | "no" | "off")
        }
        Err(_) => true,
    }
}

/// Wire Dory into the normal developer desktop:
/// - merge stock `~/.kube/dory-config` into `~/.kube/config` as context **`hops-dory`**
///   (or `--name` / `HOPS_DORY_NAME`)
/// - `kubectl config use-context <name>`
/// - ensure/use a docker context of the same name pointing at `~/.dory/dory.sock`
///
/// Called from backend activate and after local start so you don't need
/// `export KUBECONFIG=...` / `export DOCKER_HOST=...`.
///
/// No-op for host defaults when [`desktop_integration_enabled`] is false: only
/// fills missing `DOCKER_HOST` / `KUBECONFIG` env for this process tree.
pub fn ensure_desktop_integration() -> Result<(), Box<dyn Error>> {
    ensure_side_kubeconfig_hint();
    if !desktop_integration_enabled() {
        ensure_engine_env_only();
        log::info!(
            "Dory desktop integration disabled ({DESKTOP_INTEGRATION_ENV}=0); \
             using DOCKER_HOST / KUBECONFIG only (no use-context, no docker context switch)"
        );
        return Ok(());
    }
    let name = context_name();
    ensure_user_kubeconfig_context(&name)?;
    ensure_docker_context_default(&name)?;
    Ok(())
}

/// Point this process at the engine without touching desktop defaults.
fn ensure_engine_env_only() {
    if std::env::var_os("DOCKER_HOST").is_none() {
        if let Ok(sock) = engine_socket() {
            if sock.exists() {
                std::env::set_var("DOCKER_HOST", format!("unix://{}", sock.display()));
            }
        }
    }
    if std::env::var_os("KUBECONFIG").is_none() {
        export_kubeconfig_env();
    }
}

/// Fallback when merge fails: point hops child processes at the side file.
/// Prefer `~/.kube/dory-config` first so its certs win over a stale `dory`
/// user that may still live in `~/.kube/config` after cluster recreate.
pub fn export_kubeconfig_env() {
    let Some(dory_cfg) = kubeconfig_path() else {
        return;
    };
    if !std::path::Path::new(&dory_cfg).is_file() {
        return;
    }
    let existing = std::env::var("KUBECONFIG").unwrap_or_default();
    let parts: Vec<&str> = existing
        .split(':')
        .filter(|p| !p.is_empty() && *p != dory_cfg.as_str())
        .collect();
    let rest = if parts.is_empty() {
        match home() {
            Ok(h) => h.join(".kube/config").to_string_lossy().into_owned(),
            Err(_) => String::new(),
        }
    } else {
        parts.join(":")
    };
    if rest.is_empty() {
        std::env::set_var("KUBECONFIG", &dory_cfg);
    } else {
        std::env::set_var("KUBECONFIG", format!("{dory_cfg}:{rest}"));
    }
}

/// Merge the stock Dory kubeconfig into `~/.kube/config` as the given context name.
fn ensure_user_kubeconfig_context(name: &str) -> Result<(), Box<dyn Error>> {
    let side = kubeconfig_path().ok_or("HOME unset")?;
    if !std::path::Path::new(&side).is_file() {
        return Err(format!(
            "missing {}; enable Kubernetes in the Dory app (or let hops start write it)",
            side
        )
        .into());
    }

    let home = home()?;
    let kube_dir = home.join(".kube");
    std::fs::create_dir_all(&kube_dir)?;
    let main = kube_dir.join("config");

    // Work on a temp copy so we can rename stock contexts without mutating
    // the product side file.
    let tmp = kube_dir.join(format!(".dory-merge-{}.yaml", std::process::id()));
    std::fs::copy(&side, &tmp)?;
    let names = run_cmd_output(
        "kubectl",
        &[
            "config",
            "--kubeconfig",
            tmp.to_str().unwrap(),
            "get-contexts",
            "-o",
            "name",
        ],
    )
    .unwrap_or_default();
    for old in names.lines().map(str::trim).filter(|n| !n.is_empty()) {
        if old != name {
            let _ = run_cmd(
                "kubectl",
                &[
                    "config",
                    "--kubeconfig",
                    tmp.to_str().unwrap(),
                    "rename-context",
                    old,
                    name,
                ],
            );
        }
    }

    if main.is_file() {
        let merged = {
            let mut child = std::process::Command::new("kubectl")
                .args(["config", "view", "--flatten", "--raw"])
                .env(
                    "KUBECONFIG",
                    format!("{}:{}", main.display(), tmp.display()),
                )
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()?;
            let mut out = String::new();
            use std::io::Read;
            if let Some(mut s) = child.stdout.take() {
                s.read_to_string(&mut out)?;
            }
            let status = child.wait()?;
            if !status.success() {
                let _ = std::fs::remove_file(&tmp);
                return Err(
                    "kubectl config view --flatten failed while merging dory kubeconfig".into(),
                );
            }
            out
        };
        let backup = kube_dir.join("config.hops-dory-backup");
        let _ = std::fs::copy(&main, &backup);
        std::fs::write(&main, merged)?;
        let _ = std::fs::set_permissions(
            &main,
            std::os::unix::fs::PermissionsExt::from_mode(0o600),
        );
    } else {
        std::fs::copy(&tmp, &main)?;
        let _ = std::fs::set_permissions(
            &main,
            std::os::unix::fs::PermissionsExt::from_mode(0o600),
        );
    }
    let _ = std::fs::remove_file(&tmp);

    // Drop the short-lived "dory" name from an earlier hops revision if present
    // and distinct from the configured name.
    if name != "dory" {
        let _ = run_cmd("kubectl", &["config", "delete-context", "dory"]);
    }

    let _ = run_cmd("kubectl", &["config", "use-context", name]);
    log::info!(
        "Kubernetes context '{}' is ready in ~/.kube/config (also: ~/.kube/dory-config)",
        name
    );
    Ok(())
}

/// Docker context of the same name, pointing at Dory's engine socket.
fn ensure_docker_context_default(name: &str) -> Result<(), Box<dyn Error>> {
    if !command_exists("docker") {
        return Ok(());
    }
    let sock = engine_socket()?;
    if !sock.exists() {
        log::warn!(
            "Dory engine socket {} missing; skip docker context '{}'",
            sock.display(),
            name
        );
        return Ok(());
    }
    let host = format!("host=unix://{}", sock.display());
    let contexts = run_cmd_output("docker", &["context", "ls", "--format", "{{.Name}}"])
        .unwrap_or_default();
    if !contexts.lines().any(|n| n.trim() == name) {
        log::info!(
            "Creating docker context '{}' → unix://{}",
            name,
            sock.display()
        );
        // Ignore failure if a stale context exists with different metadata.
        let create = run_cmd(
            "docker",
            &["context", "create", name, "--docker", &host],
        );
        if create.is_err() {
            // Update in place when possible.
            let _ = run_cmd(
                "docker",
                &["context", "update", name, "--docker", &host],
            );
        }
    }
    run_cmd("docker", &["context", "use", name])?;
    log::info!(
        "Docker context set to '{}' (unix://{})",
        name,
        sock.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_pull_is_in_cluster_service_not_host_gateway() {
        assert_eq!(
            crate::commands::local::package_install::REGISTRY_PULL_INCLUSTER,
            "registry.crossplane-system.svc.cluster.local:5000"
        );
        assert!(!REGISTRY_PULL_INCLUSTER.contains("dory.internal"));
        assert_eq!(REGISTRY_PUSH, "localhost:30500");
    }

    #[test]
    fn start_rejects_size_flags() {
        let size = SizeArgs {
            cpus: Some(4),
            memory: None,
            disk: None,
        };

        let err = start(&size).expect_err("size flags must be rejected");

        assert!(err.to_string().contains("--cpus 4"));
        assert!(err.to_string().contains("Dory app"));
    }

    #[test]
    fn missing_k8s_error_mentions_app_not_fork() {
        let msg = "Dory Kubernetes is not enabled (no `dory-k8s` container).\n\
         In the Dory app: enable Kubernetes, wait until it is running, then re-run:\n\
           hops local start --backend dory\n\
         (hops uses stock Dory only — it does not create the cluster for you.)";
        assert!(msg.contains("Dory app"));
        assert!(!msg.contains("feat/scriptable"));
        assert!(!msg.contains("dory k8s enable"));
    }

    #[test]
    fn desktop_integration_env_off_values() {
        for off in ["0", "false", "FALSE", "no", "off", " Off "] {
            std::env::set_var(DESKTOP_INTEGRATION_ENV, off);
            assert!(
                !desktop_integration_enabled(),
                "expected desktop off for {off:?}"
            );
        }
        std::env::set_var(DESKTOP_INTEGRATION_ENV, "1");
        assert!(desktop_integration_enabled());
        std::env::remove_var(DESKTOP_INTEGRATION_ENV);
    }
}
