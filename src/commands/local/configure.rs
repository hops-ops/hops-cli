//! `hops local configure` — show or change machine Cluster settings.

use super::gitops::{self, ClusterArgs};
use super::local_state_dir;
use super::workbench::cluster_template;
use super::workbench::definition::ClusterOverrides;
use super::workbench::machine::{self, kube_context_for_name, MachineClusterRecord};
use crate::commands::local::backend::kind;
use clap::Args;
use std::error::Error;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

#[derive(Args, Debug)]
pub struct ConfigureArgs {
    /// Set a cluster field (`hostPath=...`, `localDomain=...`). Repeatable.
    #[arg(long = "set", value_name = "KEY=VALUE")]
    pub set: Vec<String>,

    /// Apply restart/reset without prompting.
    #[arg(long, default_value_t = false)]
    pub yes: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ApplyKind {
    Write,
    Restart,
    Reset,
}

pub fn run(args: &ConfigureArgs, overrides: ClusterOverrides<'_>) -> Result<(), Box<dyn Error>> {
    let state_dir = local_state_dir()?;
    if args.set.is_empty() {
        return print_config(&state_dir);
    }
    let mut record = machine::load(&state_dir)?.ok_or(
        "No machine Cluster record. Run `hops local up` or `hops local init cluster` first.",
    )?;
    let mut apply = ApplyKind::Write;
    for spec in &args.set {
        let (key, value) = spec.split_once('=').ok_or_else(|| {
            format!("invalid --set {spec:?}; expected KEY=VALUE (e.g. hostPath=/Users/me/dev)")
        })?;
        apply = apply.max_kind(apply_set(&mut record, key.trim(), value.trim())?);
    }
    if apply != ApplyKind::Write && !args.yes && !confirm(apply, &record)? {
        println!("Aborted.");
        return Ok(());
    }
    let home = PathBuf::from(std::env::var("HOME")?);
    let source = cluster_template::materialize(
        &home,
        None,
        &record.name,
        record.host_path.as_deref(),
        record.local_domain.as_deref(),
    )?;
    record.source = source;
    record.kube_context = kube_context_for_name(&record.name);
    machine::save(&state_dir, &record)?;
    println!(
        "Wrote {}  hostPath={}",
        record.source.display(),
        record
            .host_path
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "$HOME".into())
    );
    match apply {
        ApplyKind::Write => Ok(()),
        ApplyKind::Restart | ApplyKind::Reset => {
            if apply == ApplyKind::Reset && kind::cluster_exists() {
                kind::destroy()?;
            }
            let cluster_args = ClusterArgs {
                path: Some(record.source.clone()),
                down: false,
                once: true,
                watch: false,
                debounce: 1,
                dry_run: false,
            };
            let overrides = ClusterOverrides {
                machine_name: Some(record.name.as_str()),
                ..overrides
            };
            gitops::run_cluster(&cluster_args, overrides)
        }
    }
}

impl ApplyKind {
    fn max_kind(self, other: Self) -> Self {
        match (self, other) {
            (ApplyKind::Reset, _) | (_, ApplyKind::Reset) => ApplyKind::Reset,
            (ApplyKind::Restart, _) | (_, ApplyKind::Restart) => ApplyKind::Restart,
            _ => ApplyKind::Write,
        }
    }
}

fn apply_set(
    record: &mut MachineClusterRecord,
    key: &str,
    value: &str,
) -> Result<ApplyKind, Box<dyn Error>> {
    match key {
        "hostPath" | "mountRoot" | "host-path" => {
            record.host_path = Some(machine::expand_host_path(value)?);
            Ok(ApplyKind::Reset)
        }
        "localDomain" | "local-domain" => {
            record.local_domain = Some(value.to_string());
            Ok(ApplyKind::Restart)
        }
        "name" => {
            if value.trim().is_empty() {
                return Err("name must not be empty".into());
            }
            record.name = value.to_string();
            record.kube_context = kube_context_for_name(value);
            Ok(ApplyKind::Reset)
        }
        other => Err(format!(
            "unknown cluster setting {other:?}; known: hostPath, localDomain, name"
        )
        .into()),
    }
}

fn confirm(apply: ApplyKind, record: &MachineClusterRecord) -> Result<bool, Box<dyn Error>> {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Err("this change recreates or restarts the cluster; re-run with --yes".into());
    }
    let prompt = match apply {
        ApplyKind::Reset => format!(
            "hostPath/name change recreates kind cluster '{}' (delete + up). Continue?",
            record.name
        ),
        ApplyKind::Restart => format!(
            "This restarts cluster '{}' to apply the new settings. Continue?",
            record.name
        ),
        ApplyKind::Write => return Ok(true),
    };
    Ok(dialoguer::Confirm::new()
        .with_prompt(prompt)
        .default(false)
        .interact()?)
}

fn print_config(state_dir: &Path) -> Result<(), Box<dyn Error>> {
    println!(
        "files:\n  record   {}\n  cluster  ~/.gitops/local/cluster.yaml",
        machine::record_path(state_dir).display()
    );
    let Some(record) = machine::load(state_dir)? else {
        println!("No machine Cluster yet. Run `hops local up`.");
        return Ok(());
    };
    let host = record
        .host_path
        .as_ref()
        .map(|path| path.display().to_string())
        .or_else(|| yaml_field(&record.source, "mountRoot"))
        .unwrap_or_else(|| "$HOME".into());
    let domain = record
        .local_domain
        .clone()
        .or_else(|| yaml_field(&record.source, "localDomain"))
        .unwrap_or_else(|| "localhost".into());
    let provider = yaml_field(&record.source, "clusterProvider").unwrap_or_else(|| "kind".into());
    let docker = yaml_field(&record.source, "dockerProvider").unwrap_or_else(|| "dory".into());
    println!("  name             {}", record.name);
    println!("  kubeContext      {}", record.kube_context);
    println!("  hostPath         {host}");
    println!("  clusterProvider  {provider}");
    println!("  dockerProvider   {docker}");
    println!("  localDomain      {domain}");
    if let Some(chart) = yaml_nested(&record.source, &["controlPlane", "crossplane", "chart"]) {
        let version = yaml_nested(&record.source, &["controlPlane", "crossplane", "version"])
            .unwrap_or_else(|| "-".into());
        println!("  crossplane       {chart}:{version}");
    }
    println!("reset required to change: hostPath, name, clusterProvider, dockerProvider");
    println!("restart required to change: localDomain");
    Ok(())
}

fn yaml_field(source: &Path, field: &str) -> Option<String> {
    yaml_nested(source, &[field])
}

fn yaml_nested(source: &Path, path: &[&str]) -> Option<String> {
    let raw = std::fs::read_to_string(source).ok()?;
    let mut value: serde_yaml::Value = serde_yaml::from_str(&raw).ok()?;
    value = value.get("spec")?.clone();
    for key in path {
        value = value.get(*key)?.clone();
    }
    value.as_str().map(ToOwned::to_owned)
}
