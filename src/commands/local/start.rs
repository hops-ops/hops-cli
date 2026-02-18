use super::{kubectl_apply_stdin, run_cmd, run_cmd_output};
use std::error::Error;
use std::io::Write;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

const DRC: &str = include_str!("../../../bootstrap/drc/local-dev.yaml");
const PROVIDER_HELM: &str = include_str!("../../../bootstrap/providers/provider-helm.yaml");
const PROVIDER_K8S: &str = include_str!("../../../bootstrap/providers/provider-kubernetes.yaml");
const PC_HELM: &str = include_str!("../../../bootstrap/helm/pc.yaml");
const PC_K8S: &str = include_str!("../../../bootstrap/k8s/pc.yaml");
const REGISTRY: &str = include_str!("../../../bootstrap/registry/registry.yaml");

/// Cluster-internal hostname for the package registry.
const REGISTRY_HOST: &str = "registry.crossplane-system.svc.cluster.local:5000";

pub fn run() -> Result<(), Box<dyn Error>> {
    // 1. Start Colima with Kubernetes
    log::info!("Starting Colima with Kubernetes...");
    run_cmd(
        "colima",
        &[
            "start",
            "--kubernetes",
            "--cpu",
            "8",
            "--memory",
            "16",
            "--disk",
            "60",
        ],
    )?;

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

    // 7. Deploy DRC (cluster-admin SA for provider pods)
    log::info!("Applying DeploymentRuntimeConfig...");
    kubectl_apply_stdin(DRC)?;

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
    configure_registry_hosts_entry()?;

    log::info!("Local environment is ready");
    Ok(())
}

/// Add the cluster-internal registry to Docker's insecure-registries list
/// inside the Colima VM. Docker defaults to HTTPS for non-localhost registries;
/// our in-cluster registry speaks plain HTTP.
fn configure_docker_insecure_registry() -> Result<(), Box<dyn Error>> {
    let config = run_cmd_output(
        "colima",
        &["ssh", "--", "cat", "/etc/docker/daemon.json"],
    )?;

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

/// Map the registry's cluster-internal hostname to its ClusterIP in the
/// VM's /etc/hosts so the kubelet (which can't use CoreDNS) can resolve it.
fn configure_registry_hosts_entry() -> Result<(), Box<dyn Error>> {
    let hostname = "registry.crossplane-system.svc.cluster.local";

    // Already present?
    let check = run_cmd_output(
        "colima",
        &["ssh", "--", "grep", "-q", hostname, "/etc/hosts"],
    );
    if check.is_ok() {
        return Ok(());
    }

    let cluster_ip = run_cmd_output(
        "kubectl",
        &[
            "get", "svc", "registry",
            "-n", "crossplane-system",
            "-o", "jsonpath={.spec.clusterIP}",
        ],
    )?;
    let cluster_ip = cluster_ip.trim();

    log::info!(
        "Adding hosts entry: {} -> {}",
        hostname,
        cluster_ip
    );
    let entry = format!("{} {}", cluster_ip, hostname);
    run_cmd(
        "colima",
        &[
            "ssh", "--", "sudo", "sh", "-c",
            &format!("echo '{}' >> /etc/hosts", entry),
        ],
    )?;

    Ok(())
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
