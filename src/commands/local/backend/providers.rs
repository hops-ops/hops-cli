//! Cluster-provider vs docker-provider (LWB-REQ-260…263).
//!
//! Pure resolution of the two provider dimensions. Side effects (persist,
//! DOCKER_HOST) stay in the backend lifecycle layer.

use super::Backend;
use serde::{Deserialize, Serialize};
use std::error::Error;
use std::fmt;
use std::str::FromStr;

/// How Kubernetes nodes are provisioned.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, clap::ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ClusterProvider {
    /// hops-managed kind node(s) (+ extraMounts).
    Kind,
    /// Stock Dory product k3s (`dory-k8s`).
    Dory,
    /// Colima embedded k3s.
    Colima,
}

/// Container engine kind/tools talk to.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, clap::ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DockerProvider {
    /// Dory engine (`~/.dory/dory.sock`).
    Dory,
    /// Colima docker.
    Colima,
    /// Default / DOCKER_HOST / docker context.
    Docker,
}

impl ClusterProvider {
    pub fn as_str(self) -> &'static str {
        match self {
            ClusterProvider::Kind => "kind",
            ClusterProvider::Dory => "dory",
            ClusterProvider::Colima => "colima",
        }
    }
}

impl DockerProvider {
    pub fn as_str(self) -> &'static str {
        match self {
            DockerProvider::Dory => "dory",
            DockerProvider::Colima => "colima",
            DockerProvider::Docker => "docker",
        }
    }
}

impl fmt::Display for ClusterProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for DockerProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ClusterProvider {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "kind" => Ok(ClusterProvider::Kind),
            "dory" => Ok(ClusterProvider::Dory),
            "colima" => Ok(ClusterProvider::Colima),
            other => Err(format!(
                "unknown cluster-provider '{other}' (expected kind, dory, colima)"
            )),
        }
    }
}

impl FromStr for DockerProvider {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "dory" => Ok(DockerProvider::Dory),
            "colima" => Ok(DockerProvider::Colima),
            "docker" | "default" => Ok(DockerProvider::Docker),
            other => Err(format!(
                "unknown docker-provider '{other}' (expected dory, colima, docker)"
            )),
        }
    }
}

/// Resolved pair for lifecycle + engine targeting.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderPair {
    pub cluster: ClusterProvider,
    pub docker: DockerProvider,
}

impl ProviderPair {
    /// Preferred Mac hostPath path: kind nodes on Dory engine.
    pub fn kind_on_dory() -> Self {
        ProviderPair {
            cluster: ClusterProvider::Kind,
            docker: DockerProvider::Dory,
        }
    }

    /// Lifecycle backend enum used by existing start/stop/install paths.
    pub fn as_backend(self) -> Backend {
        match self.cluster {
            ClusterProvider::Kind => Backend::Kind,
            ClusterProvider::Dory => Backend::Dory,
            ClusterProvider::Colima => Backend::Colima,
        }
    }

    /// Reject impossible combinations (product dory k8s only on dory engine).
    pub fn validate(self) -> Result<(), String> {
        match (self.cluster, self.docker) {
            (ClusterProvider::Kind, _) => Ok(()),
            (ClusterProvider::Dory, DockerProvider::Dory) => Ok(()),
            (ClusterProvider::Colima, DockerProvider::Colima) => Ok(()),
            (ClusterProvider::Dory, other) => Err(format!(
                "cluster-provider dory requires docker-provider dory (got {other})"
            )),
            (ClusterProvider::Colima, other) => Err(format!(
                "cluster-provider colima requires docker-provider colima (got {other})"
            )),
        }
    }
}

/// Resolve providers from CLI flags.
///
/// Precedence:
/// 1. Explicit `--cluster-provider` / `--docker-provider` (pair, with defaults for missing half)
/// 2. `None` → caller uses persisted provider/backend state or detection
pub fn resolve_provider_pair(
    cluster_provider: Option<ClusterProvider>,
    docker_provider: Option<DockerProvider>,
) -> Result<Option<ProviderPair>, Box<dyn Error>> {
    if cluster_provider.is_none() && docker_provider.is_none() {
        return Ok(None);
    }

    // When either provider is set, fill the missing half from platform defaults.
    let base = if cfg!(target_os = "macos") {
        ProviderPair::kind_on_dory()
    } else {
        ProviderPair {
            cluster: ClusterProvider::Kind,
            docker: DockerProvider::Docker,
        }
    };

    let pair = ProviderPair {
        cluster: cluster_provider.unwrap_or(base.cluster),
        docker: docker_provider.unwrap_or(base.docker),
    };
    pair.validate()?;
    Ok(Some(pair))
}

/// Translate the deprecated one-dimensional `--backend` flag into the
/// provider pair used by the current CLI.
///
/// `kind` retains the platform default that was already used when only one
/// provider dimension was supplied: Dory on macOS and the default Docker
/// engine elsewhere. Product Dory and Colima remain self-paired.
pub fn provider_pair_for_legacy_backend(backend: Backend) -> ProviderPair {
    match backend {
        Backend::Kind if cfg!(target_os = "macos") => ProviderPair::kind_on_dory(),
        Backend::Kind => ProviderPair {
            cluster: ClusterProvider::Kind,
            docker: DockerProvider::Docker,
        },
        Backend::Dory => ProviderPair {
            cluster: ClusterProvider::Dory,
            docker: DockerProvider::Dory,
        },
        Backend::Colima => ProviderPair {
            cluster: ClusterProvider::Colima,
            docker: DockerProvider::Colima,
        },
    }
}

/// Apply docker-provider to process env for kind (and docker CLI).
///
/// - `dory`: set DOCKER_HOST to `unix://$HOME/.dory/dory.sock` when unset
/// - `colima`: leave alone (colima context usually already selected)
/// - `docker`: leave alone
pub fn apply_docker_provider_env(dp: DockerProvider) -> Result<(), Box<dyn Error>> {
    match dp {
        DockerProvider::Dory => {
            if std::env::var_os("DOCKER_HOST").is_none() {
                let home = std::env::var("HOME").map_err(|_| "HOME is not set")?;
                let sock = std::path::Path::new(&home).join(".dory/dory.sock");
                if sock.exists() {
                    let host = format!("unix://{}", sock.display());
                    log::info!("docker-provider dory: DOCKER_HOST={host}");
                    std::env::set_var("DOCKER_HOST", host);
                } else {
                    return Err(format!(
                        "docker-provider dory: socket {} missing; open the Dory app",
                        sock.display()
                    )
                    .into());
                }
            }
            Ok(())
        }
        DockerProvider::Colima | DockerProvider::Docker => Ok(()),
    }
}

const PROVIDERS_FILE: &str = "providers.json";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PersistedProviders {
    pub cluster_provider: String,
    pub docker_provider: String,
    #[serde(default)]
    pub cluster_name: Option<String>,
}

pub fn persist_providers(
    pair: ProviderPair,
    cluster_name: Option<&str>,
) -> Result<(), Box<dyn Error>> {
    let dir = super::super::local_state_dir()?;
    std::fs::create_dir_all(&dir)?;
    let rec = PersistedProviders {
        cluster_provider: pair.cluster.as_str().to_string(),
        docker_provider: pair.docker.as_str().to_string(),
        cluster_name: cluster_name.map(|s| s.to_string()),
    };
    let path = dir.join(PROVIDERS_FILE);
    std::fs::write(path, serde_json::to_string_pretty(&rec)?)?;
    // Keep legacy backend file in sync for older code paths.
    super::persist(pair.as_backend())?;
    Ok(())
}

pub fn load_persisted_providers() -> Option<PersistedProviders> {
    let path = super::super::local_state_dir().ok()?.join(PROVIDERS_FILE);
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_on_dory_is_valid() {
        let p = ProviderPair::kind_on_dory();
        p.validate().unwrap();
        assert_eq!(p.as_backend(), Backend::Kind);
    }

    #[test]
    fn dory_cluster_rejects_non_dory_docker() {
        let p = ProviderPair {
            cluster: ClusterProvider::Dory,
            docker: DockerProvider::Docker,
        };
        assert!(p.validate().is_err());
    }

    #[test]
    fn resolve_cp_dp_without_backend() {
        let p = resolve_provider_pair(Some(ClusterProvider::Kind), Some(DockerProvider::Dory))
            .unwrap()
            .unwrap();
        assert_eq!(p, ProviderPair::kind_on_dory());
    }

    #[test]
    fn resolve_neither_returns_none() {
        assert!(resolve_provider_pair(None, None).unwrap().is_none());
    }

    #[test]
    fn provider_pair_matrix_preserves_baseline_acceptance() {
        let accepted = [
            (ClusterProvider::Kind, DockerProvider::Dory),
            (ClusterProvider::Kind, DockerProvider::Colima),
            (ClusterProvider::Kind, DockerProvider::Docker),
            (ClusterProvider::Dory, DockerProvider::Dory),
            (ClusterProvider::Colima, DockerProvider::Colima),
        ];
        let rejected = [
            (ClusterProvider::Dory, DockerProvider::Colima),
            (ClusterProvider::Dory, DockerProvider::Docker),
            (ClusterProvider::Colima, DockerProvider::Dory),
            (ClusterProvider::Colima, DockerProvider::Docker),
        ];

        for (cluster, docker) in accepted {
            let pair = ProviderPair { cluster, docker };
            assert_eq!(
                resolve_provider_pair(Some(cluster), Some(docker))
                    .unwrap()
                    .unwrap(),
                pair,
                "expected {cluster}+{docker} accepted"
            );
        }
        for (cluster, docker) in rejected {
            let error = resolve_provider_pair(Some(cluster), Some(docker)).unwrap_err();
            assert_eq!(
                error.to_string(),
                format!(
                    "cluster-provider {cluster} requires docker-provider {cluster} (got {docker})"
                ),
                "expected {cluster}+{docker} rejected with the baseline diagnostic"
            );
        }
    }

    #[test]
    fn provider_partial_input_matrix_preserves_platform_defaults() {
        let default_docker = if cfg!(target_os = "macos") {
            DockerProvider::Dory
        } else {
            DockerProvider::Docker
        };

        assert_eq!(
            resolve_provider_pair(Some(ClusterProvider::Kind), None)
                .unwrap()
                .unwrap(),
            ProviderPair {
                cluster: ClusterProvider::Kind,
                docker: default_docker,
            }
        );
        assert_eq!(
            resolve_provider_pair(None, Some(DockerProvider::Dory))
                .unwrap()
                .unwrap(),
            ProviderPair::kind_on_dory()
        );
        assert_eq!(
            resolve_provider_pair(None, Some(DockerProvider::Colima))
                .unwrap()
                .unwrap(),
            ProviderPair {
                cluster: ClusterProvider::Kind,
                docker: DockerProvider::Colima,
            }
        );
        assert_eq!(
            resolve_provider_pair(None, Some(DockerProvider::Docker))
                .unwrap()
                .unwrap(),
            ProviderPair {
                cluster: ClusterProvider::Kind,
                docker: DockerProvider::Docker,
            }
        );

        let colima_error = resolve_provider_pair(Some(ClusterProvider::Colima), None).unwrap_err();
        assert_eq!(
            colima_error.to_string(),
            format!(
                "cluster-provider colima requires docker-provider colima (got {default_docker})"
            )
        );

        let dory = resolve_provider_pair(Some(ClusterProvider::Dory), None);
        if cfg!(target_os = "macos") {
            assert_eq!(
                dory.unwrap().unwrap(),
                ProviderPair {
                    cluster: ClusterProvider::Dory,
                    docker: DockerProvider::Dory,
                }
            );
        } else {
            assert_eq!(
                dory.unwrap_err().to_string(),
                "cluster-provider dory requires docker-provider dory (got docker)"
            );
        }
    }

    #[test]
    fn deprecated_backend_maps_to_provider_defaults() {
        let kind = provider_pair_for_legacy_backend(Backend::Kind);
        assert_eq!(kind.cluster, ClusterProvider::Kind);
        assert_eq!(
            kind.docker,
            if cfg!(target_os = "macos") {
                DockerProvider::Dory
            } else {
                DockerProvider::Docker
            }
        );
        assert_eq!(
            provider_pair_for_legacy_backend(Backend::Dory),
            ProviderPair {
                cluster: ClusterProvider::Dory,
                docker: DockerProvider::Dory,
            }
        );
        assert_eq!(
            provider_pair_for_legacy_backend(Backend::Colima),
            ProviderPair {
                cluster: ClusterProvider::Colima,
                docker: DockerProvider::Colima,
            }
        );
    }
}
