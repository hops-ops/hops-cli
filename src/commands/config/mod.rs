mod install;
mod uninstall;

use clap::{Args, Subcommand};
use std::error::Error;

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
