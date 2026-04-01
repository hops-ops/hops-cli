mod generate_configuration;

use clap::{Args, Subcommand};
use std::error::Error;

#[derive(Args, Debug)]
pub struct ValidateArgs {
    #[command(subcommand)]
    pub command: ValidateCommands,
}

#[derive(Subcommand, Debug)]
pub enum ValidateCommands {
    /// Generate api metadata configuration.yaml from upbound.yaml
    GenerateConfiguration(generate_configuration::GenerateArgs),
}

pub fn run(args: &ValidateArgs) -> Result<(), Box<dyn Error>> {
    match &args.command {
        ValidateCommands::GenerateConfiguration(generate_args) => {
            generate_configuration::run(generate_args)
        }
    }
}
