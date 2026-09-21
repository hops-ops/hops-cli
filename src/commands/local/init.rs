//! `hops local init` — write committed Cluster / platform / Environment files.

use super::local_state_dir;
use super::workbench::definition::{
    DEFAULT_CROSSPLANE_CHART, DEFAULT_CROSSPLANE_VERSION, DEFAULT_DEFINITION_FILE,
    DEFAULT_ENVIRONMENT_FILE,
};
use super::workbench::machine::{self, DEFAULT_MACHINE_CLUSTER_NAME};
use clap::{Args, Subcommand};
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Args, Debug)]
pub struct InitArgs {
    #[command(subcommand)]
    pub command: InitCommands,
}

#[derive(Subcommand, Debug)]
pub enum InitCommands {
    /// Write `.gitops/local/cluster.yaml` and the `cluster/` tree
    Cluster(InitPathArgs),
    /// Write `.gitops/local/platform.yaml` and platform charts
    Platform(InitPathArgs),
    /// Write `.gitops/local/environment.yaml`
    Environment(InitPathArgs),
}

#[derive(Args, Debug)]
pub struct InitPathArgs {
    /// Directory to initialize (defaults to cwd).
    #[arg(long)]
    pub path: Option<PathBuf>,

    /// Host directory bind-mounted into the kind node (Cluster.spec.mountRoot).
    #[arg(long = "host-path")]
    pub host_path: Option<PathBuf>,

    /// Overwrite existing files.
    #[arg(long, default_value_t = false)]
    pub force: bool,
}

pub fn run(args: &InitArgs) -> Result<(), Box<dyn Error>> {
    match &args.command {
        InitCommands::Cluster(path) => init_cluster(path),
        InitCommands::Platform(path) => init_platform(path),
        InitCommands::Environment(path) => init_environment(path),
    }
}

fn target_root(args: &InitPathArgs) -> Result<PathBuf, Box<dyn Error>> {
    match &args.path {
        Some(path) => Ok(path.clone()),
        None => Ok(std::env::current_dir()?),
    }
}

fn init_cluster(args: &InitPathArgs) -> Result<(), Box<dyn Error>> {
    let root = target_root(args)?;
    let yaml = root.join(DEFAULT_DEFINITION_FILE);
    let manifests = root.join(".gitops/local/cluster");
    fail_if_exists(&yaml, args.force)?;
    fs::create_dir_all(&manifests)?;
    let overlay_readme = manifests.join("README.md");
    if args.force || !overlay_readme.exists() {
        fs::write(
            overlay_readme,
            "# Cluster overlay\n\n\
             Machine Cluster manifests come from hops-cli (`hops local up`).\n\
             Add extra YAML here; it overlays the CLI template by relative path.\n\
             Do not put shared app workloads here — use a cluster-scoped Environment.\n",
        )?;
    }
    let host_path = match &args.host_path {
        Some(path) => machine::expand_host_path(&path.display().to_string())?,
        None => machine::prompt_host_path(&machine::default_host_path())?,
    };
    let mount_root = mount_root_for_yaml(&host_path);
    let body = format!(
        r#"apiVersion: hops.local/v1alpha1
kind: Cluster
metadata:
  name: {name}
spec:
  clusterProvider: kind
  dockerProvider: dory
  mountRoot: {mount_root}
  manifests:
    path: .gitops/local/cluster
  controlPlane:
    crossplane:
      chart: {chart}
      version: "{version}"
"#,
        name = DEFAULT_MACHINE_CLUSTER_NAME,
        chart = DEFAULT_CROSSPLANE_CHART,
        version = DEFAULT_CROSSPLANE_VERSION,
    );
    fs::create_dir_all(yaml.parent().unwrap())?;
    fs::write(&yaml, body)?;
    persist_host_path(&host_path)?;
    println!("Wrote {}", yaml.display());
    println!("Wrote {}", manifests.display());
    println!("hostPath {}", host_path.display());
    Ok(())
}

fn mount_root_for_yaml(host_path: &Path) -> String {
    let home = std::env::var("HOME")
        .ok()
        .and_then(|home| PathBuf::from(home).canonicalize().ok());
    let host = host_path
        .canonicalize()
        .unwrap_or_else(|_| host_path.to_path_buf());
    if home.as_ref() == Some(&host) {
        "$HOME".to_string()
    } else {
        host.display().to_string()
    }
}

fn persist_host_path(host_path: &Path) -> Result<(), Box<dyn Error>> {
    let state_dir = local_state_dir()?;
    let Some(mut record) = machine::load(&state_dir)? else {
        return Ok(());
    };
    record.host_path = Some(host_path.to_path_buf());
    machine::save(&state_dir, &record)
}

fn init_environment(args: &InitPathArgs) -> Result<(), Box<dyn Error>> {
    let root = target_root(args)?;
    let yaml = root.join(DEFAULT_ENVIRONMENT_FILE);
    fail_if_exists(&yaml, args.force)?;
    fs::create_dir_all(yaml.parent().unwrap())?;
    let body = format!(
        r#"apiVersion: hops.local/v1alpha1
kind: Environment
metadata:
  name: local
spec:
  clusterRef:
    name: {name}
  root: .
  values:
    local: true
  deploys: []
"#,
        name = DEFAULT_MACHINE_CLUSTER_NAME,
    );
    fs::write(&yaml, body)?;
    println!("Wrote {}", yaml.display());
    Ok(())
}

fn init_platform(args: &InitPathArgs) -> Result<(), Box<dyn Error>> {
    let root = target_root(args)?;
    let yaml = root.join(".gitops/local/platform.yaml");
    fail_if_exists(&yaml, args.force)?;
    fs::create_dir_all(root.join(".gitops/local/platform/minio/templates"))?;
    fs::create_dir_all(root.join(".gitops/local/platform/mailpit/templates"))?;
    fs::write(
        &yaml,
        format!(
            r#"apiVersion: hops.local/v1alpha1
kind: Environment
metadata:
  name: hops-platform
spec:
  scope: cluster
  clusterRef:
    name: {name}
  root: .
  namespace: hops-platform
  deploys:
    - path: .gitops/local/platform/minio
      type: helm
    - path: .gitops/local/platform/mailpit
      type: helm
"#,
            name = DEFAULT_MACHINE_CLUSTER_NAME,
        ),
    )?;
    write_chart(&root.join(".gitops/local/platform/minio"), "minio", 9000)?;
    write_chart(
        &root.join(".gitops/local/platform/mailpit"),
        "mailpit",
        8025,
    )?;
    println!("Wrote {}", yaml.display());
    Ok(())
}

fn write_chart(dir: &Path, name: &str, port: u16) -> Result<(), Box<dyn Error>> {
    fs::write(
        dir.join("Chart.yaml"),
        format!("apiVersion: v2\nname: {name}\nversion: 0.1.0\n"),
    )?;
    fs::write(
        dir.join("templates/service.yaml"),
        format!(
            r#"apiVersion: v1
kind: Service
metadata:
  name: {name}
spec:
  selector:
    app.kubernetes.io/name: {name}
  ports:
    - name: http
      port: {port}
      targetPort: http
"#
        ),
    )?;
    Ok(())
}

fn fail_if_exists(path: &Path, force: bool) -> Result<(), Box<dyn Error>> {
    if path.exists() && !force {
        return Err(format!(
            "{} already exists; pass --force to overwrite",
            path.display()
        )
        .into());
    }
    Ok(())
}
