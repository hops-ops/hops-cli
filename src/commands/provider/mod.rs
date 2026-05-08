mod install;

use clap::{Args, Subcommand};
use std::error::Error;

#[derive(Args, Debug)]
pub struct ProviderArgs {
    #[command(subcommand)]
    pub command: ProviderCommands,
}

#[derive(Subcommand, Debug)]
pub enum ProviderCommands {
    /// Build and load a Crossplane Provider package into the local cluster
    Install(install::ProviderInstallArgs),
}

pub fn run(args: &ProviderArgs) -> Result<(), Box<dyn Error>> {
    match &args.command {
        ProviderCommands::Install(install_args) => install::run(install_args),
    }
}
