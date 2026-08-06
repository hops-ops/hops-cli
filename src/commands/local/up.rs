//! `hops local up` — front-door: register workspace, reconcile, delivery, host access.

use super::workbench::delivery::{
    probe_from_visibility, select_delivery_strategy, DeliveryStrategy,
};
use super::workbench::net::{
    allocate_port_base, format_status_card, plan_host_access, HostAccessMode, ServiceEndpoint,
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
use std::path::PathBuf;

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

    let env_path = args
        .env_path
        .canonicalize()
        .map_err(|e| {
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

    // Project root: parent of gitops/ when path ends with gitops/env/...
    let project_root = infer_project_root(&env_path);

    // Delivery probe + strategy
    let (delivery_mode, runtime_values) = if args.no_delivery {
        (None, BTreeMap::new())
    } else {
        let strategy = resolve_delivery(args.delivery.as_deref(), project_root.as_ref())?;
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
        // cluster-dev is the north-star posture for up
        vals
            .entry("appRuntime".into())
            .or_insert(Value::String("cluster-dev".into()));
        (Some(strategy), vals)
    };

    let opts = ReconcileOptions {
        namespace: namespace.clone(),
        workspace_name: name.clone(),
        runtime_values,
        dry_run: args.dry_run,
    };

    log::info!("Workspace `{name}` → namespace `{namespace}`");
    let results = reconcile_applications(&env_path, &opts, &SystemHelm, &SystemKubectl)?;
    for r in &results {
        log::info!(
            "  reconciled {} → {}",
            r.app_name,
            if r.applied { "applied" } else { "dry-run" }
        );
    }

    // Discover services for URL card (best-effort).
    let services = discover_services(&namespace).unwrap_or_else(|e| {
        log::debug!("service discovery deferred: {e}");
        default_service_stubs(&results)
    });

    let kubefwd = command_exists("kubefwd");
    let plan = plan_host_access(&namespace, &services, kubefwd, port_base);

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

    // Print human card
    println!();
    println!("{}", format_status_card(&name, &plan));
    if let Some(d) = delivery_mode {
        log::info!("source delivery: {} (auto)", d.as_str());
    }
    if plan.mode == HostAccessMode::Map {
        log::info!(
            "host access: map mode (install kubefwd for cluster DNS-style URLs)"
        );
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
) -> Result<DeliveryStrategy, Box<dyn Error>> {
    if let Some(m) = override_mode {
        return match m {
            "hostPath" | "hostpath" => Ok(DeliveryStrategy::HostPath),
            "sync" | "mutagen" => Ok(DeliveryStrategy::Sync),
            other => Err(format!("unknown --delivery {other} (use hostPath|sync)").into()),
        };
    }
    let root = project_root
        .cloned()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    // Best-effort probe: path exists on this host (node visibility is backend-specific;
    // without a live node check we treat host path existence + dory/colima projects-root
    // heuristics as partial — full node exec probe is best-effort).
    let visible = probe_node_path_visible(&root);
    let probe = probe_from_visibility(
        &root,
        visible,
        if visible {
            "host path present; assuming node visibility for local backends"
        } else {
            "host path missing or not visible; using sync fallback"
        },
    );
    Ok(select_delivery_strategy(&probe))
}

fn probe_node_path_visible(path: &PathBuf) -> bool {
    // Prefer: if path exists and looks like a normal host worktree, claim hostPath.
    // Live backends may refine this later via `dory`/`docker exec` node checks.
    path.is_dir()
}

fn infer_project_root(env_path: &std::path::Path) -> Option<PathBuf> {
    // .../gitops/env/local → project root three levels up from env, or parent of gitops
    let mut p = env_path.to_path_buf();
    // climb until we see gitops as a component, then take its parent
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

fn discover_services(namespace: &str) -> Result<Vec<ServiceEndpoint>, Box<dyn Error>> {
    let json = run_cmd_output(
        "kubectl",
        &[
            "get",
            "svc",
            "-n",
            namespace,
            "-o",
            "json",
        ],
    )?;
    let value: serde_json::Value = serde_json::from_str(&json)?;
    let mut out = Vec::new();
    if let Some(items) = value.get("items").and_then(|i| i.as_array()) {
        for item in items {
            let name = item
                .pointer("/metadata/name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if name.is_empty() {
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
            port: if r.app_name.contains("ui") { 5180 } else { 8791 },
            protocol: "TCP".into(),
        })
        .collect()
}

fn chrono_lite_now() -> String {
    // Avoid extra dep: RFC3339-ish from system time
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{secs}")
}
