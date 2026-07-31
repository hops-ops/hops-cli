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

    // After a docker daemon restart (colima path), re-confirm the node is Ready
    // so Crossplane pods are not stuck Pending on NotReady.
    log::info!("Waiting for Kubernetes nodes to be Ready...");
    run_cmd(
        "kubectl",
        &[
            "wait",
            "--for=condition=Ready",
            "nodes",
            "--all",
            "--timeout=300s",
        ],
    )?;

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
    // Update only our repo — a bare `helm repo update` fails outright when any
    // unrelated repo in the user's helm config has gone stale.
    run_cmd("helm", &["repo", "update", "crossplane-stable"])?;

    // 5. Install Crossplane
    //
    // Do not use helm --wait: on nested-virt CI (colima/GHA) image pulls and
    // scheduling can exceed helm's single wait window, and a failed --wait
    // leaves us without structured kubectl diagnostics. Apply the chart, then
    // poll deployments ourselves with a longer budget + failure dumps.
    //
    // After stop/start the k3s API can report nodes Ready while openapi/v2 is
    // still timing out (helm validate fails). Retry helm with API re-probes.
    log::info!("Installing Crossplane...");
    {
        let helm_args = [
            "upgrade",
            "--install",
            "crossplane",
            "crossplane-stable/crossplane",
            "-n",
            "crossplane-system",
            "--create-namespace",
            "--timeout",
            "5m",
        ];
        let mut last_err: Option<Box<dyn Error>> = None;
        for attempt in 1..=6 {
            wait_for_kubernetes()?;
            match run_cmd("helm", &helm_args) {
                Ok(()) => {
                    last_err = None;
                    break;
                }
                Err(e) => {
                    log::warn!("helm install attempt {attempt}/6 failed: {e}");
                    last_err = Some(e);
                    std::thread::sleep(std::time::Duration::from_secs(20));
                }
            }
        }
        if let Some(e) = last_err {
            return Err(e);
        }
    }

    // 6. Wait for Crossplane core deployment.
    // rbac-manager can flap under nested-virt resource pressure; the core
    // controller is what providers need. Best-effort wait for rbac-manager.
    log::info!("Waiting for Crossplane to be ready...");
    wait_for_deployment_with_diagnostics("crossplane-system", "crossplane")?;
    if let Err(e) = wait_for_deployment_attempts("crossplane-system", "crossplane-rbac-manager", 36)
    {
        log::warn!(
            "crossplane-rbac-manager not Available yet ({e}); continuing — core Crossplane is ready"
        );
    }

    // API can be briefly overloaded right after Crossplane becomes leader.
    wait_for_kubernetes()?;

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

    // Re-check API before ProviderConfigs (openapi validation can time out if
    // the apiserver is still busy — apply also uses --validate=false).
    wait_for_kubernetes()?;

    // 10. Apply ProviderConfigs
    log::info!("Applying ProviderConfigs...");
    kubectl_apply_stdin(PC_HELM)?;
    kubectl_apply_stdin(PC_K8S)?;

    // 11. Wait for providers Healthy.
    //
    // CRDs can become Established while the provider runtime pods are still
    // pulling images / rolling. `hops local doctor` requires Healthy=True, and
    // the kind smoke path runs doctor immediately after start — without this
    // wait, cold CI flakes with "Provider healthy — Healthy=False".
    log::info!("Waiting for providers to become Healthy...");
    wait_for_provider_healthy("crossplane-contrib-provider-kubernetes")?;
    wait_for_provider_healthy("crossplane-contrib-provider-helm")?;

    // 12. In-cluster package registry (all backends).
    //
    // Crossplane package pulls run in the pod network → Service DNS + ClusterIP.
    // Docker push: colima/kind → localhost:30500 (host NodePort); dory →
    // {dory-k8s-ip}:30500 on the engine docker bridge (daemon is in-engine).
    wait_for_kubernetes()?;
    log::info!("Pre-pulling registry:2 (best effort)...");
    let _ = run_cmd("docker", &["pull", "registry:2"]);
    // TLS secret + registry Deployment + Crossplane CA trust (package manager is HTTPS-only).
    log::info!("Deploying local package registry (HTTPS)...");
    backend.ensure_package_registry()?;
    wait_for_deployment_with_diagnostics("crossplane-system", "registry")?;
    backend::wire_local_registry(backend)?;

    log::info!("Local environment is ready");
    Ok(())
}

/// Longer wait used for Crossplane on cold nested-virt runners (~15 minutes).
fn wait_for_deployment_with_diagnostics(
    namespace: &str,
    name: &str,
) -> Result<(), Box<dyn Error>> {
    match wait_for_deployment_attempts(namespace, name, 180) {
        Ok(()) => Ok(()),
        Err(e) => {
            dump_namespace_diagnostics(namespace);
            Err(e)
        }
    }
}

fn wait_for_deployment_attempts(
    namespace: &str,
    name: &str,
    attempts: u32,
) -> Result<(), Box<dyn Error>> {
    for i in 0..attempts {
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

        // Periodic progress so CI logs show the wait is alive.
        if i > 0 && i % 12 == 0 {
            log::info!(
                "Still waiting for deployment {}/{} ({}s elapsed)...",
                namespace,
                name,
                i * 5
            );
            let _ = run_cmd(
                "kubectl",
                &["get", "pods", "-n", namespace, "-o", "wide"],
            );
        }

        thread::sleep(Duration::from_secs(5));
    }
    Err(format!("Timed out waiting for deployment {}/{}", namespace, name).into())
}

fn dump_namespace_diagnostics(namespace: &str) {
    log::error!("Diagnostics for namespace {} after readiness timeout:", namespace);
    let _ = run_cmd("kubectl", &["get", "pods", "-n", namespace, "-o", "wide"]);
    let _ = run_cmd("kubectl", &["describe", "pods", "-n", namespace]);
    let _ = run_cmd(
        "kubectl",
        &[
            "get",
            "events",
            "-n",
            namespace,
            "--sort-by=.lastTimestamp",
        ],
    );
    let _ = run_cmd("kubectl", &["get", "nodes", "-o", "wide"]);
}

/// Poll until a CRD exists **and** is Established (API serves the kind).
///
/// Merely creating the CRD object is not enough: under load the apiserver can
/// return the CRD while discovery still lacks the kind, causing
/// `no matches for kind "ProviderConfig"` on the next apply.
fn wait_for_crd(crd: &str) -> Result<(), Box<dyn Error>> {
    log::info!("Waiting for CRD {}...", crd);
    for _ in 0..120 {
        let exists = run_cmd_output("kubectl", &["get", "crd", crd]).is_ok();
        if exists {
            let established = run_cmd_output(
                "kubectl",
                &[
                    "get",
                    "crd",
                    crd,
                    "-o",
                    "jsonpath={.status.conditions[?(@.type==\"Established\")].status}",
                ],
            )
            .unwrap_or_default();
            if established.trim() == "True" {
                // Brief settle so discovery caches pick up the new kind.
                thread::sleep(Duration::from_secs(2));
                return Ok(());
            }
        }
        thread::sleep(Duration::from_secs(5));
    }
    Err(format!("Timed out waiting for CRD {} to be Established", crd).into())
}

/// Poll until a Crossplane Provider reports Healthy=True (~10 minutes).
///
/// Package install can establish CRDs before the runtime Deployment is Ready.
/// Matching doctor's expectation here keeps `hops local start` from returning
/// while the cluster still looks half-bootstrapped.
fn wait_for_provider_healthy(provider: &str) -> Result<(), Box<dyn Error>> {
    log::info!("Waiting for Provider {} Healthy...", provider);
    for i in 0..120 {
        let healthy = run_cmd_output(
            "kubectl",
            &[
                "get",
                "provider.pkg.crossplane.io",
                provider,
                "-o",
                "jsonpath={.status.conditions[?(@.type==\"Healthy\")].status}",
            ],
        )
        .unwrap_or_default();
        if healthy.trim() == "True" {
            return Ok(());
        }

        if i > 0 && i % 12 == 0 {
            let installed = run_cmd_output(
                "kubectl",
                &[
                    "get",
                    "provider.pkg.crossplane.io",
                    provider,
                    "-o",
                    "jsonpath={.status.conditions[?(@.type==\"Installed\")].status}",
                ],
            )
            .unwrap_or_default();
            log::info!(
                "Still waiting for Provider {} Healthy ({}s elapsed; Installed={}, Healthy={})...",
                provider,
                i * 5,
                if installed.trim().is_empty() {
                    "<none>"
                } else {
                    installed.trim()
                },
                if healthy.trim().is_empty() {
                    "<none>"
                } else {
                    healthy.trim()
                }
            );
            let _ = run_cmd(
                "kubectl",
                &["get", "pods", "-n", "crossplane-system", "-o", "wide"],
            );
            let _ = run_cmd(
                "kubectl",
                &["get", "provider.pkg.crossplane.io", provider, "-o", "yaml"],
            );
        }

        thread::sleep(Duration::from_secs(5));
    }

    let _ = run_cmd(
        "kubectl",
        &["get", "provider.pkg.crossplane.io", provider, "-o", "yaml"],
    );
    dump_namespace_diagnostics("crossplane-system");
    Err(format!(
        "Timed out waiting for Provider {} to become Healthy",
        provider
    )
    .into())
}
