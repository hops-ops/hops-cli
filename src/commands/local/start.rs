use super::{kubectl_apply_stdin, run_cmd, run_cmd_output};
use std::error::Error;
use std::thread;
use std::time::Duration;

const PROVIDER_HELM: &str = include_str!("../../../bootstrap/providers/provider-helm.yaml");
const PROVIDER_K8S: &str = include_str!("../../../bootstrap/providers/provider-kubernetes.yaml");
const PC_HELM: &str = include_str!("../../../bootstrap/helm/pc.yaml");
const PC_K8S: &str = include_str!("../../../bootstrap/k8s/pc.yaml");
const REGISTRY: &str = include_str!("../../../bootstrap/registry/registry.yaml");

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

    // 2. Add Crossplane Helm repo
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

    // 3. Install Crossplane
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

    // 4. Wait for Crossplane deployment
    log::info!("Waiting for Crossplane to be ready...");
    wait_for_deployment("crossplane-system", "crossplane")?;

    // 5. Install providers
    log::info!("Installing providers...");
    kubectl_apply_stdin(PROVIDER_HELM)?;
    kubectl_apply_stdin(PROVIDER_K8S)?;

    // 6. Wait for provider CRDs
    log::info!("Waiting for provider CRDs...");
    wait_for_crd("providerconfigs.helm.m.crossplane.io")?;
    wait_for_crd("providerconfigs.kubernetes.m.crossplane.io")?;

    // 7. Apply ProviderConfigs
    log::info!("Applying ProviderConfigs...");
    kubectl_apply_stdin(PC_HELM)?;
    kubectl_apply_stdin(PC_K8S)?;

    // 8. Deploy local OCI registry for Crossplane packages
    log::info!("Deploying local package registry...");
    kubectl_apply_stdin(REGISTRY)?;
    wait_for_deployment("crossplane-system", "registry")?;

    log::info!("Local environment is ready");
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
