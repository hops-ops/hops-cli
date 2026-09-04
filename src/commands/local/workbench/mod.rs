//! Local workbench library: Environment reconcile, workspace registry, GitOps
//! watch path filtering, source delivery selection, browser ingress, and
//! optional direct Service access.
//!
//! Pure logic is unit-tested without a cluster; kubectl/helm live behind thin
//! adapters used by the CLI entrypoints.

pub mod cluster_dns;
pub mod cluster_gitops;
pub mod controller;
pub mod definition;
pub mod delivery;
pub mod ingress;
pub mod net;
pub mod reconcile;
pub mod registry;
pub mod watch;

// Selective re-exports used by CLI entrypoints.
pub use registry::slugify_name;
