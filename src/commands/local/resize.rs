use super::backend::{Backend, SizeArgs};
use clap::Args;
use std::error::Error;

#[derive(Args, Debug, Clone)]
pub struct ResizeArgs {
    #[command(flatten)]
    pub size: SizeArgs,
}

pub fn run(backend: Backend, args: &ResizeArgs) -> Result<(), Box<dyn Error>> {
    backend.resize(&args.size)?;
    log::info!("Resize complete");
    Ok(())
}
