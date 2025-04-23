use clap::{Parser, Subcommand};
use std::error::Error;
mod logging;

#[derive(Parser, Debug)]
#[command(version, about = "Command line tool scaffold", long_about = None)]
struct Args {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Bootstrap the application
    Bootstrap,
}

fn main() -> Result<(), Box<dyn Error>> {
    logging::init_logging().expect("Failed to initialize logging");
    log::debug!("Starting command line tool...");

    let args = Args::parse();
    log::debug!("Command line args: {:?}", args);

    match &args.command {
        Some(Commands::Bootstrap) => {
            log::info!("Bootstrap command was called");
            // Add bootstrap implementation here
        }
        None => {
            log::info!("No command specified, use --help for usage information");
        }
    }

    Ok(())
}
