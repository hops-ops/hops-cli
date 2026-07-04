//! dory backend: k3s in a container on dory's shared-VM dockerd
//! (https://augani.github.io/dory), driven headlessly through the `dory` CLI
//! (`dory k8s enable|disable|status`) and the engine docker socket.
//!
//! Registry plumbing differs from colima/kind because the cluster container
//! is recreated from scratch on every disable/enable:
//! - trust is static config: hops writes `~/.dory/k8s/registries.yaml`
//!   BEFORE `dory k8s enable`; dory bind-mounts it and k3s reads it at boot,
//!   aliasing both pull names to the registry Service hostname over HTTP.
//! - name resolution is dynamic: `wire_registry` syncs the hostname ->
//!   ClusterIP in the node container's /etc/hosts on every start (same
//!   re-wire-on-start model as the other backends).
//! - the host reaches `localhost:30500` because `dory k8s enable --publish
//!   30500:30500` publishes the NodePort and the Dory app's port forwarder
//!   maps published ports to host loopback.

use super::SizeArgs;
use crate::commands::local::package_install::{REGISTRY_HOSTNAME, REGISTRY_PULL, REGISTRY_PUSH};
use crate::commands::local::{command_exists, run_cmd, run_cmd_output};
use std::error::Error;
use std::path::PathBuf;

const NODE_CONTAINER: &str = "dory-k8s";
/// hops' registry NodePort, published on the cluster container at create.
const REGISTRY_PORT_PUBLISH: &str = "30500:30500";

fn home() -> Result<PathBuf, Box<dyn Error>> {
    Ok(PathBuf::from(std::env::var("HOME").map_err(|_| {
        "HOME is not set; unable to locate dory's state directory"
    })?))
}

fn engine_socket() -> Result<PathBuf, Box<dyn Error>> {
    Ok(home()?.join(".dory/engine.sock"))
}

/// dory writes the cluster kubeconfig here (context name `dory`), unmerged.
pub fn kubeconfig_path() -> Option<String> {
    home()
        .ok()
        .map(|h| h.join(".kube/dory-config").to_string_lossy().into_owned())
}

fn registries_yaml_path() -> Result<PathBuf, Box<dyn Error>> {
    Ok(home()?.join(".dory/k8s/registries.yaml"))
}

/// k3s' native registry config: alias both pull names to the registry
/// Service hostname over plain HTTP. The hostname resolves through the
/// node's /etc/hosts, which `wire_registry` keeps pointed at the ClusterIP.
fn registries_yaml() -> String {
    format!(
        "# Written by `hops local start --backend dory`.\n\
         # k3s reads this at boot; edits require `hops local reset`.\n\
         mirrors:\n\
         \x20 \"{pull}\":\n\
         \x20   endpoint:\n\
         \x20     - \"http://{pull}\"\n\
         \x20 \"{push}\":\n\
         \x20   endpoint:\n\
         \x20     - \"http://{pull}\"\n",
        pull = REGISTRY_PULL,
        push = REGISTRY_PUSH,
    )
}

/// Run docker against dory's engine socket (the daemon the Dory app manages).
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
    log::info!("Dory installed; launch the Dory app once so it provisions its engine");
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
    write_registries_yaml()?;

    // Creates, restarts, or reuses the dory-k8s container as needed;
    // recreates when the published ports or registries.yaml bind drifted.
    run_cmd(
        "dory",
        &["k8s", "enable", "--publish", REGISTRY_PORT_PUBLISH],
    )
}

pub fn stop() -> Result<(), Box<dyn Error>> {
    log::info!("Stopping dory k8s node '{}'...", NODE_CONTAINER);
    engine_docker(&["stop", NODE_CONTAINER])?;
    log::info!("dory cluster stopped");
    Ok(())
}

pub fn destroy() -> Result<(), Box<dyn Error>> {
    log::info!("Deleting dory k8s cluster...");
    run_cmd("dory", &["k8s", "disable"])?;
    log::info!("dory cluster deleted");
    Ok(())
}

/// The cluster container IS the cluster, so reset means recreate.
pub fn reset() -> Result<(), Box<dyn Error>> {
    preflight()?;
    run_cmd("dory", &["k8s", "disable"])?;
    write_registries_yaml()?;
    run_cmd(
        "dory",
        &["k8s", "enable", "--publish", REGISTRY_PORT_PUBLISH],
    )
}

pub fn resize(_size: &SizeArgs) -> Result<(), Box<dyn Error>> {
    Err("the dory backend has no hops-managed VM to resize; \
         adjust resources in the Dory app instead"
        .into())
}

/// Whether the hops-relevant dory cluster exists (running or stopped).
/// Missing app/engine/CLI reads as "no cluster".
pub fn cluster_exists() -> bool {
    let Ok(sock) = engine_socket() else {
        return false;
    };
    if !sock.exists() {
        return false;
    }
    engine_docker_output(&["inspect", "-f", "{{.State.Running}}", NODE_CONTAINER]).is_ok()
}

fn preflight() -> Result<(), Box<dyn Error>> {
    if !command_exists("dory") {
        return Err("the `dory` CLI is not on PATH; link it from the dory repo \
             (`ln -sf <dory>/scripts/dory /opt/homebrew/bin/dory`)"
            .into());
    }
    let sock = engine_socket()?;
    if !sock.exists() {
        return Err(format!(
            "dory's engine socket ({}) is missing; launch the Dory app and wait for its engine to start",
            sock.display()
        )
        .into());
    }
    if !command_exists("docker") {
        return Err("docker CLI not found; install it (dory provides the daemon)".into());
    }
    Ok(())
}

/// Write the static registry trust config read by k3s at boot. Must exist
/// before `dory k8s enable` because dory binds it at container create.
fn write_registries_yaml() -> Result<(), Box<dyn Error>> {
    let path = registries_yaml_path()?;
    let desired = registries_yaml();
    if std::fs::read_to_string(&path).ok().as_deref() == Some(desired.as_str()) {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    log::info!("Writing k3s registry config: {}", path.display());
    std::fs::write(&path, desired)?;
    Ok(())
}

/// Keep the node container's /etc/hosts pointing the registry hostname at
/// the current ClusterIP (docker regenerates /etc/hosts on restart, and the
/// ClusterIP changes if the Service is recreated — hence re-run per start).
pub fn wire_registry(cluster_ip: &str) -> Result<(), Box<dyn Error>> {
    let hostname = REGISTRY_HOSTNAME;
    let current_ip = engine_docker_output(&[
        "exec",
        NODE_CONTAINER,
        "sh",
        "-c",
        &format!("awk '$2 == \"{}\" {{print $1; exit}}' /etc/hosts", hostname),
    ])
    .unwrap_or_default();
    if current_ip.trim() == cluster_ip {
        return Ok(());
    }

    log::info!("Updating node hosts entry: {} -> {}", hostname, cluster_ip);

    // /etc/hosts is a bind mount inside the container: `sed -i` fails with
    // "Resource busy" (rename over a mount point), so rewrite it in place.
    engine_docker(&[
        "exec",
        NODE_CONTAINER,
        "sh",
        "-c",
        &format!(
            "awk '$2 != \"{host}\"' /etc/hosts > /tmp/hosts.new && \
             cat /tmp/hosts.new > /etc/hosts && rm -f /tmp/hosts.new && \
             echo '{ip} {host}' >> /etc/hosts",
            host = hostname,
            ip = cluster_ip
        ),
    ])?;

    Ok(())
}

/// Make `--context dory` resolvable: dory's kubeconfig is a side file, so
/// prepend it to KUBECONFIG (preserving whatever the user already has).
pub fn export_kubeconfig_env() {
    let Some(dory_cfg) = kubeconfig_path() else {
        return;
    };
    let existing = std::env::var("KUBECONFIG").unwrap_or_default();
    if existing.split(':').any(|p| p == dory_cfg) {
        return;
    }
    let rest = if existing.is_empty() {
        match home() {
            Ok(h) => h.join(".kube/config").to_string_lossy().into_owned(),
            Err(_) => return,
        }
    } else {
        existing
    };
    std::env::set_var("KUBECONFIG", format!("{}:{}", dory_cfg, rest));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registries_yaml_aliases_both_pull_names_to_the_service_over_http() {
        let yaml = registries_yaml();

        assert!(yaml.contains("\"registry.crossplane-system.svc.cluster.local:5000\":"));
        assert!(yaml.contains("\"localhost:30500\":"));
        // Both mirrors resolve to the same HTTP endpoint on the Service name.
        assert_eq!(
            yaml.matches("- \"http://registry.crossplane-system.svc.cluster.local:5000\"")
                .count(),
            2
        );
        assert!(!yaml.contains("https://"));
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
}
