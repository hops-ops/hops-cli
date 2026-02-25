use super::{start_kubefwd, stop_kubefwd};
use clap::{Args, Subcommand};
use std::error::Error;

#[derive(Args, Debug)]
pub struct KubefwdArgs {
    #[command(subcommand)]
    pub command: KubefwdCommands,
}

#[derive(Subcommand, Debug)]
pub enum KubefwdCommands {
    /// Start kubefwd in the background (resyncs every 30s)
    Start,
    /// Stop kubefwd started by this CLI
    Stop,
    /// Restart kubefwd immediately to refresh forwards
    Refresh,
}

pub fn run(args: &KubefwdArgs) -> Result<(), Box<dyn Error>> {
    match &args.command {
        KubefwdCommands::Start => start(),
        KubefwdCommands::Stop => stop(),
        KubefwdCommands::Refresh => refresh(),
    }
}

pub fn start() -> Result<(), Box<dyn Error>> {
    start_kubefwd()
}

pub fn stop() -> Result<(), Box<dyn Error>> {
    stop_kubefwd()
}

pub fn refresh() -> Result<(), Box<dyn Error>> {
    log::info!("Refreshing kubefwd...");
    stop()?;
    start()?;
    Ok(())
}
