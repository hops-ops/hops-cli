//! `hops local init` — write committed Cluster / platform / Environment files.

use super::workbench::definition::{
    DEFAULT_CROSSPLANE_CHART, DEFAULT_CROSSPLANE_VERSION, DEFAULT_DEFINITION_FILE,
    DEFAULT_ENVIRONMENT_FILE,
};
use super::workbench::machine::DEFAULT_MACHINE_CLUSTER_NAME;
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
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".to_string());
    let body = format!(
        "apiVersion: hops.local/v1alpha1\n\
         kind: Cluster\n\
         metadata:\n\
           name: {name}\n\
         spec:\n\
           clusterProvider: kind\n\
           dockerProvider: dory\n\
           mountRoot: {home}\n\
           manifests:\n\
             path: .gitops/local/cluster\n\
           controlPlane:\n\
             crossplane:\n\
               chart: {chart}\n\
               version: \"{version}\"\n",
        name = DEFAULT_MACHINE_CLUSTER_NAME,
        chart = DEFAULT_CROSSPLANE_CHART,
        version = DEFAULT_CROSSPLANE_VERSION,
    );
    fs::create_dir_all(yaml.parent().unwrap())?;
    fs::write(&yaml, body)?;
    println!("Wrote {}", yaml.display());
    println!("Wrote {}", manifests.display());
    Ok(())
}

fn init_environment(args: &InitPathArgs) -> Result<(), Box<dyn Error>> {
    let root = target_root(args)?;
    let yaml = root.join(DEFAULT_ENVIRONMENT_FILE);
    fail_if_exists(&yaml, args.force)?;
    fs::create_dir_all(yaml.parent().unwrap())?;
    let body = format!(
        "apiVersion: hops.local/v1alpha1\n\
         kind: Environment\n\
         metadata:\n\
           name: local\n\
         spec:\n\
           clusterRef:\n\
             name: {name}\n\
           root: .\n\
           values:\n\
             local: true\n\
           deploys: []\n",
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
            "apiVersion: hops.local/v1alpha1\n\
             kind: Environment\n\
             metadata:\n\
               name: hops-platform\n\
             spec:\n\
               scope: cluster\n\
               clusterRef:\n\
                 name: {name}\n\
               root: .\n\
               namespace: hops-platform\n\
               deploys:\n\
                 - path: .gitops/local/platform/minio\n\
                   type: helm\n\
                 - path: .gitops/local/platform/mailpit\n\
                   type: helm\n",
            name = DEFAULT_MACHINE_CLUSTER_NAME,
        ),
    )?;
    write_chart(
        &root.join(".gitops/local/platform/minio"),
        "minio",
        9000,
    )?;
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
            "apiVersion: v1\n\
             kind: Service\n\
             metadata:\n\
               name: {name}\n\
             spec:\n\
               selector:\n\
                 app.kubernetes.io/name: {name}\n\
               ports:\n\
                 - name: http\n\
                   port: {port}\n\
                   targetPort: http\n"
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
