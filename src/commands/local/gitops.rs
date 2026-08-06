//! `hops local gitops` — reconcile env Applications (advanced/internal).

use super::workbench::application::{load_applications, resolve_delivery_host_path};
use super::workbench::reconcile::{
    reconcile_applications, ReconcileOptions, SystemHelm, SystemKubectl,
};
use super::workbench::watch::{
    is_chart_or_env_path, should_ignore_watch_path, watch_roots_for_applications, WatchPathClass,
};
use super::workbench::{namespace_for_name, slugify_name};
use clap::Args;
use notify::{RecursiveMode, Watcher};
use std::collections::BTreeMap;
use std::error::Error;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

#[derive(Args, Debug)]
pub struct GitopsArgs {
    /// Path to env directory of Application YAMLs (or a single Application file).
    pub env_path: PathBuf,

    /// Destination namespace override (workspace isolation).
    #[arg(long, short = 'n')]
    pub namespace: Option<String>,

    /// Workspace name for labels (defaults from namespace / path).
    #[arg(long)]
    pub name: Option<String>,

    /// Run a single reconcile and exit.
    #[arg(long, default_value_t = false)]
    pub once: bool,

    /// Watch env + chart paths and re-reconcile on changes (not ordinary app source).
    #[arg(long, default_value_t = false)]
    pub watch: bool,

    /// Debounce seconds for --watch.
    #[arg(long, default_value_t = 1)]
    pub debounce: u64,

    /// Render only; do not apply to the cluster.
    #[arg(long, default_value_t = false)]
    pub dry_run: bool,
}

pub fn run(args: &GitopsArgs) -> Result<(), Box<dyn Error>> {
    let env_path = args
        .env_path
        .canonicalize()
        .map_err(|e| format!("env path {}: {e}", args.env_path.display()))?;

    let workspace_name = args
        .name
        .clone()
        .or_else(|| args.namespace.clone().map(|ns| strip_ns_prefix(&ns)))
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

    let mut app_delivery_host_paths = BTreeMap::new();
    if let Ok(apps) = load_applications(&env_path) {
        for (app_file, app) in apps {
            if let Ok(host) = resolve_delivery_host_path(&app_file, &app) {
                app_delivery_host_paths.insert(app.metadata.name, host);
            }
        }
    }

    let opts = ReconcileOptions {
        namespace: namespace.clone(),
        workspace_name: workspace_name.clone(),
        runtime_values: BTreeMap::new(),
        app_delivery_host_paths,
        delivery_mode: Some("sync".into()),
        dry_run: args.dry_run,
    };

    let do_once = || -> Result<(), Box<dyn Error>> {
        log::info!(
            "Reconciling Applications from {} → namespace {}",
            env_path.display(),
            opts.namespace
        );
        let results =
            reconcile_applications(&env_path, &opts, &SystemHelm, &SystemKubectl)?;
        for r in &results {
            log::info!(
                "  {} (chart {}) {}",
                r.app_name,
                r.chart_path.display(),
                if r.applied { "applied" } else { "rendered" }
            );
        }
        Ok(())
    };

    // Default: once if neither flag, or --once, or first pass before watch.
    if args.once || !args.watch {
        do_once()?;
        if !args.watch {
            return Ok(());
        }
    } else {
        do_once()?;
    }

    if args.watch {
        run_gitops_watch(&env_path, args.debounce, do_once)?;
    }
    Ok(())
}

fn strip_ns_prefix(ns: &str) -> String {
    ns.strip_prefix("hops-wt-")
        .unwrap_or(ns)
        .to_string()
}

/// Multi-root watch: env dir + chart paths only. App source edits are filtered out.
pub fn run_gitops_watch<F>(
    env_path: &Path,
    debounce_secs: u64,
    mut rebuild: F,
) -> Result<(), Box<dyn Error>>
where
    F: FnMut() -> Result<(), Box<dyn Error>>,
{
    let roots = watch_roots_for_applications(env_path)?;
    let chart_paths: Vec<PathBuf> = roots
        .iter()
        .filter(|p| *p != &env_path.canonicalize().unwrap_or_else(|_| env_path.to_path_buf()))
        .cloned()
        .collect();
    let env_canon = env_path
        .canonicalize()
        .unwrap_or_else(|_| env_path.to_path_buf());

    let debounce = Duration::from_secs(debounce_secs);
    let (tx, rx) = mpsc::channel();

    let mut watcher =
        notify::recommended_watcher(move |res: notify::Result<notify::Event>| match res {
            Ok(event) => {
                for p in &event.paths {
                    if should_ignore_watch_path(p) {
                        continue;
                    }
                    let class = is_chart_or_env_path(p, &env_canon, &chart_paths);
                    if class == WatchPathClass::ChartOrEnv {
                        let _ = tx.send(());
                        break;
                    }
                    log::debug!("watch ignore ({:?}): {}", class, p.display());
                }
            }
            Err(e) => log::debug!("watch error: {:?}", e),
        })?;

    for root in &roots {
        if root.exists() {
            watcher.watch(root, RecursiveMode::Recursive)?;
            log::info!("Watching {}", root.display());
        } else {
            log::warn!("watch root missing (skipped): {}", root.display());
        }
    }
    log::info!(
        "GitOps watch active (debounce {}s). Only env YAML and chart paths re-reconcile. Ctrl+C to stop.",
        debounce_secs
    );

    loop {
        rx.recv().map_err(|_| "watcher channel closed")?;
        wait_for_quiet(&rx, debounce)?;
        log::info!("──────────────────────────────────────────────");
        log::info!("Env/chart change detected, reconciling...");
        match rebuild() {
            Ok(()) => log::info!("Reconcile succeeded."),
            Err(e) => log::error!("Reconcile failed: {e}"),
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
