mod install;
mod uninstall;

use crate::commands::local::package_install::{sanitize_name_component, strip_registry};
use clap::{Args, Subcommand};
use std::error::Error;

fn configuration_name_from_package_ref(package_ref: &str) -> String {
    let without_digest = package_ref
        .trim()
        .split_once('@')
        .map(|(source, _)| source)
        .unwrap_or_else(|| package_ref.trim());
    let image_path = if let Some(slash) = without_digest.rfind('/') {
        let suffix = &without_digest[slash + 1..];
        suffix
            .rfind(':')
            .map(|colon| &without_digest[..slash + 1 + colon])
            .unwrap_or(without_digest)
    } else {
        without_digest
            .rfind(':')
            .map(|colon| &without_digest[..colon])
            .unwrap_or(without_digest)
    };

    strip_registry(image_path)
        .split('/')
        .map(sanitize_name_component)
        .collect::<Vec<_>>()
        .join("-")
}

#[derive(Args, Debug)]
pub struct ConfigArgs {
    #[command(subcommand)]
    pub command: ConfigCommands,
}

#[derive(Subcommand, Debug)]
pub enum ConfigCommands {
    /// Build and load a Crossplane configuration into the local cluster
    Install(install::ConfigArgs),
    /// Remove a Crossplane configuration and prune orphaned package dependencies
    Uninstall(uninstall::UnconfigArgs),
}

pub fn run(args: &ConfigArgs) -> Result<(), Box<dyn Error>> {
    match &args.command {
        ConfigCommands::Install(install_args) => install::run(install_args),
        ConfigCommands::Uninstall(uninstall_args) => uninstall::run(uninstall_args),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_object_name_uses_complete_oci_identity() {
        for package_ref in [
            "ghcr.io/hops-ops/secret-stack:v1.0.0",
            "ghcr.io/hops-ops/secret-stack@sha256:abc",
            "registry.example.com:5000/hops-ops/secret-stack:configuration",
        ] {
            assert_eq!(
                configuration_name_from_package_ref(package_ref),
                "hops-ops-secret-stack"
            );
        }
    }
}
