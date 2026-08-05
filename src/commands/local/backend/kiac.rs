//! kiac backend: Kubernetes on Apple's container runtime (apple/container).
//!
//! Each node is a lightweight VM (not a docker container). Cluster is managed
//! by the `kiac` CLI: https://github.com/saiyam1814/kiac
//!
//! Default cluster name is `hops` → kube context `kiac-hops`.
//! Uses the default **kubeadm** distro (kindest/node + kindnet) for a
//! single-node control plane. k3s is available via kiac itself but hops pins
//! kubeadm for registry wiring (containerd certs.d) and create reliability.
//!
//! ## Host reachability
//!
//! When apple/container's vmnet does not make node IPs host-reachable (common
//! when Local Network permission is missing or after some system restarts),
//! hops publishes the apiserver through a small `socat` proxy on
//! `127.0.0.1:16443` and rewrites the `kiac-hops` kubeconfig server. Inter-
//! container networking still works, so the proxy can reach the real node IP.
//!
//! ## apple/container version
//!
//! kiac 0.4.0 + container 1.2.0 fails at node boot with
//! `sysctl: permission denied on key "net.ipv4.ip_forward"`
//! (https://github.com/saiyam1814/kiac/issues/14). Prefer container **1.1.0**
//! until that is fixed.
//!
//! Package registry:
//! - Crossplane pulls via in-cluster Service DNS (same as other backends).
//! - Host docker push uses control-plane node IP:30500 when that IP is
//!   host-reachable; otherwise path-installs may need published packages.

use super::SizeArgs;
use crate::commands::local::package_install::{REGISTRY_PULL, REGISTRY_PUSH};
use crate::commands::local::{command_exists, run_cmd, run_cmd_output};
use std::error::Error;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

/// hops-owned cluster name (context becomes `kiac-hops`).
pub const CLUSTER_NAME: &str = "hops";
const REGISTRY_NODE_PORT: &str = "30500";
/// Host-side published port for apiserver when node IPs are not reachable.
const API_PROXY_HOST_PORT: u16 = 16443;
const API_PROXY_CONTAINER: &str = "hops-kiac-api-proxy";
const API_PROXY_IMAGE: &str = "docker.io/alpine/socat:latest";

pub fn install() -> Result<(), Box<dyn Error>> {
    if !cfg!(target_os = "macos") {
        return Err("the kiac backend requires macOS on Apple silicon".into());
    }
    log::info!("Installing kiac via Homebrew...");
    // Tap is required the first time; ignore if already present.
    let _ = run_cmd("brew", &["tap", "saiyam1814/tap"]);
    run_cmd("brew", &["install", "saiyam1814/tap/kiac"])?;
    if !command_exists("container") {
        log::info!("Installing apple/container via Homebrew...");
        run_cmd("brew", &["install", "container"])?;
        log::warn!(
            "brew currently ships container 1.2.x, which breaks kiac node boot \
             (sysctl ip_forward — saiyam1814/kiac#14). Prefer apple/container 1.1.0 \
             until that is fixed."
        );
    }
    log::info!("kiac installed; run `kiac doctor --fix` if the container system is stopped");
    Ok(())
}

pub fn uninstall() -> Result<(), Box<dyn Error>> {
    log::info!("Uninstalling kiac...");
    run_cmd("brew", &["uninstall", "saiyam1814/tap/kiac"])?;
    log::info!("kiac uninstalled (apple/container left in place)");
    Ok(())
}

pub fn start(size: &SizeArgs) -> Result<(), Box<dyn Error>> {
    preflight()?;

    if cluster_exists() {
        log::info!(
            "kiac cluster '{}' exists; resuming if needed...",
            CLUSTER_NAME
        );
        // Idempotent: no-op when VMs are already running.
        // Ignore non-zero if kiac warns about host reachability after a healthy resume.
        let _ = run_cmd(
            "kiac",
            &["resume", "cluster", "--name", CLUSTER_NAME, "--wait", "10m"],
        );
    } else {
        create_cluster(size)?;
    }

    ensure_host_api_access()?;
    Ok(())
}

pub fn stop() -> Result<(), Box<dyn Error>> {
    // Drop host proxy first so we don't leave a stale forwarder.
    let _ = remove_api_proxy();
    if !cluster_exists() {
        log::info!("kiac cluster '{}' not found", CLUSTER_NAME);
        return Ok(());
    }
    // kiac has no cluster-level stop; stop every node VM.
    let nodes = node_names()?;
    if nodes.is_empty() {
        log::info!("no nodes listed for kiac cluster '{}'", CLUSTER_NAME);
        return Ok(());
    }
    for node in nodes {
        log::info!("Stopping kiac node '{}'...", node);
        let _ = run_cmd("kiac", &["stop", "node", &node, "--name", CLUSTER_NAME]);
    }
    log::info!("kiac cluster '{}' nodes stopped", CLUSTER_NAME);
    Ok(())
}

pub fn destroy() -> Result<(), Box<dyn Error>> {
    let _ = remove_api_proxy();
    if !cluster_exists() {
        log::info!("kiac cluster '{}' not found", CLUSTER_NAME);
        return Ok(());
    }
    log::info!("Deleting kiac cluster '{}'...", CLUSTER_NAME);
    run_cmd("kiac", &["delete", "cluster", "--name", CLUSTER_NAME])?;
    log::info!("kiac cluster deleted");
    Ok(())
}

pub fn reset() -> Result<(), Box<dyn Error>> {
    preflight()?;
    if cluster_exists() {
        destroy()?;
    }
    create_cluster(&SizeArgs::default())?;
    ensure_host_api_access()?;
    Ok(())
}

pub fn resize(size: &SizeArgs) -> Result<(), Box<dyn Error>> {
    Err(format!(
        "kiac node size is set at create time{}; run `hops local destroy --backend kiac` \
         then `hops local start --backend kiac{}` to recreate",
        size.command_suffix(),
        size.command_suffix()
    )
    .into())
}

pub fn cluster_exists() -> bool {
    if !command_exists("kiac") {
        return false;
    }
    run_cmd_output("kiac", &["get", "clusters", "-o", "json"])
        .ok()
        .and_then(|out| parse_cluster_names_json(&out))
        .map(|names| names.iter().any(|n| n == CLUSTER_NAME))
        .or_else(|| {
            run_cmd_output("kiac", &["get", "clusters"]).ok().map(|out| {
                out.lines().any(|line| {
                    let t = line.trim();
                    t == CLUSTER_NAME || t.split_whitespace().next() == Some(CLUSTER_NAME)
                })
            })
        })
        .unwrap_or(false)
}

/// containerd certs.d is written in [`wire_registry`].
pub fn ensure_registry_trust() -> Result<(), Box<dyn Error>> {
    Ok(())
}

/// Point node containerd at the in-cluster registry Service over HTTPS
/// (skip_verify) via certs.d hosts.toml — same model as the kind backend.
pub fn wire_registry(cluster_ip: &str) -> Result<(), Box<dyn Error>> {
    // Pull name (in-cluster) + push name(s) so pod and host refs both resolve.
    let push = registry_push_addr().unwrap_or_else(|_| REGISTRY_PUSH.to_string());
    for name in [REGISTRY_PULL, REGISTRY_PUSH, push.as_str()] {
        write_hosts_toml(name, cluster_ip)?;
    }
    if let Ok(push) = registry_push_addr() {
        let _ = ensure_host_docker_hint(&push);
    }
    Ok(())
}

/// Host docker push address: control-plane node IP + NodePort 30500.
pub fn registry_push_addr() -> Result<String, Box<dyn Error>> {
    Ok(format!("{}:{}", control_plane_ip()?, REGISTRY_NODE_PORT))
}

fn preflight() -> Result<(), Box<dyn Error>> {
    if !cfg!(target_os = "macos") {
        return Err("the kiac backend requires macOS on Apple silicon".into());
    }
    if !command_exists("kiac") {
        return Err(
            "kiac is not installed; run `hops local install --backend kiac` or \
             `brew install saiyam1814/tap/kiac`"
                .into(),
        );
    }
    if !command_exists("container") {
        return Err(
            "apple/container CLI is not installed; run `brew install container` then \
             `container system start` (or `kiac doctor --fix`)"
                .into(),
        );
    }
    warn_if_broken_container_version();
    // Start container system if needed (kiac create also does this).
    let _ = run_cmd("kiac", &["doctor", "--fix"]);
    Ok(())
}

fn warn_if_broken_container_version() {
    // `container system status` prints apiserver.version; fall back to installRoot path.
    let status = run_cmd_output("container", &["system", "status"]).unwrap_or_default();
    if status.contains("1.2.") {
        log::warn!(
            "apple/container 1.2.x is active; kiac node boot often fails with \
             sysctl ip_forward permission denied (saiyam1814/kiac#14). \
             Pin apple/container 1.1.0 until fixed."
        );
    }
}

fn create_cluster(size: &SizeArgs) -> Result<(), Box<dyn Error>> {
    log::info!(
        "Creating kiac cluster '{}' (kubeadm, single-node)...",
        CLUSTER_NAME
    );
    // Default distro is kubeadm (kindest/node). Do not pass --distro k3s:
    // hops wires registry trust via containerd certs.d, and kubeadm create has
    // been more reliable with current kiac releases.
    let mut args = vec![
        "create".to_string(),
        "cluster".to_string(),
        "--name".to_string(),
        CLUSTER_NAME.to_string(),
        "--workers".to_string(),
        "0".to_string(),
        "--wait".to_string(),
        "10m".to_string(),
    ];
    if let Some(cpus) = size.cpus {
        args.push("--cpus".into());
        args.push(cpus.to_string());
    }
    if let Some(memory) = size.memory {
        // Single-node: all addons on control plane → cp-memory.
        args.push("--cp-memory".into());
        args.push(format!("{memory}G"));
    }
    if size.disk.is_some() {
        log::warn!("kiac has no --disk flag; ignoring --disk");
    }

    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    // kiac exits non-zero when the cluster is Ready but Mac→node IP fails.
    // Capture that: if the cluster exists afterward, continue to host API fixup.
    match run_cmd("kiac", &arg_refs) {
        Ok(()) => {}
        Err(e) if cluster_exists() => {
            log::warn!(
                "kiac create reported an error but cluster '{}' exists ({e}); \
                 continuing with host API access fixup",
                CLUSTER_NAME
            );
        }
        Err(e) => return Err(e),
    }
    log::info!(
        "kiac cluster '{}' ready (kube context: kiac-{})",
        CLUSTER_NAME,
        CLUSTER_NAME
    );
    Ok(())
}

/// Ensure kubectl on the Mac can reach the apiserver.
///
/// Prefer direct node InternalIP:6443. If that is not host-routable, run a
/// published-port socat proxy and point the `kiac-hops` kubeconfig at it.
fn ensure_host_api_access() -> Result<(), Box<dyn Error>> {
    let ip = control_plane_ip()?;
    if tcp_reachable(&format!("{ip}:6443"), Duration::from_secs(2)) {
        log::info!("kiac apiserver reachable at {ip}:6443");
        // Prefer direct endpoint if a previous run left a proxy in place.
        point_kubeconfig_at_server(&format!("https://{ip}:6443"), None)?;
        let _ = remove_api_proxy();
        return Ok(());
    }

    log::warn!(
        "Mac cannot reach kiac control-plane at {ip}:6443; \
         starting localhost API proxy on 127.0.0.1:{API_PROXY_HOST_PORT} \
         (allow Local Network for your terminal if prompted, or run: \
         container system stop && container system start && kiac resume cluster --name {CLUSTER_NAME})"
    );
    ensure_api_proxy(&ip)?;
    let proxy = format!("https://127.0.0.1:{API_PROXY_HOST_PORT}");
    point_kubeconfig_at_server(&proxy, Some("kubernetes"))?;

    // Prove kubectl path works.
    for _ in 0..30 {
        if run_cmd_output("kubectl", &["--context", &context_name(), "get", "--raw", "/readyz"])
            .map(|s| s.contains("ok"))
            .unwrap_or(false)
        {
            log::info!("kiac apiserver reachable via {proxy}");
            return Ok(());
        }
        thread::sleep(Duration::from_secs(1));
    }
    Err(format!(
        "kiac apiserver not reachable on node IP or via localhost:{API_PROXY_HOST_PORT} proxy"
    )
    .into())
}

fn context_name() -> String {
    format!("kiac-{CLUSTER_NAME}")
}

fn ensure_api_proxy(node_ip: &str) -> Result<(), Box<dyn Error>> {
    // Recreate when missing or when target IP may have changed after resume.
    let _ = remove_api_proxy();
    log::info!(
        "Starting {API_PROXY_CONTAINER}: 127.0.0.1:{API_PROXY_HOST_PORT} → {node_ip}:6443"
    );
    run_cmd(
        "container",
        &[
            "run",
            "-d",
            "--name",
            API_PROXY_CONTAINER,
            "-p",
            &format!("{API_PROXY_HOST_PORT}:6443"),
            API_PROXY_IMAGE,
            &format!("TCP-LISTEN:6443,fork,reuseaddr"),
            &format!("TCP:{node_ip}:6443"),
        ],
    )?;
    // Wait for host published port.
    let addr = format!("127.0.0.1:{API_PROXY_HOST_PORT}");
    for _ in 0..30 {
        if tcp_reachable(&addr, Duration::from_secs(1)) {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(500));
    }
    Err(format!("API proxy did not open {addr}").into())
}

fn remove_api_proxy() -> Result<(), Box<dyn Error>> {
    let _ = run_cmd("container", &["stop", API_PROXY_CONTAINER]);
    let _ = run_cmd("container", &["rm", "-f", API_PROXY_CONTAINER]);
    Ok(())
}

/// Rewrite the kiac-hops cluster server while preserving CA from the node.
fn point_kubeconfig_at_server(
    server: &str,
    tls_server_name: Option<&str>,
) -> Result<(), Box<dyn Error>> {
    let ctx = context_name();
    // CA from node admin.conf (authoritative after create/resume).
    let ca_b64 = container_exec_output(
        &control_plane_container(),
        &[
            "sh",
            "-c",
            "grep 'certificate-authority-data:' /etc/kubernetes/admin.conf | awk '{print $2}'",
        ],
    )?
    .trim()
    .to_string();
    if ca_b64.is_empty() {
        return Err("could not read certificate-authority-data from control-plane admin.conf".into());
    }

    let ca_bytes = base64_decode(&ca_b64)?;
    let tmp = std::env::temp_dir().join("hops-kiac-ca.crt");
    std::fs::write(&tmp, ca_bytes)?;

    let mut args = vec![
        "config".to_string(),
        "set-cluster".to_string(),
        ctx.clone(),
        format!("--server={server}"),
        format!("--certificate-authority={}", tmp.display()),
        "--embed-certs=true".to_string(),
    ];
    if let Some(sni) = tls_server_name {
        args.push(format!("--tls-server-name={sni}"));
    }
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    run_cmd("kubectl", &arg_refs)?;
    // Clear any leftover insecure flag from earlier experiments.
    let _ = run_cmd(
        "kubectl",
        &[
            "config",
            "unset",
            &format!("clusters.{ctx}.insecure-skip-tls-verify"),
        ],
    );
    Ok(())
}

fn tcp_reachable(addr: &str, timeout: Duration) -> bool {
    let Ok(sock_addr) = addr.parse() else {
        return false;
    };
    TcpStream::connect_timeout(&sock_addr, timeout).is_ok()
}

fn parse_cluster_names_json(out: &str) -> Option<Vec<String>> {
    let v: serde_json::Value = serde_json::from_str(out).ok()?;
    // Accept either ["hops", ...] or [{ "name": "hops" }, ...]
    if let Some(arr) = v.as_array() {
        let mut names = Vec::new();
        for item in arr {
            if let Some(s) = item.as_str() {
                names.push(s.to_string());
            } else if let Some(n) = item.get("name").and_then(|x| x.as_str()) {
                names.push(n.to_string());
            }
        }
        return Some(names);
    }
    if let Some(arr) = v.get("items").and_then(|x| x.as_array()) {
        let mut names = Vec::new();
        for item in arr {
            if let Some(n) = item.get("name").and_then(|x| x.as_str()) {
                names.push(n.to_string());
            }
        }
        return Some(names);
    }
    None
}

fn node_names() -> Result<Vec<String>, Box<dyn Error>> {
    let out = run_cmd_output("kiac", &["get", "nodes", "--name", CLUSTER_NAME])?;
    let mut names = Vec::new();
    for line in out.lines() {
        let t = line.trim();
        if t.is_empty() || t.to_lowercase().starts_with("name") {
            continue;
        }
        if let Some(name) = t.split_whitespace().next() {
            names.push(name.to_string());
        }
    }
    Ok(names)
}

fn control_plane_container() -> String {
    format!("kiac-{CLUSTER_NAME}-control-plane")
}

fn control_plane_ip() -> Result<String, Box<dyn Error>> {
    // 1) From running container list (works without kubectl).
    if let Ok(ip) = container_ip(&control_plane_container()) {
        return Ok(ip);
    }
    // 2) kubectl node InternalIP (works once API is up on current kubeconfig).
    let from_k8s = run_cmd_output(
        "kubectl",
        &[
            "get",
            "nodes",
            "-o",
            "jsonpath={.items[0].status.addresses[?(@.type==\"InternalIP\")].address}",
        ],
    );
    if let Ok(ip) = from_k8s {
        let ip = ip.trim();
        if !ip.is_empty() {
            return Ok(ip.to_string());
        }
    }
    Err("could not determine kiac control-plane InternalIP".into())
}

fn container_ip(name: &str) -> Result<String, Box<dyn Error>> {
    // `container list` table includes IP as CIDR; parse inspect JSON when possible.
    let inspect = run_cmd_output("container", &["inspect", name])?;
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&inspect) {
        // Shape varies; search for first 192.168.64.x or any IPv4 in networks.
        if let Some(ip) = find_ipv4_in_json(&v) {
            return Ok(ip);
        }
    }
    // Fallback: list table
    let list = run_cmd_output("container", &["list"])?;
    for line in list.lines() {
        if line.contains(name) {
            for tok in line.split_whitespace() {
                if let Some(ip) = tok.split('/').next() {
                    if ip.parse::<std::net::Ipv4Addr>().is_ok() {
                        return Ok(ip.to_string());
                    }
                }
            }
        }
    }
    Err(format!("no IP for container {name}").into())
}

fn find_ipv4_in_json(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::String(s) => {
            let s = s.split('/').next().unwrap_or(s);
            if s.parse::<std::net::Ipv4Addr>().is_ok() && s != "0.0.0.0" && s != "127.0.0.1" {
                return Some(s.to_string());
            }
            None
        }
        serde_json::Value::Array(a) => a.iter().find_map(find_ipv4_in_json),
        serde_json::Value::Object(m) => {
            // Prefer explicit address fields.
            for key in ["address", "ip", "IPAddress", "ipv4"] {
                if let Some(ip) = m.get(key).and_then(find_ipv4_in_json) {
                    return Some(ip);
                }
            }
            m.values().find_map(find_ipv4_in_json)
        }
        _ => None,
    }
}

fn write_hosts_toml(registry_name: &str, cluster_ip: &str) -> Result<(), Box<dyn Error>> {
    let dir = format!("/etc/containerd/certs.d/{registry_name}");
    let path = format!("{dir}/hosts.toml");
    let desired = format!(
        "[host.\"https://{cluster_ip}:5000\"]\n  capabilities = [\"pull\", \"resolve\"]\n  skip_verify = true\n"
    );

    let container = control_plane_container();
    let current = container_exec_output(&container, &["sh", "-c", &format!("cat {path} 2>/dev/null || true")])
        .unwrap_or_default();
    if current == desired {
        return Ok(());
    }

    log::info!("Wiring containerd registry alias: {registry_name} -> {cluster_ip}:5000");
    let b64 = base64_encode(desired.as_bytes());
    container_exec(
        &container,
        &[
            "sh",
            "-c",
            &format!("mkdir -p '{dir}' && echo {b64} | base64 -d > '{path}'"),
        ],
    )?;
    Ok(())
}

fn ensure_host_docker_hint(push_hostport: &str) -> Result<(), Box<dyn Error>> {
    if !command_exists("docker") {
        log::warn!(
            "docker CLI not found; package path installs need a host docker daemon that can \
             reach {push_hostport} (kiac NodePort). Install Docker Desktop or use published packages."
        );
        return Ok(());
    }
    if run_cmd_output("docker", &["info", "--format", "{{.ServerVersion}}"]).is_err() {
        log::warn!(
            "no reachable host docker daemon; config install --path push to {push_hostport} may fail"
        );
        return Ok(());
    }
    log::info!(
        "package push endpoint for kiac is {push_hostport} (NodePort on control-plane). \
         Ensure docker trusts this host for HTTPS (self-signed) or mark it insecure."
    );
    Ok(())
}

fn container_exec(container: &str, cmd: &[&str]) -> Result<(), Box<dyn Error>> {
    // apple/container: `container exec <id> <args...>` (no `--` separator).
    let mut args = vec!["exec", container];
    args.extend_from_slice(cmd);
    run_cmd("container", &args)
}

fn container_exec_output(container: &str, cmd: &[&str]) -> Result<String, Box<dyn Error>> {
    let mut args = vec!["exec", container];
    args.extend_from_slice(cmd);
    run_cmd_output("container", &args)
}

/// Minimal base64 encoder (no extra crate) for writing files via shell.
fn base64_encode(data: &[u8]) -> String {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    let mut i = 0;
    while i < data.len() {
        let b0 = data[i] as u32;
        let b1 = if i + 1 < data.len() {
            data[i + 1] as u32
        } else {
            0
        };
        let b2 = if i + 2 < data.len() {
            data[i + 2] as u32
        } else {
            0
        };
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((triple >> 18) & 63) as usize] as char);
        out.push(TABLE[((triple >> 12) & 63) as usize] as char);
        if i + 1 < data.len() {
            out.push(TABLE[((triple >> 6) & 63) as usize] as char);
        } else {
            out.push('=');
        }
        if i + 2 < data.len() {
            out.push(TABLE[(triple & 63) as usize] as char);
        } else {
            out.push('=');
        }
        i += 3;
    }
    out
}

fn base64_decode(s: &str) -> Result<Vec<u8>, Box<dyn Error>> {
    // Use host base64 for decode reliability (stdin).
    let mut child = Command::new("base64")
        .args(["-d"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(s.as_bytes())?;
    }
    let mut stdout = Vec::new();
    if let Some(mut out) = child.stdout.take() {
        out.read_to_end(&mut stdout)?;
    }
    let status = child.wait()?;
    if !status.success() || stdout.is_empty() {
        return Err("base64 -d failed for kube CA".into());
    }
    Ok(stdout)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_plane_container_name() {
        assert_eq!(control_plane_container(), "kiac-hops-control-plane");
    }

    #[test]
    fn parse_cluster_json_string_array() {
        let names = parse_cluster_names_json(r#"["hops","dev"]"#).unwrap();
        assert!(names.contains(&"hops".to_string()));
    }

    #[test]
    fn parse_cluster_json_object_array() {
        let names = parse_cluster_names_json(r#"[{"name":"hops"},{"name":"dev"}]"#).unwrap();
        assert_eq!(names[0], "hops");
    }

    #[test]
    fn base64_roundtrip_shape() {
        let s = base64_encode(b"hello");
        assert_eq!(s, "aGVsbG8=");
    }

    #[test]
    fn start_rejects_nothing_when_no_size() {
        assert!(!SizeArgs::default().any_set());
    }

    #[test]
    fn find_ipv4_in_nested_json() {
        let v: serde_json::Value = serde_json::from_str(
            r#"[{"configuration":{"networks":[{"address":"192.168.64.3/24"}]}}]"#,
        )
        .unwrap();
        assert_eq!(find_ipv4_in_json(&v).as_deref(), Some("192.168.64.3"));
    }
}
