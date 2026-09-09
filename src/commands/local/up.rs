//! `hops local up` — create or reconnect the one machine Cluster.

use super::gitops::{self, ClusterArgs};
use super::local_state_dir;
use super::workbench::definition::{self, ClusterOverrides, DEFAULT_DEFINITION_FILE};
use super::workbench::machine::{
    self, kube_context_for_name, MachineClusterRecord, DEFAULT_MACHINE_CLUSTER_NAME,
};
use clap::Args;
use std::error::Error;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

#[derive(Args, Debug)]
pub struct UpArgs {
    /// Kubernetes-shaped Cluster definition. Defaults to
    /// `.gitops/local/cluster.yaml` or the persisted machine record.
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
    let source = resolve_source(
        args.path.as_deref(),
        record.as_ref(),
        &cwd_yaml,
        &state_dir,
        &machine_name,
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

fn resolve_source(
    explicit: Option<&Path>,
    record: Option<&MachineClusterRecord>,
    cwd_yaml: &Path,
    state_dir: &Path,
    machine_name: &str,
) -> Result<PathBuf, Box<dyn Error>> {
    if let Some(path) = explicit {
        return Ok(path.to_path_buf());
    }
    if let Some(record) = record {
        if record.source.exists() {
            return Ok(record.source.clone());
        }
    }
    if cwd_yaml.exists() {
        return Ok(cwd_yaml.to_path_buf());
    }
    write_generated_cluster(state_dir, machine_name)
}

fn write_generated_cluster(state_dir: &Path, machine_name: &str) -> Result<PathBuf, Box<dyn Error>> {
    let root = state_dir.join("generated");
    let yaml_path = root.join(DEFAULT_DEFINITION_FILE);
    let manifests = root.join(".gitops/local/cluster");
    fs::create_dir_all(&manifests)?;
    if !yaml_path.exists() {
        let body = format!(
            "apiVersion: hops.local/v1alpha1\n\
             kind: Cluster\n\
             metadata:\n\
               name: {machine_name}\n\
             spec:\n\
               clusterProvider: kind\n\
               dockerProvider: dory\n\
               mountRoot: {}\n\
               manifests:\n\
                 path: .gitops/local/cluster\n\
               controlPlane:\n\
                 crossplane:\n\
                   chart: {chart}\n\
                   version: \"{version}\"\n",
            root.display(),
            chart = definition::DEFAULT_CROSSPLANE_CHART,
            version = definition::DEFAULT_CROSSPLANE_VERSION,
        );
        fs::create_dir_all(yaml_path.parent().unwrap())?;
        fs::write(&yaml_path, body)?;
        eprintln!(
            "No Cluster definition found; wrote {} (not committed). Run `hops local init cluster` in a meta repo to share the profile.",
            yaml_path.display()
        );
    }
    Ok(yaml_path)
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
