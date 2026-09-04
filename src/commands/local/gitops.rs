//! `hops local gitops` — control-plane and Environment reconcile.
//!
//! ```text
//! hops local gitops cluster [cluster.yaml] # lifecycle + shared CP manifests
//! hops local gitops environment <PATH> # Environment deploys → namespace = --name
//! ```
//!
//! Both **watch by default**; pass `--once` for a single reconcile (CI/scripts).

use super::backend::{ClusterProvider, DockerProvider};
use super::local_state_dir;
use super::workbench::cluster_gitops::{
    reconcile_cluster_dir_with_inventory, should_reconcile_cluster_change,
};
use super::workbench::controller::{
    acquire_controller, controller_lock_path, down_environment, list_environment_snapshots,
    reconcile_environment, release_controller_for_down, reset_absent_cluster_state,
    save_environment_snapshot,
};
use super::workbench::definition::{
    is_missing_definition_directory, load_environment_definition, local_deploy_name,
    prepare_cluster, prepare_cluster_for_stop, ClusterOverrides,
};
use super::workbench::delivery::{
    stop_delivery_runtime, DeliveryStrategy, NodePathProber, SystemNodeProber,
};
use super::workbench::ingress::ensure_ingress_access;
use super::workbench::reconcile::{ReconcileOptions, SystemHelm, SystemKubectl, SystemKustomize};
use super::workbench::registry::{load_workspace, save_workspace, WorkspaceRecord};
use super::workbench::slugify_name;
use super::workbench::watch::should_ignore_watch_path;
use clap::{Args, Subcommand};
use notify::{RecursiveMode, Watcher};
#[cfg(test)]
use serde_yaml::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fs;
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
    /// Reconcile an Environment's explicit deploy directories → namespace = --name
    Environment(EnvironmentArgs),
}

#[derive(Args, Debug)]
pub struct ClusterArgs {
    /// Kubernetes-shaped Cluster definition. Defaults to .gitops/local/cluster.yaml.
    #[arg(value_name = "PATH")]
    pub path: Option<PathBuf>,

    /// Stop the declared Cluster instead of starting and watching it.
    #[arg(long, default_value_t = false, conflicts_with = "dry_run")]
    pub down: bool,

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
pub struct EnvironmentArgs {
    /// Reusable Environment YAML.
    /// Optional with --down, which resolves the registered Environment by name.
    #[arg(value_name = "PATH")]
    pub path: Option<PathBuf>,

    /// Purge and unregister this Environment instead of reconciling it.
    #[arg(long, default_value_t = false, conflicts_with = "dry_run")]
    pub down: bool,

    /// Destination namespace override (workspace isolation).
    #[arg(long, short = 'n')]
    pub namespace: Option<String>,

    /// Runtime Environment name (defaults from the containing worktree name).
    #[arg(long)]
    pub name: Option<String>,

    /// Run a single reconcile and exit (disables the default watch).
    #[arg(long, default_value_t = false)]
    pub once: bool,

    /// Watch env + deploy paths and re-reconcile (default). Use `--once` to disable.
    #[arg(long, default_value_t = false)]
    pub watch: bool,

    /// Debounce seconds while watching.
    #[arg(long, default_value_t = 1)]
    pub debounce: u64,

    /// Render only; do not apply to the cluster.
    #[arg(long, default_value_t = false)]
    pub dry_run: bool,
}

pub fn run_environment_command(
    args: &GitopsArgs,
    overrides: ClusterOverrides<'_>,
) -> Result<(), Box<dyn Error>> {
    match &args.command {
        GitopsCommands::Environment(a) => run_environment(a, overrides),
        GitopsCommands::Cluster(_) => Err(
            "internal dispatch error: Cluster must be activated before generic local dispatch"
                .into(),
        ),
    }
}

/// Start or resume the declared control plane, then reconcile its shared
/// manifests. The Cluster definition is the single lifecycle entry point.
pub fn run_cluster(
    args: &ClusterArgs,
    overrides: ClusterOverrides<'_>,
) -> Result<(), Box<dyn Error>> {
    let (definition, backend) = if args.down {
        prepare_cluster_for_stop(args.path.as_deref(), overrides)?
    } else {
        prepare_cluster(args.path.as_deref(), overrides)?
    };

    if args.down {
        release_controller_for_down(&definition.cluster.name, &definition.source)?;
        stop_cluster_environment_runtime(&definition.cluster.name)?;
        if definition.cluster.cluster_provider == super::backend::ClusterProvider::Kind
            && !super::backend::kind::cluster_exists()
        {
            log::info!("Cluster '{}' is already down", definition.cluster.name);
            return Ok(());
        }
        backend.stop()?;
        return Ok(());
    }

    let cluster = definition.cluster.manifests_path.clone();
    let inventory = local_state_dir()?
        .join("clusters")
        .join(slugify_name(&definition.cluster.name))
        .join("cluster-inventory.json");

    let context = super::kube_context_from_env()
        .unwrap_or_else(|| format!("kind-{}", definition.cluster.name));
    if args.dry_run {
        log::info!(
            "Dry-run uses the declared Cluster '{}' without changing its lifecycle",
            definition.cluster.name
        );
        let controller =
            acquire_controller(&definition.cluster.name, &definition.source, &context, true)?;
        return run_cluster_reconcile_loop(args, definition, cluster, inventory, true, controller);
    }

    if !backend.cluster_exists() {
        stop_cluster_environment_runtime(&definition.cluster.name)?;
        if reset_absent_cluster_state(&definition.cluster.name)? {
            log::info!(
                "Cluster '{}' backend is absent; cleared obsolete machine-local ownership and inventory before creating it again",
                definition.cluster.name
            );
        }
    }

    let controller = acquire_controller(
        &definition.cluster.name,
        &definition.source,
        &context,
        false,
    )?;
    if controller.reused {
        log::info!(
            "Cluster '{}' is already reconciled by controller pid {}; reusing that owner",
            definition.cluster.name,
            controller.lease.pid
        );
        return Ok(());
    }
    super::start::run_gitops_seed(
        backend,
        &super::start::StartArgs {
            size: super::backend::SizeArgs::default(),
            yes: false,
            bootstrap: false,
        },
        &definition.cluster.control_plane.crossplane.chart,
        &definition.cluster.control_plane.crossplane.version,
        &definition.cluster.control_plane.crossplane.values,
    )?;

    // Keep the lease alive through the foreground watcher. The guard is
    // intentionally scoped to this command so Ctrl-C releases only this
    // controller's lock; the Kubernetes inventory remains last-known-good.
    run_cluster_reconcile_loop(args, definition, cluster, inventory, false, controller)
}

fn stop_cluster_environment_runtime(cluster_name: &str) -> Result<(), Box<dyn Error>> {
    let state_dir = local_state_dir()?;
    for snapshot in list_environment_snapshots(cluster_name)? {
        if let Err(error) =
            super::workbench::ingress::stop_ingress_access(&state_dir, &snapshot.name)
        {
            log::warn!(
                "Cluster {} Environment {} ingress-access cleanup: {}",
                cluster_name,
                snapshot.name,
                error
            );
        }
        if let Err(error) = super::workbench::net::stop_host_access(&state_dir, &snapshot.name) {
            log::warn!(
                "Cluster {} Environment {} host-access cleanup: {}",
                cluster_name,
                snapshot.name,
                error
            );
        }
        stop_delivery_runtime(&state_dir, &snapshot.name);
    }
    Ok(())
}

fn run_cluster_reconcile_loop(
    args: &ClusterArgs,
    definition: super::workbench::definition::LoadedDefinition,
    cluster: PathBuf,
    inventory: PathBuf,
    dry_run: bool,
    _controller: super::workbench::controller::ControllerHandle,
) -> Result<(), Box<dyn Error>> {
    let do_once = || -> Result<(), Box<dyn Error>> {
        log::info!("cluster gitops → local CP: {}", cluster.display());
        let r = reconcile_cluster_dir_with_inventory(&cluster, &inventory, dry_run)?;
        log::info!(
            "cluster gitops: {} applied, {} pruned, {} error(s)",
            r.applied.len(),
            r.pruned.len(),
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
        if !reconcile_cluster_secret_sync(&definition, dry_run) {
            return Ok(());
        }
        reconcile_cluster_environments_with_retry(&definition, dry_run)?;
        Ok(())
    };

    do_once()?;
    if args.once || args.dry_run {
        return Ok(());
    }

    run_cluster_watch(
        &cluster,
        &definition.cluster.mount_root,
        definition
            .cluster
            .secret_sync
            .as_ref()
            .map(|secret_sync| secret_sync.path.as_path()),
        args.debounce,
        do_once,
    )
}

fn reconcile_cluster_secret_sync(
    definition: &super::workbench::definition::LoadedDefinition,
    dry_run: bool,
) -> bool {
    run_secret_sync_phase(
        definition
            .cluster
            .secret_sync
            .as_ref()
            .map(|secret_sync| secret_sync.path.as_path()),
        dry_run,
        crate::commands::secrets::sync_vault_path,
    )
}

fn run_secret_sync_phase<F>(secret_sync_path: Option<&Path>, dry_run: bool, sync: F) -> bool
where
    F: FnOnce(&Path) -> Result<(), Box<dyn Error>>,
{
    let Some(secret_sync_path) = secret_sync_path else {
        return true;
    };
    if dry_run {
        log::info!(
            "Dry-run leaves configured local Vault inputs at {} unchanged",
            secret_sync_path.display()
        );
        return true;
    }

    match sync(secret_sync_path) {
        Ok(()) => true,
        Err(error) => {
            // A failed sync must not replace a healthy Environment with a
            // partial render. The controller remains alive so a credential,
            // Vault recovery, or watched input change can converge it later.
            log::error!(
                "Local Vault secret sync failed; Environment reconciliation is deferred: {error}"
            );
            false
        }
    }
}

fn reconcile_cluster_environments(
    definition: &super::workbench::definition::LoadedDefinition,
    dry_run: bool,
) -> Result<(), Box<dyn Error>> {
    let environment_files = discover_environment_definitions(&definition.cluster.mount_root)?;
    let kube_context = super::kube_context_from_env()
        .unwrap_or_else(|| format!("kind-{}", definition.cluster.name));
    let mut errors = Vec::new();
    let mut loaded_environments = Vec::new();
    let mut names = BTreeSet::new();
    let mut has_pending_environment = false;
    for environment_file in environment_files {
        let loaded = match load_environment_definition(&environment_file, definition, None, None) {
            Ok(loaded) => loaded,
            Err(error) if is_missing_definition_directory(error.as_ref()) => {
                has_pending_environment = true;
                log::warn!(
                    "controller deferred incomplete Environment {}: {}",
                    environment_file.display(),
                    error
                );
                continue;
            }
            Err(error) => {
                errors.push(format!("{}: {error}", environment_file.display()));
                continue;
            }
        };
        if !names.insert(loaded.environment.name.clone()) {
            errors.push(format!(
                "{}: duplicate Environment runtime name {:?}",
                environment_file.display(),
                loaded.environment.name
            ));
            continue;
        }
        loaded_environments.push(loaded);
    }
    if !errors.is_empty() {
        return Err(format!(
            "controller Environment validation failed for {} source(s):\n  - {}",
            errors.len(),
            errors.join("\n  - ")
        )
        .into());
    }
    for loaded in &loaded_environments {
        let mut hosts = BTreeMap::new();
        for deploy in &loaded.environment.deploys {
            hosts.insert(local_deploy_name(deploy), loaded.environment.root.clone());
        }
        let (delivery_strategy, detail) = match resolve_worktree_delivery(&hosts, &SystemNodeProber)
        {
            Ok(result) => result,
            Err(error) => {
                errors.push(format!(
                    "{}: source delivery: {error}",
                    loaded.source.display()
                ));
                continue;
            }
        };
        log::info!(
            "controller Environment {}: source delivery {} ({})",
            loaded.environment.name,
            delivery_strategy.as_str(),
            detail
        );
        let opts = ReconcileOptions {
            namespace: loaded.environment.namespace.clone(),
            workspace_name: loaded.environment.name.clone(),
            runtime_values: BTreeMap::new(),
            app_delivery_host_paths: hosts,
            delivery_mode: Some(delivery_strategy.as_str().into()),
            dry_run,
        };
        match reconcile_environment(loaded, &opts, &SystemHelm, &SystemKustomize, &SystemKubectl) {
            Ok(results) => {
                if !dry_run {
                    if let Err(error) = save_environment_snapshot(
                        &definition.cluster.name,
                        &kube_context,
                        &loaded,
                        &results,
                    ) {
                        errors.push(format!(
                            "{}: save ownership: {error}",
                            loaded.source.display()
                        ));
                        continue;
                    }
                    if let Err(error) = persist_environment_registration(
                        &loaded.environment.name,
                        &loaded.source,
                        &loaded.environment.root,
                        &definition.cluster.name,
                        &kube_context,
                        &loaded.environment.namespace,
                        delivery_strategy,
                    ) {
                        errors.push(format!(
                            "{}: save registration: {error}",
                            loaded.source.display()
                        ));
                        continue;
                    }
                    if let Err(error) = reconcile_browser_ingress(
                        definition.cluster.cluster_provider,
                        definition.cluster.docker_provider,
                        false,
                        &loaded.environment.namespace,
                        &loaded.environment.name,
                    ) {
                        errors.push(format!(
                            "{}: browser ingress: {error}",
                            loaded.source.display()
                        ));
                    }
                }
            }
            Err(error) => errors.push(format!("{}: {error}", loaded.source.display())),
        }
    }
    if errors.is_empty() && !has_pending_environment {
        let active_names = loaded_environments
            .iter()
            .map(|loaded| loaded.environment.name.clone())
            .collect::<BTreeSet<_>>();
        for snapshot in list_environment_snapshots(&definition.cluster.name)? {
            if active_names.contains(&snapshot.name) {
                continue;
            }
            if dry_run {
                log::info!(
                    "dry-run would prune removed Environment {} from Cluster {}",
                    snapshot.name,
                    definition.cluster.name
                );
            } else if let Err(error) = down_environment(&definition.cluster.name, &snapshot.name) {
                errors.push(format!("removed Environment {}: {error}", snapshot.name));
            }
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "controller Environment reconcile failed for {} source(s):\n  - {}",
            errors.len(),
            errors.join("\n  - ")
        )
        .into())
    }
}

fn reconcile_cluster_environments_with_retry(
    definition: &super::workbench::definition::LoadedDefinition,
    dry_run: bool,
) -> Result<(), Box<dyn Error>> {
    const ATTEMPTS: usize = 6;
    let mut last_error = None;
    for attempt in 0..ATTEMPTS {
        match reconcile_cluster_environments(definition, dry_run) {
            Ok(()) => return Ok(()),
            Err(error) => {
                last_error = Some(error);
                if attempt + 1 < ATTEMPTS {
                    let seconds = 1_u64 << attempt.min(3);
                    log::warn!(
                        "Environment reconcile is not ready yet; retrying in {}s ({}/{})",
                        seconds,
                        attempt + 1,
                        ATTEMPTS
                    );
                    std::thread::sleep(Duration::from_secs(seconds));
                }
            }
        }
    }
    Err(last_error.unwrap_or_else(|| "Environment reconcile failed".into()))
}

fn discover_environment_definitions(root: &Path) -> Result<Vec<PathBuf>, Box<dyn Error>> {
    let mut found = Vec::new();
    discover_environment_definitions_rec(root, &mut found)?;
    found.sort();
    found.dedup();
    Ok(found)
}

fn discover_environment_definitions_rec(
    directory: &Path,
    found: &mut Vec<PathBuf>,
) -> Result<(), Box<dyn Error>> {
    if !directory.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            continue;
        }
        if path.is_dir() {
            let name = path
                .file_name()
                .and_then(|value| value.to_str())
                .unwrap_or("");
            if matches!(name, ".git" | "node_modules" | "target" | "dist" | "build") {
                continue;
            }
            discover_environment_definitions_rec(&path, found)?;
        } else if path.file_name().and_then(|value| value.to_str()) == Some("environment.yaml")
            && path
                .parent()
                .and_then(Path::file_name)
                .and_then(|value| value.to_str())
                == Some("local")
            && path
                .parent()
                .and_then(Path::parent)
                .and_then(Path::file_name)
                .and_then(|value| value.to_str())
                == Some(".gitops")
        {
            found.push(path.canonicalize()?);
        }
    }
    Ok(())
}

// ── environment ──────────────────────────────────────────────────────────────

fn run_environment(
    args: &EnvironmentArgs,
    overrides: ClusterOverrides<'_>,
) -> Result<(), Box<dyn Error>> {
    if args.down {
        let name = args
            .name
            .as_deref()
            .ok_or("gitops environment --down requires --name")?;
        let state_dir = local_state_dir()?;
        let record = load_workspace(&state_dir, name)?
            .ok_or_else(|| format!("Environment {name:?} has no durable registration"))?;
        let cluster_name = record
            .cluster_name
            .as_deref()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| format!("Environment {name:?} has no bound Cluster"))?;
        if !down_environment(cluster_name, name)? {
            log::info!("Environment {name:?} is already down; no ownership snapshot exists");
        }
        return Ok(());
    }

    let path = args
        .path
        .as_deref()
        .ok_or("Environment PATH is required unless --down is used with a registered --name")?;
    run_environment_definition(args, path, overrides)
}

fn run_environment_definition(
    args: &EnvironmentArgs,
    path: &Path,
    overrides: ClusterOverrides<'_>,
) -> Result<(), Box<dyn Error>> {
    let source = path
        .canonicalize()
        .map_err(|error| format!("Environment path {}: {error}", path.display()))?;
    let cluster_path = discover_cluster_definition(&source).ok_or_else(|| {
        format!(
            "no sibling or ancestor Cluster definition found for {}",
            source.display()
        )
    })?;
    let (cluster, _) = prepare_cluster(Some(&cluster_path), overrides)?;
    let loaded = load_environment_definition(
        &source,
        &cluster,
        args.name.as_deref(),
        args.namespace.as_deref(),
    )?;
    let workspace_name = loaded.environment.name.clone();
    let namespace = loaded.environment.namespace.clone();
    let worktree_root = loaded.environment.root.clone();
    let mut deploy_watch_roots = BTreeSet::new();
    for deploy in &loaded.environment.deploys {
        deploy_watch_roots.insert(deploy.source_path.clone());
    }
    let deploy_watch_roots: Vec<_> = deploy_watch_roots.into_iter().collect();
    let kube_context = super::kube_context_from_env().ok_or_else(|| {
        format!(
            "declared Cluster {:?} has no available kube context; start it with `hops local gitops cluster {}` first",
            cluster.cluster.name,
            cluster_path.display()
        )
    })?;
    let deploys = loaded.environment.deploys.clone();
    let mut app_delivery_host_paths = BTreeMap::new();
    for deploy in &deploys {
        app_delivery_host_paths.insert(local_deploy_name(deploy), loaded.environment.root.clone());
    }
    let (delivery_strategy, delivery_detail) =
        resolve_worktree_delivery(&app_delivery_host_paths, &SystemNodeProber)?;
    log::info!(
        "environment gitops: source delivery {} ({})",
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

    let reconcile = || -> Result<(), Box<dyn Error>> {
        let results = reconcile_environment(
            &loaded,
            &opts,
            &SystemHelm,
            &SystemKustomize,
            &SystemKubectl,
        )?;
        if !args.dry_run {
            save_environment_snapshot(&cluster.cluster.name, &kube_context, &loaded, &results)?;
            persist_environment_registration(
                &workspace_name,
                &source,
                &worktree_root,
                &cluster.cluster.name,
                &kube_context,
                &namespace,
                delivery_strategy,
            )?;
            reconcile_browser_ingress(
                cluster.cluster.cluster_provider,
                cluster.cluster.docker_provider,
                false,
                &namespace,
                &workspace_name,
            )?;
        }
        Ok(())
    };

    reconcile()?;
    if args.once || args.dry_run {
        return Ok(());
    }

    if controller_lock_path(&cluster.cluster.name)?.is_file() {
        log::info!(
            "Cluster controller owns Environment watch for {}; reconcile completed without starting a second watcher",
            workspace_name
        );
        return Ok(());
    }

    run_environment_watch(
        &source,
        &worktree_root,
        &deploy_watch_roots,
        &workspace_name,
        args.debounce,
        reconcile,
    )
}

fn discover_cluster_definition(environment_file: &Path) -> Option<PathBuf> {
    environment_file
        .parent()?
        .ancestors()
        .map(|directory| directory.join("cluster.yaml"))
        .find(|candidate| candidate.is_file())
}

#[cfg(test)]
fn merge_mapping(base: &mut serde_yaml::Mapping, overlay: &serde_yaml::Mapping) {
    for (key, value) in overlay {
        match (base.get_mut(key), value) {
            (Some(Value::Mapping(base_map)), Value::Mapping(overlay_map)) => {
                merge_mapping(base_map, overlay_map)
            }
            _ => {
                base.insert(key.clone(), value.clone());
            }
        }
    }
}

#[cfg(test)]
fn string_mapping(values: &[(&str, &str)]) -> Value {
    let mut mapping = serde_yaml::Mapping::new();
    for (key, value) in values {
        mapping.insert(
            Value::String((*key).to_string()),
            Value::String((*value).to_string()),
        );
    }
    Value::Mapping(mapping)
}

fn persist_environment_registration(
    workspace_name: &str,
    source: &Path,
    worktree_root: &Path,
    cluster_name: &str,
    kube_context: &str,
    namespace: &str,
    delivery_strategy: DeliveryStrategy,
) -> Result<(), Box<dyn Error>> {
    let state_dir = local_state_dir()?;
    let mut record =
        load_workspace(&state_dir, workspace_name)?.unwrap_or_else(|| WorkspaceRecord {
            name: workspace_name.to_string(),
            namespace: namespace.to_string(),
            env_path: source.to_string_lossy().into_owned(),
            project_root: Some(worktree_root.to_string_lossy().into_owned()),
            delivery_mode: Some(delivery_strategy.as_str().to_string()),
            updated_at: None,
            cluster_name: Some(cluster_name.to_string()),
            kube_context: Some(kube_context.to_string()),
        });
    record.env_path = source.to_string_lossy().into_owned();
    record.project_root = Some(worktree_root.to_string_lossy().into_owned());
    record.namespace = namespace.to_string();
    record.delivery_mode = Some(delivery_strategy.as_str().to_string());
    record.cluster_name = Some(cluster_name.to_string());
    record.kube_context = Some(kube_context.to_string());
    save_workspace(&state_dir, &record)?;
    Ok(())
}

fn should_reconcile_browser_ingress(
    cluster_provider: ClusterProvider,
    docker_provider: DockerProvider,
    dry_run: bool,
) -> bool {
    cluster_provider == ClusterProvider::Kind && docker_provider == DockerProvider::Dory && !dry_run
}

fn reconcile_browser_ingress(
    cluster_provider: ClusterProvider,
    docker_provider: DockerProvider,
    dry_run: bool,
    namespace: &str,
    workspace_name: &str,
) -> Result<(), Box<dyn Error>> {
    if !should_reconcile_browser_ingress(cluster_provider, docker_provider, dry_run) {
        return Ok(());
    }
    let state_dir = local_state_dir()?;
    let (plan, _runtime, changed) = ensure_ingress_access(namespace, &state_dir, workspace_name)?;
    if changed && !plan.urls.is_empty() {
        log::info!(
            "Environment {} browser ingress reconciled: {}",
            workspace_name,
            plan.urls.values().cloned().collect::<Vec<_>>().join(", ")
        );
    }
    Ok(())
}

fn run_environment_watch<F>(
    environment_file: &Path,
    worktree_root: &Path,
    chart_roots: &[PathBuf],
    workspace_name: &str,
    debounce_secs: u64,
    mut rebuild: F,
) -> Result<(), Box<dyn Error>>
where
    F: FnMut() -> Result<(), Box<dyn Error>>,
{
    let debounce = Duration::from_secs(debounce_secs);
    let (tx, rx) = mpsc::channel();
    let source = environment_file.to_path_buf();
    let watched_charts = chart_roots.to_vec();
    let mut watcher =
        notify::recommended_watcher(move |result: notify::Result<notify::Event>| match result {
            Ok(event) => {
                for path in event.paths {
                    if should_ignore_watch_path(&path) {
                        continue;
                    }
                    if is_environment_watch_path(&path, &source, &watched_charts) {
                        let _ = tx.send(());
                        break;
                    }
                }
            }
            Err(error) => log::debug!("Environment watch error: {error:?}"),
        })?;
    let environment_parent = environment_file.parent().ok_or_else(|| {
        format!(
            "Environment definition has no parent: {}",
            environment_file.display()
        )
    })?;
    watcher.watch(environment_parent, RecursiveMode::NonRecursive)?;
    for root in chart_roots {
        if root.is_dir() {
            watcher.watch(root, RecursiveMode::Recursive)?;
        }
    }
    log::info!(
        "Watching Environment {} and {} referenced local chart roots under {} (debounce {}s). Ctrl+C to stop.",
        environment_file.display(),
        chart_roots.len(),
        worktree_root.display(),
        debounce_secs
    );
    loop {
        rx.recv()
            .map_err(|_| "Environment watcher channel closed")?;
        wait_for_quiet(&rx, debounce)?;
        if !environment_file.exists() {
            log::info!(
                "Environment definition {} was removed; purging Environment `{}`",
                environment_file.display(),
                workspace_name
            );
            let state_dir = local_state_dir()?;
            let record = load_workspace(&state_dir, workspace_name)?;
            let cluster_name = record
                .as_ref()
                .and_then(|record| record.cluster_name.as_deref())
                .filter(|value| !value.is_empty())
                .ok_or_else(|| format!("Environment {workspace_name:?} has no bound Cluster"))?;
            if !down_environment(cluster_name, workspace_name)? {
                log::info!(
                    "Environment {workspace_name:?} has no ownership snapshot; leaving resources untouched"
                );
            }
            return Ok(());
        }
        match rebuild() {
            Ok(()) => log::info!("Environment reconcile succeeded."),
            Err(error) => log::error!("Environment reconcile failed: {error}"),
        }
    }
}

fn is_environment_watch_path(path: &Path, source: &Path, chart_roots: &[PathBuf]) -> bool {
    path == source || chart_roots.iter().any(|root| path.starts_with(root))
}

fn resolve_worktree_delivery(
    app_paths: &BTreeMap<String, PathBuf>,
    prober: &dyn NodePathProber,
) -> Result<(DeliveryStrategy, String), Box<dyn Error>> {
    if app_paths.is_empty() {
        return Err("environment gitops found no deploy source paths".into());
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

// ── shared watch helpers ─────────────────────────────────────────────────────

fn run_cluster_watch<F>(
    cluster: &Path,
    project_root: &Path,
    secret_sync_root: Option<&Path>,
    debounce_secs: u64,
    mut rebuild: F,
) -> Result<(), Box<dyn Error>>
where
    F: FnMut() -> Result<(), Box<dyn Error>>,
{
    let debounce = Duration::from_secs(debounce_secs);
    let (tx, rx) = mpsc::channel();
    let cluster_c = cluster.to_path_buf();
    let project_root_c = project_root.to_path_buf();
    let secret_sync_root_c = secret_sync_root.map(Path::to_path_buf);

    let mut watcher =
        notify::recommended_watcher(move |res: notify::Result<notify::Event>| match res {
            Ok(event) => {
                for p in &event.paths {
                    if should_ignore_watch_path(p) {
                        continue;
                    }
                    if is_cluster_watch_path(
                        p,
                        &cluster_c,
                        &project_root_c,
                        secret_sync_root_c.as_deref(),
                    ) {
                        let _ = tx.send(());
                        break;
                    }
                }
            }
            Err(e) => log::debug!("watch error: {e:?}"),
        })?;

    watcher.watch(cluster, RecursiveMode::Recursive)?;
    if project_root != cluster && project_root.is_dir() {
        watcher.watch(project_root, RecursiveMode::Recursive)?;
    }
    log::info!(
        "Watching Cluster tree {}, project Environment/deploy paths, and configured secret inputs (debounce {}s). Ctrl+C to stop.",
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

fn is_cluster_watch_path(
    path: &Path,
    cluster: &Path,
    project_root: &Path,
    secret_sync_root: Option<&Path>,
) -> bool {
    should_reconcile_cluster_change(path, cluster)
        || is_controller_owned_path(path, project_root)
        || secret_sync_root.is_some_and(|root| path.starts_with(root))
}

fn is_controller_owned_path(path: &Path, project_root: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(project_root) else {
        return false;
    };
    let components = relative
        .components()
        .filter_map(|component| component.as_os_str().to_str())
        .collect::<Vec<_>>();
    let Some(gitops) = components
        .iter()
        .position(|component| *component == ".gitops")
    else {
        return false;
    };
    let Some(scope) = components.get(gitops + 1) else {
        return false;
    };
    matches!(*scope, "local" | "test-users" | "promote")
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

#[cfg(test)]
mod tests {
    use super::super::workbench::definition::load_definition;
    use super::*;
    use std::fs;

    #[test]
    fn renders_reusable_environment_for_runtime_identity() {
        let root = std::env::temp_dir().join(format!(
            "hops-gitops-environment-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&root).unwrap();
        let root = root.canonicalize().unwrap();
        fs::create_dir_all(root.join(".gitops/local/cluster")).unwrap();
        fs::create_dir_all(root.join("apps/gateway/.gitops/local")).unwrap();
        fs::write(
            root.join(".gitops/local/cluster.yaml"),
            r#"apiVersion: hops.local/v1alpha1
kind: Cluster
metadata:
  name: project-dev
spec:
  clusterProvider: kind
  dockerProvider: dory
  mountRoot: ../..
  manifests:
    path: .gitops/local/cluster
"#,
        )
        .unwrap();
        fs::write(
            root.join(".gitops/local/environment.yaml"),
            r#"apiVersion: hops.local/v1alpha1
kind: Environment
metadata:
  name: local
spec:
  clusterRef:
    name: project-dev
  root: .
  values:
    local: false
    preview: false
    feature:
      enabled: false
  deploys:
    - path: apps/gateway/.gitops/local
      type: helm
      values:
        feature:
          enabled: true
        revision: worktree
"#,
        )
        .unwrap();

        let cluster = load_definition(&root.join(".gitops/local/cluster.yaml")).unwrap();
        let loaded = load_environment_definition(
            &root.join(".gitops/local/environment.yaml"),
            &cluster,
            Some("feature-auth"),
            Some("feature-auth-ns"),
        )
        .unwrap();
        let deploy = &loaded.environment.deploys[0];
        let mut values = loaded.environment.values.clone();
        merge_mapping(&mut values, &deploy.values);
        values.insert(Value::String("local".into()), Value::Bool(true));
        values.insert(
            Value::String("environment".into()),
            string_mapping(&[
                ("name", &loaded.environment.name),
                ("namespace", &loaded.environment.namespace),
            ]),
        );
        assert_eq!(values["local"], Value::Bool(true));
        assert_eq!(values["preview"], Value::Bool(false));
        assert_eq!(values["feature"]["enabled"], Value::Bool(true));
        assert_eq!(values["revision"], Value::String("worktree".into()));
        assert_eq!(
            values["environment"]["name"],
            Value::String("feature-auth".into())
        );
        assert_eq!(
            values["environment"]["namespace"],
            Value::String("feature-auth-ns".into())
        );
        assert_eq!(local_deploy_name(deploy), "gateway");
        assert_eq!(deploy.source_path, root.join("apps/gateway/.gitops/local"));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn environment_watch_filters_to_definition_and_referenced_charts() {
        let source = Path::new("/project/.gitops/local/environment.yaml");
        let chart_roots = vec![PathBuf::from("/project/apps/api/.gitops/local")];
        assert!(is_environment_watch_path(source, source, &chart_roots));
        assert!(is_environment_watch_path(
            Path::new("/project/apps/api/.gitops/local/values.yaml"),
            source,
            &chart_roots,
        ));
        assert!(!is_environment_watch_path(
            Path::new("/project/apps/api/src/main.rs"),
            source,
            &chart_roots,
        ));
        assert!(!is_environment_watch_path(
            Path::new("/project/apps/api/.gitops/deploy/values.yaml"),
            source,
            &chart_roots,
        ));
        assert!(!is_environment_watch_path(
            Path::new("/project/apps/other/.gitops/deploy/values.yaml"),
            source,
            &chart_roots,
        ));
    }

    #[test]
    fn cluster_watch_includes_only_the_configured_secret_input_tree() {
        let project_root = Path::new("/project");
        let cluster = project_root.join(".gitops/local/cluster");
        let secret_root = project_root.join("secrets/vault");

        assert!(is_cluster_watch_path(
            &secret_root.join("harmony/application/.env"),
            &cluster,
            project_root,
            Some(&secret_root),
        ));
        assert!(!is_cluster_watch_path(
            &project_root.join("secrets/not-configured/.env"),
            &cluster,
            project_root,
            Some(&secret_root),
        ));
    }

    #[test]
    fn failed_secret_sync_defers_environment_phase_without_mutating_dry_runs() {
        let path = Path::new("/project/secrets/vault");
        let mut calls = 0;
        let ready = run_secret_sync_phase(Some(path), false, |_| {
            calls += 1;
            Err("vault unavailable".into())
        });
        assert!(!ready);
        assert_eq!(calls, 1);

        let ready = run_secret_sync_phase(Some(path), true, |_| {
            calls += 1;
            Ok(())
        });
        assert!(ready);
        assert_eq!(calls, 1);
    }

    #[test]
    fn controller_discovers_only_project_environment_definitions() {
        let root = std::env::temp_dir().join(format!(
            "hops-controller-discovery-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(root.join("app/.gitops/local")).unwrap();
        fs::create_dir_all(root.join("app/.gitops/deploy")).unwrap();
        fs::write(
            root.join("app/.gitops/local/environment.yaml"),
            "kind: Environment\n",
        )
        .unwrap();
        fs::write(
            root.join("app/.gitops/deploy/environment.yaml"),
            "kind: Environment\n",
        )
        .unwrap();
        let found = discover_environment_definitions(&root).unwrap();
        assert_eq!(
            found,
            vec![root
                .join("app/.gitops/local/environment.yaml")
                .canonicalize()
                .unwrap()]
        );
        assert!(is_controller_owned_path(
            &root.join("app/.gitops/local/values.yaml"),
            &root
        ));
        assert!(!is_controller_owned_path(
            &root.join("app/.gitops/deploy/values.yaml"),
            &root
        ));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn browser_ingress_reconciles_only_for_dory_and_never_during_dry_run() {
        assert!(should_reconcile_browser_ingress(
            ClusterProvider::Kind,
            DockerProvider::Dory,
            false
        ));
        assert!(!should_reconcile_browser_ingress(
            ClusterProvider::Kind,
            DockerProvider::Dory,
            true
        ));
        assert!(!should_reconcile_browser_ingress(
            ClusterProvider::Kind,
            DockerProvider::Docker,
            false
        ));
        assert!(!should_reconcile_browser_ingress(
            ClusterProvider::Colima,
            DockerProvider::Colima,
            false
        ));
        assert!(!should_reconcile_browser_ingress(
            ClusterProvider::Dory,
            DockerProvider::Dory,
            false
        ));
    }
}
