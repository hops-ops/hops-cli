//! Local-cluster backend abstraction.
//!
//! Abstracts the node/VM-level operations that differ between local cluster
//! providers (lifecycle, sizing, registry trust). Everything kubectl/helm
//! shaped lives outside this module and is backend-agnostic.

mod colima;
mod kind;

use super::{local_state_dir, run_cmd_output};
use clap::Args;
use std::error::Error;
use std::fmt;
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
}

impl Backend {
    /// Human-facing backend name (also the persisted spelling).
    pub fn name(self) -> &'static str {
        match self {
            Backend::Colima => "colima",
            Backend::Kind => "kind",
        }
    }

    /// kubeconfig context name this backend's cluster registers under.
    pub fn kube_context(self) -> &'static str {
        match self {
            Backend::Colima => "colima",
            Backend::Kind => "kind-hops",
        }
    }

    pub fn install(self) -> Result<(), Box<dyn Error>> {
        match self {
            Backend::Colima => colima::install(),
            Backend::Kind => kind::install(),
        }
    }

    pub fn uninstall(self) -> Result<(), Box<dyn Error>> {
        match self {
            Backend::Colima => colima::uninstall(),
            Backend::Kind => kind::uninstall(),
        }
    }

    /// Bring the cluster up (create, start, or resize as needed). Does not
    /// wait for the Kubernetes API; callers follow with `wait_for_kubernetes`.
    pub fn start(self, size: &SizeArgs, assume_yes: bool) -> Result<(), Box<dyn Error>> {
        match self {
            Backend::Colima => colima::start(size, assume_yes),
            Backend::Kind => kind::start(size),
        }
    }

    pub fn stop(self) -> Result<(), Box<dyn Error>> {
        match self {
            Backend::Colima => colima::stop(),
            Backend::Kind => kind::stop(),
        }
    }

    pub fn destroy(self) -> Result<(), Box<dyn Error>> {
        match self {
            Backend::Colima => colima::destroy(),
            Backend::Kind => kind::destroy(),
        }
    }

    pub fn reset(self) -> Result<(), Box<dyn Error>> {
        match self {
            Backend::Colima => colima::reset(),
            Backend::Kind => kind::reset(),
        }
    }

    pub fn resize(self, size: &SizeArgs) -> Result<(), Box<dyn Error>> {
        match self {
            Backend::Colima => colima::resize(size),
            Backend::Kind => kind::resize(size),
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
        }
    }

    /// Point the node at the registry Service's current ClusterIP so pulls of
    /// both registry names resolve. Idempotent; re-run on every start because
    /// the ClusterIP changes if the Service is recreated.
    pub fn wire_registry(self, cluster_ip: &str) -> Result<(), Box<dyn Error>> {
        match self {
            Backend::Colima => colima::sync_hosts_entry(cluster_ip),
            Backend::Kind => kind::wire_registry(cluster_ip),
        }
    }
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
            other => Err(format!(
                "unknown backend '{}' (expected colima or kind)",
                other
            )),
        }
    }
}

const BACKEND_FILE: &str = "backend";

/// Resolve which backend to operate on: explicit flag > preference persisted
/// by the last successful start > detection of an existing cluster (colima
/// wins for back-compat with pre-backend installs) > platform default.
pub fn resolve(flag: Option<Backend>) -> Backend {
    resolve_from(
        flag,
        persisted(),
        colima::instance_exists,
        kind::cluster_exists,
        cfg!(target_os = "macos"),
    )
}

fn resolve_from(
    flag: Option<Backend>,
    persisted: Option<Backend>,
    colima_detected: impl FnOnce() -> bool,
    kind_detected: impl FnOnce() -> bool,
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
/// ClusterIP and apply the backend-specific trust/aliasing.
pub fn wire_local_registry(backend: Backend) -> Result<(), Box<dyn Error>> {
    backend.wire_registry(&registry_cluster_ip()?)
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
            true,
        );

        assert_eq!(resolved, Backend::Kind);
    }

    #[test]
    fn persisted_beats_detection() {
        let resolved = resolve_from(None, Some(Backend::Kind), || true, no_detect, true);

        assert_eq!(resolved, Backend::Kind);
    }

    #[test]
    fn colima_detection_beats_kind_detection() {
        let resolved = resolve_from(None, None, || true, || true, false);

        assert_eq!(resolved, Backend::Colima);
    }

    #[test]
    fn kind_detection_used_when_no_colima() {
        let resolved = resolve_from(None, None, no_detect, || true, true);

        assert_eq!(resolved, Backend::Kind);
    }

    #[test]
    fn platform_default_when_nothing_detected() {
        assert_eq!(
            resolve_from(None, None, no_detect, no_detect, true),
            Backend::Colima
        );
        assert_eq!(
            resolve_from(None, None, no_detect, no_detect, false),
            Backend::Kind
        );
    }

    #[test]
    fn backend_name_round_trips_through_from_str() {
        for backend in [Backend::Colima, Backend::Kind] {
            assert_eq!(backend.name().parse::<Backend>().unwrap(), backend);
        }
        assert!("dory".parse::<Backend>().is_err());
    }
}
