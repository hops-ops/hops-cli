//! `hops local up` — front-door: register workspace, reconcile, delivery, host access.

use super::workbench::delivery::{
    attach_sync_delivery, discover_sync_targets, probe_node_path_visibility,
    select_delivery_strategy, stop_mutagen_sessions, DeliveryStrategy, NodePathProber,
    SystemNodeProber,
};
use super::workbench::net::{
    allocate_port_base, format_status_card, host_access_status_line, plan_host_access,
    start_host_access_auto, HostAccessMode, ServiceEndpoint,
};
use super::workbench::reconcile::{
    reconcile_applications, ReconcileOptions, SystemHelm, SystemKubectl,
};
use super::workbench::registry::{
    default_name_from_cwd, list_workspaces, namespace_for_name, save_workspace, WorkspaceRecord,
};
use super::{command_exists, local_state_dir, run_cmd_output};
use clap::Args;
use serde_yaml::Value;
use std::collections::BTreeMap;
use std::error::Error;
use std::path::{Path, PathBuf};

#[derive(Args, Debug)]
pub struct UpArgs {
    /// Path to env directory of Application YAMLs (e.g. ./gitops/env/local).
    pub env_path: PathBuf,

    /// Workspace name (isolates namespace). Defaults to cwd basename.
    #[arg(long)]
    pub name: Option<String>,

    /// Watch env/chart paths after first reconcile.
    #[arg(long, default_value_t = false)]
    pub watch: bool,

    /// Debounce seconds for --watch.
    #[arg(long, default_value_t = 1)]
    pub debounce: u64,

    /// Skip source delivery attach (still reconciles charts).
    #[arg(long, default_value_t = false)]
    pub no_delivery: bool,

    /// Force delivery strategy: hostPath | sync (default: auto probe).
    #[arg(long)]
    pub delivery: Option<String>,

    /// Skip starting kubefwd / port-forward (plan URLs only).
    #[arg(long, default_value_t = false)]
    pub no_net: bool,

    /// Render only; do not apply.
    #[arg(long, default_value_t = false)]
    pub dry_run: bool,
}

pub fn run(args: &UpArgs) -> Result<(), Box<dyn Error>> {
    // CP readiness: plain-language error if kubectl cannot reach API.
    if !args.dry_run {
        match run_cmd_output("kubectl", &["cluster-info"]) {
            Ok(_) => {}
            Err(e) => {
                return Err(format!(
                    "Local control plane is not reachable ({e}).\n\
                     Start it once with: hops local start\n\
                     Then re-run: hops local up {}",
                    args.env_path.display()
                )
                .into());
            }
        }
    }

    let env_path = args.env_path.canonicalize().map_err(|e| {
        format!(
            "env path {} not found ({e}). Pass a directory of Application YAMLs, e.g. ./gitops/env/local",
            args.env_path.display()
        )
    })?;

    let cwd = std::env::current_dir()?;
    let name = args
        .name
        .clone()
        .unwrap_or_else(|| default_name_from_cwd(&cwd));
    let namespace = namespace_for_name(&name);

    let state_dir = local_state_dir()?;
    let existing = list_workspaces(&state_dir).unwrap_or_default();
    let port_base = existing
        .iter()
        .find(|r| r.name == name)
        .and_then(|r| r.port_base)
        .unwrap_or_else(|| allocate_port_base(&existing));

    let project_root = infer_project_root(&env_path);

    // Delivery probe + strategy (real node visibility, not just host is_dir)
    let (delivery_mode, runtime_values, probe_detail) = if args.no_delivery {
        (None, BTreeMap::new(), None)
    } else {
        let (strategy, detail) =
            resolve_delivery(args.delivery.as_deref(), project_root.as_ref(), &SystemNodeProber)?;
        let mut vals = BTreeMap::new();
        let mut sd = serde_yaml::Mapping::new();
        sd.insert(
            Value::String("mode".into()),
            Value::String(strategy.helm_mode_value().into()),
        );
        if let Some(root) = &project_root {
            if strategy == DeliveryStrategy::HostPath {
                sd.insert(
                    Value::String("hostPath".into()),
                    Value::String(root.display().to_string()),
                );
            }
        }
        vals.insert("sourceDelivery".into(), Value::Mapping(sd));
        vals.entry("appRuntime".into())
            .or_insert(Value::String("cluster-dev".into()));
        (Some(strategy), vals, Some(detail))
    };

    let opts = ReconcileOptions {
        namespace: namespace.clone(),
        workspace_name: name.clone(),
        runtime_values,
        dry_run: args.dry_run,
    };

    log::info!("Workspace `{name}` → namespace `{namespace}`");
    if let Some(d) = &probe_detail {
        log::info!("delivery probe: {d}");
    }
    let results = reconcile_applications(&env_path, &opts, &SystemHelm, &SystemKubectl)?;
    for r in &results {
        log::info!(
            "  reconciled {} → {}",
            r.app_name,
            if r.applied { "applied" } else { "dry-run" }
        );
    }

    // Attach real sync delivery when strategy is Sync (mutagen or tar|kubectl)
    let mut sync_pids: Vec<u32> = Vec::new();
    let mut mutagen_sessions: Vec<String> = Vec::new();
    if !args.dry_run && !args.no_delivery {
        if let Some(DeliveryStrategy::Sync) = delivery_mode {
            if let Some(root) = &project_root {
                let targets = wait_for_sync_targets(&namespace, &name, "/workspace", 30);
                match attach_sync_delivery(root, &targets, &name, args.watch) {
                    Ok(attach) => {
                        sync_pids = attach.sync_pids;
                        mutagen_sessions = attach.mutagen_sessions;
                        for m in attach.messages {
                            log::info!("delivery: {m}");
                        }
                    }
                    Err(e) => log::warn!("source delivery attach failed: {e}"),
                }
            }
        } else if let Some(DeliveryStrategy::HostPath) = delivery_mode {
            log::info!("source delivery: hostPath (node can see worktree; no sync session)");
        }
    }

    // Discover services for URL card
    let services = if args.dry_run {
        default_service_stubs(&results)
    } else {
        discover_services(&namespace).unwrap_or_else(|e| {
            log::debug!("service discovery deferred: {e}");
            default_service_stubs(&results)
        })
    };

    let kubefwd = command_exists("kubefwd");
    // Plan first for dry-run / no-net; live path may rewrite plan via auto fallback.
    let mut plan = plan_host_access(&namespace, &services, kubefwd, port_base);

    // Start real host access (kubefwd if it stays up, else map port-forwards)
    if !args.dry_run && !args.no_net && !services.is_empty() {
        match start_host_access_auto(
            &namespace,
            &services,
            kubefwd,
            port_base,
            &state_dir,
            &name,
        ) {
            Ok((live_plan, rt)) => {
                plan = live_plan;
                log::info!("{}", host_access_status_line(&rt));
            }
            Err(e) => {
                log::warn!("host access start failed: {e}");
            }
        }
    } else if plan.mode == HostAccessMode::Map && services.is_empty() {
        log::info!("host access: deferred until services exist");
    }

    // Persist delivery runtime pids alongside workspace record (in runtime dir via net helpers
    // for host access; store sync info in a small sidecar file)
    save_delivery_runtime(&state_dir, &name, &mutagen_sessions, &sync_pids)?;

    let record = WorkspaceRecord {
        name: name.clone(),
        namespace: namespace.clone(),
        env_path: env_path.display().to_string(),
        project_root: project_root.map(|p| p.display().to_string()),
        host_access_mode: Some(plan.mode.as_str().to_string()),
        port_base: plan.port_base.or(Some(port_base)),
        delivery_mode: delivery_mode.map(|d| d.as_str().to_string()),
        updated_at: Some(chrono_lite_now()),
    };
    if !args.dry_run {
        save_workspace(&state_dir, &record)?;
    }

    println!();
    println!("{}", format_status_card(&name, &plan));
    if let Some(d) = delivery_mode {
        println!("delivery: {} ({})", d.as_str(), probe_detail.as_deref().unwrap_or("auto"));
    }
    if plan.mode == HostAccessMode::Map {
        log::info!("host access: map mode port-forwards (install kubefwd for cluster DNS URLs)");
    }
    println!();
    println!("Useful commands:");
    println!("  hops local status");
    println!("  hops local open");
    println!("  hops local down --name {name}");

    if args.watch {
        use super::gitops::run_gitops_watch;
        let env_for_watch = env_path.clone();
        let opts_watch = opts.clone();
        run_gitops_watch(&env_path, args.debounce, move || {
            let _ = reconcile_applications(
                &env_for_watch,
                &opts_watch,
                &SystemHelm,
                &SystemKubectl,
            )?;
            Ok(())
        })?;
    }

    Ok(())
}

fn resolve_delivery(
    override_mode: Option<&str>,
    project_root: Option<&PathBuf>,
    prober: &dyn NodePathProber,
) -> Result<(DeliveryStrategy, String), Box<dyn Error>> {
    if let Some(m) = override_mode {
        let strategy = match m {
            "hostPath" | "hostpath" => DeliveryStrategy::HostPath,
            "sync" | "mutagen" => DeliveryStrategy::Sync,
            other => {
                return Err(format!("unknown --delivery {other} (use hostPath|sync)").into())
            }
        };
        return Ok((strategy, format!("forced via --delivery {m}")));
    }
    let root = project_root
        .cloned()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let probe = prober.probe(&root)?;
    let strategy = select_delivery_strategy(&probe);
    Ok((strategy, probe.detail))
}

fn infer_project_root(env_path: &Path) -> Option<PathBuf> {
    let mut p = env_path.to_path_buf();
    loop {
        if p.file_name().and_then(|s| s.to_str()) == Some("gitops") {
            return p.parent().map(|x| x.to_path_buf());
        }
        if !p.pop() {
            break;
        }
    }
    env_path.parent().map(|x| x.to_path_buf())
}

/// Poll for pods labeled for this workspace (any phase that can accept exec).
fn wait_for_sync_targets(
    namespace: &str,
    workspace: &str,
    mount_path: &str,
    timeout_secs: u64,
) -> Vec<super::workbench::delivery::SyncPodTarget> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    loop {
        match discover_sync_targets(namespace, workspace, mount_path) {
            Ok(t) if !t.is_empty() => return t,
            Ok(_) => {}
            Err(e) => log::debug!("pod discovery: {e}"),
        }
        if std::time::Instant::now() >= deadline {
            return discover_sync_targets(namespace, workspace, mount_path).unwrap_or_default();
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}

fn discover_services(namespace: &str) -> Result<Vec<ServiceEndpoint>, Box<dyn Error>> {
    let json = run_cmd_output("kubectl", &["get", "svc", "-n", namespace, "-o", "json"])?;
    let value: serde_json::Value = serde_json::from_str(&json)?;
    let mut out = Vec::new();
    if let Some(items) = value.get("items").and_then(|i| i.as_array()) {
        for item in items {
            let name = item
                .pointer("/metadata/name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if name.is_empty() || name == "kubernetes" {
                continue;
            }
            let port = item
                .pointer("/spec/ports/0/port")
                .and_then(|v| v.as_u64())
                .unwrap_or(80) as u16;
            out.push(ServiceEndpoint {
                name,
                port,
                protocol: "TCP".into(),
            });
        }
    }
    Ok(out)
}

fn default_service_stubs(
    results: &[super::workbench::reconcile::ReconcileResult],
) -> Vec<ServiceEndpoint> {
    results
        .iter()
        .map(|r| ServiceEndpoint {
            name: r.app_name.clone(),
            port: if r.app_name.contains("ui") {
                5180
            } else {
                8791
            },
            protocol: "TCP".into(),
        })
        .collect()
}

fn chrono_lite_now() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{secs}")
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct DeliveryRuntime {
    mutagen_sessions: Vec<String>,
    sync_pids: Vec<u32>,
}

fn delivery_runtime_path(state_dir: &Path, workspace: &str) -> PathBuf {
    state_dir
        .join("runtime")
        .join(format!("{workspace}.delivery.json"))
}

fn save_delivery_runtime(
    state_dir: &Path,
    workspace: &str,
    sessions: &[String],
    pids: &[u32],
) -> Result<(), Box<dyn Error>> {
    let dir = state_dir.join("runtime");
    std::fs::create_dir_all(&dir)?;
    let rt = DeliveryRuntime {
        mutagen_sessions: sessions.to_vec(),
        sync_pids: pids.to_vec(),
    };
    std::fs::write(
        delivery_runtime_path(state_dir, workspace),
        serde_json::to_string_pretty(&rt)?,
    )?;
    Ok(())
}

pub(crate) fn stop_delivery_runtime(state_dir: &Path, workspace: &str) {
    let path = delivery_runtime_path(state_dir, workspace);
    if let Ok(text) = std::fs::read_to_string(&path) {
        if let Ok(rt) = serde_json::from_str::<DeliveryRuntime>(&text) {
            stop_mutagen_sessions(&rt.mutagen_sessions);
            for pid in rt.sync_pids {
                let _ = std::process::Command::new("kill")
                    .args(["-TERM", &pid.to_string()])
                    .status();
            }
        }
    }
    let _ = std::fs::remove_file(path);
}

// Re-export probe for tests that want the production path
#[allow(dead_code)]
pub fn probe_for_tests(path: &Path) -> Result<super::workbench::delivery::DeliveryProbe, Box<dyn Error>> {
    probe_node_path_visibility(path)
}
