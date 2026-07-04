use super::backend::{self, SizeArgs};
use super::{kubectl_apply_stdin, run_cmd, run_cmd_output, wait_for_kubernetes};
use clap::Args;
use std::error::Error;
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

#[derive(Args, Debug, Clone)]
pub struct StartArgs {
    #[command(flatten)]
    pub size: SizeArgs,

    /// Stop and restart a running cluster VM without prompting when requested size differs.
    #[arg(long)]
    pub yes: bool,
}

pub fn run(backend: backend::Backend, args: &StartArgs) -> Result<(), Box<dyn Error>> {
    // 1. Bring the backend cluster up
    backend.start(&args.size, args.yes)?;

    // Remember the choice so stop/destroy/doctor and package installs target
    // this backend without needing the flag again.
    backend::persist(backend)?;

    // 2. Wait for the Kubernetes API to become reachable.
    //    The backend may return immediately ("already running") before the
    //    API server is ready, or a fresh start needs time to initialise.
    wait_for_kubernetes()?;

    // 3. Make the node runtime trust the cluster-internal registry over HTTP.
    backend.ensure_registry_trust()?;

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

    // 12. Point the node at the registry Service's ClusterIP so pulls of the
    //     cluster-internal registry names resolve.
    backend::wire_local_registry(backend)?;

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
