mod generate_configuration;

use clap::{Args, Subcommand};
use std::error::Error;

#[derive(Args, Debug)]
pub struct ConfigArgs {
    #[command(subcommand)]
    pub command: ConfigCommands,
}

#[derive(Subcommand, Debug)]
pub enum ConfigCommands {
    /// Generate api metadata configuration.yaml from upbound.yaml
    Generate(generate_configuration::GenerateArgs),
}

pub fn run(args: &ConfigArgs) -> Result<(), Box<dyn Error>> {
    match &args.command {
        ConfigCommands::Generate(generate_args) => generate_configuration::run(generate_args),
    }
}
