mod aws;
mod destroy;
mod github;
mod install;
mod reset;
mod start;
mod stop;
mod uninstall;

use clap::{Args, Subcommand};
use std::error::Error;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const LOCAL_STATE_DIR: &str = ".hops/local";
const REPO_CACHE_DIR: &str = "repo-cache";

/// Env var checked by kubectl helpers to inject `--context <name>`.
pub const HOPS_KUBE_CONTEXT_ENV: &str = "HOPS_KUBE_CONTEXT";

/// Build the kubectl args prefix. Returns `["--context", ctx]` when the env var
/// is set, or an empty vec otherwise.
fn kubectl_context_args() -> Vec<String> {
    match std::env::var(HOPS_KUBE_CONTEXT_ENV) {
        Ok(ctx) if !ctx.is_empty() => vec!["--context".to_string(), ctx],
        _ => vec![],
    }
}

/// Prepend `--context` to a kubectl arg slice when configured.
fn with_kube_context(args: &[&str]) -> Vec<String> {
    let mut out = kubectl_context_args();
    out.extend(args.iter().map(|s| s.to_string()));
    out
}

/// Build a `Command` for kubectl with `--context` injected when configured.
pub fn kubectl_command(args: &[&str]) -> Command {
    let full = with_kube_context(args);
    let mut cmd = Command::new("kubectl");
    cmd.args(&full);
    cmd
}

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
    }
}

/// Run an external command with inherited stdio. Fails on non-zero exit.
/// For kubectl commands, automatically injects `--context` when configured.
pub fn run_cmd(program: &str, args: &[&str]) -> Result<(), Box<dyn Error>> {
    if program == "kubectl" {
        let full = with_kube_context(args);
        let refs: Vec<&str> = full.iter().map(|s| s.as_str()).collect();
        return run_cmd_with_logged_args(program, &refs, &refs);
    }
    run_cmd_with_logged_args(program, args, args)
}

/// Run an external command and capture stdout.
/// For kubectl commands, automatically injects `--context` when configured.
pub fn run_cmd_output(program: &str, args: &[&str]) -> Result<String, Box<dyn Error>> {
    if program == "kubectl" {
        let full = with_kube_context(args);
        log::debug!("Running: {} {}", program, full.join(" "));
        let output = Command::new(program).args(&full).output()?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("{} exited with {}: {}", program, output.status, stderr).into());
        }
        return Ok(String::from_utf8_lossy(&output.stdout).to_string());
    }

    log::debug!("Running: {} {}", program, args.join(" "));
    let output = Command::new(program).args(args).output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("{} exited with {}: {}", program, output.status, stderr).into());
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn run_cmd_with_logged_args(
    program: &str,
    args: &[&str],
    logged_args: &[&str],
) -> Result<(), Box<dyn Error>> {
    log::debug!("Running: {} {}", program, logged_args.join(" "));
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
/// Automatically injects `--context` when configured.
pub fn kubectl_apply_stdin(yaml: &str) -> Result<(), Box<dyn Error>> {
    let full = with_kube_context(&["apply", "-f", "-"]);
    let mut child = Command::new("kubectl")
        .args(&full)
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
/// Automatically injects `--context` when configured.
pub fn kubectl_patch_merge(
    resource: &str,
    name: &str,
    namespace: &str,
    patch_json: &str,
) -> Result<(), Box<dyn Error>> {
    let base_args = [
        "patch", resource, name, "-n", namespace, "--type", "merge", "-p", patch_json,
    ];
    let base_logged = [
        "patch", resource, name, "-n", namespace, "--type", "merge", "-p", "<REDACTED>",
    ];
    let full_args = with_kube_context(&base_args);
    let full_logged = with_kube_context(&base_logged);
    let args_refs: Vec<&str> = full_args.iter().map(|s| s.as_str()).collect();
    let logged_refs: Vec<&str> = full_logged.iter().map(|s| s.as_str()).collect();
    run_cmd_with_logged_args("kubectl", &args_refs, &logged_refs)
}
