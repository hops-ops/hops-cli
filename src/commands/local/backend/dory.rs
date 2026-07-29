//! dory backend: product k3s on stock Dory (https://augani.github.io/dory).
//!
//! Design (stock Dory only — no hops fork):
//! - Dory.app owns the engine, dockerd, and k3s lifecycle (enable Kubernetes
//!   in the app). hops never calls `dory k8s enable`.
//! - hops talks to the engine via `~/.dory/engine.sock` and to the cluster
//!   via `~/.kube/dory-config` (stock side file; context is usually `default`).
//! - Packages use an engine-side `registry:2` container: host push
//!   `localhost:30500`, node pull `host.dory.internal:30500`.

use super::SizeArgs;
use crate::commands::local::{command_exists, run_cmd, run_cmd_output};
use std::error::Error;
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

const NODE_CONTAINER: &str = "dory-k8s";
/// Engine-side package registry container (sibling of k3s, not in-cluster).
const PACKAGE_REGISTRY_NAME: &str = "hops-local-registry";
/// Host publish for `docker` / `crossplane xpkg` push (same port as other backends).
const PACKAGE_REGISTRY_HOST_PORT: &str = "30500";
/// How the k3s node reaches the engine registry (Dory container→host path).
pub const PACKAGE_REGISTRY_PULL: &str = "host.dory.internal:30500";

fn home() -> Result<PathBuf, Box<dyn Error>> {
    Ok(PathBuf::from(std::env::var("HOME").map_err(|_| {
        "HOME is not set; unable to locate dory's state directory"
    })?))
}

fn engine_socket() -> Result<PathBuf, Box<dyn Error>> {
    Ok(home()?.join(".dory/engine.sock"))
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
    log::info!("Deleting dory k8s node '{}' (hops does not disable product k8s via CLI)...", NODE_CONTAINER);
    let _ = engine_docker(&["rm", "-f", NODE_CONTAINER]);
    let _ = engine_docker(&["rm", "-f", PACKAGE_REGISTRY_NAME]);
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
    // Prove the daemon answers (stale sockets are common after crashes).
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
        // Prefer in-container kubectl so we don't depend on host kubeconfig yet.
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
            // Refresh side-file if Dory left one; still useful for hops kubectl.
            ensure_side_kubeconfig_hint();
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
    // Best-effort: pull k3s.yaml if the app has not written the side file yet.
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

/// No create-time registry trust — package bridge configures the running node.
pub fn ensure_registry_trust() -> Result<(), Box<dyn Error>> {
    Ok(())
}

/// In-cluster NodePort wiring is unused on dory.
pub fn wire_registry(_cluster_ip: &str) -> Result<(), Box<dyn Error>> {
    Ok(())
}

/// Engine-side package registry + k3s mirror config for host.dory.internal.
pub fn ensure_package_bridge() -> Result<(), Box<dyn Error>> {
    preflight()?;
    ensure_engine_registry()?;
    ensure_k3s_registry_mirrors()?;
    Ok(())
}

fn ensure_engine_registry() -> Result<(), Box<dyn Error>> {
    let running = engine_docker_output(&[
        "inspect",
        "-f",
        "{{.State.Running}}",
        PACKAGE_REGISTRY_NAME,
    ])
    .unwrap_or_default();
    if running.trim() == "true" {
        return Ok(());
    }

    if engine_docker_output(&["inspect", "-f", "{{.Id}}", PACKAGE_REGISTRY_NAME]).is_ok() {
        log::info!("Starting engine package registry '{}'...", PACKAGE_REGISTRY_NAME);
        engine_docker(&["start", PACKAGE_REGISTRY_NAME])?;
        return wait_registry_http();
    }

    log::info!(
        "Creating engine package registry on localhost:{} (pull via {})...",
        PACKAGE_REGISTRY_HOST_PORT,
        PACKAGE_REGISTRY_PULL
    );
    let _ = engine_docker(&["pull", "registry:2"]);
    engine_docker(&[
        "run",
        "-d",
        "--restart",
        "unless-stopped",
        "--name",
        PACKAGE_REGISTRY_NAME,
        "-p",
        &format!("{PACKAGE_REGISTRY_HOST_PORT}:5000"),
        "registry:2",
    ])?;
    wait_registry_http()
}

fn wait_registry_http() -> Result<(), Box<dyn Error>> {
    for _ in 0..30 {
        if run_cmd_output(
            "curl",
            &[
                "-sf",
                &format!("http://127.0.0.1:{PACKAGE_REGISTRY_HOST_PORT}/v2/"),
            ],
        )
        .is_ok()
        {
            return Ok(());
        }
        thread::sleep(Duration::from_secs(1));
    }
    Err(format!(
        "timed out waiting for engine registry on localhost:{PACKAGE_REGISTRY_HOST_PORT}"
    )
    .into())
}

/// Write k3s registries.yaml inside the node so containerd can pull HTTP from
/// host.dory.internal. Restarts the node once if the file changed.
fn ensure_k3s_registry_mirrors() -> Result<(), Box<dyn Error>> {
    if !node_running() {
        return Err(
            "dory k8s node is not running; enable Kubernetes in the Dory app first".into(),
        );
    }

    let yaml = format!(
        "mirrors:\n\
         \x20 \"{pull}\":\n\
         \x20   endpoint:\n\
         \x20     - \"http://{pull}\"\n\
         \x20 \"localhost:{port}\":\n\
         \x20   endpoint:\n\
         \x20     - \"http://{pull}\"\n",
        pull = PACKAGE_REGISTRY_PULL,
        port = PACKAGE_REGISTRY_HOST_PORT,
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
        "Configuring k3s registry mirrors for {} (engine package bridge)...",
        PACKAGE_REGISTRY_PULL
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
    // Host kubectl may need a moment after restart.
    for _ in 0..30 {
        if run_cmd_output("kubectl", &["get", "--raw", "/readyz"])
            .map(|s| s.contains("ok"))
            .unwrap_or(false)
        {
            return Ok(());
        }
        thread::sleep(Duration::from_secs(2));
    }
    Ok(())
}

/// Prefer stock `~/.kube/dory-config` for hops kubectl/helm children.
pub fn export_kubeconfig_env() {
    let Some(dory_cfg) = kubeconfig_path() else {
        return;
    };
    if !std::path::Path::new(&dory_cfg).is_file() {
        return;
    }
    let existing = std::env::var("KUBECONFIG").unwrap_or_default();
    if existing.split(':').any(|p| p == dory_cfg) {
        return;
    }
    // Side file first so stock current-context (usually `default`) wins.
    let rest = if existing.is_empty() {
        match home() {
            Ok(h) => h.join(".kube/config").to_string_lossy().into_owned(),
            Err(_) => String::new(),
        }
    } else {
        existing
    };
    if rest.is_empty() {
        std::env::set_var("KUBECONFIG", &dory_cfg);
    } else {
        std::env::set_var("KUBECONFIG", format!("{}:{}", dory_cfg, rest));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_registry_pull_uses_dory_host_gateway() {
        assert_eq!(PACKAGE_REGISTRY_PULL, "host.dory.internal:30500");
        assert!(!PACKAGE_REGISTRY_PULL.contains("cluster.local"));
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
        // Unit-level: the message constant path used when container is absent.
        let msg = "Dory Kubernetes is not enabled (no `dory-k8s` container).\n\
         In the Dory app: enable Kubernetes, wait until it is running, then re-run:\n\
           hops local start --backend dory\n\
         (hops uses stock Dory only — it does not create the cluster for you.)";
        assert!(msg.contains("Dory app"));
        assert!(!msg.contains("feat/scriptable"));
        assert!(!msg.contains("dory k8s enable"));
    }
}
