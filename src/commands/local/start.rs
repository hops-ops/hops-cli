use super::{kubectl_apply_stdin, run_cmd, run_cmd_output, sync_registry_hosts_entry};
use clap::Args;
use dialoguer::Confirm;
use serde::Deserialize;
use std::error::Error;
use std::io::{IsTerminal, Write};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

// Per-provider DRCs — never shared. Each pins its own cluster-admin SA so the
// providers can never clobber each other's runtime config. See bootstrap/drc/.
const DRC_K8S: &str = include_str!("../../../bootstrap/drc/kubernetes.yaml");
const DRC_HELM: &str = include_str!("../../../bootstrap/drc/helm.yaml");
const PROVIDER_HELM: &str = include_str!("../../../bootstrap/providers/provider-helm.yaml");
const PROVIDER_K8S: &str = include_str!("../../../bootstrap/providers/provider-kubernetes.yaml");
const PC_HELM: &str = include_str!("../../../bootstrap/helm/pc.yaml");
const PC_K8S: &str = include_str!("../../../bootstrap/k8s/pc.yaml");
const REGISTRY: &str = include_str!("../../../bootstrap/registry/registry.yaml");

/// Cluster-internal hostname for the package registry.
const REGISTRY_HOST: &str = "registry.crossplane-system.svc.cluster.local:5000";
const REGISTRY_HOSTNAME: &str = "registry.crossplane-system.svc.cluster.local";

const DEFAULT_CPUS: u32 = 8;
const DEFAULT_MEMORY_GIB: u32 = 16;
const DEFAULT_DISK_GIB: u32 = 60;
const GIB: u64 = 1024 * 1024 * 1024;

#[derive(Args, Debug, Clone)]
pub struct StartArgs {
    #[command(flatten)]
    pub size: ColimaSizeArgs,

    /// Stop and restart a running Colima VM without prompting when requested size differs.
    #[arg(long)]
    pub yes: bool,
}

#[derive(Args, Debug, Clone, Default, PartialEq, Eq)]
pub struct ColimaSizeArgs {
    /// Number of CPUs to allocate to the Colima VM.
    #[arg(long = "cpus", visible_alias = "cpu", value_name = "N")]
    pub cpus: Option<u32>,

    /// Memory to allocate to the Colima VM, in GiB.
    #[arg(long, value_name = "GIB")]
    pub memory: Option<u32>,

    /// Disk size to allocate to the Colima VM, in GiB.
    #[arg(long, value_name = "GIB")]
    pub disk: Option<u32>,
}

impl ColimaSizeArgs {
    pub fn any_set(&self) -> bool {
        self.cpus.is_some() || self.memory.is_some() || self.disk.is_some()
    }

    pub fn command_suffix(&self) -> String {
        let mut parts = Vec::new();
        if let Some(cpus) = self.cpus {
            parts.push(format!("--cpus {}", cpus));
        }
        if let Some(memory) = self.memory {
            parts.push(format!("--memory {}", memory));
        }
        if let Some(disk) = self.disk {
            parts.push(format!("--disk {}", disk));
        }

        if parts.is_empty() {
            String::new()
        } else {
            format!(" {}", parts.join(" "))
        }
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub(crate) struct ColimaInstance {
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

pub fn run(args: &StartArgs) -> Result<(), Box<dyn Error>> {
    let instance = colima_instance()?;
    validate_requested_size(&args.size, instance.as_ref())?;

    // 1. Start Colima with Kubernetes
    start_or_resize_colima(args, instance.as_ref())?;

    // 2. Wait for the Kubernetes API to become reachable.
    //    Colima may return immediately ("already running") before the
    //    API server is ready, or a fresh start needs time to initialise.
    wait_for_kubernetes()?;

    // 3. Configure Docker in the VM to allow HTTP pulls from the
    //    cluster-internal registry. Without this the kubelet's Docker
    //    daemon defaults to HTTPS and fails.
    configure_docker_insecure_registry()?;

    // 4. Add Crossplane Helm repo
    log::info!("Adding Crossplane Helm repo...");
    run_cmd(
        "helm",
        &[
            "repo",
            "add",
            "crossplane-stable",
            "https://charts.crossplane.io/stable",
        ],
    )?;
    run_cmd("helm", &["repo", "update"])?;

    // 5. Install Crossplane
    log::info!("Installing Crossplane...");
    run_cmd(
        "helm",
        &[
            "upgrade",
            "--install",
            "crossplane",
            "crossplane-stable/crossplane",
            "-n",
            "crossplane-system",
            "--create-namespace",
            "--wait",
            "--timeout",
            "5m",
        ],
    )?;

    // 6. Wait for Crossplane deployment
    log::info!("Waiting for Crossplane to be ready...");
    wait_for_deployment("crossplane-system", "crossplane")?;

    // 7. Deploy per-provider DRCs (each pins its own cluster-admin SA)
    log::info!("Applying DeploymentRuntimeConfigs (per-provider)...");
    kubectl_apply_stdin(DRC_K8S)?;
    kubectl_apply_stdin(DRC_HELM)?;

    // 8. Install providers
    log::info!("Installing providers...");
    kubectl_apply_stdin(PROVIDER_HELM)?;
    kubectl_apply_stdin(PROVIDER_K8S)?;

    // 9. Wait for provider CRDs
    log::info!("Waiting for provider CRDs...");
    wait_for_crd("providerconfigs.helm.m.crossplane.io")?;
    wait_for_crd("providerconfigs.kubernetes.m.crossplane.io")?;

    // 10. Apply ProviderConfigs
    log::info!("Applying ProviderConfigs...");
    kubectl_apply_stdin(PC_HELM)?;
    kubectl_apply_stdin(PC_K8S)?;

    // 11. Deploy local OCI registry for Crossplane packages
    log::info!("Deploying local package registry...");
    kubectl_apply_stdin(REGISTRY)?;
    wait_for_deployment("crossplane-system", "registry")?;

    // 12. Map the registry's cluster-internal hostname to its ClusterIP
    //     inside the VM so the kubelet can resolve it.
    sync_registry_hosts_entry("crossplane-system", "registry", REGISTRY_HOSTNAME)?;

    log::info!("Local environment is ready");
    Ok(())
}

fn start_or_resize_colima(
    args: &StartArgs,
    instance: Option<&ColimaInstance>,
) -> Result<(), Box<dyn Error>> {
    let is_running = instance.map(ColimaInstance::is_running).unwrap_or(false);

    if is_running && args.size.any_set() {
        let changes = requested_size_changes(&args.size, instance.expect("checked is_running"));
        if !changes.is_empty() {
            confirm_running_resize(args, &changes)?;
            resize_existing_colima(&args.size, instance)?;
            return Ok(());
        }

        log::info!("Requested Colima size already matches the running VM");
    }

    log::info!("Starting Colima with Kubernetes...");

    let size = if is_running {
        ColimaSizeArgs::default()
    } else {
        args.size.clone()
    };
    let include_defaults = instance.is_none();
    start_colima(&size, include_defaults)
}

pub(crate) fn resize_colima(size: &ColimaSizeArgs) -> Result<(), Box<dyn Error>> {
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

fn resize_existing_colima(
    size: &ColimaSizeArgs,
    instance: Option<&ColimaInstance>,
) -> Result<(), Box<dyn Error>> {
    if instance.map(ColimaInstance::is_running).unwrap_or(false) {
        log::info!("Stopping Colima to apply requested size...");
        run_cmd("colima", &["stop"])?;
    }

    log::info!("Starting Colima with requested size...");
    start_colima(size, false)
}

fn start_colima(size: &ColimaSizeArgs, include_defaults: bool) -> Result<(), Box<dyn Error>> {
    let args = colima_start_args(size, include_defaults);
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    run_cmd("colima", &refs)
}

fn colima_start_args(size: &ColimaSizeArgs, include_defaults: bool) -> Vec<String> {
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

fn confirm_running_resize(args: &StartArgs, changes: &[String]) -> Result<(), Box<dyn Error>> {
    if args.yes {
        return Ok(());
    }

    let change_text = changes.join(", ");
    let resize_command = format!("hops local resize{}", args.size.command_suffix());
    let start_command = format!("hops local start{} --yes", args.size.command_suffix());

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
    size: &ColimaSizeArgs,
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

fn requested_size_changes(size: &ColimaSizeArgs, instance: &ColimaInstance) -> Vec<String> {
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

pub(crate) fn colima_instance() -> Result<Option<ColimaInstance>, Box<dyn Error>> {
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
fn configure_docker_insecure_registry() -> Result<(), Box<dyn Error>> {
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
            prefix, REGISTRY_HOST
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

/// Poll until the Kubernetes API server is reachable.
fn wait_for_kubernetes() -> Result<(), Box<dyn Error>> {
    log::info!("Waiting for Kubernetes API...");
    for _ in 0..60 {
        let result = run_cmd_output("kubectl", &["cluster-info"]);
        if result.is_ok() {
            return Ok(());
        }
        thread::sleep(Duration::from_secs(5));
    }
    Err("Timed out waiting for Kubernetes API".into())
}

/// Poll until a deployment's Available condition is True.
fn wait_for_deployment(namespace: &str, name: &str) -> Result<(), Box<dyn Error>> {
    for _ in 0..60 {
        let output = run_cmd_output(
            "kubectl",
            &[
                "get",
                "deployment",
                name,
                "-n",
                namespace,
                "-o",
                "jsonpath={.status.conditions[?(@.type==\"Available\")].status}",
            ],
        );

        if let Ok(status) = output {
            if status.trim() == "True" {
                return Ok(());
            }
        }

        thread::sleep(Duration::from_secs(5));
    }
    Err(format!("Timed out waiting for deployment {}/{}", namespace, name).into())
}

/// Poll until a CRD exists in the cluster.
fn wait_for_crd(crd: &str) -> Result<(), Box<dyn Error>> {
    log::info!("Waiting for CRD {}...", crd);
    for _ in 0..60 {
        let result = run_cmd_output("kubectl", &["get", "crd", crd]);
        if result.is_ok() {
            return Ok(());
        }
        thread::sleep(Duration::from_secs(5));
    }
    Err(format!("Timed out waiting for CRD {}", crd).into())
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
        let args = colima_start_args(&ColimaSizeArgs::default(), true);

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
        let size = ColimaSizeArgs {
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
        let size = ColimaSizeArgs {
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
        let size = ColimaSizeArgs {
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
        let size = ColimaSizeArgs {
            cpus: None,
            memory: None,
            disk: Some(60),
        };

        let err = validate_requested_size(&size, Some(&current)).expect_err("disk shrink");

        assert!(err.to_string().contains("cannot be shrunk"));
    }
}
