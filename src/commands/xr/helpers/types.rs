use clap::{Args, Subcommand};

pub(crate) const NETWORK_TAG_KEY: &str = "hops.ops.com.ai/network";
pub(crate) const SUBNET_TIER_TAG_KEY: &str = "hops.ops.com.ai/tier";
pub(crate) const ROUTE_TABLE_AZ_TAG_KEY: &str = "hops.ops.com.ai/az";

#[derive(Args, Debug)]
pub struct XrArgs {
    #[command(subcommand)]
    pub command: XrCommand,
}

#[derive(Subcommand, Debug)]
pub enum XrCommand {
    /// Generate an observe-only XR manifest for an existing resource
    Observe(ObserveArgs),
    /// Reconcile additional non-identity XR spec fields from live backing systems
    Reconcile(ReconcileArgs),
    /// Generate the final managed XR manifest from an observed/adopted XR
    Manage(ManageXrArgs),
    /// Patch an observe XR with import identities so it can attach to existing resources
    Adopt(AdoptArgs),
    /// Render managed-resource patches that remove Delete from management policies
    Orphan(OrphanArgs),
}

#[derive(Args, Debug)]
pub struct ObserveArgs {
    /// XR kind, plural, or project slug (for example: Network, networks, network)
    #[arg(long)]
    pub kind: String,

    /// Kubernetes object name and AWS lookup selector
    #[arg(long)]
    pub name: String,

    /// Namespace to write into the generated manifest
    #[arg(long, default_value = "default")]
    pub namespace: String,

    /// AWS region used for live AWS discovery
    #[arg(long)]
    pub aws_region: String,

    /// Write the generated manifest to a file instead of stdout
    #[arg(long)]
    pub output: Option<String>,

    /// Apply the generated manifest to the cluster
    #[arg(long)]
    pub apply: bool,
}

#[derive(Args, Debug)]
pub struct ManageXrArgs {
    /// XR kind, plural, or project slug (for example: Network, networks, network)
    #[arg(long)]
    pub kind: String,

    /// Kubernetes object name and AWS lookup selector
    #[arg(long)]
    pub name: String,

    /// Namespace of the existing XR
    #[arg(long, default_value = "default")]
    pub namespace: String,

    /// Also write the resulting object to a file
    #[arg(long)]
    pub output: Option<String>,

    /// Apply the generated manifest to the cluster
    #[arg(long)]
    pub apply: bool,
}

#[derive(Args, Debug)]
pub struct ReconcileArgs {
    /// XR kind, plural, or project slug (for example: Network, networks, network)
    #[arg(long)]
    pub kind: String,

    /// Kubernetes object name and AWS lookup selector
    #[arg(long)]
    pub name: String,

    /// Namespace of the existing XR
    #[arg(long, default_value = "default")]
    pub namespace: String,

    /// AWS region used for live AWS discovery
    #[arg(long)]
    pub aws_region: String,

    /// Also write the resulting object to a file
    #[arg(long)]
    pub output: Option<String>,

    /// Apply the generated manifest to the cluster
    #[arg(long)]
    pub apply: bool,
}

#[derive(Args, Debug)]
pub struct AdoptArgs {
    /// XR kind, plural, or project slug (for example: Network, networks, network)
    #[arg(long)]
    pub kind: String,

    /// Kubernetes object name and AWS lookup selector
    #[arg(long)]
    pub name: String,

    /// Namespace of the existing XR
    #[arg(long, default_value = "default")]
    pub namespace: String,

    /// Also write the resulting object to a file
    #[arg(long)]
    pub output: Option<String>,

    /// Apply the generated patches to the cluster
    #[arg(long)]
    pub apply: bool,
}

#[derive(Args, Debug)]
pub struct OrphanArgs {
    /// XR kind, plural, or project slug (for example: Network, networks, network)
    #[arg(long)]
    pub kind: String,

    /// Kubernetes object name and AWS lookup selector
    #[arg(long)]
    pub name: String,

    /// Namespace of the existing XR
    #[arg(long, default_value = "default")]
    pub namespace: String,

    /// Also write the resulting object to a file
    #[arg(long)]
    pub output: Option<String>,

    /// Apply the generated patches to the cluster
    #[arg(long)]
    pub apply: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct ReclaimSpec {
    pub api_version: String,
    pub kind: String,
    pub plural: String,
    pub group: String,
    pub project_slug: String,
    pub composed_resources: Vec<ResourceRef>,
    pub live_resolver: Option<String>,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct ResourceRef {
    pub api_version: String,
    pub kind: String,
}

#[derive(Debug)]
pub(crate) struct ReclaimReport {
    pub spec: ReclaimSpec,
    pub live_notes: Vec<String>,
    pub cluster_notes: Vec<String>,
    pub source: ManifestSource,
}

#[derive(Debug, Clone)]
pub(crate) struct ManagedResourcePatch {
    pub api_version: String,
    pub kind: String,
    pub namespace: String,
    pub name: String,
    pub external_name: Option<String>,
    pub management_policies: Option<Vec<String>>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum ManifestSource {
    Cluster,
    Generated,
}
