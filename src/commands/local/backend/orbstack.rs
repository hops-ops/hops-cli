//! orbstack backend: OrbStack's built-in single-node k3s (https://orbstack.dev).
//!
//! Design:
//! - OrbStack owns the VM, dockerd and k3s. hops enables Kubernetes with
//!   `orb config set k8s.enable true`, starts OrbStack, and targets the
//!   `orbstack` kube context. Memory is returned to macOS when idle, and any
//!   Mac path works as a `hostPath` at the same path, so there is no
//!   extra-mount or VM sizing step.
//! - Enabling Kubernetes makes OrbStack switch the kubeconfig
//!   current-context; hops restores the previous one.
//!
//! Package registry: k3s runs on OrbStack's dockerd, and that dockerd (like
//! the Mac) resolves `*.svc.cluster.local`. So push and pull both use the
//! in-cluster Service name; dockerd only needs it listed as insecure because
//! the registry serves a self-signed certificate.

use super::SizeArgs;
use crate::commands::local::package_install::REGISTRY_PULL_INCLUSTER;
use crate::commands::local::{
    command_exists, kube_context_from_env, run_cmd, run_cmd_output, HOPS_KUBE_CONTEXT_ENV,
};
use std::error::Error;
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

pub const CONTEXT_NAME: &str = "orbstack";

/// Istio CNI paths on OrbStack's k3s (values for the `cni` chart).
pub const ISTIO_CNI_BIN_DIR: &str = "/var/lib/rancher/k3s/data/current/bin/";
pub const ISTIO_CNI_CONF_DIR: &str = "/var/lib/rancher/k3s/agent/etc/cni/net.d";

fn home() -> Result<PathBuf, Box<dyn Error>> {
    Ok(PathBuf::from(std::env::var("HOME").map_err(|_| {
        "HOME is not set; unable to locate OrbStack's state directory"
    })?))
}

/// OrbStack's Docker API socket.
pub fn engine_socket() -> Result<PathBuf, Box<dyn Error>> {
    Ok(home()?.join(".orbstack/run/docker.sock"))
}

/// dockerd settings OrbStack applies on `orb restart docker`.
fn docker_config_path() -> Result<PathBuf, Box<dyn Error>> {
    Ok(home()?.join(".orbstack/config/docker.json"))
}

pub fn install() -> Result<(), Box<dyn Error>> {
    log::info!("Installing OrbStack via Homebrew...");
    run_cmd("brew", &["install", "--cask", "orbstack"])?;
    log::info!(
        "OrbStack installed; run `hops local start --cluster-provider orbstack` to enable its Kubernetes"
    );
    Ok(())
}

pub fn uninstall() -> Result<(), Box<dyn Error>> {
    log::info!("Uninstalling OrbStack...");
    run_cmd("brew", &["uninstall", "--cask", "orbstack"])?;
    log::info!("OrbStack uninstalled");
    Ok(())
}

pub fn start(size: &SizeArgs) -> Result<(), Box<dyn Error>> {
    if size.any_set() {
        return Err(format!(
            "the orbstack cluster provider's VM is sized by OrbStack (`orb config set cpu|memory_mib`), not hops; drop{}",
            size.command_suffix()
        )
        .into());
    }
    preflight()?;
    if !k8s_enabled() {
        log::info!("Starting OrbStack Kubernetes...");
    }
    // OrbStack switches current-context once the cluster is up.
    keeping_kube_context(|| {
        // Enables k8s and starts OrbStack if needed, without restarting
        // OrbStack's other containers; returns before the node is up. No-op
        // when running.
        run_cmd("orb", &["start", "k8s"])?;
        // A first enable creates the context after activation saw it missing,
        // and the restored current-context belongs to some other cluster: pin it.
        if kube_context_from_env().is_none() {
            std::env::set_var(HOPS_KUBE_CONTEXT_ENV, CONTEXT_NAME);
        }
        wait_for_node_ready()
    })
}

/// Stops only Kubernetes (OrbStack records it as disabled; cluster data is
/// kept); OrbStack's other containers and machines keep running.
pub fn stop() -> Result<(), Box<dyn Error>> {
    if !command_exists("orb") || !orbstack_running() {
        log::info!("OrbStack is not running");
        return Ok(());
    }
    log::info!("Stopping OrbStack Kubernetes...");
    run_cmd("orb", &["stop", "k8s"])?;
    log::info!("OrbStack Kubernetes stopped");
    Ok(())
}

/// Delete the cluster's state; OrbStack recreates an empty one on next start.
pub fn destroy() -> Result<(), Box<dyn Error>> {
    if !command_exists("orb") {
        log::info!("OrbStack is not installed");
        return Ok(());
    }
    log::info!("Deleting OrbStack's Kubernetes cluster...");
    run_cmd("orb", &["delete", "-f", "k8s"])?;
    log::info!("OrbStack Kubernetes deleted");
    Ok(())
}

pub fn reset() -> Result<(), Box<dyn Error>> {
    preflight()?;
    destroy()?;
    start(&SizeArgs::default())
}

pub fn resize(_size: &SizeArgs) -> Result<(), Box<dyn Error>> {
    Err("the orbstack backend has no hops-managed VM to resize; \
         use `orb config set cpu <n>` / `orb config set memory_mib <mib>` instead"
        .into())
}

/// Whether OrbStack has Kubernetes enabled. `orb stop k8s` disables it but
/// keeps its data, so a stopped cluster reads as absent here.
pub fn cluster_exists() -> bool {
    command_exists("orb") && k8s_enabled()
}

fn k8s_enabled() -> bool {
    run_cmd_output("orb", &["config", "get", "k8s.enable"])
        .map(|value| value.trim() == "true")
        .unwrap_or(false)
}

fn orbstack_running() -> bool {
    run_cmd_output("orb", &["status"])
        .map(|status| status.trim() == "Running")
        .unwrap_or(false)
}

fn preflight() -> Result<(), Box<dyn Error>> {
    if !command_exists("orb") {
        return Err(
            "OrbStack's `orb` CLI not found; install it with `hops local install --cluster-provider orbstack`"
                .into(),
        );
    }
    // No docker CLI check: OrbStack installs it on first start.
    Ok(())
}

fn current_kube_context() -> Option<String> {
    run_cmd_output("kubectl", &["config", "current-context"])
        .ok()
        .map(|ctx| ctx.trim().to_string())
        .filter(|ctx| !ctx.is_empty())
}

/// Run `operation` and put back the kubeconfig current-context it found,
/// whether or not the operation succeeds. The operation's error wins.
fn keeping_kube_context(
    operation: impl FnOnce() -> Result<(), Box<dyn Error>>,
) -> Result<(), Box<dyn Error>> {
    let previous = current_kube_context();
    let result = operation();
    let restored = restore_kube_context(previous.as_deref());
    result.and(restored)
}

fn restore_kube_context(previous: Option<&str>) -> Result<(), Box<dyn Error>> {
    let Some(previous) = previous else {
        return Ok(());
    };
    if current_kube_context().as_deref() == Some(previous) {
        return Ok(());
    }
    log::info!("Restoring kubeconfig current-context '{previous}' (OrbStack switched it)");
    run_cmd("kubectl", &["config", "use-context", previous])
}

fn wait_for_node_ready() -> Result<(), Box<dyn Error>> {
    log::info!("Waiting for the OrbStack k8s node to become Ready...");
    for i in 0..90 {
        let ready = run_cmd_output(
            "kubectl",
            &["--context", CONTEXT_NAME, "get", "nodes", "--no-headers"],
        )
        .map(|out| out.contains(" Ready"))
        .unwrap_or(false);
        if ready {
            return Ok(());
        }
        if i > 0 && i % 15 == 0 {
            log::info!("Still waiting for k3s Ready ({}s)...", i * 2);
        }
        thread::sleep(Duration::from_secs(2));
    }
    Err("timed out waiting for the OrbStack k8s node to become Ready; check `orb logs k8s`".into())
}

/// List the in-cluster registry as insecure in OrbStack's dockerd (it serves a
/// self-signed certificate). Restarts the engine only when the file changes.
pub fn ensure_registry_trust() -> Result<(), Box<dyn Error>> {
    let path = docker_config_path()?;
    let raw = std::fs::read_to_string(&path).unwrap_or_else(|_| "{}".to_string());
    let updated = with_insecure_registry(&raw, REGISTRY_PULL_INCLUSTER)
        .map_err(|error| format!("{}: {error}", path.display()))?;
    if updated == raw {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, &updated)?;
    log::info!(
        "Restarting OrbStack's Docker engine to trust {}...",
        REGISTRY_PULL_INCLUSTER
    );
    keeping_kube_context(|| {
        run_cmd("orb", &["restart", "docker"])?;
        wait_for_node_ready()
    })
}

/// Add `registry` to `insecure-registries` in a docker.json document.
fn with_insecure_registry(raw: &str, registry: &str) -> Result<String, Box<dyn Error>> {
    let mut config: serde_json::Value = if raw.trim().is_empty() {
        serde_json::json!({})
    } else {
        serde_json::from_str(raw)?
    };
    let object = config
        .as_object_mut()
        .ok_or("docker.json must be a JSON object")?;
    let list = object
        .entry("insecure-registries")
        .or_insert_with(|| serde_json::json!([]))
        .as_array_mut()
        .ok_or("insecure-registries must be an array")?;
    if list.iter().any(|entry| entry.as_str() == Some(registry)) {
        return Ok(raw.to_string());
    }
    list.push(serde_json::Value::String(registry.to_string()));
    Ok(format!("{}\n", serde_json::to_string_pretty(&config)?))
}

/// Node pulls resolve the Service name through the cluster DNS; nothing to
/// wire per ClusterIP.
pub fn wire_registry(_cluster_ip: &str) -> Result<(), Box<dyn Error>> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insecure_registry_added_to_empty_config() {
        let updated = with_insecure_registry("{}", "reg:5000").unwrap();
        let value: serde_json::Value = serde_json::from_str(&updated).unwrap();
        assert_eq!(
            value["insecure-registries"],
            serde_json::json!(["reg:5000"])
        );
    }

    #[test]
    fn insecure_registry_keeps_other_settings_and_is_idempotent() {
        let raw = r#"{"features":{"buildkit":true},"insecure-registries":["other:1"]}"#;
        let updated = with_insecure_registry(raw, "reg:5000").unwrap();
        let value: serde_json::Value = serde_json::from_str(&updated).unwrap();
        assert_eq!(value["features"]["buildkit"], true);
        assert_eq!(
            value["insecure-registries"],
            serde_json::json!(["other:1", "reg:5000"])
        );
        assert_eq!(
            with_insecure_registry(&updated, "reg:5000").unwrap(),
            updated
        );
    }

    #[test]
    fn insecure_registry_rejects_non_object_config() {
        assert!(with_insecure_registry("[]", "reg:5000").is_err());
    }
}
