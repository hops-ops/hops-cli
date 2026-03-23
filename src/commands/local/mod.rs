mod aws;
mod config;
mod destroy;
mod github;
mod install;
mod reset;
mod start;
mod stop;
mod unconfig;
mod uninstall;

use clap::{Args, Subcommand};
use std::error::Error;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const LOCAL_STATE_DIR: &str = ".hops/local";
const REPO_CACHE_DIR: &str = "repo-cache";

#[derive(Args, Debug)]
pub struct LocalArgs {
    #[command(subcommand)]
    pub command: LocalCommands,
}

#[derive(Subcommand, Debug)]
pub enum LocalCommands {
    /// Install Colima via Homebrew
    Install,
    /// Reset local Colima Kubernetes state
    Reset,
    /// Start local k8s cluster with Crossplane and providers
    Start,
    /// Configure crossplane-contrib provider-family-aws and AWS ProviderConfig
    Aws(aws::AwsArgs),
    /// Configure crossplane-contrib provider-upjet-github and GitHub ProviderConfig
    Github(github::GithubArgs),
    /// Stop the local cluster
    Stop,
    /// Destroy the local cluster VM
    Destroy,
    /// Uninstall Colima
    Uninstall,
    /// Build and load a Crossplane configuration into the local cluster
    Config(config::ConfigArgs),
    /// Remove a Crossplane configuration and prune orphaned package dependencies
    Unconfig(unconfig::UnconfigArgs),
}

pub fn run(args: &LocalArgs) -> Result<(), Box<dyn Error>> {
    match &args.command {
        LocalCommands::Install => install::run(),
        LocalCommands::Reset => reset::run(),
        LocalCommands::Start => start::run(),
        LocalCommands::Aws(aws_args) => aws::run(aws_args),
        LocalCommands::Github(github_args) => github::run(github_args),
        LocalCommands::Stop => stop::run(),
        LocalCommands::Destroy => destroy::run(),
        LocalCommands::Uninstall => uninstall::run(),
        LocalCommands::Config(config_args) => config::run(config_args),
        LocalCommands::Unconfig(unconfig_args) => unconfig::run(unconfig_args),
    }
}

/// Run an external command with inherited stdio. Fails on non-zero exit.
pub fn run_cmd(program: &str, args: &[&str]) -> Result<(), Box<dyn Error>> {
    log::debug!("Running: {} {}", program, args.join(" "));
    let status = Command::new(program)
        .args(args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()?;

    if !status.success() {
        return Err(format!("{} exited with {}", program, status).into());
    }
    Ok(())
}

/// Run an external command and capture stdout.
pub fn run_cmd_output(program: &str, args: &[&str]) -> Result<String, Box<dyn Error>> {
    log::debug!("Running: {} {}", program, args.join(" "));
    let output = Command::new(program).args(args).output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("{} exited with {}: {}", program, output.status, stderr).into());
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

pub fn repo_cache_path(org: &str, repo: &str) -> Result<PathBuf, Box<dyn Error>> {
    Ok(local_state_dir()?.join(REPO_CACHE_DIR).join(org).join(repo))
}

fn local_state_dir() -> Result<PathBuf, Box<dyn Error>> {
    let home = std::env::var("HOME")
        .map_err(|_| "HOME is not set; unable to determine local state directory")?;
    Ok(Path::new(&home).join(LOCAL_STATE_DIR))
}

fn command_exists(program: &str) -> bool {
    Command::new("sh")
        .args(["-c", &format!("command -v {} >/dev/null 2>&1", program)])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Ensure Colima's /etc/hosts maps a service hostname to the current ClusterIP.
pub fn sync_registry_hosts_entry(
    namespace: &str,
    service: &str,
    hostname: &str,
) -> Result<(), Box<dyn Error>> {
    let cluster_ip = run_cmd_output(
        "kubectl",
        &[
            "get",
            "svc",
            service,
            "-n",
            namespace,
            "-o",
            "jsonpath={.spec.clusterIP}",
        ],
    )?;
    let cluster_ip = cluster_ip.trim();
    if cluster_ip.is_empty() {
        return Err(format!("Service {}/{} has no ClusterIP", namespace, service).into());
    }

    let current_ip = run_cmd_output(
        "colima",
        &[
            "ssh",
            "--",
            "sh",
            "-c",
            &format!("awk '$2 == \"{}\" {{print $1; exit}}' /etc/hosts", hostname),
        ],
    )
    .unwrap_or_default();
    if current_ip.trim() == cluster_ip {
        return Ok(());
    }

    log::info!("Updating hosts entry: {} -> {}", hostname, cluster_ip);

    let escaped_host = hostname.replace('.', "\\.");
    run_cmd(
        "colima",
        &[
            "ssh",
            "--",
            "sudo",
            "sed",
            "-i",
            &format!("/{}/d", escaped_host),
            "/etc/hosts",
        ],
    )?;
    run_cmd(
        "colima",
        &[
            "ssh",
            "--",
            "sudo",
            "sh",
            "-c",
            &format!("echo '{} {}' >> /etc/hosts", cluster_ip, hostname),
        ],
    )?;

    Ok(())
}

/// Pipe a YAML string into `kubectl apply -f -`.
pub fn kubectl_apply_stdin(yaml: &str) -> Result<(), Box<dyn Error>> {
    let mut child = Command::new("kubectl")
        .args(["apply", "-f", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()?;

    if let Some(ref mut stdin) = child.stdin {
        stdin.write_all(yaml.as_bytes())?;
    }

    let status = child.wait()?;
    if !status.success() {
        return Err(format!("kubectl apply exited with {}", status).into());
    }
    Ok(())
}

/// Apply a JSON merge patch with `kubectl patch --type merge`.
pub fn kubectl_patch_merge(
    resource: &str,
    name: &str,
    namespace: &str,
    patch_json: &str,
) -> Result<(), Box<dyn Error>> {
    run_cmd(
        "kubectl",
        &[
            "patch", resource, name, "-n", namespace, "--type", "merge", "-p", patch_json,
        ],
    )
}
