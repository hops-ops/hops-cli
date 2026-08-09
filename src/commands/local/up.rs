//! `hops local up` — front-door: register workspace, reconcile, delivery, host access.

use super::workbench::application::{load_applications, resolve_delivery_host_path};
use super::workbench::cluster_gitops::{
    reconcile_cluster_dir, resolve_cluster_path, should_reconcile_cluster_change,
};
use super::workbench::delivery::{
    attach_sync_delivery, discover_sync_targets, probe_node_path_visibility,
    select_delivery_strategy, stop_mutagen_sessions, DeliveryStrategy, NodePathProber,
    SystemNodeProber,
};
use super::workbench::net::{
    discover_workspace_endpoints, format_status_card, host_access_status_line, plan_host_access,
    start_host_access, ServiceEndpoint,
};
use super::workbench::reconcile::{
    reconcile_applications, ReconcileOptions, SystemHelm, SystemKubectl,
};
use super::workbench::registry::{
    default_name_from_cwd, namespace_for_name, save_workspace, WorkspaceRecord,
};
use super::workbench::watch::{
    is_chart_or_env_path, should_ignore_watch_path, watch_roots_for_applications, WatchPathClass,
};
use super::{local_state_dir, run_cmd_output};
use clap::Args;
use notify::{RecursiveMode, Watcher};
use std::collections::BTreeMap;
use std::error::Error;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

#[derive(Args, Debug)]
pub struct UpArgs {
    /// Path to env directory of Application YAMLs (e.g. ./gitops/envs/local).
    pub env_path: PathBuf,

    /// Workspace name (isolates namespace). Defaults to cwd basename.
    #[arg(long)]
    pub name: Option<String>,

    /// Path to **shared** control-plane gitops (PSQLStack, AuthStack, packages).
    /// Not per-worktree: one tree per local CP, usually meta-repo `gitops/cluster`.
    /// Default: `--cluster`, else `$HOPS_LOCAL_CLUSTER`, else walk up from env/cwd
    /// for `gitops/cluster`. Project charts stay under each app's `.gitops/deploy`.
    #[arg(long)]
    pub cluster: Option<PathBuf>,

    /// Skip applying/watching cluster gitops.
    #[arg(long, default_value_t = false)]
    pub no_cluster: bool,

    /// Run a single bring-up and exit (disables the default watch).
    #[arg(long, default_value_t = false)]
    pub once: bool,

    /// Watch env/chart/cluster paths after first reconcile (default).
    /// Redundant unless scripting; use `--once` to disable.
    #[arg(long, default_value_t = false)]
    pub watch: bool,

    /// Debounce seconds while watching.
    #[arg(long, default_value_t = 1)]
    pub debounce: u64,

    /// Skip source delivery attach (still reconciles charts).
    #[arg(long, default_value_t = false)]
    pub no_delivery: bool,

    /// Force delivery strategy: hostPath | sync (default: auto probe).
    #[arg(long)]
    pub delivery: Option<String>,

    /// Skip host access (Service FQDNs + port-forward supervisor).
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

    // Per-app host roots (UI → ui/, API monorepo → e2e-ui root, etc.)
    let app_delivery_hosts = collect_app_delivery_hosts(&env_path)?;
    for (app, host) in &app_delivery_hosts {
        log::info!("delivery host for `{app}`: {}", host.display());
    }
    // Probe union: prefer hostPath only if EVERY app host path is visible on the node.
    let project_root = infer_project_root(&env_path);

    let (delivery_mode, probe_detail) = if args.no_delivery {
        (None, None)
    } else {
        let (strategy, detail) = resolve_delivery_for_apps(
            args.delivery.as_deref(),
            &app_delivery_hosts,
            &SystemNodeProber,
        )?;
        (Some(strategy), Some(detail))
    };

    let mut runtime_values = BTreeMap::new();
    runtime_values.insert(
        "appRuntime".into(),
        serde_yaml::Value::String("cluster-dev".into()),
    );

    let opts = ReconcileOptions {
        namespace: namespace.clone(),
        workspace_name: name.clone(),
        runtime_values,
        app_delivery_host_paths: if matches!(
            delivery_mode,
            Some(DeliveryStrategy::HostPath) | Some(DeliveryStrategy::Sync)
        ) {
            app_delivery_hosts.clone()
        } else {
            BTreeMap::new()
        },
        delivery_mode: delivery_mode.map(|d| d.as_str().to_string()),
        dry_run: args.dry_run,
    };

    log::info!("Workspace `{name}` → namespace `{namespace}`");
    if let Some(d) = &probe_detail {
        log::info!("delivery probe: {d}");
    }

    // Shared CP gitops first (one cluster tree for the whole local CP), then env apps.
    let cluster_path = if args.no_cluster {
        None
    } else {
        match resolve_cluster_path(Some(&env_path), args.cluster.as_deref()) {
            Some(p) => Some(p.canonicalize().map_err(|e| {
                format!(
                    "cluster path {}: {e} (pass --cluster <dir> or set HOPS_LOCAL_CLUSTER)",
                    p.display()
                )
            })?),
            None => None,
        }
    };
    if let Some(ref cluster) = cluster_path {
        log::info!(
            "cluster gitops (shared CP, not per-worktree): {}",
            cluster.display()
        );
        match reconcile_cluster_dir(cluster, args.dry_run) {
            Ok(r) => {
                log::info!(
                    "cluster gitops: {} applied, {} error(s)",
                    r.applied.len(),
                    r.errors.len()
                );
            }
            Err(e) => {
                // Don't hard-fail app bring-up if packages aren't installed yet.
                log::warn!("cluster gitops reconcile: {e}");
            }
        }
    } else if !args.no_cluster {
        log::debug!(
            "no cluster gitops found (tried --cluster, $HOPS_LOCAL_CLUSTER, walk-up gitops/cluster); skipping platform apply"
        );
    }

    let results = reconcile_applications(&env_path, &opts, &SystemHelm, &SystemKubectl)?;
    for r in &results {
        log::info!(
            "  reconciled {} → {}",
            r.app_name,
            if r.applied { "applied" } else { "dry-run" }
        );
    }

    // Attach real sync delivery when strategy is Sync (per-app host paths)
    let mut sync_pids: Vec<u32> = Vec::new();
    let mut mutagen_sessions: Vec<String> = Vec::new();
    if !args.dry_run && !args.no_delivery {
        if let Some(DeliveryStrategy::Sync) = delivery_mode {
            let targets =
                wait_for_sync_targets(&namespace, &name, "/workspace", &app_delivery_hosts, 90);
            match attach_sync_delivery(&targets, &name, wants_watch(args)) {
                Ok(attach) => {
                    sync_pids = attach.sync_pids;
                    mutagen_sessions = attach.mutagen_sessions;
                    for m in attach.messages {
                        log::info!("delivery: {m}");
                    }
                }
                Err(e) => log::warn!("source delivery attach failed: {e}"),
            }
        } else if let Some(DeliveryStrategy::HostPath) = delivery_mode {
            log::info!(
                "source delivery: hostPath (per-app node-visible paths; no tar sync)"
            );
        }
    }

    // Discover services for URL card (workspace + related in-cluster FQDNs)
    let services = if args.dry_run {
        default_service_stubs(&namespace, &results)
    } else {
        discover_workspace_endpoints(&namespace).unwrap_or_else(|e| {
            log::debug!("service discovery deferred: {e}");
            default_service_stubs(&namespace, &results)
        })
    };

    // Workspace Services → cluster FQDNs + supervisor-kept port-forwards.
    let mut plan = plan_host_access(&namespace, &services);

    if !args.dry_run && !args.no_net && !services.is_empty() {
        match start_host_access(&namespace, &services, &state_dir, &name) {
            Ok((live_plan, rt)) => {
                plan = live_plan;
                log::info!("{}", host_access_status_line(&rt));
            }
            Err(e) => {
                log::warn!("host access start failed: {e}");
            }
        }
    } else if services.is_empty() {
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
    println!(
        "access:   cluster DNS (Service FQDNs; supervisor restarts port-forwards)"
    );
    println!();
    println!("Useful commands:");
    println!("  hops local status");
    println!("  hops local open");
    println!("  hops local down --name {name}");

    if wants_watch(args) {
        let env_for_watch = env_path.clone();
        let opts_watch = opts.clone();
        let cluster_for_watch = cluster_path.clone();
        let dry = args.dry_run;
        let cluster_arg = cluster_for_watch.clone();
        run_combined_gitops_watch(
            &env_path,
            cluster_arg.as_deref(),
            args.debounce,
            move |kind| {
                match kind {
                    WatchRebuild::Cluster => {
                        if let Some(ref c) = cluster_for_watch {
                            reconcile_cluster_dir(c, dry)?;
                        }
                    }
                    WatchRebuild::Env => {
                        reconcile_applications(
                            &env_for_watch,
                            &opts_watch,
                            &SystemHelm,
                            &SystemKubectl,
                        )?;
                    }
                    WatchRebuild::Both => {
                        if let Some(ref c) = cluster_for_watch {
                            let _ = reconcile_cluster_dir(c, dry);
                        }
                        reconcile_applications(
                            &env_for_watch,
                            &opts_watch,
                            &SystemHelm,
                            &SystemKubectl,
                        )?;
                    }
                }
                Ok(())
            },
        )?;
    }

    Ok(())
}

/// Watch by default; `--once` or dry-run for one-shot / CI.
fn wants_watch(args: &UpArgs) -> bool {
    !args.once && !args.dry_run
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WatchRebuild {
    Cluster,
    Env,
    Both,
}

/// Watch env Applications + charts + optional cluster gitops tree.
fn run_combined_gitops_watch<F>(
    env_path: &Path,
    cluster_path: Option<&Path>,
    debounce_secs: u64,
    mut rebuild: F,
) -> Result<(), Box<dyn Error>>
where
    F: FnMut(WatchRebuild) -> Result<(), Box<dyn Error>>,
{
    let roots = watch_roots_for_applications(env_path)?;
    let env_canon = env_path
        .canonicalize()
        .unwrap_or_else(|_| env_path.to_path_buf());
    let chart_paths: Vec<PathBuf> = roots
        .iter()
        .filter(|p| *p != &env_canon)
        .cloned()
        .collect();
    let cluster_canon = cluster_path.map(|c| {
        c.canonicalize().unwrap_or_else(|_| c.to_path_buf())
    });

    let debounce = Duration::from_secs(debounce_secs);
    let (tx, rx) = mpsc::channel::<WatchRebuild>();

    let env_c = env_canon.clone();
    let charts = chart_paths.clone();
    let cluster_c = cluster_canon.clone();
    let mut watcher =
        notify::recommended_watcher(move |res: notify::Result<notify::Event>| match res {
            Ok(event) => {
                let mut hit_cluster = false;
                let mut hit_env = false;
                for p in &event.paths {
                    if should_ignore_watch_path(p) {
                        continue;
                    }
                    if let Some(ref cp) = cluster_c {
                        if should_reconcile_cluster_change(p, cp) {
                            hit_cluster = true;
                            continue;
                        }
                    }
                    if is_chart_or_env_path(p, &env_c, &charts) == WatchPathClass::ChartOrEnv {
                        hit_env = true;
                    }
                }
                if hit_cluster && hit_env {
                    let _ = tx.send(WatchRebuild::Both);
                } else if hit_cluster {
                    let _ = tx.send(WatchRebuild::Cluster);
                } else if hit_env {
                    let _ = tx.send(WatchRebuild::Env);
                }
            }
            Err(e) => log::debug!("watch error: {e:?}"),
        })?;

    for root in &roots {
        if root.exists() {
            watcher.watch(root, RecursiveMode::Recursive)?;
            log::info!("Watching {}", root.display());
        }
    }
    if let Some(ref cp) = cluster_canon {
        if cp.exists() {
            watcher.watch(cp, RecursiveMode::Recursive)?;
            log::info!("Watching cluster gitops {}", cp.display());
        }
    }
    log::info!(
        "GitOps watch active (debounce {}s): env/charts + cluster → local CP. Ctrl+C to stop.",
        debounce_secs
    );

    loop {
        let first = rx.recv().map_err(|_| "watcher channel closed")?;
        let mut kind = first;
        // Debounce and merge events
        let mut deadline = Instant::now() + debounce;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            match rx.recv_timeout(remaining) {
                Ok(next) => {
                    kind = match (kind, next) {
                        (WatchRebuild::Both, _) | (_, WatchRebuild::Both) => WatchRebuild::Both,
                        (WatchRebuild::Cluster, WatchRebuild::Env)
                        | (WatchRebuild::Env, WatchRebuild::Cluster) => WatchRebuild::Both,
                        (a, _) => a,
                    };
                    deadline = Instant::now() + debounce;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => break,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err("watcher channel closed".into());
                }
            }
        }
        log::info!("──────────────────────────────────────────────");
        log::info!("GitOps change ({kind:?}), reconciling...");
        match rebuild(kind) {
            Ok(()) => log::info!("Reconcile succeeded."),
            Err(e) => log::error!("Reconcile failed: {e}"),
        }
    }
}

fn collect_app_delivery_hosts(
    env_path: &Path,
) -> Result<BTreeMap<String, PathBuf>, Box<dyn Error>> {
    let apps = load_applications(env_path)?;
    let mut map = BTreeMap::new();
    for (app_file, app) in apps {
        let host = resolve_delivery_host_path(&app_file, &app)?;
        map.insert(app.metadata.name, host);
    }
    Ok(map)
}

fn resolve_delivery_for_apps(
    override_mode: Option<&str>,
    app_hosts: &BTreeMap<String, PathBuf>,
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
    // HostPath only if every per-app path is visible on the node.
    let mut details = Vec::new();
    let mut all_visible = !app_hosts.is_empty();
    for (app, host) in app_hosts {
        let probe = prober.probe(host)?;
        details.push(format!("{app}: {}", probe.detail));
        if !probe.host_path_visible {
            all_visible = false;
        }
    }
    if app_hosts.is_empty() {
        all_visible = false;
        details.push("no apps".into());
    }
    let strategy = if all_visible {
        DeliveryStrategy::HostPath
    } else {
        DeliveryStrategy::Sync
    };
    let _ = select_delivery_strategy; // strategy already chosen from multi-path rule
    Ok((strategy, details.join("; ")))
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

/// Poll for Running pods labeled for this workspace, with per-app host paths.
fn wait_for_sync_targets(
    namespace: &str,
    workspace: &str,
    mount_path: &str,
    app_hosts: &BTreeMap<String, PathBuf>,
    timeout_secs: u64,
) -> Vec<super::workbench::delivery::SyncPodTarget> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    loop {
        match discover_sync_targets(namespace, workspace, mount_path, app_hosts) {
            Ok(t) if !t.is_empty() => return t,
            Ok(_) => {}
            Err(e) => log::debug!("pod discovery: {e}"),
        }
        if std::time::Instant::now() >= deadline {
            return discover_sync_targets(namespace, workspace, mount_path, app_hosts)
                .unwrap_or_default();
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}

fn default_service_stubs(
    namespace: &str,
    results: &[super::workbench::reconcile::ReconcileResult],
) -> Vec<ServiceEndpoint> {
    results
        .iter()
        .map(|r| ServiceEndpoint {
            namespace: namespace.to_string(),
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
