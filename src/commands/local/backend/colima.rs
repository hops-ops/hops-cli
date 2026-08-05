//! Colima backend: VM + dockerd + k3s (docker runtime).

use super::SizeArgs;
use crate::commands::local::package_install::{REGISTRY_HOSTNAME, REGISTRY_PULL};
use crate::commands::local::{run_cmd, run_cmd_output, wait_for_kubernetes};
use dialoguer::Confirm;
use serde::Deserialize;
use std::error::Error;
use std::io::{IsTerminal, Write};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

const DEFAULT_CPUS: u32 = 8;
const DEFAULT_MEMORY_GIB: u32 = 16;
const DEFAULT_DISK_GIB: u32 = 60;
const GIB: u64 = 1024 * 1024 * 1024;

pub fn install() -> Result<(), Box<dyn Error>> {
    log::info!("Installing Colima via Homebrew...");
    run_cmd("brew", &["install", "colima"])?;
    log::info!("Colima installed successfully");
    Ok(())
}

pub fn uninstall() -> Result<(), Box<dyn Error>> {
    log::info!("Uninstalling Colima...");
    run_cmd("brew", &["uninstall", "colima"])?;
    log::info!("Colima uninstalled");
    Ok(())
}

pub fn stop() -> Result<(), Box<dyn Error>> {
    log::info!("Stopping Colima...");
    run_cmd("colima", &["stop"])?;
    log::info!("Colima stopped");
    Ok(())
}

pub fn destroy() -> Result<(), Box<dyn Error>> {
    log::info!("Destroying Colima VM...");
    run_cmd("colima", &["delete", "--force"])?;
    log::info!("Colima VM destroyed");
    Ok(())
}

pub fn reset() -> Result<(), Box<dyn Error>> {
    log::info!("Resetting Colima Kubernetes...");
    run_cmd("colima", &["kubernetes", "reset"])?;
    log::info!("Colima Kubernetes reset complete");
    Ok(())
}

pub fn start(size: &SizeArgs, assume_yes: bool) -> Result<(), Box<dyn Error>> {
    let instance = colima_instance()?;
    validate_requested_size(size, instance.as_ref())?;
    start_or_resize_colima(size, assume_yes, instance.as_ref())
}

pub fn resize(size: &SizeArgs) -> Result<(), Box<dyn Error>> {
    if !size.any_set() {
        return Err("Specify at least one of --cpus, --memory, or --disk".into());
    }

    let instance = colima_instance()?;
    let instance = instance
        .as_ref()
        .ok_or("No Colima instance exists yet; use `hops local start` to create one")?;

    validate_requested_size(size, Some(instance))?;
    resize_existing_colima(size, Some(instance))
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
struct ColimaInstance {
    #[serde(default)]
    status: String,
    #[serde(default)]
    cpus: Option<u32>,
    #[serde(default)]
    memory: Option<u64>,
    #[serde(default)]
    disk: Option<u64>,
}

impl ColimaInstance {
    fn is_running(&self) -> bool {
        self.status.eq_ignore_ascii_case("running")
    }

    fn memory_gib(&self) -> Option<u32> {
        self.memory.map(bytes_to_gib)
    }

    fn disk_gib(&self) -> Option<u32> {
        self.disk.map(bytes_to_gib)
    }
}

fn start_or_resize_colima(
    size: &SizeArgs,
    assume_yes: bool,
    instance: Option<&ColimaInstance>,
) -> Result<(), Box<dyn Error>> {
    let is_running = instance.map(ColimaInstance::is_running).unwrap_or(false);

    if is_running && size.any_set() {
        let changes = requested_size_changes(size, instance.expect("checked is_running"));
        if !changes.is_empty() {
            confirm_running_resize(size, assume_yes, &changes)?;
            resize_existing_colima(size, instance)?;
            return Ok(());
        }

        log::info!("Requested Colima size already matches the running VM");
    }

    log::info!("Starting Colima with Kubernetes...");

    let start_size = if is_running {
        SizeArgs::default()
    } else {
        size.clone()
    };
    let include_defaults = instance.is_none();
    start_colima(&start_size, include_defaults)
}

fn resize_existing_colima(
    size: &SizeArgs,
    instance: Option<&ColimaInstance>,
) -> Result<(), Box<dyn Error>> {
    if instance.map(ColimaInstance::is_running).unwrap_or(false) {
        log::info!("Stopping Colima to apply requested size...");
        run_cmd("colima", &["stop"])?;
    }

    log::info!("Starting Colima with requested size...");
    start_colima(size, false)
}

fn start_colima(size: &SizeArgs, include_defaults: bool) -> Result<(), Box<dyn Error>> {
    let args = colima_start_args(size, include_defaults);
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    run_cmd("colima", &refs)
}

fn colima_start_args(size: &SizeArgs, include_defaults: bool) -> Vec<String> {
    let mut args = vec!["start".to_string(), "--kubernetes".to_string()];

    if let Some(cpus) = size.cpus.or(include_defaults.then_some(DEFAULT_CPUS)) {
        args.push("--cpus".to_string());
        args.push(cpus.to_string());
    }
    if let Some(memory) = size
        .memory
        .or(include_defaults.then_some(DEFAULT_MEMORY_GIB))
    {
        args.push("--memory".to_string());
        args.push(memory.to_string());
    }
    if let Some(disk) = size.disk.or(include_defaults.then_some(DEFAULT_DISK_GIB)) {
        args.push("--disk".to_string());
        args.push(disk.to_string());
    }

    args
}

fn confirm_running_resize(
    size: &SizeArgs,
    assume_yes: bool,
    changes: &[String],
) -> Result<(), Box<dyn Error>> {
    if assume_yes {
        return Ok(());
    }

    let change_text = changes.join(", ");
    let resize_command = format!("hops local resize{}", size.command_suffix());
    let start_command = format!("hops local start{} --yes", size.command_suffix());

    if !std::io::stdin().is_terminal() {
        return Err(format!(
            "Colima is already running with different size ({change_text}). Run `{resize_command}` first, or rerun `{start_command}` to stop and resize automatically."
        )
        .into());
    }

    let confirmed = Confirm::new()
        .with_prompt(format!(
            "Colima is already running with different size ({change_text}). Stop and restart it now?"
        ))
        .default(false)
        .interact()?;

    if confirmed {
        Ok(())
    } else {
        Err(format!(
            "Colima size was not changed. Run `{resize_command}` first, then rerun `hops local start`."
        )
        .into())
    }
}

fn validate_requested_size(
    size: &SizeArgs,
    instance: Option<&ColimaInstance>,
) -> Result<(), Box<dyn Error>> {
    if let (Some(requested), Some(current)) =
        (size.disk, instance.and_then(ColimaInstance::disk_gib))
    {
        if requested < current {
            return Err(format!(
                "Colima disk cannot be shrunk from {current}GiB to {requested}GiB. Use --disk {current} or larger, or destroy and recreate the VM."
            )
            .into());
        }
    }

    Ok(())
}

fn requested_size_changes(size: &SizeArgs, instance: &ColimaInstance) -> Vec<String> {
    let mut changes = Vec::new();

    if let Some(requested) = size.cpus {
        match instance.cpus {
            Some(current) if requested == current => {}
            Some(current) => changes.push(format!("cpus {current} -> {requested}")),
            None => changes.push(format!("cpus unknown -> {requested}")),
        }
    }
    if let Some(requested) = size.memory {
        match instance.memory_gib() {
            Some(current) if requested == current => {}
            Some(current) => changes.push(format!("memory {current}GiB -> {requested}GiB")),
            None => changes.push(format!("memory unknown -> {requested}GiB")),
        }
    }
    if let Some(requested) = size.disk {
        match instance.disk_gib() {
            Some(current) if requested == current => {}
            Some(current) => changes.push(format!("disk {current}GiB -> {requested}GiB")),
            None => changes.push(format!("disk unknown -> {requested}GiB")),
        }
    }

    changes
}

/// Whether a Colima instance exists (running or stopped). Missing binary or
/// failing command reads as "no instance".
pub fn instance_exists() -> bool {
    matches!(colima_instance(), Ok(Some(_)))
}

fn colima_instance() -> Result<Option<ColimaInstance>, Box<dyn Error>> {
    let output = match run_cmd_output("colima", &["list", "--json"]) {
        Ok(output) => output,
        Err(_) => return Ok(None),
    };

    parse_colima_list(&output)
}

fn parse_colima_list(output: &str) -> Result<Option<ColimaInstance>, Box<dyn Error>> {
    let trimmed = output.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }

    if let Ok(instance) = serde_json::from_str::<ColimaInstance>(trimmed) {
        return Ok(Some(instance));
    }

    if let Ok(instances) = serde_json::from_str::<Vec<ColimaInstance>>(trimmed) {
        return Ok(instances.into_iter().next());
    }

    for line in trimmed
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        if let Ok(instance) = serde_json::from_str::<ColimaInstance>(line) {
            return Ok(Some(instance));
        }
    }

    Err("Unable to parse `colima list --json` output".into())
}

fn bytes_to_gib(bytes: u64) -> u32 {
    (bytes / GIB) as u32
}

/// Add the cluster-internal registry to Docker's insecure-registries list
/// inside the Colima VM. Docker defaults to HTTPS for non-localhost registries;
/// our in-cluster registry speaks plain HTTP.
pub fn configure_docker_insecure_registry() -> Result<(), Box<dyn Error>> {
    let config = run_cmd_output("colima", &["ssh", "--", "cat", "/etc/docker/daemon.json"])?;

    if config.contains("insecure-registries") {
        return Ok(());
    }

    log::info!("Configuring Docker for insecure local registry...");

    // Insert the insecure-registries key before the final closing brace.
    let new_config = if let Some(pos) = config.rfind('}') {
        let prefix = config[..pos].trim_end();
        format!(
            "{},\n  \"insecure-registries\": [\"{}\"]\n}}\n",
            prefix, REGISTRY_PULL
        )
    } else {
        return Err("Invalid daemon.json: no closing brace".into());
    };

    let mut child = Command::new("colima")
        .args(["ssh", "--", "sudo", "tee", "/etc/docker/daemon.json"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()?;
    if let Some(ref mut stdin) = child.stdin {
        stdin.write_all(new_config.as_bytes())?;
    }
    let status = child.wait()?;
    if !status.success() {
        return Err("Failed to write Docker daemon.json".into());
    }

    log::info!("Restarting Docker daemon...");
    run_cmd(
        "colima",
        &["ssh", "--", "sudo", "systemctl", "restart", "docker"],
    )?;

    // Wait for Docker to come back.
    for _ in 0..30 {
        if run_cmd_output("docker", &["info"]).is_ok() {
            // Docker restart can temporarily disrupt the Kubernetes API.
            wait_for_kubernetes()?;
            return Ok(());
        }
        thread::sleep(Duration::from_secs(2));
    }
    Err("Docker did not come back after restart".into())
}

/// Ensure Colima's /etc/hosts maps the registry hostname to the current
/// ClusterIP so the kubelet's docker daemon can resolve pull refs.
pub fn sync_hosts_entry(cluster_ip: &str) -> Result<(), Box<dyn Error>> {
    let hostname = REGISTRY_HOSTNAME;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn instance(status: &str, cpus: u32, memory_gib: u32, disk_gib: u32) -> ColimaInstance {
        ColimaInstance {
            status: status.to_string(),
            cpus: Some(cpus),
            memory: Some(memory_gib as u64 * GIB),
            disk: Some(disk_gib as u64 * GIB),
        }
    }

    #[test]
    fn colima_start_args_use_hops_defaults_for_new_profiles() {
        let args = colima_start_args(&SizeArgs::default(), true);

        assert_eq!(
            args,
            vec![
                "start",
                "--kubernetes",
                "--cpus",
                "8",
                "--memory",
                "16",
                "--disk",
                "60"
            ]
        );
    }

    #[test]
    fn colima_start_args_pass_only_requested_size_for_existing_profiles() {
        let size = SizeArgs {
            cpus: Some(12),
            memory: Some(32),
            disk: None,
        };

        let args = colima_start_args(&size, false);

        assert_eq!(
            args,
            vec!["start", "--kubernetes", "--cpus", "12", "--memory", "32"]
        );
    }

    #[test]
    fn requested_size_changes_compare_only_explicit_fields() {
        let current = instance("Running", 8, 16, 60);
        let size = SizeArgs {
            cpus: None,
            memory: Some(32),
            disk: None,
        };

        assert_eq!(
            requested_size_changes(&size, &current),
            vec!["memory 16GiB -> 32GiB"]
        );
    }

    #[test]
    fn requested_size_changes_treat_missing_current_value_as_change() {
        let current = ColimaInstance {
            status: "Running".to_string(),
            cpus: None,
            memory: None,
            disk: None,
        };
        let size = SizeArgs {
            cpus: Some(12),
            memory: None,
            disk: None,
        };

        assert_eq!(
            requested_size_changes(&size, &current),
            vec!["cpus unknown -> 12"]
        );
    }

    #[test]
    fn parse_colima_list_accepts_single_object() {
        let output = r#"{"name":"default","status":"Stopped","arch":"aarch64","cpus":8,"memory":17179869184,"disk":64424509440,"runtime":"docker+k3s"}"#;

        let parsed = parse_colima_list(output).expect("parse").expect("instance");

        assert_eq!(parsed.status, "Stopped");
        assert_eq!(parsed.cpus, Some(8));
        assert_eq!(parsed.memory_gib(), Some(16));
        assert_eq!(parsed.disk_gib(), Some(60));
    }

    #[test]
    fn parse_colima_list_accepts_array_output() {
        let output = r#"[{"status":"Running","cpus":12,"memory":34359738368,"disk":107374182400}]"#;

        let parsed = parse_colima_list(output).expect("parse").expect("instance");

        assert!(parsed.is_running());
        assert_eq!(parsed.cpus, Some(12));
        assert_eq!(parsed.memory_gib(), Some(32));
        assert_eq!(parsed.disk_gib(), Some(100));
    }

    #[test]
    fn validate_requested_size_rejects_disk_shrink() {
        let current = instance("Stopped", 8, 16, 100);
        let size = SizeArgs {
            cpus: None,
            memory: None,
            disk: Some(60),
        };

        let err = validate_requested_size(&size, Some(&current)).expect_err("disk shrink");

        assert!(err.to_string().contains("cannot be shrunk"));
    }
}
