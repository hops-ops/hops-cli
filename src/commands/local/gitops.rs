//! `hops local gitops` — control-plane and worktree Application reconcile.
//!
//! ```text
//! hops local gitops cluster  [PATH]   # shared CP (meta .gitops/cluster)
//! hops local gitops worktree <PATH>   # per-worktree apps (envs → namespace = --name)
//! ```
//!
//! Both **watch by default**; pass `--once` for a single reconcile (CI/scripts).

use super::local_state_dir;
use super::workbench::application::{
    load_applications, resolve_delivery_host_path, Application, APPLICATION_API_VERSION,
    APPLICATION_KIND,
};
use super::workbench::cluster_gitops::{
    reconcile_cluster_dir, resolve_cluster_path, should_reconcile_cluster_change,
};
use super::workbench::definition::{load_definition, load_environment_definition};
use super::workbench::delivery::{
    attach_sync_delivery, discover_sync_targets, save_delivery_runtime, stop_delivery_runtime,
    DeliveryStrategy, NodePathProber, SystemNodeProber,
};
use super::workbench::reconcile::{
    reconcile_applications, HelmRunner, ReconcileOptions, SystemHelm, SystemKubectl,
};
use super::workbench::registry::{
    activate_workspace_cluster, load_workspace, save_workspace, WorkspaceRecord,
};
use super::workbench::watch::{
    is_chart_or_env_path, should_ignore_watch_path, watch_roots_for_applications, WatchPathClass,
};
use super::workbench::{namespace_for_name, slugify_name};
use clap::{Args, Subcommand};
use notify::{RecursiveMode, Watcher};
use serde::Deserialize;
use serde_yaml::{Mapping, Value};
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
    /// Per-worktree app Applications (charts → namespace = --name)
    Worktree(WorktreeArgs),
}

#[derive(Args, Debug)]
pub struct ClusterArgs {
    /// Path to cluster gitops directory (PSQLStack, AuthStack, packages, …).
    /// Default: `$HOPS_LOCAL_CLUSTER`, else walk up from cwd for `.gitops/cluster`.
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
    /// Reusable Environment YAML, or a legacy directory of Application YAMLs.
    #[arg(value_name = "PATH")]
    pub path: PathBuf,

    /// Destination namespace override (workspace isolation).
    #[arg(long, short = 'n')]
    pub namespace: Option<String>,

    /// Runtime Environment name (defaults from Environment metadata / namespace / path).
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
         Pass a path: hops local gitops cluster ./.gitops/cluster\n\
         Or set HOPS_LOCAL_CLUSTER, or create .gitops/cluster at the meta repo root."
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
    if args.path.is_file() && yaml_kind(&args.path)?.as_deref() == Some("Environment") {
        return run_environment_worktree(args);
    }
    run_application_worktree(args)
}

fn run_environment_worktree(args: &WorktreeArgs) -> Result<(), Box<dyn Error>> {
    let source = args
        .path
        .canonicalize()
        .map_err(|error| format!("Environment path {}: {error}", args.path.display()))?;
    let cluster_path = discover_cluster_definition(&source).ok_or_else(|| {
        format!(
            "no cluster.yaml found above Environment definition {}",
            source.display()
        )
    })?;
    let cluster = load_definition(&cluster_path)?;
    let loaded = load_environment_definition(
        &source,
        &cluster,
        args.name.as_deref(),
        args.namespace.as_deref(),
    )?;
    let workspace_name = loaded.environment.name.clone();
    let namespace = loaded.environment.namespace.clone();
    let worktree_root = loaded.environment.root.clone();
    let mut chart_watch_roots = BTreeSet::new();
    for deploy in &loaded.environment.deploys {
        chart_watch_roots.insert(deploy.promote_chart.clone());
        chart_watch_roots.insert(deploy.application_root.join(".gitops/deploy"));
    }
    let chart_watch_roots: Vec<_> = chart_watch_roots.into_iter().collect();
    super::backend::kind::set_active_cluster_name(&cluster.cluster.name);

    let generated = if args.dry_run {
        std::env::temp_dir().join(format!(
            "hops-local-environment-{}-{workspace_name}",
            std::process::id()
        ))
    } else {
        local_state_dir()?.join("generated").join(&workspace_name)
    };

    let reconcile = || -> Result<(), Box<dyn Error>> {
        render_environment_applications(
            &source,
            &cluster_path,
            &generated,
            &workspace_name,
            &namespace,
        )?;
        let legacy = WorktreeArgs {
            path: generated.clone(),
            namespace: Some(namespace.clone()),
            name: Some(workspace_name.clone()),
            once: true,
            watch: false,
            debounce: args.debounce,
            dry_run: args.dry_run,
        };
        run_application_worktree(&legacy)?;
        if !args.dry_run {
            persist_environment_registration(
                &workspace_name,
                &source,
                &worktree_root,
                &cluster.cluster.name,
            )?;
        }
        Ok(())
    };

    reconcile()?;
    if args.once || args.dry_run {
        if args.dry_run {
            let _ = fs::remove_dir_all(&generated);
        }
        return Ok(());
    }

    run_environment_watch(
        &source,
        &worktree_root,
        &chart_watch_roots,
        args.debounce,
        reconcile,
    )
}

fn yaml_kind(path: &Path) -> Result<Option<String>, Box<dyn Error>> {
    let text = fs::read_to_string(path)?;
    let Some(document) = serde_yaml::Deserializer::from_str(&text).next() else {
        return Ok(None);
    };
    let value = Value::deserialize(document)?;
    Ok(value
        .get("kind")
        .and_then(Value::as_str)
        .map(str::to_string))
}

fn discover_cluster_definition(environment_file: &Path) -> Option<PathBuf> {
    environment_file
        .parent()?
        .ancestors()
        .map(|directory| directory.join("cluster.yaml"))
        .find(|candidate| candidate.is_file())
}

fn render_environment_applications(
    environment_file: &Path,
    cluster_file: &Path,
    generated: &Path,
    workspace_name: &str,
    namespace: &str,
) -> Result<(), Box<dyn Error>> {
    render_environment_applications_with(
        environment_file,
        cluster_file,
        generated,
        workspace_name,
        namespace,
        &SystemHelm,
    )
}

fn render_environment_applications_with<H: HelmRunner>(
    environment_file: &Path,
    cluster_file: &Path,
    generated: &Path,
    workspace_name: &str,
    namespace: &str,
    helm: &H,
) -> Result<(), Box<dyn Error>> {
    let cluster = load_definition(cluster_file)?;
    let loaded = load_environment_definition(
        environment_file,
        &cluster,
        Some(workspace_name),
        Some(namespace),
    )?;
    fs::create_dir_all(generated)?;
    for entry in fs::read_dir(generated)? {
        let path = entry?.path();
        if path.is_file()
            && matches!(
                path.extension().and_then(|value| value.to_str()),
                Some("yaml" | "yml")
            )
        {
            fs::remove_file(path)?;
        }
    }

    let mut rendered_apps = BTreeMap::<String, Application>::new();
    for (index, deploy) in loaded.environment.deploys.iter().enumerate() {
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
        values.insert(
            Value::String("source".into()),
            string_mapping(&[("localPath", &deploy.application_root.to_string_lossy())]),
        );
        let values_yaml = serde_yaml::to_string(&Value::Mapping(values))?;
        let output = helm.template(
            &format!("{}-promote-{index}", sanitize_name(workspace_name)),
            &deploy.promote_chart,
            &loaded.environment.namespace,
            &values_yaml,
        )?;
        for document in serde_yaml::Deserializer::from_str(&output) {
            let value = Value::deserialize(document)?;
            if value.is_null() {
                continue;
            }
            let kind = value.get("kind").and_then(Value::as_str).unwrap_or("");
            if kind != APPLICATION_KIND {
                return Err(format!(
                    "promotion chart {} emitted unsupported local kind {kind:?}; direct KRM reconciliation belongs to the Cluster controller task",
                    deploy.promote_chart.display()
                )
                .into());
            }
            let mut application: Application = serde_yaml::from_value(value)?;
            if application.api_version != APPLICATION_API_VERSION {
                return Err(format!(
                    "promotion chart {} emitted Application apiVersion {:?}; expected {APPLICATION_API_VERSION}",
                    deploy.promote_chart.display(),
                    application.api_version
                )
                .into());
            }
            application.spec.source.delivery_path =
                Some(loaded.environment.root.to_string_lossy().into_owned());
            application.spec.destination.namespace = Some(loaded.environment.namespace.clone());
            let name = application.metadata.name.clone();
            if rendered_apps.insert(name.clone(), application).is_some() {
                return Err(format!("duplicate promoted Application name {name:?}").into());
            }
        }
    }
    if rendered_apps.is_empty() {
        return Err(format!(
            "Environment {} produced no local Applications",
            loaded.environment.name
        )
        .into());
    }
    for (name, application) in rendered_apps {
        let path = generated.join(format!("{}.yaml", sanitize_name(&name)));
        fs::write(path, serde_yaml::to_string(&application)?)?;
    }
    Ok(())
}

fn merge_mapping(base: &mut Mapping, overlay: &Mapping) {
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

fn string_mapping(values: &[(&str, &str)]) -> Value {
    let mut mapping = Mapping::new();
    for (key, value) in values {
        mapping.insert(
            Value::String((*key).to_string()),
            Value::String((*value).to_string()),
        );
    }
    Value::Mapping(mapping)
}

fn sanitize_name(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '-' {
                character
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}

fn persist_environment_registration(
    workspace_name: &str,
    source: &Path,
    worktree_root: &Path,
    cluster_name: &str,
) -> Result<(), Box<dyn Error>> {
    let state_dir = local_state_dir()?;
    let Some(mut record) = load_workspace(&state_dir, workspace_name)? else {
        return Err(format!("workspace {workspace_name:?} was not registered").into());
    };
    record.env_path = source.to_string_lossy().into_owned();
    record.project_root = Some(worktree_root.to_string_lossy().into_owned());
    record.cluster_name = Some(cluster_name.to_string());
    save_workspace(&state_dir, &record)?;
    Ok(())
}

fn run_environment_watch<F>(
    environment_file: &Path,
    worktree_root: &Path,
    chart_roots: &[PathBuf],
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
        "Watching Environment {} and {} referenced promotion/deploy chart roots under {} (debounce {}s). Ctrl+C to stop.",
        environment_file.display(),
        chart_roots.len(),
        worktree_root.display(),
        debounce_secs
    );
    loop {
        rx.recv()
            .map_err(|_| "Environment watcher channel closed")?;
        wait_for_quiet(&rx, debounce)?;
        match rebuild() {
            Ok(()) => log::info!("Environment reconcile succeeded."),
            Err(error) => log::error!("Environment reconcile failed: {error}"),
        }
    }
}

fn is_environment_watch_path(path: &Path, source: &Path, chart_roots: &[PathBuf]) -> bool {
    path == source || chart_roots.iter().any(|root| path.starts_with(root))
}

fn run_application_worktree(args: &WorktreeArgs) -> Result<(), Box<dyn Error>> {
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
    let existing_workspace = local_state_dir()
        .ok()
        .and_then(|state_dir| load_workspace(&state_dir, &workspace_name).ok().flatten());
    if let Some(rec) = existing_workspace.as_ref() {
        if let Some((cluster, ctx)) = activate_workspace_cluster(rec) {
            log::info!("worktree gitops: bound cluster `{cluster}` (context {ctx})");
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
    if !args.dry_run {
        register_worktree(
            &env_path,
            &workspace_name,
            &namespace,
            delivery_strategy,
            existing_workspace.as_ref(),
        )?;
    }
    if args.once || args.dry_run {
        return Ok(());
    }

    run_worktree_watch(&env_path, args.debounce, do_once)
}

fn register_worktree(
    env_path: &Path,
    workspace_name: &str,
    namespace: &str,
    delivery_strategy: DeliveryStrategy,
    existing: Option<&WorkspaceRecord>,
) -> Result<(), Box<dyn Error>> {
    let cluster_name = existing
        .and_then(|record| record.cluster_name.clone())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(super::backend::kind::active_cluster_name);
    let kube_context = existing
        .and_then(|record| record.kube_context.clone())
        .filter(|context| !context.is_empty())
        .or_else(super::kube_context_from_env)
        .or_else(|| Some(format!("kind-{cluster_name}")));
    let project_root = discover_project_root(env_path)
        .map(|path| path.to_string_lossy().into_owned())
        .or_else(|| existing.and_then(|record| record.project_root.clone()));

    let record = WorkspaceRecord {
        name: workspace_name.to_string(),
        namespace: namespace.to_string(),
        env_path: env_path.to_string_lossy().into_owned(),
        project_root,
        delivery_mode: Some(delivery_strategy.as_str().to_string()),
        updated_at: None,
        cluster_name: Some(cluster_name),
        kube_context,
    };
    let path = save_workspace(&local_state_dir()?, &record)?;
    log::info!(
        "worktree gitops: registered workspace `{workspace_name}` at {}",
        path.display()
    );
    Ok(())
}

fn discover_project_root(env_path: &Path) -> Option<PathBuf> {
    env_path
        .ancestors()
        .find(|candidate| candidate.join(".git").exists())
        .map(Path::to_path_buf)
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::Mutex;

    struct PromotionHelm {
        values: Mutex<Vec<Value>>,
    }

    impl PromotionHelm {
        fn new() -> Self {
            Self {
                values: Mutex::new(Vec::new()),
            }
        }
    }

    impl HelmRunner for PromotionHelm {
        fn template(
            &self,
            _release: &str,
            chart_path: &Path,
            namespace: &str,
            values_yaml: &str,
        ) -> Result<String, Box<dyn Error>> {
            self.values
                .lock()
                .unwrap()
                .push(serde_yaml::from_str(values_yaml)?);
            let application_root = chart_path
                .parent()
                .and_then(Path::parent)
                .ok_or("promotion chart has no application root")?;
            Ok(format!(
                r#"apiVersion: hops.local/v1alpha1
kind: Application
metadata:
  name: gateway
spec:
  source:
    path: {}/.gitops/deploy
  destination:
    namespace: ignored
"#,
                application_root.display()
            )
            .replace("namespace: ignored", &format!("namespace: {namespace}")))
        }
    }

    #[test]
    fn discovers_project_root_from_git_ancestor() {
        let root = std::env::temp_dir().join(format!(
            "hops-gitops-root-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let env_path = root.join("gitops/envs/local");
        fs::create_dir_all(&env_path).unwrap();
        fs::write(root.join(".git"), "gitdir: /tmp/example\n").unwrap();

        assert_eq!(discover_project_root(&env_path), Some(root.clone()));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn project_root_is_unknown_without_git_ancestor() {
        let root = std::env::temp_dir().join(format!(
            "hops-gitops-no-root-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let env_path = root.join("gitops/envs/local");
        fs::create_dir_all(&env_path).unwrap();

        assert_eq!(discover_project_root(&env_path), None);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn renders_reusable_environment_for_runtime_identity() {
        let root = std::env::temp_dir().join(format!(
            "hops-gitops-environment-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let promote = root.join("apps/gateway/.gitops/promote");
        fs::create_dir_all(root.join(".gitops/cluster")).unwrap();
        fs::create_dir_all(root.join("apps/gateway/.gitops/deploy")).unwrap();
        fs::create_dir_all(&promote).unwrap();
        fs::write(
            promote.join("Chart.yaml"),
            "apiVersion: v2\nname: gateway-promote\nversion: 0.1.0\n",
        )
        .unwrap();
        fs::write(
            root.join("cluster.yaml"),
            r#"apiVersion: hops.local/v1alpha1
kind: Cluster
metadata:
  name: project-dev
spec:
  clusterProvider: kind
  dockerProvider: dory
  mountRoot: .
  manifests:
    path: .gitops/cluster
"#,
        )
        .unwrap();
        fs::write(
            root.join("environment.yaml"),
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
    - path: apps/gateway
      values:
        feature:
          enabled: true
        revision: worktree
"#,
        )
        .unwrap();

        let generated = root.join("generated");
        let helm = PromotionHelm::new();
        render_environment_applications_with(
            &root.join("environment.yaml"),
            &root.join("cluster.yaml"),
            &generated,
            "feature-auth",
            "feature-auth-ns",
            &helm,
        )
        .unwrap();

        let values = helm.values.lock().unwrap();
        let values = values[0].as_mapping().unwrap();
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

        let applications = load_applications(&generated).unwrap();
        assert_eq!(applications.len(), 1);
        let application = &applications[0].1;
        assert_eq!(application.metadata.name, "gateway");
        assert_eq!(
            application.spec.destination.namespace.as_deref(),
            Some("feature-auth-ns")
        );
        let expected_delivery_path = root.to_string_lossy().into_owned();
        assert_eq!(
            application.spec.source.delivery_path.as_deref(),
            Some(expected_delivery_path.as_str())
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn environment_watch_filters_to_definition_and_referenced_charts() {
        let source = Path::new("/project/environment.yaml");
        let chart_roots = vec![
            PathBuf::from("/project/apps/api/.gitops/promote"),
            PathBuf::from("/project/apps/api/.gitops/deploy"),
        ];
        assert!(is_environment_watch_path(source, source, &chart_roots));
        assert!(is_environment_watch_path(
            Path::new("/project/apps/api/.gitops/promote/templates/application.yaml"),
            source,
            &chart_roots,
        ));
        assert!(is_environment_watch_path(
            Path::new("/project/apps/api/.gitops/deploy/values.yaml"),
            source,
            &chart_roots,
        ));
        assert!(!is_environment_watch_path(
            Path::new("/project/apps/api/src/main.rs"),
            source,
            &chart_roots,
        ));
        assert!(!is_environment_watch_path(
            Path::new("/project/apps/other/.gitops/deploy/values.yaml"),
            source,
            &chart_roots,
        ));
    }
}
