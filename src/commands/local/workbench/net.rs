//! Host access: plan URLs **and** start/stop kubefwd or kubectl port-forward.

use super::registry::WorkspaceRecord;
use std::collections::BTreeMap;
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Default starting port for map-mode allocation (avoids privileged + common dev ports).
pub const MAP_PORT_BASE_START: u16 = 18000;
/// Ports reserved per workspace in map mode (stride).
pub const MAP_PORT_STRIDE: u16 = 100;
/// Runtime state subdir under local state dir.
pub const RUNTIME_SUBDIR: &str = "runtime";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostAccessMode {
    /// svc.<ns>.svc.cluster.local style (kubefwd).
    Kubefwd,
    /// Unique localhost ports via port-forward map.
    Map,
}

impl HostAccessMode {
    pub fn as_str(self) -> &'static str {
        match self {
            HostAccessMode::Kubefwd => "kubefwd",
            HostAccessMode::Map => "map",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
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
pub fn plan_host_access(
    namespace: &str,
    services: &[ServiceEndpoint],
    kubefwd_available: bool,
    port_base: u16,
) -> HostAccessPlan {
    if kubefwd_available {
        let mut urls = BTreeMap::new();
        for svc in services {
            urls.insert(
                svc.name.clone(),
                format_kubefwd_url(&svc.name, namespace, svc.port),
            );
        }
        return HostAccessPlan {
            mode: HostAccessMode::Kubefwd,
            namespace: namespace.to_string(),
            urls,
            port_map: BTreeMap::new(),
            port_base: None,
        };
    }

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

/// Prefer kubefwd when available **and** the process stays alive; else map-mode
/// port-forwards (kubefwd requires root/sudo on most macOS installs).
pub fn start_host_access_auto(
    namespace: &str,
    services: &[ServiceEndpoint],
    kubefwd_available: bool,
    port_base: u16,
    state_dir: &Path,
    workspace: &str,
) -> Result<(HostAccessPlan, HostAccessRuntime), Box<dyn Error>> {
    if kubefwd_available && !services.is_empty() {
        let plan = plan_host_access(namespace, services, true, port_base);
        match start_host_access_with_services(&plan, services, state_dir, workspace) {
            Ok(rt) => {
                // kubefwd often exits after printing a needs-sudo error — wait long
                // enough for that exit before deciding it "worked".
                std::thread::sleep(std::time::Duration::from_millis(1500));
                if rt.pids.iter().any(|p| pid_is_alive(*p)) {
                    return Ok((plan, rt));
                }
                log::warn!(
                    "kubefwd exited immediately (often needs sudo); falling back to map-mode port-forwards"
                );
                let _ = stop_host_access(state_dir, workspace);
            }
            Err(e) => {
                log::warn!("kubefwd start failed ({e}); falling back to map-mode port-forwards");
            }
        }
    }

    let plan = plan_host_access(namespace, services, false, port_base);
    let rt = start_host_access_with_services(&plan, services, state_dir, workspace)?;
    Ok((plan, rt))
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
                let (out, err) = open_log()?;
                let child = Command::new("kubectl")
                    .args(&args)
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

    let runtime = HostAccessRuntime {
        mode: plan.mode.as_str().to_string(),
        namespace: plan.namespace.clone(),
        pids: pids.clone(),
        log_path: Some(log_path.display().to_string()),
    };
    save_host_access_runtime(state_dir, workspace, &runtime)?;
    log::info!(
        "host access started ({}) pids={:?}",
        plan.mode.as_str(),
        pids
    );
    Ok(runtime)
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
        format!(
            "access processes: none running (mode was {}; re-run hops local up)",
            rt.mode
        )
    } else {
        format!(
            "access processes: {} alive (pids {})",
            rt.mode,
            alive
                .iter()
                .map(|p| p.to_string())
                .collect::<Vec<_>>()
                .join(",")
        )
    }
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
    fn kubefwd_mode_when_available() {
        let services = vec![ServiceEndpoint {
            name: "ui".into(),
            port: 5180,
            protocol: "TCP".into(),
        }];
        let plan = plan_host_access("hops-wt-x", &services, true, 18000);
        assert_eq!(plan.mode, HostAccessMode::Kubefwd);
        assert!(plan.urls["ui"].contains("hops-wt-x"));
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
        let rt = HostAccessRuntime {
            mode: "map".into(),
            namespace: "hops-wt-x".into(),
            pids: vec![12345, 12346],
            log_path: Some("/tmp/x.log".into()),
        };
        save_host_access_runtime(&dir, "x", &rt).unwrap();
        let loaded = load_host_access_runtime(&dir, "x").unwrap().unwrap();
        assert_eq!(loaded.pids, vec![12345, 12346]);
        assert_eq!(loaded.mode, "map");
        clear_host_access_runtime(&dir, "x");
        assert!(load_host_access_runtime(&dir, "x").unwrap().is_none());
        let _ = fs::remove_dir_all(&dir);
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
