//! `hops local gitops` — control-plane and worktree Application reconcile.
//!
//! ```text
//! hops local gitops cluster  [PATH]   # shared CP (meta gitops/cluster)
//! hops local gitops worktree <PATH>   # per-worktree apps (envs → namespace = --name)
//! ```
//!
//! Both **watch by default**; pass `--once` for a single reconcile (CI/scripts).

use super::local_state_dir;
use super::workbench::application::{load_applications, resolve_delivery_host_path};
use super::workbench::cluster_gitops::{
    reconcile_cluster_dir, resolve_cluster_path, should_reconcile_cluster_change,
};
use super::workbench::delivery::{
    attach_sync_delivery, discover_sync_targets, save_delivery_runtime, stop_delivery_runtime,
    DeliveryStrategy, NodePathProber, SystemNodeProber,
};
use super::workbench::reconcile::{
    reconcile_applications, ReconcileOptions, SystemHelm, SystemKubectl,
};
use super::workbench::registry::{activate_workspace_cluster, load_workspace};
use super::workbench::watch::{
    is_chart_or_env_path, should_ignore_watch_path, watch_roots_for_applications, WatchPathClass,
};
use super::workbench::{namespace_for_name, slugify_name};
use clap::{Args, Subcommand};
use notify::{RecursiveMode, Watcher};
use std::collections::BTreeMap;
use std::error::Error;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

#[derive(Args, Debug)]
pub struct GitopsArgs {
    #[command(subcommand)]
    pub command: GitopsCommands,
}

#[derive(Subcommand, Debug)]
pub enum GitopsCommands {
    /// Shared control-plane gitops (packages, PSQLStack, AuthStack → local CP)
    Cluster(ClusterArgs),
    /// Per-worktree app Applications (charts → namespace = --name)
    Worktree(WorktreeArgs),
}

#[derive(Args, Debug)]
pub struct ClusterArgs {
    /// Path to cluster gitops directory (PSQLStack, AuthStack, packages, …).
    /// Default: `$HOPS_LOCAL_CLUSTER`, else walk up from cwd for `gitops/cluster`.
    #[arg(value_name = "PATH")]
    pub path: Option<PathBuf>,

    /// Run a single reconcile and exit (disables the default watch).
    #[arg(long, default_value_t = false)]
    pub once: bool,

    /// Watch and re-apply on YAML changes (default). Use `--once` to disable.
    #[arg(long, default_value_t = false)]
    pub watch: bool,

    /// Debounce seconds while watching.
    #[arg(long, default_value_t = 1)]
    pub debounce: u64,

    /// Server/client dry-run; do not persist to the cluster.
    #[arg(long, default_value_t = false)]
    pub dry_run: bool,
}

#[derive(Args, Debug)]
pub struct WorktreeArgs {
    /// Path to env directory of Application YAMLs (e.g. ./gitops/envs/local).
    #[arg(value_name = "PATH")]
    pub path: PathBuf,

    /// Destination namespace override (workspace isolation).
    #[arg(long, short = 'n')]
    pub namespace: Option<String>,

    /// Workspace name for labels (defaults from namespace / path).
    #[arg(long)]
    pub name: Option<String>,

    /// Run a single reconcile and exit (disables the default watch).
    #[arg(long, default_value_t = false)]
    pub once: bool,

    /// Watch env + chart paths and re-reconcile (default). Use `--once` to disable.
    #[arg(long, default_value_t = false)]
    pub watch: bool,

    /// Debounce seconds while watching.
    #[arg(long, default_value_t = 1)]
    pub debounce: u64,

    /// Render only; do not apply to the cluster.
    #[arg(long, default_value_t = false)]
    pub dry_run: bool,
}

pub fn run(args: &GitopsArgs) -> Result<(), Box<dyn Error>> {
    match &args.command {
        GitopsCommands::Cluster(a) => run_cluster(a),
        GitopsCommands::Worktree(a) => run_worktree(a),
    }
}

/// Run cluster gitops (same as `hops local gitops cluster`).
/// Used by `hops local start --gitops` so start is not a special code path.
pub fn run_cluster(args: &ClusterArgs) -> Result<(), Box<dyn Error>> {
    if !args.dry_run {
        if let Err(e) = super::run_cmd_output("kubectl", &["cluster-info"]) {
            return Err(format!(
                "Local control plane is not reachable ({e}).\n\
                 Ensure the selected control plane is Ready, then run `hops local start` with matching --cluster-provider and --docker-provider values."
            )
            .into());
        }
    }

    let cluster = resolve_cluster_path(None, args.path.as_deref()).ok_or_else(|| {
        "no cluster gitops directory found.\n\
         Pass a path: hops local gitops cluster ./gitops/cluster\n\
         Or set HOPS_LOCAL_CLUSTER, or create gitops/cluster at the meta repo root."
            .to_string()
    })?;
    let cluster = cluster
        .canonicalize()
        .map_err(|e| format!("cluster path {}: {e}", cluster.display()))?;

    let dry_run = args.dry_run;
    let do_once = || -> Result<(), Box<dyn Error>> {
        log::info!("cluster gitops → local CP: {}", cluster.display());
        let r = reconcile_cluster_dir(&cluster, dry_run)?;
        log::info!(
            "cluster gitops: {} applied, {} error(s)",
            r.applied.len(),
            r.errors.len()
        );
        if !r.errors.is_empty() && r.applied.is_empty() {
            return Err(format!(
                "cluster gitops failed ({} error(s)); first: {}",
                r.errors.len(),
                r.errors.first().map(String::as_str).unwrap_or("")
            )
            .into());
        }
        Ok(())
    };

    do_once()?;
    if args.once || args.dry_run {
        return Ok(());
    }

    run_cluster_watch(&cluster, args.debounce, do_once)
}

// ── worktree ─────────────────────────────────────────────────────────────────

fn run_worktree(args: &WorktreeArgs) -> Result<(), Box<dyn Error>> {
    let env_path = args
        .path
        .canonicalize()
        .map_err(|e| format!("env path {}: {e}", args.path.display()))?;

    let workspace_name = args
        .name
        .clone()
        .or_else(|| args.namespace.clone())
        .unwrap_or_else(|| {
            env_path
                .file_name()
                .and_then(|s| s.to_str())
                .map(slugify_name)
                .unwrap_or_else(|| "local".into())
        });

    let namespace = args
        .namespace
        .clone()
        .unwrap_or_else(|| namespace_for_name(&workspace_name));

    // Sticky workspace→cluster: use bound kube context when registered.
    if let Ok(state_dir) = local_state_dir() {
        if let Ok(Some(rec)) = load_workspace(&state_dir, &workspace_name) {
            if let Some((cluster, ctx)) = activate_workspace_cluster(&rec) {
                log::info!("worktree gitops: bound cluster `{cluster}` (context {ctx})");
            }
        }
    }

    let mut app_delivery_host_paths = BTreeMap::new();
    for (app_file, app) in load_applications(&env_path)? {
        let host = resolve_delivery_host_path(&app_file, &app)?;
        app_delivery_host_paths.insert(app.metadata.name, host);
    }
    let (delivery_strategy, delivery_detail) =
        resolve_worktree_delivery(&app_delivery_host_paths, &SystemNodeProber)?;
    log::info!(
        "worktree gitops: source delivery {} ({})",
        delivery_strategy.as_str(),
        delivery_detail
    );

    let opts = ReconcileOptions {
        namespace: namespace.clone(),
        workspace_name: workspace_name.clone(),
        runtime_values: BTreeMap::new(),
        app_delivery_host_paths,
        delivery_mode: Some(delivery_strategy.as_str().into()),
        dry_run: args.dry_run,
    };

    let do_once = || -> Result<(), Box<dyn Error>> {
        log::info!(
            "worktree gitops: Applications from {} → namespace {}",
            env_path.display(),
            opts.namespace
        );
        let results = reconcile_applications(&env_path, &opts, &SystemHelm, &SystemKubectl)?;
        for r in &results {
            log::info!(
                "  {} (chart {}) {}",
                r.app_name,
                r.chart_path.display(),
                if r.applied { "applied" } else { "rendered" }
            );
        }

        if !opts.dry_run {
            let state_dir = local_state_dir()?;
            // A previous run may have fallen back to a detached tar/mutagen
            // sync runtime. Retire it even when the current probe selects
            // hostPath, otherwise that stale writer keeps replacing files in
            // the mounted tree and repeatedly restarts dev servers.
            stop_delivery_runtime(&state_dir, &workspace_name);

            if delivery_strategy != DeliveryStrategy::Sync {
                return Ok(());
            }

            let targets = wait_for_sync_targets(
                &opts.namespace,
                &workspace_name,
                "/workspace",
                &opts.app_delivery_host_paths,
                90,
            );
            let attached =
                attach_sync_delivery(&targets, &workspace_name, !args.once && !args.dry_run)?;
            save_delivery_runtime(
                &state_dir,
                &workspace_name,
                &attached.mutagen_sessions,
                &attached.sync_pids,
            )?;
            for message in attached.messages {
                log::info!("delivery: {message}");
            }
        }
        Ok(())
    };

    do_once()?;
    if args.once || args.dry_run {
        return Ok(());
    }

    run_worktree_watch(&env_path, args.debounce, do_once)
}

fn resolve_worktree_delivery(
    app_paths: &BTreeMap<String, PathBuf>,
    prober: &dyn NodePathProber,
) -> Result<(DeliveryStrategy, String), Box<dyn Error>> {
    if app_paths.is_empty() {
        return Err("worktree gitops found no Application source paths".into());
    }

    let mut all_visible = true;
    let mut details = Vec::new();
    for (app, host) in app_paths {
        let probe = prober.probe(host)?;
        all_visible &= probe.host_path_visible;
        details.push(format!("{app}: {}", probe.detail));
    }

    let strategy = if all_visible {
        DeliveryStrategy::HostPath
    } else {
        DeliveryStrategy::Sync
    };
    Ok((strategy, details.join("; ")))
}

fn wait_for_sync_targets(
    namespace: &str,
    workspace: &str,
    mount_path: &str,
    app_hosts: &BTreeMap<String, PathBuf>,
    timeout_secs: u64,
) -> Vec<super::workbench::delivery::SyncPodTarget> {
    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    loop {
        match discover_sync_targets(namespace, workspace, mount_path, app_hosts) {
            Ok(targets) if !targets.is_empty() => return targets,
            Ok(_) => {}
            Err(error) => log::debug!("sync target discovery: {error}"),
        }
        if Instant::now() >= deadline {
            return discover_sync_targets(namespace, workspace, mount_path, app_hosts)
                .unwrap_or_default();
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

fn run_worktree_watch<F>(
    env_path: &Path,
    debounce_secs: u64,
    mut rebuild: F,
) -> Result<(), Box<dyn Error>>
where
    F: FnMut() -> Result<(), Box<dyn Error>>,
{
    let roots = watch_roots_for_applications(env_path)?;
    let env_canon = env_path
        .canonicalize()
        .unwrap_or_else(|_| env_path.to_path_buf());
    let chart_paths: Vec<PathBuf> = roots.iter().filter(|p| *p != &env_canon).cloned().collect();

    let debounce = Duration::from_secs(debounce_secs);
    let (tx, rx) = mpsc::channel();

    let env_c = env_canon.clone();
    let charts = chart_paths.clone();
    let mut watcher =
        notify::recommended_watcher(move |res: notify::Result<notify::Event>| match res {
            Ok(event) => {
                for p in &event.paths {
                    if should_ignore_watch_path(p) {
                        continue;
                    }
                    if is_chart_or_env_path(p, &env_c, &charts) == WatchPathClass::ChartOrEnv {
                        let _ = tx.send(());
                        break;
                    }
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
    log::info!(
        "Worktree gitops watch active (debounce {}s). Env YAML + charts only. Ctrl+C to stop.",
        debounce_secs
    );

    loop {
        rx.recv().map_err(|_| "watcher channel closed")?;
        wait_for_quiet(&rx, debounce)?;
        log::info!("──────────────────────────────────────────────");
        log::info!("Worktree gitops change, reconciling...");
        match rebuild() {
            Ok(()) => log::info!("Reconcile succeeded."),
            Err(e) => log::error!("Reconcile failed: {e}"),
        }
    }
}

// ── shared watch helpers ─────────────────────────────────────────────────────

fn run_cluster_watch<F>(
    cluster: &Path,
    debounce_secs: u64,
    mut rebuild: F,
) -> Result<(), Box<dyn Error>>
where
    F: FnMut() -> Result<(), Box<dyn Error>>,
{
    let debounce = Duration::from_secs(debounce_secs);
    let (tx, rx) = mpsc::channel();
    let cluster_c = cluster.to_path_buf();

    let mut watcher =
        notify::recommended_watcher(move |res: notify::Result<notify::Event>| match res {
            Ok(event) => {
                for p in &event.paths {
                    if should_ignore_watch_path(p) {
                        continue;
                    }
                    if should_reconcile_cluster_change(p, &cluster_c) {
                        let _ = tx.send(());
                        break;
                    }
                }
            }
            Err(e) => log::debug!("watch error: {e:?}"),
        })?;

    watcher.watch(cluster, RecursiveMode::Recursive)?;
    log::info!(
        "Watching cluster gitops {} (debounce {}s). Crossplane reconciles XRs after apply. Ctrl+C to stop.",
        cluster.display(),
        debounce_secs
    );

    loop {
        rx.recv().map_err(|_| "watcher channel closed")?;
        wait_for_quiet(&rx, debounce)?;
        log::info!("──────────────────────────────────────────────");
        log::info!("Cluster gitops change, applying to local CP...");
        match rebuild() {
            Ok(()) => log::info!("Cluster reconcile succeeded."),
            Err(e) => log::error!("Cluster reconcile failed: {e}"),
        }
    }
}

fn wait_for_quiet(rx: &mpsc::Receiver<()>, debounce: Duration) -> Result<(), Box<dyn Error>> {
    let mut deadline = Instant::now() + debounce;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(());
        }
        match rx.recv_timeout(remaining) {
            Ok(()) => deadline = Instant::now() + debounce,
            Err(mpsc::RecvTimeoutError::Timeout) => return Ok(()),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err("watcher channel closed".into());
            }
        }
    }
}
