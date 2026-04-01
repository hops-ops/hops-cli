use clap::{Parser, Subcommand};
use std::error::Error;
mod commands;
mod logging;

#[derive(Parser, Debug)]
#[command(version, about = "hops CLI", long_about = None)]
struct Args {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Manage the local development environment
    Local(commands::local::LocalArgs),
    /// Manage Crossplane configuration packages in the connected cluster
    Config(commands::config::ConfigArgs),
    /// Manage validation helpers for Crossplane projects
    Validate(commands::validate::ValidateArgs),
    /// Manage live XR observe/manage/adopt workflows
    Xr(commands::xr::XrArgs),
}

fn main() -> Result<(), Box<dyn Error>> {
    logging::init_logging().expect("Failed to initialize logging");
    log::debug!("Starting hops CLI...");

    let args = Args::parse();
    log::debug!("Command line args: {:?}", args);

    match &args.command {
        Some(Commands::Local(local_args)) => {
            commands::local::run(local_args)?;
        }
        Some(Commands::Config(config_args)) => {
            commands::config::run(config_args)?;
        }
        Some(Commands::Validate(validate_args)) => {
            commands::validate::run(validate_args)?;
        }
        Some(Commands::Xr(xr_args)) => {
            commands::xr::run(xr_args)?;
        }
        None => {
            log::info!("No command specified, use --help for usage information");
        }
    }

    Ok(())
}
