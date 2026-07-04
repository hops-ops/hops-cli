//! kind backend: docker containers as nodes, containerd runtime.
//!
//! Works against any reachable docker daemon — Docker Desktop, colima's
//! dockerd, dory's dockerd, or a CI runner's. Registry trust is wired through
//! containerd's certs.d (`config_path` is enabled by default in kind node
//! images since v0.27.0), written after cluster creation; containerd reads
//! certs.d per-pull, so no restart is needed.

use super::SizeArgs;
use crate::commands::local::package_install::{REGISTRY_PULL, REGISTRY_PUSH};
use crate::commands::local::{command_exists, run_cmd, run_cmd_output};
use std::error::Error;
use std::io::Write;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

pub const CLUSTER_NAME: &str = "hops";
const NODE_CONTAINER: &str = "hops-control-plane";

/// kind node images before v0.27.0 ship containerd 1.x without certs.d
/// `config_path` enabled, so our hosts.toml files would be ignored.
const MIN_KIND_VERSION: (u32, u32) = (0, 27);

const KIND_CONFIG: &str = r#"kind: Cluster
apiVersion: kind.x-k8s.io/v1alpha4
nodes:
- role: control-plane
  extraPortMappings:
  - containerPort: 30500
    hostPort: 30500
    listenAddress: "127.0.0.1"
"#;

pub fn install() -> Result<(), Box<dyn Error>> {
    log::info!("Installing kind via Homebrew...");
    run_cmd("brew", &["install", "kind"])?;
    log::info!("kind installed successfully");
    Ok(())
}

pub fn uninstall() -> Result<(), Box<dyn Error>> {
    log::info!("Uninstalling kind...");
    run_cmd("brew", &["uninstall", "kind"])?;
    log::info!("kind uninstalled");
    Ok(())
}

pub fn start(size: &SizeArgs) -> Result<(), Box<dyn Error>> {
    if size.any_set() {
        return Err(format!(
            "the kind backend has no VM to size; drop{} (resources are governed by the docker daemon kind runs on)",
            size.command_suffix()
        )
        .into());
    }

    preflight()?;

    if !cluster_exists() {
        return create_cluster();
    }

    if node_running() {
        log::info!("kind cluster '{}' is already running", CLUSTER_NAME);
        return Ok(());
    }

    // kind has no start/stop; the node is a docker container. Restarting a
    // single-node cluster is reliable in practice but not guaranteed by kind.
    log::info!("Starting stopped kind node '{}'...", NODE_CONTAINER);
    run_cmd("docker", &["start", NODE_CONTAINER])?;
    wait_for_api_after_restart()
}

pub fn stop() -> Result<(), Box<dyn Error>> {
    log::info!("Stopping kind node '{}'...", NODE_CONTAINER);
    run_cmd("docker", &["stop", NODE_CONTAINER])?;
    log::info!("kind cluster stopped");
    Ok(())
}

pub fn destroy() -> Result<(), Box<dyn Error>> {
    log::info!("Deleting kind cluster '{}'...", CLUSTER_NAME);
    run_cmd("kind", &["delete", "cluster", "--name", CLUSTER_NAME])?;
    log::info!("kind cluster deleted");
    Ok(())
}

/// kind's node container IS the cluster, so reset means recreate.
pub fn reset() -> Result<(), Box<dyn Error>> {
    preflight()?;
    if cluster_exists() {
        destroy()?;
    }
    create_cluster()
}

pub fn resize(_size: &SizeArgs) -> Result<(), Box<dyn Error>> {
    Err(
        "kind clusters have no VM to resize; adjust the docker daemon's resources, \
         or `hops local destroy && hops local start` to recreate the cluster"
            .into(),
    )
}

/// Whether the hops kind cluster exists (running or stopped). Missing binary
/// or failing command reads as "no cluster".
pub fn cluster_exists() -> bool {
    if !command_exists("kind") {
        return false;
    }
    run_cmd_output("kind", &["get", "clusters"])
        .map(|out| out.lines().any(|line| line.trim() == CLUSTER_NAME))
        .unwrap_or(false)
}

fn node_running() -> bool {
    run_cmd_output(
        "docker",
        &["inspect", "-f", "{{.State.Running}}", NODE_CONTAINER],
    )
    .map(|out| out.trim() == "true")
    .unwrap_or(false)
}

fn preflight() -> Result<(), Box<dyn Error>> {
    if !command_exists("kind") {
        return Err(
            "kind is not installed; run `hops local install --backend kind` or `brew install kind`"
                .into(),
        );
    }

    let version_output = run_cmd_output("kind", &["version"])?;
    match parse_kind_version(&version_output) {
        Some(version) if version >= MIN_KIND_VERSION => {}
        Some((major, minor)) => {
            return Err(format!(
                "kind v{major}.{minor} is too old: node images before v{}.{} lack containerd \
                 certs.d support needed for the local registry. Upgrade with `brew upgrade kind`.",
                MIN_KIND_VERSION.0, MIN_KIND_VERSION.1
            )
            .into());
        }
        None => log::warn!(
            "Unable to parse `kind version` output ({}); continuing",
            version_output.trim()
        ),
    }

    if run_cmd_output("docker", &["info", "--format", "{{.ServerVersion}}"]).is_err() {
        return Err(
            "no reachable docker daemon; start Docker Desktop / colima / dory \
             (or point DOCKER_HOST / `docker context use` at one) and retry"
                .into(),
        );
    }

    Ok(())
}

fn create_cluster() -> Result<(), Box<dyn Error>> {
    log::info!("Creating kind cluster '{}'...", CLUSTER_NAME);
    let mut child = Command::new("kind")
        .args(["create", "cluster", "--name", CLUSTER_NAME, "--config", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()?;
    if let Some(ref mut stdin) = child.stdin {
        stdin.write_all(KIND_CONFIG.as_bytes())?;
    }
    let status = child.wait()?;
    if !status.success() {
        return Err(format!("kind create cluster exited with {}", status).into());
    }
    Ok(())
}

fn wait_for_api_after_restart() -> Result<(), Box<dyn Error>> {
    log::info!("Waiting for Kubernetes API...");
    for _ in 0..24 {
        if run_cmd_output("kubectl", &["cluster-info"]).is_ok() {
            return Ok(());
        }
        thread::sleep(Duration::from_secs(5));
    }
    Err(
        "Kubernetes API did not come back after restarting the kind node; \
         run `hops local reset` to recreate the cluster"
            .into(),
    )
}

/// Alias both registry pull names to the registry Service's ClusterIP via
/// containerd certs.d files on the node. `localhost:30500` is what provider
/// runtime pods reference; aliasing it here means the name never depends on
/// kube-proxy's localhost-NodePort behavior. Files live on the node's
/// writable layer, so they survive docker stop/start (unlike /etc/hosts,
/// which docker regenerates).
pub fn wire_registry(cluster_ip: &str) -> Result<(), Box<dyn Error>> {
    for name in [REGISTRY_PULL, REGISTRY_PUSH] {
        write_hosts_toml(name, cluster_ip)?;
    }
    Ok(())
}

fn hosts_toml(cluster_ip: &str) -> String {
    format!(
        "[host.\"http://{}:5000\"]\n  capabilities = [\"pull\", \"resolve\"]\n  skip_verify = true\n",
        cluster_ip
    )
}

fn write_hosts_toml(registry_name: &str, cluster_ip: &str) -> Result<(), Box<dyn Error>> {
    let dir = format!("/etc/containerd/certs.d/{}", registry_name);
    let path = format!("{}/hosts.toml", dir);
    let desired = hosts_toml(cluster_ip);

    let current =
        run_cmd_output("docker", &["exec", NODE_CONTAINER, "cat", &path]).unwrap_or_default();
    if current == desired {
        return Ok(());
    }

    log::info!(
        "Wiring containerd registry alias: {} -> {}:5000",
        registry_name,
        cluster_ip
    );

    let mut child = Command::new("docker")
        .args([
            "exec",
            "-i",
            NODE_CONTAINER,
            "sh",
            "-c",
            &format!("mkdir -p '{}' && cat > '{}'", dir, path),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()?;
    if let Some(ref mut stdin) = child.stdin {
        stdin.write_all(desired.as_bytes())?;
    }
    let status = child.wait()?;
    if !status.success() {
        return Err(format!("failed to write {} on kind node", path).into());
    }
    Ok(())
}

fn parse_kind_version(output: &str) -> Option<(u32, u32)> {
    // Typical output: "kind v0.32.0 go1.23.4 darwin/arm64"
    output.split_whitespace().find_map(|token| {
        let token = token.strip_prefix('v').unwrap_or(token);
        let mut parts = token.split('.');
        let major: u32 = parts.next()?.parse().ok()?;
        let minor: u32 = parts.next()?.parse().ok()?;
        Some((major, minor))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_config_pins_registry_nodeport_to_localhost() {
        assert!(KIND_CONFIG.contains("containerPort: 30500"));
        assert!(KIND_CONFIG.contains("hostPort: 30500"));
        assert!(KIND_CONFIG.contains("listenAddress: \"127.0.0.1\""));
        assert!(KIND_CONFIG.contains("role: control-plane"));
    }

    #[test]
    fn hosts_toml_aliases_to_cluster_ip_over_http() {
        let toml = hosts_toml("10.43.12.7");

        assert_eq!(
            toml,
            "[host.\"http://10.43.12.7:5000\"]\n  capabilities = [\"pull\", \"resolve\"]\n  skip_verify = true\n"
        );
    }

    #[test]
    fn parse_kind_version_reads_standard_output() {
        assert_eq!(
            parse_kind_version("kind v0.32.0 go1.23.4 darwin/arm64"),
            Some((0, 32))
        );
        assert_eq!(
            parse_kind_version("kind v0.27.0 go1.22 linux/amd64"),
            Some((0, 27))
        );
    }

    #[test]
    fn parse_kind_version_handles_unexpected_output() {
        assert_eq!(parse_kind_version("something unparseable"), None);
        assert_eq!(parse_kind_version(""), None);
    }

    #[test]
    fn old_kind_versions_fail_the_minimum_check() {
        let version = parse_kind_version("kind v0.26.0 go1.22 linux/amd64").unwrap();
        assert!(version < MIN_KIND_VERSION);

        let new_enough = parse_kind_version("kind v0.27.0 go1.22 linux/amd64").unwrap();
        assert!(new_enough >= MIN_KIND_VERSION);
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
        assert!(err.to_string().contains("no VM to size"));
    }
}
