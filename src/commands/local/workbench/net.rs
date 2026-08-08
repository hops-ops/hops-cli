//! Host access: plan URLs **and** start/stop port-forwards / cluster DNS.
//!
//! Modes (preferred first):
//! - **dns** — hops-native kubefwd-class: loopback IPs + `/etc/hosts` + port-forward
//!   on real service ports so `svc.ns.svc.cluster.local` works on the host
//! - **map** — unique `127.0.0.1:18xxx` ports (no sudo)
//! - **kubefwd** — external binary (optional legacy)
//!
//! Map/dns port-forwards die on pod rollouts; [`ensure_host_access`] restarts them.

use super::cluster_dns::{
    self, build_dns_port_forward_args, format_dns_url, rebuild_hosts_from_blocks,
    remove_loopback_aliases, sync_alloc_for_namespace,
};
use super::registry::WorkspaceRecord;
use crate::commands::local::kubectl_command;
use std::collections::BTreeMap;
use std::error::Error;
use std::fs;
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

/// Default starting port for map-mode allocation (avoids privileged + common dev ports).
pub const MAP_PORT_BASE_START: u16 = 18000;
/// Ports reserved per workspace in map mode (stride).
pub const MAP_PORT_STRIDE: u16 = 100;
/// Runtime state subdir under local state dir.
pub const RUNTIME_SUBDIR: &str = "runtime";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostAccessMode {
    /// Hops-native cluster DNS (hosts + loopback IP + port-forward on service ports).
    Dns,
    /// External kubefwd binary (optional).
    Kubefwd,
    /// Unique localhost ports via port-forward map (no sudo).
    Map,
}

impl HostAccessMode {
    pub fn as_str(self) -> &'static str {
        match self {
            HostAccessMode::Dns => "dns",
            HostAccessMode::Kubefwd => "kubefwd",
            HostAccessMode::Map => "map",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "dns" | "cluster-dns" | "cluster" => Some(HostAccessMode::Dns),
            "kubefwd" => Some(HostAccessMode::Kubefwd),
            "map" => Some(HostAccessMode::Map),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceEndpoint {
    pub name: String,
    pub port: u16,
    pub protocol: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostAccessPlan {
    pub mode: HostAccessMode,
    pub namespace: String,
    /// Service name → URL for host browser/curl.
    pub urls: BTreeMap<String, String>,
    /// Map-mode only: service → host port.
    pub port_map: BTreeMap<String, u16>,
    pub port_base: Option<u16>,
    /// Dns mode: service → loopback IP (127.53.x.y).
    pub ip_map: BTreeMap<String, String>,
}

impl Default for HostAccessPlan {
    fn default() -> Self {
        Self {
            mode: HostAccessMode::Map,
            namespace: String::new(),
            urls: BTreeMap::new(),
            port_map: BTreeMap::new(),
            port_base: None,
            ip_map: BTreeMap::new(),
        }
    }
}

/// Started host-access processes for a workspace.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct HostAccessRuntime {
    pub mode: String,
    pub namespace: String,
    pub pids: Vec<u32>,
    /// Optional log path.
    #[serde(default)]
    pub log_path: Option<String>,
    /// Map mode: service name → host port (needed to re-heal without re-planning).
    #[serde(default)]
    pub port_map: BTreeMap<String, u16>,
    /// Map/dns: service name → service port inside the cluster.
    #[serde(default)]
    pub service_ports: BTreeMap<String, u16>,
    /// Dns mode: service → loopback bind IP.
    #[serde(default)]
    pub ip_map: BTreeMap<String, String>,
}

/// Format kubefwd-style URL for a service in a namespace.
pub fn format_kubefwd_url(service: &str, namespace: &str, port: u16) -> String {
    format!("http://{service}.{namespace}.svc.cluster.local:{port}")
}

/// Format map-mode localhost URL.
pub fn format_map_url(host_port: u16) -> String {
    format!("http://127.0.0.1:{host_port}")
}

/// Allocate a non-overlapping port base for a new workspace given existing records.
pub fn allocate_port_base(existing: &[WorkspaceRecord]) -> u16 {
    let mut used: Vec<u16> = existing.iter().filter_map(|r| r.port_base).collect();
    used.sort_unstable();
    let mut candidate = MAP_PORT_BASE_START;
    for base in used {
        if candidate == base {
            candidate = base.saturating_add(MAP_PORT_STRIDE);
        } else if candidate < base {
            break;
        }
    }
    candidate
}

/// Plan host access for services in a workspace namespace.
///
/// `prefer` selects mode when possible; dns needs IPs from `ip_map` (pass empty
/// to get placeholder URLs until start allocates IPs).
pub fn plan_host_access(
    namespace: &str,
    services: &[ServiceEndpoint],
    prefer_cluster_dns: bool,
    port_base: u16,
) -> HostAccessPlan {
    plan_host_access_mode(
        namespace,
        services,
        if prefer_cluster_dns {
            HostAccessMode::Dns
        } else {
            HostAccessMode::Map
        },
        port_base,
        &BTreeMap::new(),
    )
}

/// Plan with an explicit mode and optional pre-allocated dns IPs.
pub fn plan_host_access_mode(
    namespace: &str,
    services: &[ServiceEndpoint],
    mode: HostAccessMode,
    port_base: u16,
    ip_map: &BTreeMap<String, String>,
) -> HostAccessPlan {
    match mode {
        HostAccessMode::Dns | HostAccessMode::Kubefwd => {
            let mut urls = BTreeMap::new();
            for svc in services {
                urls.insert(
                    svc.name.clone(),
                    format_dns_url(&svc.name, namespace, svc.port),
                );
            }
            HostAccessPlan {
                mode,
                namespace: namespace.to_string(),
                urls,
                port_map: BTreeMap::new(),
                port_base: None,
                ip_map: ip_map.clone(),
            }
        }
        HostAccessMode::Map => {
            let mut urls = BTreeMap::new();
            let mut port_map = BTreeMap::new();
            for (i, svc) in services.iter().enumerate() {
                let host_port = port_base.saturating_add(i as u16);
                port_map.insert(svc.name.clone(), host_port);
                urls.insert(svc.name.clone(), format_map_url(host_port));
            }
            HostAccessPlan {
                mode: HostAccessMode::Map,
                namespace: namespace.to_string(),
                urls,
                port_map,
                port_base: Some(port_base),
                ip_map: BTreeMap::new(),
            }
        }
    }
}

/// Render a short status card for humans (no kubectl literacy).
pub fn format_status_card(workspace: &str, plan: &HostAccessPlan) -> String {
    let mut lines = Vec::new();
    lines.push(format!("workspace: {workspace}"));
    lines.push(format!("namespace: {}", plan.namespace));
    lines.push(format!("access:   {}", plan.mode.as_str()));
    if plan.urls.is_empty() {
        lines.push("urls:     (no services discovered yet)".into());
    } else {
        lines.push("urls:".into());
        for (name, url) in &plan.urls {
            lines.push(format!("  - {name}: {url}"));
        }
    }
    lines.join("\n")
}

fn runtime_path(state_dir: &Path, workspace: &str) -> PathBuf {
    state_dir
        .join(RUNTIME_SUBDIR)
        .join(format!("{workspace}.host-access.json"))
}

pub fn save_host_access_runtime(
    state_dir: &Path,
    workspace: &str,
    runtime: &HostAccessRuntime,
) -> Result<PathBuf, Box<dyn Error>> {
    let dir = state_dir.join(RUNTIME_SUBDIR);
    fs::create_dir_all(&dir)?;
    let path = runtime_path(state_dir, workspace);
    fs::write(&path, serde_json::to_string_pretty(runtime)?)?;
    Ok(path)
}

pub fn load_host_access_runtime(
    state_dir: &Path,
    workspace: &str,
) -> Result<Option<HostAccessRuntime>, Box<dyn Error>> {
    let path = runtime_path(state_dir, workspace);
    if !path.exists() {
        return Ok(None);
    }
    let text = fs::read_to_string(&path)?;
    Ok(Some(serde_json::from_str(&text)?))
}

pub fn clear_host_access_runtime(state_dir: &Path, workspace: &str) {
    let path = runtime_path(state_dir, workspace);
    let _ = fs::remove_file(path);
}

/// Build argv for map-mode port-forward (testable pure function).
pub fn build_port_forward_args(
    namespace: &str,
    service: &str,
    host_port: u16,
    service_port: u16,
) -> Vec<String> {
    vec![
        "port-forward".into(),
        "-n".into(),
        namespace.into(),
        format!("svc/{service}"),
        format!("{host_port}:{service_port}"),
    ]
}

/// Build argv for kubefwd (testable pure function).
pub fn build_kubefwd_args(namespace: &str) -> Vec<String> {
    vec!["svc".into(), "-n".into(), namespace.into()]
}

fn command_exists(program: &str) -> bool {
    Command::new("sh")
        .args(["-c", &format!("command -v {program} >/dev/null 2>&1")])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Prefer hops-native **dns** mode: real k8s FQDNs + service ports on the host.
///
/// Requires admin to write `/etc/hosts` and (macOS) lo0 aliases — that is
/// intentional so app configs using in-cluster URLs work unchanged.
/// Falls back to map-mode only if elevation is denied.
pub fn start_host_access_auto(
    namespace: &str,
    services: &[ServiceEndpoint],
    _kubefwd_available: bool,
    port_base: u16,
    state_dir: &Path,
    workspace: &str,
) -> Result<(HostAccessPlan, HostAccessRuntime), Box<dyn Error>> {
    if !services.is_empty() {
        match try_start_dns_access(namespace, services, state_dir, workspace) {
            Ok((plan, rt)) => {
                log::info!(
                    "host access: cluster DNS — use in-cluster Service URLs on this machine"
                );
                return Ok((plan, rt));
            }
            Err(e) => {
                log::warn!(
                    "cluster DNS setup failed ({e}); falling back to map-mode 127.0.0.1 ports"
                );
                let _ = stop_host_access(state_dir, workspace);
            }
        }
    }

    let plan = plan_host_access_mode(
        namespace,
        services,
        HostAccessMode::Map,
        port_base,
        &BTreeMap::new(),
    );
    let rt = start_host_access_with_services(&plan, services, state_dir, workspace)?;
    Ok((plan, rt))
}

fn try_start_dns_access(
    namespace: &str,
    services: &[ServiceEndpoint],
    state_dir: &Path,
    workspace: &str,
) -> Result<(HostAccessPlan, HostAccessRuntime), Box<dyn Error>> {
    // Unique 127.53.x.y per service so every Service keeps its real cluster port
    // (kubefwd model). Privileged: lo0 aliases (macOS) + /etc/hosts.
    let ip_map = sync_alloc_for_namespace(state_dir, namespace, services)?;
    let ips: Vec<String> = ip_map.values().cloned().collect();

    // Merge hosts for all dns workspaces + this one.
    let mut blocks = collect_dns_blocks_from_runtimes(state_dir, Some(workspace))?;
    blocks.push((namespace.to_string(), ip_map.clone()));
    let mut by_ns: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    for (ns, m) in blocks {
        by_ns.insert(ns, m);
    }
    let merged_blocks: Vec<(String, BTreeMap<String, String>)> = by_ns.into_iter().collect();
    let mut host_lines = Vec::new();
    for (ns, m) in &merged_blocks {
        host_lines.extend(cluster_dns::hosts_lines_for_workspace(ns, m));
    }
    let current = fs::read_to_string("/etc/hosts").unwrap_or_default();
    let hosts_body = cluster_dns::merge_hosts_file(&current, &host_lines);

    // One elevation: hosts + all loopback aliases.
    cluster_dns::apply_privileged_dns_config(&hosts_body, &ips)?;

    let plan = plan_host_access_mode(namespace, services, HostAccessMode::Dns, 0, &ip_map);
    let rt = start_host_access_with_services(&plan, services, state_dir, workspace)?;
    std::thread::sleep(Duration::from_millis(500));
    if !rt.pids.iter().any(|p| pid_is_alive(*p)) {
        return Err("dns port-forwards exited immediately".into());
    }
    Ok((plan, rt))
}

/// Gather ip_maps from saved runtimes in dns mode (optionally skip one workspace).
fn collect_dns_blocks_from_runtimes(
    state_dir: &Path,
    skip_workspace: Option<&str>,
) -> Result<Vec<(String, BTreeMap<String, String>)>, Box<dyn Error>> {
    let dir = state_dir.join(RUNTIME_SUBDIR);
    let mut out = Vec::new();
    let entries = match fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return Ok(out),
    };
    for ent in entries.flatten() {
        let name = ent.file_name().to_string_lossy().into_owned();
        if !name.ends_with(".host-access.json") {
            continue;
        }
        let ws = name.trim_end_matches(".host-access.json");
        if skip_workspace == Some(ws) {
            continue;
        }
        let text = match fs::read_to_string(ent.path()) {
            Ok(t) => t,
            Err(_) => continue,
        };
        let rt: HostAccessRuntime = match serde_json::from_str(&text) {
            Ok(r) => r,
            Err(_) => continue,
        };
        if rt.mode != HostAccessMode::Dns.as_str() || rt.ip_map.is_empty() {
            continue;
        }
        out.push((rt.namespace, rt.ip_map));
    }
    Ok(out)
}

/// Start host access processes (kubefwd or kubectl port-forward). Records PIDs.
pub fn start_host_access_with_services(
    plan: &HostAccessPlan,
    services: &[ServiceEndpoint],
    state_dir: &Path,
    workspace: &str,
) -> Result<HostAccessRuntime, Box<dyn Error>> {
    let _ = stop_host_access(state_dir, workspace);

    let log_dir = state_dir.join(RUNTIME_SUBDIR);
    fs::create_dir_all(&log_dir)?;
    let log_path = log_dir.join(format!("{workspace}.host-access.log"));

    let mut pids = Vec::new();
    let open_log = || -> Result<(fs::File, fs::File), Box<dyn Error>> {
        let f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)?;
        let f2 = f.try_clone()?;
        Ok((f, f2))
    };

    match plan.mode {
        HostAccessMode::Kubefwd => {
            if !command_exists("kubefwd") {
                return Err("kubefwd not on PATH".into());
            }
            let args = build_kubefwd_args(&plan.namespace);
            let (out, err) = open_log()?;
            let child = Command::new("kubefwd")
                .args(&args)
                .stdin(Stdio::null())
                .stdout(Stdio::from(out))
                .stderr(Stdio::from(err))
                .spawn()
                .map_err(|e| format!("failed to spawn kubefwd: {e}"))?;
            pids.push(child.id());
            std::mem::forget(child);
        }
        HostAccessMode::Dns => {
            let port_by_name: BTreeMap<&str, u16> = services
                .iter()
                .map(|s| (s.name.as_str(), s.port))
                .collect();
            for (svc_name, bind_ip) in &plan.ip_map {
                let svc_port = port_by_name.get(svc_name.as_str()).copied().unwrap_or(80);
                let args = build_dns_port_forward_args(
                    &plan.namespace,
                    svc_name,
                    bind_ip,
                    svc_port,
                );
                let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
                let (out, err) = open_log()?;
                let child = kubectl_command(&arg_refs)
                    .stdin(Stdio::null())
                    .stdout(Stdio::from(out))
                    .stderr(Stdio::from(err))
                    .spawn()
                    .map_err(|e| format!("failed to spawn kubectl port-forward (dns): {e}"))?;
                pids.push(child.id());
                std::mem::forget(child);
            }
        }
        HostAccessMode::Map => {
            let port_by_name: BTreeMap<&str, u16> = services
                .iter()
                .map(|s| (s.name.as_str(), s.port))
                .collect();
            for (svc_name, host_port) in &plan.port_map {
                let svc_port = port_by_name.get(svc_name.as_str()).copied().unwrap_or(80);
                let args = build_port_forward_args(
                    &plan.namespace,
                    svc_name,
                    *host_port,
                    svc_port,
                );
                let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
                let (out, err) = open_log()?;
                let child = kubectl_command(&arg_refs)
                    .stdin(Stdio::null())
                    .stdout(Stdio::from(out))
                    .stderr(Stdio::from(err))
                    .spawn()
                    .map_err(|e| format!("failed to spawn kubectl port-forward: {e}"))?;
                pids.push(child.id());
                std::mem::forget(child);
            }
        }
    }

    let service_ports: BTreeMap<String, u16> = services
        .iter()
        .map(|s| (s.name.clone(), s.port))
        .collect();
    let runtime = HostAccessRuntime {
        mode: plan.mode.as_str().to_string(),
        namespace: plan.namespace.clone(),
        pids: pids.clone(),
        log_path: Some(log_path.display().to_string()),
        port_map: plan.port_map.clone(),
        service_ports,
        ip_map: plan.ip_map.clone(),
    };
    save_host_access_runtime(state_dir, workspace, &runtime)?;
    log::info!(
        "host access started ({}) pids={:?}",
        plan.mode.as_str(),
        pids
    );
    Ok(runtime)
}

/// True if something is accepting TCP connections on 127.0.0.1:port.
pub fn localhost_port_listening(port: u16) -> bool {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_ok()
}

/// Whether host access needs a restart (dead PIDs or ports not listening).
pub fn host_access_needs_heal(rt: &HostAccessRuntime) -> bool {
    let any_pid = rt.pids.iter().any(|p| pid_is_alive(*p));
    if !any_pid {
        return true;
    }
    match HostAccessMode::parse(&rt.mode).unwrap_or(HostAccessMode::Map) {
        HostAccessMode::Map => {
            if rt.port_map.is_empty() {
                return false;
            }
            rt.port_map
                .values()
                .any(|port| !localhost_port_listening(*port))
        }
        HostAccessMode::Dns => {
            // Probe service_port on each bind IP.
            for (svc, ip) in &rt.ip_map {
                let port = rt.service_ports.get(svc).copied().unwrap_or(80);
                if !ip_port_listening(ip, port) {
                    return true;
                }
            }
            false
        }
        HostAccessMode::Kubefwd => false,
    }
}

/// TCP probe for `ip:port` (dns mode uses 127.53.x.y).
pub fn ip_port_listening(ip: &str, port: u16) -> bool {
    let Ok(addr) = format!("{ip}:{port}").parse::<SocketAddr>() else {
        return false;
    };
    TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_ok()
}

/// Restart host access when processes or localhost ports are dead.
///
/// Uses recorded `port_map` / `service_ports` when present; otherwise rebuilds
/// from `services` + `port_base`.
pub fn ensure_host_access(
    namespace: &str,
    services: &[ServiceEndpoint],
    port_base: u16,
    state_dir: &Path,
    workspace: &str,
) -> Result<(HostAccessPlan, HostAccessRuntime, bool), Box<dyn Error>> {
    let prior = load_host_access_runtime(state_dir, workspace)?;

    if let Some(rt) = &prior {
        if !host_access_needs_heal(rt) {
            return Ok((plan_from_runtime(rt), rt.clone(), false));
        }
        log::info!("host access unhealthy; restarting ({})", rt.mode);
    }

    // Rebuild services list from runtime maps if discovery is empty.
    let services = if services.is_empty() {
        prior
            .as_ref()
            .map(services_from_runtime)
            .unwrap_or_else(|| services.to_vec())
    } else {
        services.to_vec()
    };

    // Always prefer dns on restart; auto falls back to map if sudo/hosts fails.
    let (plan, rt) =
        start_host_access_auto(namespace, &services, false, port_base, state_dir, workspace)?;
    std::thread::sleep(Duration::from_millis(400));
    Ok((plan, rt, true))
}

fn plan_from_runtime(rt: &HostAccessRuntime) -> HostAccessPlan {
    let mode = HostAccessMode::parse(&rt.mode).unwrap_or(HostAccessMode::Map);
    let mut urls = BTreeMap::new();
    match mode {
        HostAccessMode::Map => {
            for (name, host_port) in &rt.port_map {
                urls.insert(name.clone(), format_map_url(*host_port));
            }
        }
        HostAccessMode::Dns | HostAccessMode::Kubefwd => {
            for (name, port) in &rt.service_ports {
                urls.insert(
                    name.clone(),
                    format_dns_url(name, &rt.namespace, *port),
                );
            }
            // Fallback if service_ports empty
            if urls.is_empty() {
                for name in rt.ip_map.keys() {
                    urls.insert(
                        name.clone(),
                        format_dns_url(name, &rt.namespace, 80),
                    );
                }
            }
        }
    }
    HostAccessPlan {
        mode,
        namespace: rt.namespace.clone(),
        urls,
        port_map: rt.port_map.clone(),
        port_base: rt.port_map.values().copied().min(),
        ip_map: rt.ip_map.clone(),
    }
}

fn services_from_runtime(rt: &HostAccessRuntime) -> Vec<ServiceEndpoint> {
    let names: Vec<String> = if !rt.service_ports.is_empty() {
        rt.service_ports.keys().cloned().collect()
    } else if !rt.port_map.is_empty() {
        rt.port_map.keys().cloned().collect()
    } else {
        rt.ip_map.keys().cloned().collect()
    };
    names
        .into_iter()
        .map(|name| ServiceEndpoint {
            port: rt.service_ports.get(&name).copied().unwrap_or(80),
            name,
            protocol: "TCP".into(),
        })
        .collect()
}

/// Per-URL listen probe for status cards.
pub fn url_listen_status(plan: &HostAccessPlan) -> BTreeMap<String, bool> {
    let mut out = BTreeMap::new();
    match plan.mode {
        HostAccessMode::Map => {
            for (name, host_port) in &plan.port_map {
                out.insert(name.clone(), localhost_port_listening(*host_port));
            }
        }
        HostAccessMode::Dns => {
            for (name, ip) in &plan.ip_map {
                // Extract port from URL if possible
                let port = plan
                    .urls
                    .get(name)
                    .and_then(|u| u.rsplit(':').next())
                    .and_then(|p| p.parse().ok())
                    .unwrap_or(80);
                out.insert(name.clone(), ip_port_listening(ip, port));
            }
        }
        HostAccessMode::Kubefwd => {
            for name in plan.urls.keys() {
                out.insert(name.clone(), true);
            }
        }
    }
    out
}

/// Stop host access processes recorded for workspace (and best-effort pkill).
pub fn stop_host_access(state_dir: &Path, workspace: &str) -> Result<(), Box<dyn Error>> {
    if let Some(rt) = load_host_access_runtime(state_dir, workspace)? {
        for pid in &rt.pids {
            let _ = Command::new("kill").args(["-TERM", &pid.to_string()]).status();
        }
        // Give processes a moment, then KILL
        std::thread::sleep(std::time::Duration::from_millis(200));
        for pid in &rt.pids {
            let _ = Command::new("kill").args(["-KILL", &pid.to_string()]).status();
        }
        // Also pkill by namespace pattern as safety net
        let ns = &rt.namespace;
        let _ = Command::new("sh")
            .args([
                "-c",
                &format!(
                    "pkill -f 'kubefwd.*{ns}' 2>/dev/null || true; \
                     pkill -f 'port-forward.*-n {ns}' 2>/dev/null || true"
                ),
            ])
            .status();

        if rt.mode == HostAccessMode::Dns.as_str() {
            let ips: Vec<String> = rt.ip_map.values().cloned().collect();
            remove_loopback_aliases(&ips);
            // Rebuild hosts without this workspace
            if let Ok(blocks) = collect_dns_blocks_from_runtimes(state_dir, Some(workspace)) {
                let _ = rebuild_hosts_from_blocks(&blocks);
            }
        }
    }
    clear_host_access_runtime(state_dir, workspace);
    Ok(())
}

/// Whether a PID appears alive (not exited / not a zombie).
///
/// `kill -0` alone is insufficient after `mem::forget(Child)`: unreaped children
/// stay as zombies and still "succeed" kill -0.
pub fn pid_is_alive(pid: u32) -> bool {
    let output = Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "state="])
        .output();
    match output {
        Ok(o) if o.status.success() => {
            let state = String::from_utf8_lossy(&o.stdout);
            let state = state.trim();
            // Empty or Z/zombie → not usefully alive
            !state.is_empty() && !state.starts_with('Z') && !state.starts_with('z')
        }
        _ => false,
    }
}

/// Status line for host access runtime.
pub fn host_access_status_line(rt: &HostAccessRuntime) -> String {
    let alive: Vec<u32> = rt.pids.iter().copied().filter(|p| pid_is_alive(*p)).collect();
    if alive.is_empty() {
        return format!(
            "access processes: none running (mode was {}; will heal on status/up)",
            rt.mode
        );
    }
    let listen = match HostAccessMode::parse(&rt.mode).unwrap_or(HostAccessMode::Map) {
        HostAccessMode::Map if !rt.port_map.is_empty() => {
            let ok = rt
                .port_map
                .values()
                .filter(|p| localhost_port_listening(**p))
                .count();
            format!("; localhost {ok}/{} ports listening", rt.port_map.len())
        }
        HostAccessMode::Dns if !rt.ip_map.is_empty() => {
            let mut ok = 0;
            for (svc, ip) in &rt.ip_map {
                let port = rt.service_ports.get(svc).copied().unwrap_or(80);
                if ip_port_listening(ip, port) {
                    ok += 1;
                }
            }
            format!("; cluster-dns {ok}/{} endpoints listening", rt.ip_map.len())
        }
        _ => String::new(),
    };
    format!(
        "access processes: {} alive (pids {}){}",
        rt.mode,
        alive
            .iter()
            .map(|p| p.to_string())
            .collect::<Vec<_>>()
            .join(","),
        listen
    )
}

/// Human status card with optional listen markers on map URLs.
pub fn format_status_card_with_listen(
    workspace: &str,
    plan: &HostAccessPlan,
    listen: &BTreeMap<String, bool>,
) -> String {
    let mut lines = Vec::new();
    lines.push(format!("workspace: {workspace}"));
    lines.push(format!("namespace: {}", plan.namespace));
    lines.push(format!("access:   {}", plan.mode.as_str()));
    if plan.urls.is_empty() {
        lines.push("urls:     (no services discovered yet)".into());
    } else {
        lines.push("urls:".into());
        for (name, url) in &plan.urls {
            let mark = match listen.get(name) {
                Some(true) => "  ok",
                Some(false) => "  DOWN",
                None => "",
            };
            lines.push(format!("  - {name}: {url}{mark}"));
        }
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kubefwd_urls_include_namespace() {
        let url = format_kubefwd_url("e2e-ui-ui", "hops-wt-alice", 5180);
        assert_eq!(
            url,
            "http://e2e-ui-ui.hops-wt-alice.svc.cluster.local:5180"
        );
        let url2 = format_kubefwd_url("e2e-ui-ui", "hops-wt-bob", 5180);
        assert_ne!(url, url2);
    }

    #[test]
    fn two_workspaces_get_distinct_map_ports() {
        let existing = vec![WorkspaceRecord {
            name: "alice".into(),
            namespace: "hops-wt-alice".into(),
            env_path: "/x".into(),
            project_root: None,
            host_access_mode: Some("map".into()),
            port_base: Some(18000),
            delivery_mode: None,
            updated_at: None,
        }];
        let bob_base = allocate_port_base(&existing);
        assert_ne!(bob_base, 18000);
        assert_eq!(bob_base, 18100);

        let services = vec![
            ServiceEndpoint {
                name: "ui".into(),
                port: 5180,
                protocol: "TCP".into(),
            },
            ServiceEndpoint {
                name: "api".into(),
                port: 8791,
                protocol: "TCP".into(),
            },
        ];
        let alice = plan_host_access("hops-wt-alice", &services, false, 18000);
        let bob = plan_host_access("hops-wt-bob", &services, false, bob_base);
        assert_eq!(alice.mode, HostAccessMode::Map);
        assert_eq!(bob.mode, HostAccessMode::Map);
        assert_ne!(alice.urls.get("ui"), bob.urls.get("ui"));
        assert_ne!(alice.port_map.get("ui"), bob.port_map.get("ui"));
        assert!(alice.port_map.get("ui").unwrap() < bob.port_map.get("ui").unwrap());
    }

    #[test]
    fn dns_mode_urls_use_cluster_fqdn() {
        let services = vec![ServiceEndpoint {
            name: "ui".into(),
            port: 5180,
            protocol: "TCP".into(),
        }];
        let plan = plan_host_access("hops-wt-x", &services, true, 18000);
        assert_eq!(plan.mode, HostAccessMode::Dns);
        assert!(plan.urls["ui"].contains("ui.hops-wt-x.svc.cluster.local:5180"));
        assert!(plan.port_map.is_empty());
    }

    #[test]
    fn status_card_lists_urls_without_kubectl() {
        let services = vec![ServiceEndpoint {
            name: "ui".into(),
            port: 5180,
            protocol: "TCP".into(),
        }];
        let plan = plan_host_access("hops-wt-alice", &services, true, 0);
        let card = format_status_card("alice", &plan);
        assert!(card.contains("workspace: alice"));
        assert!(card.contains("ui:"));
        assert!(!card.to_lowercase().contains("kubectl"));
        assert!(card.contains("svc.cluster.local"));
    }

    #[test]
    fn port_forward_args_bind_host_to_service_port() {
        let args = build_port_forward_args("hops-wt-alice", "e2e-ui-ui", 18000, 5180);
        assert_eq!(
            args,
            vec![
                "port-forward",
                "-n",
                "hops-wt-alice",
                "svc/e2e-ui-ui",
                "18000:5180",
            ]
        );
        let args2 = build_port_forward_args("hops-wt-bob", "e2e-ui-ui", 18100, 5180);
        assert_ne!(args, args2);
        assert!(args2.contains(&"18100:5180".to_string()));
    }

    #[test]
    fn kubefwd_args_target_namespace() {
        assert_eq!(
            build_kubefwd_args("hops-wt-alice"),
            vec!["svc", "-n", "hops-wt-alice"]
        );
    }

    #[test]
    fn host_access_runtime_round_trip() {
        let dir = std::env::temp_dir().join(format!(
            "lwb-net-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&dir).unwrap();
        let mut port_map = BTreeMap::new();
        port_map.insert("ui".into(), 18000);
        let mut service_ports = BTreeMap::new();
        service_ports.insert("ui".into(), 5180);
        let rt = HostAccessRuntime {
            mode: "map".into(),
            namespace: "hops-wt-x".into(),
            pids: vec![12345, 12346],
            log_path: Some("/tmp/x.log".into()),
            port_map,
            service_ports,
            ip_map: BTreeMap::new(),
        };
        save_host_access_runtime(&dir, "x", &rt).unwrap();
        let loaded = load_host_access_runtime(&dir, "x").unwrap().unwrap();
        assert_eq!(loaded.pids, vec![12345, 12346]);
        assert_eq!(loaded.mode, "map");
        assert_eq!(loaded.port_map.get("ui"), Some(&18000));
        clear_host_access_runtime(&dir, "x");
        assert!(load_host_access_runtime(&dir, "x").unwrap().is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn host_access_needs_heal_when_no_pids() {
        let rt = HostAccessRuntime {
            mode: "map".into(),
            namespace: "ns".into(),
            pids: vec![],
            log_path: None,
            port_map: BTreeMap::new(),
            service_ports: BTreeMap::new(),
            ip_map: BTreeMap::new(),
        };
        assert!(host_access_needs_heal(&rt));
    }

    #[test]
    fn map_mode_port_forward_args_match_started_processes() {
        // Guarantees start_host_access_with_services map branch uses the pure builder.
        let services = vec![
            ServiceEndpoint {
                name: "ui".into(),
                port: 5180,
                protocol: "TCP".into(),
            },
            ServiceEndpoint {
                name: "api".into(),
                port: 8791,
                protocol: "TCP".into(),
            },
        ];
        let plan = plan_host_access("hops-wt-alice", &services, false, 18000);
        assert_eq!(plan.mode, HostAccessMode::Map);
        for (svc, host_port) in &plan.port_map {
            let svc_port = services.iter().find(|s| &s.name == svc).unwrap().port;
            let args = build_port_forward_args(&plan.namespace, svc, *host_port, svc_port);
            assert_eq!(args[0], "port-forward");
            assert!(args.iter().any(|a| a == "hops-wt-alice"));
            assert!(args.iter().any(|a| a == &format!("svc/{svc}")));
            assert!(args.iter().any(|a| a == &format!("{host_port}:{svc_port}")));
        }
    }
}
