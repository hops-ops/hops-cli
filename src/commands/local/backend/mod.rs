//! Local-cluster backend abstraction.
//!
//! Abstracts the node/VM-level operations that differ between local cluster
//! providers (lifecycle, sizing, registry trust). Everything kubectl/helm
//! shaped lives outside this module and is backend-agnostic.

mod colima;

use super::run_cmd_output;
use clap::Args;
use std::error::Error;

/// Sizing flags for backends with a resizable VM.
#[derive(Args, Debug, Clone, Default, PartialEq, Eq)]
pub struct SizeArgs {
    /// Number of CPUs to allocate to the cluster VM.
    #[arg(long = "cpus", visible_alias = "cpu", value_name = "N")]
    pub cpus: Option<u32>,

    /// Memory to allocate to the cluster VM, in GiB.
    #[arg(long, value_name = "GIB")]
    pub memory: Option<u32>,

    /// Disk size to allocate to the cluster VM, in GiB.
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

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Backend {
    Colima,
}

impl Backend {
    /// Human-facing backend name (also the persisted spelling).
    pub fn name(self) -> &'static str {
        match self {
            Backend::Colima => "colima",
        }
    }

    pub fn install(self) -> Result<(), Box<dyn Error>> {
        match self {
            Backend::Colima => colima::install(),
        }
    }

    pub fn uninstall(self) -> Result<(), Box<dyn Error>> {
        match self {
            Backend::Colima => colima::uninstall(),
        }
    }

    /// Bring the cluster up (create, start, or resize as needed). Does not
    /// wait for the Kubernetes API; callers follow with `wait_for_kubernetes`.
    pub fn start(self, size: &SizeArgs, assume_yes: bool) -> Result<(), Box<dyn Error>> {
        match self {
            Backend::Colima => colima::start(size, assume_yes),
        }
    }

    pub fn stop(self) -> Result<(), Box<dyn Error>> {
        match self {
            Backend::Colima => colima::stop(),
        }
    }

    pub fn destroy(self) -> Result<(), Box<dyn Error>> {
        match self {
            Backend::Colima => colima::destroy(),
        }
    }

    pub fn reset(self) -> Result<(), Box<dyn Error>> {
        match self {
            Backend::Colima => colima::reset(),
        }
    }

    pub fn resize(self, size: &SizeArgs) -> Result<(), Box<dyn Error>> {
        match self {
            Backend::Colima => colima::resize(size),
        }
    }

    /// Make the node runtime trust the in-cluster registry over HTTP.
    /// Runs before any images exist; must be safe to re-run.
    pub fn ensure_registry_trust(self) -> Result<(), Box<dyn Error>> {
        match self {
            Backend::Colima => colima::configure_docker_insecure_registry(),
        }
    }

    /// Point the node at the registry Service's current ClusterIP so pulls of
    /// both registry names resolve. Idempotent; re-run on every start because
    /// the ClusterIP changes if the Service is recreated.
    pub fn wire_registry(self, cluster_ip: &str) -> Result<(), Box<dyn Error>> {
        match self {
            Backend::Colima => colima::sync_hosts_entry(cluster_ip),
        }
    }
}

/// Resolve which backend to operate on.
pub fn resolve(flag: Option<Backend>) -> Backend {
    flag.unwrap_or(Backend::Colima)
}

/// Fetch the in-cluster registry Service's ClusterIP.
pub fn registry_cluster_ip() -> Result<String, Box<dyn Error>> {
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
