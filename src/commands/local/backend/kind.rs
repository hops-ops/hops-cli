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
//! (`hops local reset --backend kind`).
//!
//! ## Docker engine selection (spike toward --docker-provider)
//!
//! Prefer explicit `DOCKER_HOST` / active docker context. If unset and
//! `~/.dory/dory.sock` exists, kind commands use that socket (kind-on-Dory).

use super::SizeArgs;
use crate::commands::local::package_install::{REGISTRY_PULL, REGISTRY_PUSH};
use crate::commands::local::{command_exists, run_cmd, run_cmd_output};
use std::error::Error;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

/// Default kind cluster name (and historical hard-coded value).
pub const DEFAULT_CLUSTER_NAME: &str = "hops";
const KIND_CLUSTER_NAME_ENV: &str = "HOPS_KIND_CLUSTER_NAME";

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
/// When kind shares a docker engine with product Dory k8s, host 30500 is often
/// already bound — use 30501 so create succeeds (LWB-REQ-254).
pub fn pick_registry_host_port(env_override: Option<u16>, dory_k8s_present: bool) -> u16 {
    if let Some(p) = env_override {
        return p;
    }
    if dory_k8s_present {
        return 30501;
    }
    30500
}

/// Host port published for the in-cluster registry NodePort (container 30500).
pub fn registry_host_port() -> u16 {
    let env_override = std::env::var("HOPS_KIND_REGISTRY_HOST_PORT")
        .ok()
        .and_then(|raw| raw.trim().parse::<u16>().ok());
    let dory_k8s = docker_output(&["inspect", "-f", "{{.Id}}", "dory-k8s"]).is_ok();
    pick_registry_host_port(env_override, dory_k8s)
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
                "kind node not running (start/reset --backend kind for hostPath mounts)".into()
            }
            NodeMountReport::NoMountRoot => "no HOME/projects root to mount".into(),
            NodeMountReport::Visible { path } => {
                format!("hostPath capable — kind node sees {path}")
            }
            NodeMountReport::Missing { path } => format!(
                "kind node missing mount {path}; run `hops local reset --backend kind` to apply extraMounts"
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
            "the kind backend has no VM to size; drop{} (resources are governed by the docker daemon kind runs on)",
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
        log_mount_hint();
        return Ok(());
    }

    // kind has no start/stop; the node is a docker container. Restarting a
    // single-node cluster is reliable in practice but not guaranteed by kind.
    log::info!("Starting stopped kind node '{node}'...");
    docker_run(&["start", &node])?;
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
    let reg_port = registry_host_port();
    let config = build_kind_config(mount.as_deref(), reg_port);
    if reg_port != 30500 {
        log::info!(
            "kind registry hostPort={reg_port} (30500 busy or HOPS_KIND_REGISTRY_HOST_PORT set; \
             package push may need the same port)"
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

    if let Some(ref m) = mount {
        verify_node_mount(m)?;
    }
    // Raise inotify limits: mounting large host trees (even $HOME/dev) can make
    // kube-proxy fail with "too many open files" under default instance caps.
    raise_node_inotify_limits();
    Ok(())
}

fn raise_node_inotify_limits() {
    let node = node_container_name();
    let script = "sysctl -w fs.inotify.max_user_instances=8192 fs.inotify.max_user_watches=1048576 >/dev/null 2>&1 || true";
    match docker_output(&["exec", &node, "sh", "-c", script]) {
        Ok(_) => log::info!("raised kind node inotify limits for host mounts"),
        Err(e) => log::debug!("inotify sysctl skipped: {e}"),
    }
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
    fn pick_registry_host_port_shifts_when_dory_k8s_present() {
        assert_eq!(pick_registry_host_port(None, false), 30500);
        assert_eq!(pick_registry_host_port(None, true), 30501);
        assert_eq!(pick_registry_host_port(Some(30555), true), 30555);
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
}

