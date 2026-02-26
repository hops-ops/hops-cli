mod aws;
mod config;
mod destroy;
mod install;
mod kubefwd;
mod reset;
mod start;
mod stop;
mod unconfig;
mod uninstall;

use clap::{Args, Subcommand};
use std::error::Error;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

const KUBEFWD_STATE_DIR: &str = ".hops/local";
const KUBEFWD_PID_FILE: &str = "kubefwd.pid";
const KUBEFWD_LOG_FILE: &str = "kubefwd.log";
const KUBEFWD_RESYNC_INTERVAL: &str = "30s";

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
    /// Manage background kubefwd forwarding
    Kubefwd(kubefwd::KubefwdArgs),
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
        LocalCommands::Kubefwd(kubefwd_args) => kubefwd::run(kubefwd_args),
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

/// Start kubefwd in the background to forward all services in the current cluster.
pub fn start_kubefwd() -> Result<(), Box<dyn Error>> {
    if !command_exists("kubefwd") {
        return Err(
            "kubefwd is not installed or not in PATH (install it, e.g. `brew install kubefwd`)"
                .into(),
        );
    }

    let state_dir = kubefwd_state_dir()?;
    fs::create_dir_all(&state_dir)?;
    let pid_path = state_dir.join(KUBEFWD_PID_FILE);
    let log_path = state_dir.join(KUBEFWD_LOG_FILE);

    if let Some(pid) = read_pid_file(&pid_path) {
        if process_is_running(pid) {
            if process_is_kubefwd(pid) {
                log::info!(
                    "kubefwd is already running (pid {}), skipping start (log: {})",
                    pid,
                    log_path.display()
                );
                return Ok(());
            }

            log::warn!(
                "Ignoring stale kubefwd PID file: pid {} is running but is not kubefwd",
                pid
            );
        }
        let _ = fs::remove_file(&pid_path);
    }

    // kubefwd typically needs sudo for hosts-file updates and privileged ports.
    run_cmd("sudo", &["-v"])?;

    let log_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;
    let log_file_err = log_file.try_clone()?;

    let mut child = Command::new("sudo")
        .args([
            "-n",
            "kubefwd",
            "services",
            "-A",
            "--resync-interval",
            KUBEFWD_RESYNC_INTERVAL,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::from(log_file))
        .stderr(Stdio::from(log_file_err))
        .spawn()?;

    let pid = child.id();
    fs::write(&pid_path, format!("{pid}\n"))?;

    // Fail fast if the process exits immediately (e.g. bad kube context).
    thread::sleep(Duration::from_millis(500));
    if let Some(status) = child.try_wait()? {
        let _ = fs::remove_file(&pid_path);
        return Err(format!(
            "kubefwd exited immediately with {} (check {})",
            status,
            log_path.display()
        )
        .into());
    }

    log::info!(
        "kubefwd started in background (pid {}, log: {})",
        pid,
        log_path.display()
    );
    Ok(())
}

/// Stop kubefwd previously started by this CLI.
pub fn stop_kubefwd() -> Result<(), Box<dyn Error>> {
    let pid_path = kubefwd_state_dir()?.join(KUBEFWD_PID_FILE);
    if !pid_path.exists() {
        return Ok(());
    }

    let pid = match read_pid_file(&pid_path) {
        Some(pid) => pid,
        None => {
            let _ = fs::remove_file(&pid_path);
            return Ok(());
        }
    };

    if !process_is_running(pid) {
        let _ = fs::remove_file(&pid_path);
        return Ok(());
    }
    if !process_is_kubefwd(pid) {
        log::warn!(
            "Refusing to stop pid {} from kubefwd PID file because it is not a kubefwd process",
            pid
        );
        let _ = fs::remove_file(&pid_path);
        return Ok(());
    }

    let pid_str = pid.to_string();
    log::info!("Stopping kubefwd (pid {})...", pid);

    let stopped = Command::new("kill")
        .arg(&pid_str)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
        || Command::new("sudo")
            .args(["kill", &pid_str])
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);

    if !stopped {
        return Err(format!("failed to stop kubefwd process {}", pid).into());
    }

    for _ in 0..20 {
        if !process_is_running(pid) {
            let _ = fs::remove_file(&pid_path);
            log::info!("kubefwd stopped");
            return Ok(());
        }
        thread::sleep(Duration::from_millis(200));
    }

    let forced = Command::new("sudo")
        .args(["kill", "-9", &pid_str])
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    if !forced || process_is_running(pid) {
        return Err(format!("kubefwd process {} did not terminate", pid).into());
    }

    let _ = fs::remove_file(&pid_path);
    log::info!("kubefwd stopped");
    Ok(())
}

fn kubefwd_state_dir() -> Result<PathBuf, Box<dyn Error>> {
    let home = std::env::var("HOME")
        .map_err(|_| "HOME is not set; unable to determine kubefwd state directory")?;
    Ok(Path::new(&home).join(KUBEFWD_STATE_DIR))
}

fn command_exists(program: &str) -> bool {
    Command::new("sh")
        .args(["-c", &format!("command -v {} >/dev/null 2>&1", program)])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn read_pid_file(path: &Path) -> Option<u32> {
    let contents = fs::read_to_string(path).ok()?;
    contents.trim().parse::<u32>().ok()
}

fn process_is_running(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn process_is_kubefwd(pid: u32) -> bool {
    let output = Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "command="])
        .output();
    let Ok(output) = output else {
        return false;
    };
    if !output.status.success() {
        return false;
    }

    let cmd = String::from_utf8_lossy(&output.stdout);
    cmd.contains("kubefwd")
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
