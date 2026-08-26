//! Local workbench library: Application reconcile, workspace registry,
//! gitops watch path filtering, source delivery selection, and host access URLs.
//!
//! Pure logic is unit-tested without a cluster; kubectl/helm live behind thin
//! adapters used by the CLI entrypoints.

pub mod application;
pub mod cluster_dns;
pub mod cluster_gitops;
pub mod controller;
pub mod definition;
pub mod delivery;
pub mod net;
pub mod reconcile;
pub mod registry;
pub mod watch;

// Selective re-exports used by CLI entrypoints.
pub use registry::{namespace_for_name, slugify_name};
