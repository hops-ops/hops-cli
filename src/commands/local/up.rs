//! `hops local up` — create or reconnect the one machine Cluster.

use super::gitops::{self, ClusterArgs};
use super::local_state_dir;
use super::workbench::cluster_template;
use super::workbench::definition::{self, ClusterOverrides, DEFAULT_DEFINITION_FILE};
use super::workbench::machine::{
    self, kube_context_for_name, MachineClusterRecord, DEFAULT_MACHINE_CLUSTER_NAME,
};
use clap::Args;
use std::error::Error;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

#[derive(Args, Debug)]
pub struct UpArgs {
    /// Optional overlay Cluster document. The machine profile is always
    /// `$HOME/.gitops/local/cluster.yaml` (CLI template + project extras).
    #[arg(value_name = "PATH")]
    pub path: Option<PathBuf>,

    /// Run a single reconcile and exit (disables the default watch).
    #[arg(long, default_value_t = false)]
    pub once: bool,

    /// Server/client dry-run; do not persist to the cluster.
    #[arg(long, default_value_t = false)]
    pub dry_run: bool,
}

pub fn run(args: &UpArgs, overrides: ClusterOverrides<'_>) -> Result<(), Box<dyn Error>> {
    let state_dir = local_state_dir()?;
    let cwd = std::env::current_dir()?;
    let escape = overrides
        .cluster_name
        .map(str::trim)
        .filter(|name| !name.is_empty());
    if escape.is_some() {
        warn_escape_hatch()?;
    }

    let record = machine::load(&state_dir)?;
    let machine_name = escape
        .map(str::to_string)
        .or_else(|| record.as_ref().map(|record| record.name.clone()))
        .unwrap_or_else(|| DEFAULT_MACHINE_CLUSTER_NAME.to_string());

    let cwd_yaml = cwd.join(DEFAULT_DEFINITION_FILE);
    let overlay = args
        .path
        .clone()
        .or_else(|| cwd_yaml.exists().then_some(cwd_yaml.clone()));
    let home =
        PathBuf::from(std::env::var("HOME").map_err(|_| "HOME is required for hops local up")?);
    let host_path = resolve_up_host_path(record.as_ref(), &home)?;
    let local_domain = record
        .as_ref()
        .and_then(|record| record.local_domain.clone());
    let source = cluster_template::materialize(
        &home,
        overlay.as_deref(),
        &machine_name,
        Some(&host_path),
        local_domain.as_deref(),
    )?;

    if cwd_yaml.exists() && args.path.is_none() {
        if let Ok(leaf) = definition::load_cluster_document_name(&cwd_yaml) {
            if leaf != machine_name {
                warn_leaf_name(&leaf, &machine_name, &cwd_yaml)?;
            }
        }
    }

    warn_multiple_kind_clusters(&machine_name)?;

    let record = MachineClusterRecord {
        name: machine_name.clone(),
        kube_context: kube_context_for_name(&machine_name),
        source: source.clone(),
        host_path: Some(host_path),
        local_domain,
    };
    if !args.dry_run {
        machine::save(&state_dir, &record)?;
    }

    let cluster_args = ClusterArgs {
        path: Some(source),
        down: false,
        once: args.once,
        watch: false,
        debounce: 1,
        dry_run: args.dry_run,
    };
    let overrides = ClusterOverrides {
        machine_name: Some(record.name.as_str()),
        ..overrides
    };
    gitops::run_cluster(&cluster_args, overrides)
}

fn resolve_up_host_path(
    record: Option<&MachineClusterRecord>,
    home: &Path,
) -> Result<PathBuf, Box<dyn Error>> {
    if let Some(path) = record.and_then(|record| record.host_path.clone()) {
        return Ok(path);
    }
    if let Some(record) = record {
        if let Some(path) = host_path_from_cluster_yaml(&record.source) {
            return Ok(path);
        }
        return Ok(home.canonicalize().unwrap_or_else(|_| home.to_path_buf()));
    }
    machine::prompt_host_path(&machine::default_host_path())
}

fn host_path_from_cluster_yaml(source: &Path) -> Option<PathBuf> {
    let raw = std::fs::read_to_string(source).ok()?;
    let value: serde_yaml::Value = serde_yaml::from_str(&raw).ok()?;
    let mount = value.get("spec")?.get("mountRoot")?.as_str()?;
    machine::expand_host_path(mount).ok()
}

fn warn_escape_hatch() -> io::Result<()> {
    writeln!(
        io::stderr(),
        "warning: --cluster-name is an escape hatch; the happy path is one machine cluster (`hops local up`)."
    )
}

fn warn_leaf_name(leaf: &str, machine: &str, path: &Path) -> io::Result<()> {
    writeln!(
        io::stderr(),
        "warning: {} names Cluster {leaf:?} but the machine cluster is {machine:?}; reconnecting does not create a second kind cluster.",
        path.display()
    )
}

fn warn_multiple_kind_clusters(machine: &str) -> Result<(), Box<dyn Error>> {
    let names = super::backend::kind::list_cluster_names();
    if names.len() > 1 {
        writeln!(
            io::stderr(),
            "warning: multiple hops-managed kind clusters are present ({}); happy path is one machine cluster {machine:?}. --cluster-name is an escape hatch.",
            names.join(", ")
        )?;
    }
    Ok(())
}
