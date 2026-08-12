//! Local-cluster backend abstraction.
//!
//! Abstracts the node/VM-level operations that differ between local cluster
//! providers (lifecycle, sizing, registry trust). Everything kubectl/helm
//! shaped lives outside this module and is backend-agnostic.

mod colima;
mod dory;
pub(crate) mod kind;
pub mod providers;

pub use providers::{
    apply_docker_provider_env, load_persisted_providers, persist_providers, resolve_provider_pair,
    ClusterProvider, DockerProvider,
};

use super::{local_state_dir, run_cmd_output, HOPS_KUBE_CONTEXT_ENV};
use clap::Args;
use std::error::Error;
use std::fmt;
use std::process::Command;
use std::str::FromStr;

/// Sizing flags for backends with a resizable VM.
#[derive(Args, Debug, Clone, Default, PartialEq, Eq)]
pub struct SizeArgs {
    /// Number of CPUs to allocate to the cluster VM (colima backend only).
    #[arg(long = "cpus", visible_alias = "cpu", value_name = "N")]
    pub cpus: Option<u32>,

    /// Memory to allocate to the cluster VM, in GiB (colima backend only).
    #[arg(long, value_name = "GIB")]
    pub memory: Option<u32>,

    /// Disk size to allocate to the cluster VM, in GiB (colima backend only).
    #[arg(long, value_name = "GIB")]
    pub disk: Option<u32>,
}

impl SizeArgs {
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

#[derive(Copy, Clone, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Backend {
    /// VM + dockerd + k3s (macOS/Linux)
    Colima,
    /// docker containers as nodes; works on any docker daemon
    /// (Docker Desktop, colima, dory, CI runners)
    Kind,
    /// k3s on dory's shared-VM engine, via the `dory` CLI
    Dory,
}

impl Backend {
    /// Human-facing backend name (also the persisted spelling).
    pub fn name(self) -> &'static str {
        match self {
            Backend::Colima => "colima",
            Backend::Kind => "kind",
            Backend::Dory => "dory",
        }
    }

    /// kubeconfig context name this backend's cluster registers under.
    pub fn kube_context(self) -> String {
        match self {
            Backend::Colima => "colima".to_string(),
            Backend::Kind => kind::kube_context_name(),
            // Merged into ~/.kube/config (default name hops-dory; see dory::context_name).
            Backend::Dory => dory::context_name(),
        }
    }

    pub fn install(self) -> Result<(), Box<dyn Error>> {
        match self {
            Backend::Colima => colima::install(),
            Backend::Kind => kind::install(),
            Backend::Dory => dory::install(),
        }
    }

    pub fn uninstall(self) -> Result<(), Box<dyn Error>> {
        match self {
            Backend::Colima => colima::uninstall(),
            Backend::Kind => kind::uninstall(),
            Backend::Dory => dory::uninstall(),
        }
    }

    /// Whether this backend's local cluster/VM exists, running or stopped.
    pub fn cluster_exists(self) -> bool {
        match self {
            Backend::Colima => colima::instance_exists(),
            Backend::Kind => kind::cluster_exists(),
            Backend::Dory => dory::cluster_exists(),
        }
    }

    /// Bring the cluster up (create, start, or resize as needed). Does not
    /// wait for the Kubernetes API; callers follow with `wait_for_kubernetes`.
    pub fn start(self, size: &SizeArgs, assume_yes: bool) -> Result<(), Box<dyn Error>> {
        match self {
            Backend::Colima => colima::start(size, assume_yes),
            Backend::Kind => kind::start(size),
            Backend::Dory => dory::start(size),
        }
    }

    pub fn stop(self) -> Result<(), Box<dyn Error>> {
        match self {
            Backend::Colima => colima::stop(),
            Backend::Kind => kind::stop(),
            Backend::Dory => dory::stop(),
        }
    }

    pub fn destroy(self) -> Result<(), Box<dyn Error>> {
        match self {
            Backend::Colima => colima::destroy(),
            Backend::Kind => kind::destroy(),
            Backend::Dory => dory::destroy(),
        }
    }

    pub fn reset(self) -> Result<(), Box<dyn Error>> {
        match self {
            Backend::Colima => colima::reset(),
            Backend::Kind => kind::reset(),
            Backend::Dory => dory::reset(),
        }
    }

    pub fn resize(self, size: &SizeArgs) -> Result<(), Box<dyn Error>> {
        match self {
            Backend::Colima => colima::resize(size),
            Backend::Kind => kind::resize(size),
            Backend::Dory => dory::resize(size),
        }
    }

    /// Make the node runtime trust the in-cluster registry over HTTP.
    /// Runs before any images exist; must be safe to re-run.
    pub fn ensure_registry_trust(self) -> Result<(), Box<dyn Error>> {
        match self {
            Backend::Colima => colima::configure_docker_insecure_registry(),
            // containerd trust is per-name via certs.d, written in
            // wire_registry once the registry Service's ClusterIP is known.
            Backend::Kind => Ok(()),
            // k3s registries.yaml is written in wire_registry once ClusterIP is known.
            Backend::Dory => dory::ensure_registry_trust(),
        }
    }

    /// Point the node at the registry Service's current ClusterIP so pulls of
    /// both registry names resolve. Idempotent; re-run on every start because
    /// the ClusterIP changes if the Service is recreated.
    ///
    /// Dory also publishes host localhost:30500 → node NodePort (engine proxy).
    pub fn wire_registry(self, cluster_ip: &str) -> Result<(), Box<dyn Error>> {
        match self {
            Backend::Colima => colima::sync_hosts_entry(cluster_ip),
            Backend::Kind => kind::wire_registry(cluster_ip),
            Backend::Dory => dory::wire_registry(cluster_ip),
        }
    }

    /// Cluster-provider-specific package registry for local provider/config installs.
    /// Every cluster provider uses an in-cluster NodePort registry for Crossplane
    /// package pulls (pod network).
    pub fn ensure_package_registry(self) -> Result<(), Box<dyn Error>> {
        crate::commands::local::package_install::ensure_incluster_registry()
    }

    /// Pull address used in Crossplane package / ImageConfig refs for this backend.
    /// Always the in-cluster Service — Crossplane runs in the pod network.
    pub fn registry_pull(self) -> &'static str {
        crate::commands::local::package_install::REGISTRY_PULL_INCLUSTER
    }

    /// Docker push host:port for package installs on this backend.
    pub fn registry_push(self) -> String {
        match self {
            Backend::Dory => dory::registry_push_addr().unwrap_or_else(|_| {
                crate::commands::local::package_install::REGISTRY_PUSH.to_string()
            }),
            // Use explicit IPv4. Dory's dockerd can resolve `localhost` to ::1,
            // where its published kind port terminates the TLS request with EOF.
            Backend::Kind => kind_registry_push(kind::registry_host_port()),
            Backend::Colima => crate::commands::local::package_install::REGISTRY_PUSH.to_string(),
        }
    }
}

fn kind_registry_push(host_port: u16) -> String {
    format!("127.0.0.1:{host_port}")
}

impl fmt::Display for Backend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl FromStr for Backend {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim() {
            "colima" => Ok(Backend::Colima),
            "kind" => Ok(Backend::Kind),
            "dory" => Ok(Backend::Dory),
            other => Err(format!(
                "unknown persisted cluster provider '{}' (expected colima, kind, or dory)",
                other
            )),
        }
    }
}

const BACKEND_FILE: &str = "backend";

/// Persist the Dory desktop context name (`--dory-name`, default hops-dory).
pub fn persist_dory_context_name(name: &str) -> Result<(), Box<dyn Error>> {
    dory::persist_context_name(name)
}

pub fn platform_default() -> Backend {
    if cfg!(target_os = "macos") {
        Backend::Colima
    } else {
        Backend::Kind
    }
}

/// Resolve which backend to operate on: explicit flag > preference persisted
/// by the last successful start > detection of an existing cluster (colima
/// wins for back-compat with pre-backend installs) > platform default.
pub fn resolve(flag: Option<Backend>) -> Backend {
    resolve_from(
        flag,
        persisted(),
        colima::instance_exists,
        kind::cluster_exists,
        dory::cluster_exists,
        platform_default() == Backend::Colima,
    )
}

/// Resolve the backend adapter from the optional provider pair, then activate.
pub fn activate_with_providers(
    cluster_provider: Option<ClusterProvider>,
    docker_provider: Option<DockerProvider>,
    cluster_name: Option<&str>,
    context: Option<&str>,
) -> Result<Backend, Box<dyn Error>> {
    if let Some(name) = cluster_name.map(str::trim).filter(|s| !s.is_empty()) {
        kind::set_active_cluster_name(name);
    } else if let Some(persisted) = load_persisted_providers() {
        if let Some(name) = persisted.cluster_name.as_deref() {
            kind::set_active_cluster_name(name);
        }
    }

    let pair = resolve_provider_pair(cluster_provider, docker_provider)?;
    let backend = match pair {
        Some(p) => {
            apply_docker_provider_env(p.docker)?;
            let cname = kind::active_cluster_name();
            let _ = persist_providers(p, Some(cname.as_str()));
            p.as_backend()
        }
        None => resolve(None),
    };

    // Kind + default docker provider still auto-picks dory.sock inside kind module.
    Ok(activate(Some(backend), context))
}

/// Resolve the backend once and activate the kube-targeting environment for
/// child kubectl/helm processes.
pub fn activate(flag: Option<Backend>, context: Option<&str>) -> Backend {
    let backend = resolve(flag);

    if backend == Backend::Dory {
        // Merge ~/.kube/dory-config → named context and prefer matching docker context.
        if let Err(e) = dory::ensure_desktop_integration() {
            log::warn!("dory desktop integration incomplete: {e}");
            dory::export_kubeconfig_env();
        }
    }

    let explicit_context = context.filter(|ctx| !ctx.is_empty());
    let backend_context = backend.kube_context();
    let backend_context_exists = if explicit_context.is_none() {
        kube_context_exists(&backend_context)
    } else {
        false
    };

    match kube_context_export(explicit_context, &backend_context, backend_context_exists) {
        KubeContextExport::Set(ctx) => std::env::set_var(HOPS_KUBE_CONTEXT_ENV, ctx),
        KubeContextExport::Unset { missing_context } => {
            std::env::remove_var(HOPS_KUBE_CONTEXT_ENV);
            log::warn!(
                "Kubernetes context '{}' for cluster provider '{}' was not found; using kubeconfig current-context. Pass --context to target a specific cluster.",
                missing_context,
                backend.name()
            );
        }
    }

    backend
}

fn resolve_from(
    flag: Option<Backend>,
    persisted: Option<Backend>,
    colima_detected: impl FnOnce() -> bool,
    kind_detected: impl FnOnce() -> bool,
    dory_detected: impl FnOnce() -> bool,
    macos: bool,
) -> Backend {
    if let Some(backend) = flag {
        return backend;
    }
    if let Some(backend) = persisted {
        return backend;
    }
    if colima_detected() {
        return Backend::Colima;
    }
    if kind_detected() {
        return Backend::Kind;
    }
    if dory_detected() {
        return Backend::Dory;
    }
    if macos {
        Backend::Colima
    } else {
        Backend::Kind
    }
}

fn persisted() -> Option<Backend> {
    let path = local_state_dir().ok()?.join(BACKEND_FILE);
    std::fs::read_to_string(path).ok()?.parse().ok()
}

/// Record the backend so later invocations (stop, destroy, doctor, installs)
/// target the same cluster without re-detection.
pub fn persist(backend: Backend) -> Result<(), Box<dyn Error>> {
    let dir = local_state_dir()?;
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join(BACKEND_FILE), format!("{}\n", backend.name()))?;
    Ok(())
}

/// Drop the persisted preference (used by uninstall; destroy keeps it).
pub fn clear_persisted() -> Result<(), Box<dyn Error>> {
    match std::fs::remove_file(local_state_dir()?.join(BACKEND_FILE)) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err.into()),
    }
}

/// Fetch the in-cluster registry Service's ClusterIP.
fn registry_cluster_ip() -> Result<String, Box<dyn Error>> {
    let cluster_ip = run_cmd_output(
        "kubectl",
        &[
            "get",
            "svc",
            "registry",
            "-n",
            "crossplane-system",
            "-o",
            "jsonpath={.spec.clusterIP}",
        ],
    )?;
    let cluster_ip = cluster_ip.trim().to_string();
    if cluster_ip.is_empty() {
        return Err("Service crossplane-system/registry has no ClusterIP".into());
    }
    Ok(cluster_ip)
}

/// Wire node-level registry pulls for a backend: fetch the registry Service
/// ClusterIP and apply the backend-specific trust/aliasing (and Dory host publish).
pub fn wire_local_registry(backend: Backend) -> Result<(), Box<dyn Error>> {
    backend.wire_registry(&registry_cluster_ip()?)
}

pub fn should_wire_local_registry(
    provider_selected: bool,
    context: Option<&str>,
    backend: Backend,
) -> bool {
    if provider_selected {
        return true;
    }

    match context.filter(|ctx| !ctx.is_empty()) {
        Some(ctx) => ctx == backend.kube_context(),
        None => true,
    }
}

pub fn wire_local_registry_for_target(
    backend: Backend,
    provider_selected: bool,
    context: Option<&str>,
) -> Result<(), Box<dyn Error>> {
    if should_wire_local_registry(provider_selected, context, backend) {
        return wire_local_registry(backend);
    }

    log::warn!(
        "registry node wiring skipped: explicit --context does not match the selected cluster provider"
    );
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
enum KubeContextExport<'a> {
    Set(&'a str),
    Unset { missing_context: &'a str },
}

fn kube_context_export<'a>(
    explicit_context: Option<&'a str>,
    backend_context: &'a str,
    backend_context_exists: bool,
) -> KubeContextExport<'a> {
    if let Some(ctx) = explicit_context {
        return KubeContextExport::Set(ctx);
    }

    if backend_context_exists {
        KubeContextExport::Set(backend_context)
    } else {
        KubeContextExport::Unset {
            missing_context: backend_context,
        }
    }
}

fn kube_context_exists(context: &str) -> bool {
    let output = Command::new("kubectl")
        .args(["config", "get-contexts", "-o", "name"])
        .output();

    let Ok(output) = output else {
        return false;
    };
    if !output.status.success() {
        return false;
    }

    String::from_utf8_lossy(&output.stdout)
        .lines()
        .any(|line| line.trim() == context)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_detect() -> bool {
        false
    }

    #[test]
    fn flag_beats_persisted_and_detection() {
        let resolved = resolve_from(
            Some(Backend::Kind),
            Some(Backend::Colima),
            || true,
            no_detect,
            no_detect,
            true,
        );

        assert_eq!(resolved, Backend::Kind);
    }

    #[test]
    fn persisted_beats_detection() {
        let resolved = resolve_from(
            None,
            Some(Backend::Kind),
            || true,
            no_detect,
            no_detect,
            true,
        );

        assert_eq!(resolved, Backend::Kind);
    }

    #[test]
    fn colima_detection_beats_kind_detection() {
        let resolved = resolve_from(None, None, || true, || true, || true, false);

        assert_eq!(resolved, Backend::Colima);
    }

    #[test]
    fn dory_detection_used_when_no_colima_or_kind() {
        let resolved = resolve_from(None, None, no_detect, no_detect, || true, true);

        assert_eq!(resolved, Backend::Dory);
    }

    #[test]
    fn kind_detection_used_when_no_colima() {
        let resolved = resolve_from(None, None, no_detect, || true, no_detect, true);

        assert_eq!(resolved, Backend::Kind);
    }

    #[test]
    fn platform_default_when_nothing_detected() {
        assert_eq!(
            resolve_from(None, None, no_detect, no_detect, no_detect, true),
            Backend::Colima
        );
        assert_eq!(
            resolve_from(None, None, no_detect, no_detect, no_detect, false),
            Backend::Kind
        );
    }

    #[test]
    fn backend_name_round_trips_through_from_str() {
        for backend in [Backend::Colima, Backend::Kind, Backend::Dory] {
            assert_eq!(backend.name().parse::<Backend>().unwrap(), backend);
        }
        assert!("podman".parse::<Backend>().is_err());
    }

    #[test]
    fn explicit_context_is_exported_even_when_backend_context_is_absent() {
        assert_eq!(
            kube_context_export(Some("foreign"), "colima", false),
            KubeContextExport::Set("foreign")
        );
    }

    #[test]
    fn backend_context_is_exported_only_when_present() {
        assert_eq!(
            kube_context_export(None, "kind-hops", true),
            KubeContextExport::Set("kind-hops")
        );
        assert_eq!(
            kube_context_export(None, "kind-hops", false),
            KubeContextExport::Unset {
                missing_context: "kind-hops"
            }
        );
    }

    #[test]
    fn registry_wiring_allowed_without_explicit_context() {
        assert!(should_wire_local_registry(false, None, Backend::Colima));
    }

    #[test]
    fn registry_wiring_skips_foreign_explicit_context_without_provider_selection() {
        assert!(!should_wire_local_registry(
            false,
            Some("kind-hops"),
            Backend::Colima
        ));
    }

    #[test]
    fn registry_wiring_allowed_when_context_matches_backend() {
        assert!(should_wire_local_registry(
            false,
            Some("kind-hops"),
            Backend::Kind
        ));
    }

    #[test]
    fn registry_wiring_allowed_when_provider_is_explicit() {
        assert!(should_wire_local_registry(
            true,
            Some("foreign"),
            Backend::Colima
        ));
    }

    #[test]
    fn kind_registry_push_uses_ipv4_loopback() {
        assert_eq!(kind_registry_push(30501), "127.0.0.1:30501");
    }
}
