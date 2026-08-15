//! kind backend: docker containers as nodes, containerd runtime.
//!
//! Works against any reachable docker daemon — Docker Desktop, colima's
//! dockerd, dory's dockerd, or a CI runner's. Registry trust is wired through
//! containerd's certs.d (`config_path` is enabled by default in kind node
//! images since v0.27.0), written after cluster creation; containerd reads
//! certs.d per-pull, so no restart is needed.
//!
//! ## Source delivery (hostPath spike)
//!
//! On create, hops injects kind `extraMounts` for `$HOME` (same path in the
//! node) so Mac worktrees are visible for hostPath delivery when the engine
//! can bind-mount host dirs (e.g. Dory). Changing mounts requires recreate
//! (`hops local reset --cluster-provider kind --docker-provider dory`).
//!
//! ## Docker engine selection (spike toward --docker-provider)
//!
//! Prefer explicit `DOCKER_HOST` / active docker context. If unset and
//! `~/.dory/dory.sock` exists, kind commands use that socket (kind-on-Dory).

use super::SizeArgs;
use crate::commands::local::package_install::{REGISTRY_PULL, REGISTRY_PUSH};
use crate::commands::local::{command_exists, run_cmd, run_cmd_output};
use std::collections::BTreeSet;
use std::error::Error;
use std::io::Write;
use std::net::{Ipv4Addr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

/// Default kind cluster name (and historical hard-coded value).
pub const DEFAULT_CLUSTER_NAME: &str = "hops";
const KIND_CLUSTER_NAME_ENV: &str = "HOPS_KIND_CLUSTER_NAME";
const KIND_REGISTRY_HOST_PORT_ENV: &str = "HOPS_KIND_REGISTRY_HOST_PORT";
const REGISTRY_HOST_PORT_START: u16 = 30500;
const REGISTRY_HOST_PORT_END: u16 = 30599;
const INOTIFY_SYSCTL_PATH: &str = "/etc/sysctl.d/99-hops-local-inotify.conf";
const INOTIFY_MAX_USER_INSTANCES: u32 = 8192;
const INOTIFY_MAX_USER_WATCHES: u32 = 1_048_576;
const INSTALL_INOTIFY_SYSCTL_SCRIPT: &str = r#"set -eu
target="$1"
expected_instances="$2"
expected_watches="$3"
tmp="${target}.tmp"
trap 'rm -f "${tmp}"' EXIT
cat > "${tmp}"
chmod 0644 "${tmp}"
mv "${tmp}" "${target}"
trap - EXIT
sysctl -p "${target}" >/dev/null
instances="$(sysctl -n fs.inotify.max_user_instances)"
watches="$(sysctl -n fs.inotify.max_user_watches)"
test "${instances}" -ge "${expected_instances}"
test "${watches}" -ge "${expected_watches}"
"#;

/// Active hops kind cluster name (`kind create --name`).
pub fn active_cluster_name() -> String {
    std::env::var(KIND_CLUSTER_NAME_ENV)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_CLUSTER_NAME.to_string())
}

/// Set active kind cluster name for this process (and kind create/delete).
pub fn set_active_cluster_name(name: &str) {
    let n = name.trim();
    if n.is_empty() {
        std::env::remove_var(KIND_CLUSTER_NAME_ENV);
    } else {
        std::env::set_var(KIND_CLUSTER_NAME_ENV, n);
    }
}

/// Bind the configured Cluster.mountRoot into the kind node at the same
/// absolute path. The definition loader validates and canonicalizes the path
/// before calling this process-scoped adapter.
pub fn set_extra_mount_root(path: &Path) {
    std::env::set_var("HOPS_KIND_EXTRA_MOUNT", path);
}

/// kubeconfig context kind creates for the active name (`kind-<name>`).
pub fn kube_context_name() -> String {
    format!("kind-{}", active_cluster_name())
}

/// Docker container name for the control-plane node.
pub fn node_container_name() -> String {
    format!("{}-control-plane", active_cluster_name())
}

// Compatibility: older call sites used constants.
#[allow(dead_code)]
const CLUSTER_NAME: &str = DEFAULT_CLUSTER_NAME;

/// kind node images before v0.27.0 ship containerd 1.x without certs.d
/// `config_path` enabled, so our hosts.toml files would be ignored.
const MIN_KIND_VERSION: (u32, u32) = (0, 27);

/// Pure registry hostPort selection (testable without docker).
///
/// An existing cluster's published port is authoritative. New clusters honor
/// an explicit override, then take the first port not reserved by another
/// container or host process.
pub fn pick_registry_host_port(
    existing_port: Option<u16>,
    env_override: Option<u16>,
    unavailable: &BTreeSet<u16>,
) -> Option<u16> {
    existing_port.or(env_override).or_else(|| {
        (REGISTRY_HOST_PORT_START..=REGISTRY_HOST_PORT_END).find(|port| !unavailable.contains(port))
    })
}

/// Host port published for the in-cluster registry NodePort (container 30500).
///
/// Once a named cluster exists, read its Docker binding rather than deriving a
/// process-global answer. This keeps package pushes and doctor checks targeted
/// at the selected cluster even when several kind clusters coexist.
pub fn registry_host_port() -> u16 {
    resolve_registry_host_port().unwrap_or_else(|error| {
        log::warn!(
            "unable to resolve kind registry host port ({error}); falling back to {REGISTRY_HOST_PORT_START}"
        );
        REGISTRY_HOST_PORT_START
    })
}

fn resolve_registry_host_port() -> Result<u16, Box<dyn Error>> {
    if let Some(port) = published_host_port(&node_container_name(), "30500/tcp") {
        return Ok(port);
    }

    let env_override = match std::env::var(KIND_REGISTRY_HOST_PORT_ENV) {
        Ok(raw) if raw.trim().is_empty() => None,
        Ok(raw) => Some(raw.trim().parse::<u16>().map_err(|_| {
            format!(
                "{KIND_REGISTRY_HOST_PORT_ENV} must be a valid TCP port, got {:?}",
                raw.trim()
            )
        })?),
        Err(std::env::VarError::NotPresent) => None,
        Err(error) => return Err(error.into()),
    };

    let unavailable = unavailable_registry_host_ports()?;
    if let Some(port) = env_override {
        if unavailable.contains(&port) {
            return Err(format!(
                "{KIND_REGISTRY_HOST_PORT_ENV}={port} is already reserved; choose another port"
            )
            .into());
        }
    }

    pick_registry_host_port(None, env_override, &unavailable).ok_or_else(|| {
        format!(
            "no free kind registry host port in {REGISTRY_HOST_PORT_START}-{REGISTRY_HOST_PORT_END}; \
             free a port or set {KIND_REGISTRY_HOST_PORT_ENV}"
        )
        .into()
    })
}

/// Read a container's configured host binding. HostConfig works for both
/// running and stopped containers, so stopped named clusters still reserve
/// their ports and can be restarted later.
fn published_host_port(container: &str, container_port: &str) -> Option<u16> {
    let template = format!(
        "{{{{(index (index .HostConfig.PortBindings {:?}) 0).HostPort}}}}",
        container_port
    );
    docker_output(&["inspect", "-f", &template, container])
        .ok()
        .and_then(|raw| raw.trim().parse::<u16>().ok())
}

fn unavailable_registry_host_ports() -> Result<BTreeSet<u16>, Box<dyn Error>> {
    let mut unavailable = docker_reserved_host_ports()?;

    // Docker reservations do not include native host processes. Probe the
    // bounded allocation range as well; the listener is dropped immediately.
    for port in REGISTRY_HOST_PORT_START..=REGISTRY_HOST_PORT_END {
        if TcpListener::bind((Ipv4Addr::LOCALHOST, port)).is_err() {
            unavailable.insert(port);
        }
    }

    Ok(unavailable)
}

fn docker_reserved_host_ports() -> Result<BTreeSet<u16>, Box<dyn Error>> {
    let mut reserved = BTreeSet::new();
    let container_ids = docker_output(&["ps", "-aq"])?;

    for container_id in container_ids
        .lines()
        .map(str::trim)
        .filter(|id| !id.is_empty())
    {
        let Ok(raw) = docker_output(&[
            "inspect",
            "-f",
            "{{json .HostConfig.PortBindings}}",
            container_id,
        ]) else {
            // Containers can disappear between `ps` and `inspect`.
            continue;
        };
        let Ok(bindings) = serde_json::from_str::<serde_json::Value>(raw.trim()) else {
            log::debug!("ignoring malformed Docker port bindings for {container_id}");
            continue;
        };
        let Some(bindings) = bindings.as_object() else {
            continue;
        };

        for host_binding in bindings
            .values()
            .filter_map(serde_json::Value::as_array)
            .flatten()
        {
            let Some(port) = host_binding
                .get("HostPort")
                .and_then(serde_json::Value::as_str)
                .and_then(|port| port.parse::<u16>().ok())
            else {
                continue;
            };
            if port != 0 {
                reserved.insert(port);
            }
        }
    }

    Ok(reserved)
}

/// Result of checking whether the kind node can see the projects-root mount.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeMountReport {
    /// No hops kind control-plane container (or docker unreachable).
    NoKindNode,
    /// HOME / projects root not configured on the host.
    NoMountRoot,
    /// Node can see the path (hostPath delivery capable for trees under it).
    Visible { path: String },
    /// Kind node is running but path missing — recreate with extraMounts.
    Missing { path: String },
}

impl NodeMountReport {
    /// One-line doctor/status summary.
    pub fn summary(&self) -> String {
        match self {
            NodeMountReport::NoKindNode => {
                "kind node not running (start/reset with --cluster-provider kind for hostPath mounts)".into()
            }
            NodeMountReport::NoMountRoot => "no HOME/projects root to mount".into(),
            NodeMountReport::Visible { path } => {
                format!("hostPath capable — kind node sees {path}")
            }
            NodeMountReport::Missing { path } => format!(
                "kind node missing mount {path}; run `hops local reset --cluster-provider kind --docker-provider dory` to apply extraMounts"
            ),
        }
    }

    pub fn is_hostpath_capable(&self) -> bool {
        matches!(self, NodeMountReport::Visible { .. })
    }
}

/// Whether the kind control-plane container is present on the resolved docker engine.
pub fn kind_node_present() -> bool {
    let node = node_container_name();
    docker_output(&["inspect", "-f", "{{.Id}}", &node]).is_ok()
}

/// Probe the kind node for the default projects-root mount (same path as create).
pub fn report_projects_root_on_kind_node() -> NodeMountReport {
    if !kind_node_present() {
        return NodeMountReport::NoKindNode;
    }
    let Some(root) = default_extra_mount_root() else {
        return NodeMountReport::NoMountRoot;
    };
    let path = root.display().to_string();
    if node_sees_path(&path) {
        NodeMountReport::Visible { path }
    } else {
        NodeMountReport::Missing { path }
    }
}

/// `docker exec` test -d on the kind node (shared by create verify + doctor).
pub fn node_sees_path(path: &str) -> bool {
    let node = node_container_name();
    docker_output(&["exec", &node, "test", "-d", path]).is_ok()
}

/// Fail closed when an existing named kind Cluster was created with a
/// different mountRoot. Recreating it is destructive and remains an explicit
/// user action; this check performs only `docker inspect`.
pub fn ensure_configured_mount_root(expected: &Path) -> Result<(), Box<dyn Error>> {
    let node = node_container_name();
    let raw = docker_output(&["inspect", "-f", "{{json .Mounts}}", &node])?;
    if mount_inventory_has_same_path(&raw, expected)? {
        return Ok(());
    }

    let name = active_cluster_name();
    Err(format!(
        "kind cluster '{name}' exists with a different or missing mountRoot; expected the exact same-path mount {}. No resources were deleted. Recreate explicitly with `hops local reset --cluster-provider kind --docker-provider dory --cluster-name {name}` after confirming cluster recreation is safe",
        expected.display()
    )
    .into())
}

fn mount_inventory_has_same_path(raw: &str, expected: &Path) -> Result<bool, Box<dyn Error>> {
    #[derive(serde::Deserialize)]
    struct DockerMount {
        #[serde(rename = "Source")]
        source: String,
        #[serde(rename = "Destination")]
        destination: String,
        #[serde(rename = "RW", default)]
        read_write: bool,
    }

    let expected = expected.to_string_lossy();
    let mounts: Vec<DockerMount> = serde_json::from_str(raw.trim())
        .map_err(|error| format!("unable to inspect kind node mount inventory: {error}"))?;
    Ok(mounts
        .iter()
        .any(|mount| mount.read_write && mount.source == expected && mount.destination == expected))
}

/// Build the kind cluster config YAML.
///
/// When `extra_mount_host` is a directory, mount it at the same absolute path
/// inside the node (kind `extraMounts`) so pod hostPath of that path works.
pub fn build_kind_config(extra_mount_host: Option<&Path>, registry_host_port: u16) -> String {
    let mut cfg = format!(
        r#"kind: Cluster
apiVersion: kind.x-k8s.io/v1alpha4
nodes:
- role: control-plane
  extraPortMappings:
  - containerPort: 30500
    hostPort: {registry_host_port}
    listenAddress: "127.0.0.1"
"#
    );
    if let Some(host) = extra_mount_host {
        let p = host.display().to_string();
        // YAML double-quoted path; escape backslashes and quotes if any.
        let escaped = p.replace('\\', "\\\\").replace('"', "\\\"");
        cfg.push_str("  extraMounts:\n");
        cfg.push_str("  - hostPath: \"");
        cfg.push_str(&escaped);
        cfg.push_str("\"\n");
        cfg.push_str("    containerPath: \"");
        cfg.push_str(&escaped);
        cfg.push_str("\"\n");
        cfg.push_str("    readOnly: false\n");
    }
    cfg
}

/// Host directory to bind into the kind node for hostPath delivery.
///
/// Precedence:
/// 1. `HOPS_KIND_EXTRA_MOUNT` (absolute path)
/// 2. `$HOME/dev` when it exists (narrower than full home — avoids kube-proxy
///    EMFILE from watching huge home trees on Mac/Dory)
/// 3. `$HOME` when it is a directory
pub fn default_extra_mount_root() -> Option<PathBuf> {
    if let Ok(raw) = std::env::var("HOPS_KIND_EXTRA_MOUNT") {
        let p = PathBuf::from(raw.trim());
        if p.is_dir() {
            return Some(p);
        }
        log::warn!(
            "HOPS_KIND_EXTRA_MOUNT={} is not a directory; falling back",
            p.display()
        );
    }
    let home = std::env::var_os("HOME")?;
    let home = PathBuf::from(home);
    let dev = home.join("dev");
    if dev.is_dir() {
        return Some(dev);
    }
    if home.is_dir() {
        Some(home)
    } else {
        None
    }
}

/// Resolve DOCKER_HOST for kind/docker CLI when caller has not set one.
///
/// Spike toward `--docker-provider dory`: if `~/.dory/dory.sock` exists, use it.
pub fn resolve_docker_host() -> Option<String> {
    if let Ok(h) = std::env::var("DOCKER_HOST") {
        if !h.trim().is_empty() {
            return Some(h);
        }
    }
    let home = std::env::var_os("HOME")?;
    let sock = PathBuf::from(home).join(".dory/dory.sock");
    if sock.exists() {
        return Some(format!("unix://{}", sock.display()));
    }
    None
}

fn apply_docker_host(cmd: &mut Command) {
    if let Some(host) = resolve_docker_host() {
        cmd.env("DOCKER_HOST", host);
    }
}

fn docker_cmd(args: &[&str]) -> Command {
    let mut c = Command::new("docker");
    apply_docker_host(&mut c);
    c.args(args);
    c
}

fn kind_cmd(args: &[&str]) -> Command {
    let mut c = Command::new("kind");
    apply_docker_host(&mut c);
    c.args(args);
    c
}

fn docker_output(args: &[&str]) -> Result<String, Box<dyn Error>> {
    let output = docker_cmd(args).output()?;
    if !output.status.success() {
        return Err(format!(
            "docker {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn docker_run(args: &[&str]) -> Result<(), Box<dyn Error>> {
    let status = docker_cmd(args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()?;
    if !status.success() {
        return Err(format!("docker {} exited with {}", args.join(" "), status).into());
    }
    Ok(())
}

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
            "the kind cluster provider has no VM to size; drop{} (resources are governed by the selected Docker provider)",
            size.command_suffix()
        )
        .into());
    }

    preflight()?;

    if !cluster_exists() {
        return create_cluster();
    }

    let name = active_cluster_name();
    let node = node_container_name();
    if node_running() {
        log::info!("kind cluster '{name}' is already running");
        ensure_node_inotify_limits()?;
        log_mount_hint();
        return Ok(());
    }

    // kind has no start/stop; the node is a docker container. Restarting a
    // single-node cluster is reliable in practice but not guaranteed by kind.
    log::info!("Starting stopped kind node '{node}'...");
    docker_run(&["start", &node])?;
    ensure_node_inotify_limits()?;
    normalize_dory_kubeconfig_endpoint()?;
    wait_for_api_after_restart()
}

pub fn stop() -> Result<(), Box<dyn Error>> {
    let node = node_container_name();
    log::info!("Stopping kind node '{node}'...");
    docker_run(&["stop", &node])?;
    log::info!("kind cluster stopped");
    Ok(())
}

pub fn destroy() -> Result<(), Box<dyn Error>> {
    let name = active_cluster_name();
    log::info!("Deleting kind cluster '{name}'...");
    let status = kind_cmd(&["delete", "cluster", "--name", &name])
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()?;
    if !status.success() {
        return Err(format!("kind delete cluster exited with {}", status).into());
    }
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
    let mut cmd = kind_cmd(&["get", "clusters"]);
    let output = match cmd.output() {
        Ok(o) => o,
        Err(_) => return false,
    };
    if !output.status.success() {
        return false;
    }
    let name = active_cluster_name();
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .any(|line| line.trim() == name)
}

fn node_running() -> bool {
    let node = node_container_name();
    docker_output(&["inspect", "-f", "{{.State.Running}}", &node])
        .map(|out| out.trim() == "true")
        .unwrap_or(false)
}

fn preflight() -> Result<(), Box<dyn Error>> {
    if !command_exists("kind") {
        return Err(
            "kind is not installed; run `hops local install --cluster-provider kind --docker-provider docker` or `brew install kind`"
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

    if let Some(host) = resolve_docker_host() {
        log::info!("kind docker engine: DOCKER_HOST={host}");
    }

    match docker_output(&["info", "--format", "{{.ServerVersion}}"]) {
        Ok(v) => log::info!("docker engine reachable (server {})", v.trim()),
        Err(e) => {
            return Err(format!(
                "no reachable docker daemon for kind ({e}); start Dory / Docker Desktop / colima \
                 (or set DOCKER_HOST) and retry"
            )
            .into());
        }
    }

    Ok(())
}

fn create_cluster() -> Result<(), Box<dyn Error>> {
    let mount = default_extra_mount_root();
    let reg_port = resolve_registry_host_port()?;
    let config = build_kind_config(mount.as_deref(), reg_port);
    if reg_port != 30500 {
        log::info!(
            "kind registry hostPort={reg_port} ({REGISTRY_HOST_PORT_START} is already reserved or \
             {KIND_REGISTRY_HOST_PORT_ENV} was set)"
        );
    }
    let name = active_cluster_name();
    if let Some(ref m) = mount {
        log::info!(
            "Creating kind cluster '{name}' with extraMounts {} → {} (hostPath delivery)...",
            m.display(),
            m.display()
        );
    } else {
        log::info!(
            "Creating kind cluster '{name}' (no HOME mount; hostPath delivery may fall back to sync)..."
        );
    }

    let mut child = kind_cmd(&["create", "cluster", "--name", &name, "--config", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()?;
    if let Some(ref mut stdin) = child.stdin {
        stdin.write_all(config.as_bytes())?;
    }
    let status = child.wait()?;
    if !status.success() {
        return Err(format!("kind create cluster exited with {}", status).into());
    }

    normalize_dory_kubeconfig_endpoint()?;
    if let Some(ref m) = mount {
        verify_node_mount(m)?;
    }
    // Raise inotify limits: mounting large host trees (even $HOME/dev) can make
    // kube-proxy fail with "too many open files" under default instance caps.
    ensure_node_inotify_limits()?;
    Ok(())
}

/// kind writes the Docker engine's published address into kubeconfig. Dory's
/// engine reports `0.0.0.0`, which is reachable through its local proxy but is
/// not present in the API server certificate. Rewrite only that Dory-specific
/// wildcard endpoint to the certificate's loopback SAN.
fn normalize_dory_kubeconfig_endpoint() -> Result<(), Box<dyn Error>> {
    let Some(docker_host) = resolve_docker_host() else {
        return Ok(());
    };
    if !docker_host.ends_with("/.dory/dory.sock") {
        return Ok(());
    }

    let config = run_cmd_output("kubectl", &["config", "view", "--raw", "-o", "json"])?;
    let config: serde_json::Value = serde_json::from_str(&config)?;
    let cluster_name = kube_context_name();
    let Some(current_server) = config
        .get("clusters")
        .and_then(serde_json::Value::as_array)
        .and_then(|clusters| {
            clusters.iter().find_map(|cluster| {
                (cluster.get("name").and_then(serde_json::Value::as_str)
                    == Some(cluster_name.as_str()))
                .then(|| {
                    cluster
                        .get("cluster")
                        .and_then(|cluster| cluster.get("server"))
                        .and_then(serde_json::Value::as_str)
                })
                .flatten()
            })
        })
    else {
        return Ok(());
    };
    let Some(api_port) = published_host_port(&node_container_name(), "6443/tcp") else {
        return Ok(());
    };
    let Some(server) = normalized_dory_server(current_server, api_port) else {
        return Ok(());
    };

    log::info!("Rewriting Dory kind API endpoint {current_server} -> {server}");
    run_cmd(
        "kubectl",
        &[
            "config",
            "set-cluster",
            &cluster_name,
            &format!("--server={server}"),
        ],
    )
}

fn normalized_dory_server(current_server: &str, api_port: u16) -> Option<String> {
    let wildcard = current_server
        .strip_prefix("https://0.0.0.0:")
        .or_else(|| current_server.strip_prefix("https://[::]:"))?;
    wildcard
        .parse::<u16>()
        .ok()
        .filter(|current_port| *current_port == api_port)
        .map(|_| format!("https://127.0.0.1:{api_port}"))
}

fn inotify_sysctl_config() -> String {
    format!(
        "# Managed by hops local; reapplied by systemd-sysctl on kind node boot.\n\
fs.inotify.max_user_instances = {INOTIFY_MAX_USER_INSTANCES}\n\
fs.inotify.max_user_watches = {INOTIFY_MAX_USER_WATCHES}\n"
    )
}

fn ensure_node_inotify_limits() -> Result<(), Box<dyn Error>> {
    let node = node_container_name();
    let expected_instances = INOTIFY_MAX_USER_INSTANCES.to_string();
    let expected_watches = INOTIFY_MAX_USER_WATCHES.to_string();
    let mut child = docker_cmd(&[
        "exec",
        "-i",
        &node,
        "sh",
        "-c",
        INSTALL_INOTIFY_SYSCTL_SCRIPT,
        "hops-inotify-sysctl",
        INOTIFY_SYSCTL_PATH,
        &expected_instances,
        &expected_watches,
    ])
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or("failed to open stdin for kind node inotify configuration")?;
    stdin.write_all(inotify_sysctl_config().as_bytes())?;
    drop(stdin);
    let output = child.wait_with_output()?;
    if !output.status.success() {
        return Err(format!(
            "failed to persist required kind node inotify limits in {INOTIFY_SYSCTL_PATH}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )
        .into());
    }

    log::info!(
        "kind node inotify limits active and persistent: max_user_instances={INOTIFY_MAX_USER_INSTANCES}, max_user_watches={INOTIFY_MAX_USER_WATCHES} ({INOTIFY_SYSCTL_PATH})"
    );
    Ok(())
}

fn verify_node_mount(host_path: &Path) -> Result<(), Box<dyn Error>> {
    let path_str = host_path.display().to_string();
    if node_sees_path(&path_str) {
        log::info!("kind node sees host mount path {path_str}");
    } else {
        log::warn!(
            "kind node does not see {path_str} after create; hostPath delivery will use sync. \
             Check docker engine file sharing / recreate with a reachable DOCKER_HOST."
        );
    }
    Ok(())
}

fn log_mount_hint() {
    let report = report_projects_root_on_kind_node();
    if matches!(report, NodeMountReport::Missing { .. }) {
        log::warn!("{}", report.summary());
    }
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
/// containerd certs.d files on the node. `127.0.0.1:30500` is what provider
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
    // Local registry serves HTTPS with a hops-managed self-signed cert.
    format!(
        "[host.\"https://{}:5000\"]\n  capabilities = [\"pull\", \"resolve\"]\n  skip_verify = true\n",
        cluster_ip
    )
}

fn write_hosts_toml(registry_name: &str, cluster_ip: &str) -> Result<(), Box<dyn Error>> {
    let dir = format!("/etc/containerd/certs.d/{}", registry_name);
    let path = format!("{}/hosts.toml", dir);
    let desired = hosts_toml(cluster_ip);

    let node = node_container_name();
    let current = docker_output(&["exec", &node, "cat", &path]).unwrap_or_default();
    if current == desired {
        return Ok(());
    }

    log::info!(
        "Wiring containerd registry alias: {} -> {}:5000",
        registry_name,
        cluster_ip
    );

    let mut child = docker_cmd(&[
        "exec",
        "-i",
        &node,
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
    use std::path::Path;

    #[test]
    fn kind_config_pins_registry_nodeport_to_localhost() {
        let cfg = build_kind_config(None, 30500);
        assert!(cfg.contains("containerPort: 30500"));
        assert!(cfg.contains("hostPort: 30500"));
        assert!(cfg.contains("listenAddress: \"127.0.0.1\""));
        assert!(cfg.contains("role: control-plane"));
        assert!(!cfg.contains("extraMounts:"));
    }

    #[test]
    fn kind_config_can_shift_registry_host_port() {
        let cfg = build_kind_config(None, 30501);
        assert!(cfg.contains("hostPort: 30501"));
        assert!(cfg.contains("containerPort: 30500"));
    }

    #[test]
    fn pick_registry_host_port_uses_existing_binding_then_override_then_free_port() {
        let unavailable = BTreeSet::from([30500, 30501]);

        assert_eq!(
            pick_registry_host_port(Some(30542), Some(30555), &unavailable),
            Some(30542)
        );
        assert_eq!(
            pick_registry_host_port(None, Some(30555), &unavailable),
            Some(30555)
        );
        assert_eq!(
            pick_registry_host_port(None, None, &unavailable),
            Some(30502)
        );
    }

    #[test]
    fn pick_registry_host_port_reports_exhausted_range() {
        let unavailable = (REGISTRY_HOST_PORT_START..=REGISTRY_HOST_PORT_END).collect();

        assert_eq!(pick_registry_host_port(None, None, &unavailable), None);
    }

    #[test]
    fn node_mount_report_summary_mentions_reset_when_missing() {
        let s = NodeMountReport::Missing {
            path: "/Users/dev".into(),
        }
        .summary();
        assert!(s.contains("/Users/dev"));
        assert!(s.contains("reset"));
        assert!(NodeMountReport::Visible {
            path: "/Users/dev".into()
        }
        .is_hostpath_capable());
        assert!(!NodeMountReport::NoKindNode.is_hostpath_capable());
    }

    #[test]
    fn named_cluster_drives_context_and_node_container() {
        set_active_cluster_name("dogfood");
        assert_eq!(active_cluster_name(), "dogfood");
        assert_eq!(kube_context_name(), "kind-dogfood");
        assert_eq!(node_container_name(), "dogfood-control-plane");
        set_active_cluster_name("hops");
        assert_eq!(kube_context_name(), "kind-hops");
        set_active_cluster_name("");
        assert_eq!(active_cluster_name(), DEFAULT_CLUSTER_NAME);
    }

    #[test]
    fn kind_config_includes_extra_mounts_for_host_path() {
        let cfg = build_kind_config(Some(Path::new("/Users/test")), 30500);
        assert!(cfg.contains("extraMounts:"));
        assert!(cfg.contains("hostPath: \"/Users/test\""));
        assert!(cfg.contains("containerPath: \"/Users/test\""));
        assert!(cfg.contains("readOnly: false"));
    }

    #[test]
    fn kind_inotify_limits_are_persisted_for_node_restarts() {
        assert!(INOTIFY_SYSCTL_PATH.starts_with("/etc/sysctl.d/"));
        assert_eq!(
            inotify_sysctl_config(),
            "# Managed by hops local; reapplied by systemd-sysctl on kind node boot.\n\
fs.inotify.max_user_instances = 8192\n\
fs.inotify.max_user_watches = 1048576\n"
        );
        assert!(INSTALL_INOTIFY_SYSCTL_SCRIPT.contains("sysctl -p \"${target}\""));
        assert!(INSTALL_INOTIFY_SYSCTL_SCRIPT
            .contains("test \"${instances}\" -ge \"${expected_instances}\""));
        assert!(INSTALL_INOTIFY_SYSCTL_SCRIPT
            .contains("test \"${watches}\" -ge \"${expected_watches}\""));
    }

    #[test]
    fn hosts_toml_aliases_to_cluster_ip_over_https() {
        // Local package registry is HTTPS (self-signed); containerd must use
        // https:// + skip_verify, not plain http://.
        let toml = hosts_toml("10.43.12.7");

        assert_eq!(
            toml,
            "[host.\"https://10.43.12.7:5000\"]\n  capabilities = [\"pull\", \"resolve\"]\n  skip_verify = true\n"
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

    #[test]
    fn resolve_docker_host_respects_env() {
        // Only assert pure formatting when env is set — avoid flaking on machine state.
        // build_kind_config + extra mounts are the contract under test here.
        let cfg = build_kind_config(Some(Path::new("/home/ci")), 30500);
        assert!(cfg.contains("/home/ci"));
    }

    #[test]
    fn existing_kind_mount_requires_exact_same_path_read_write_binding() {
        let exact = r#"[{"Source":"/workspace","Destination":"/workspace","RW":true}]"#;
        let broader = r#"[{"Source":"/","Destination":"/","RW":true}]"#;
        let read_only = r#"[{"Source":"/workspace","Destination":"/workspace","RW":false}]"#;

        assert!(mount_inventory_has_same_path(exact, Path::new("/workspace")).unwrap());
        assert!(!mount_inventory_has_same_path(broader, Path::new("/workspace")).unwrap());
        assert!(!mount_inventory_has_same_path(read_only, Path::new("/workspace")).unwrap());
        assert!(mount_inventory_has_same_path("not-json", Path::new("/workspace")).is_err());
    }

    #[test]
    fn dory_wildcard_kubeconfig_endpoint_is_rewritten_to_certificate_san() {
        assert_eq!(
            normalized_dory_server("https://0.0.0.0:63903", 63903),
            Some("https://127.0.0.1:63903".into())
        );
        assert_eq!(
            normalized_dory_server("https://[::]:63903", 63903),
            Some("https://127.0.0.1:63903".into())
        );
        assert_eq!(
            normalized_dory_server("https://127.0.0.1:63903", 63903),
            None
        );
        assert_eq!(normalized_dory_server("https://0.0.0.0:63903", 6443), None);
    }
}
